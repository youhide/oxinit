//! Loading units, starting them in order, and keeping them running.

use std::collections::BTreeMap;
use std::process::{Child, Command};
use std::time::Duration;

use rustix::process::{Pid, Signal};

use oxinit_cgroup::Cgroup;
use oxinit_service::{Exit, Instance, State, StopCause};
use oxinit_unit::{Kind, Resources, ServiceType, Unit};

use crate::cgroup::{self, Cgroups};
use crate::notify::{self, Notify};
use crate::reap;
use crate::sys::raw::{setup_child, ChildSetup};
use crate::timer::{Alarm, Timers};

pub struct Supervisor {
    instances: BTreeMap<String, Instance>,
    /// Resolved start order. Also the order things are reported in.
    order: Vec<String>,
    pub timers: Timers,
    pub notify: Notify,
    pub cgroups: Cgroups,
}

impl Supervisor {
    /// Load and resolve. Starting is a separate step so the caller can
    /// register the timerfd with epoll first.
    pub fn load(hostname: &str, timers: Timers, notify: Notify, cgroups: Cgroups) -> Self {
        let loaded = oxinit_unit::load_default(hostname);
        for error in &loaded.errors {
            eprintln!("oxinit: {error}");
        }

        let instances = BTreeMap::new();
        let mut order = Vec::new();

        if loaded.units.is_empty() {
            eprintln!("oxinit: no units found");
            return Self {
                instances,
                order,
                timers,
                notify,
                cgroups,
            };
        }

        // A broken graph is refused whole rather than partially applied: a
        // half-started machine matches no configuration on disk.
        match oxinit_graph::resolve(&loaded.units) {
            Ok(plan) => {
                for warning in &plan.warnings {
                    eprintln!("oxinit: {warning}");
                }
                order = plan.order;
            }
            Err(e) => {
                eprintln!("oxinit: {e}");
                eprintln!("oxinit: refusing to start with an unresolvable unit set");
            }
        }

        let mut supervisor = Self {
            instances,
            order,
            timers,
            notify,
            cgroups,
        };

        for name in supervisor.order.clone() {
            let Some(unit) = loaded.units.get(&name) else {
                continue;
            };

            // Every cgroup is created now, before anything starts, so the
            // whole set of `cgroup.events` descriptors can be registered with
            // epoll in one pass and never touched again. A restart reuses the
            // cgroup rather than churning the hierarchy and the registration.
            if matches!(unit.kind, Kind::Service(_)) {
                if let Err(e) = supervisor.cgroups.add(&name, &unit.full_name()) {
                    eprintln!("oxinit: {name}: {e}");
                }
            }

            supervisor
                .instances
                .insert(name.clone(), Instance::new(unit.clone()));
        }

        supervisor
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Start everything, in resolved order.
    pub fn start_all(&mut self) {
        for name in self.order.clone() {
            self.start(&name);
        }
    }

    fn start(&mut self, name: &str) {
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };

        if matches!(instance.unit.kind, Kind::Target) {
            instance.entered_active();
            println!("oxinit: reached target {name}");
            return;
        }

        let ty = instance.unit.service().map(|service| service.ty);
        let resources = instance
            .unit
            .service()
            .map_or_else(Resources::default, |service| service.resources);

        let cgroup = self.cgroups.get(name);

        // A limit that cannot be applied is not a degraded mode, it is a
        // different configuration. The unit asked to be capped; running it
        // uncapped is not a smaller version of that.
        if let Err(e) = apply_resources(&resources, cgroup) {
            if let Some(instance) = self.instances.get_mut(name) {
                instance.failed_to_start();
            }
            eprintln!("oxinit: {name}: {e}");
            return;
        }

        let spawned = self
            .instances
            .get(name)
            .map(|instance| spawn(&instance.unit, cgroup));

        let Some(spawned) = spawned else {
            return;
        };

        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };

        match spawned {
            Ok(child) => {
                let pid = child.id();
                instance.entered_activating(child);

                match ty {
                    // Ready when the process exists, and no more accurate than
                    // that: a unit ordered after a `simple` service may start
                    // before that service can accept connections.
                    Some(ServiceType::Simple) => instance.entered_active(),

                    // Ready when the process exits successfully. Stays
                    // Activating until then.
                    Some(ServiceType::Oneshot) => {}

                    // Ready when READY=1 arrives on the notify socket.
                    Some(ServiceType::Notify) => {}

                    // Ready when the initial process exits and the cgroup is
                    // still populated. Both halves are decided in on_sigchld;
                    // there is nothing to do until the parent goes away.
                    Some(ServiceType::Forking) => {}

                    None => {}
                }

                let ready = instance.state == State::Active;
                println!("oxinit: started {name} as pid {pid}");
                if ready {
                    self.report_usage(name);
                }
            }
            Err(e) => {
                instance.failed_to_start();
                eprintln!("oxinit: {name}: {e}");
            }
        }
    }

    /// Reap everything that exited and apply each unit's restart policy.
    pub fn on_sigchld(&mut self) {
        for (pid, status) in reap::reap_all() {
            let raw = pid.as_raw_nonzero().get() as u32;
            let exit = Exit::from_status(status.exit_status(), status.terminating_signal());

            let Some(name) = self.unit_for_pid(raw) else {
                // An orphan the machine reparented here. Reaping it was the
                // whole job.
                continue;
            };

            let Some(instance) = self.instances.get_mut(&name) else {
                continue;
            };

            let ty = instance.unit.service().map(|service| service.ty);

            // A forking service's initial process is expected to exit. That
            // exit is the readiness signal, not a failure.
            if ty == Some(ServiceType::Forking) && instance.state == State::Activating {
                self.forking_parent_exited(&name, exit);
                continue;
            }

            let deactivating = instance.state == State::Deactivating;
            let oneshot = ty == Some(ServiceType::Oneshot);

            match instance.on_exit(exit) {
                Some(delay) => {
                    println!(
                        "oxinit: {name} {exit}, restarting in {:?} (restart {})",
                        delay, instance.restarts
                    );
                    if let Err(e) = self.timers.schedule(delay, Alarm::Restart { unit: name }) {
                        eprintln!("oxinit: {e}");
                    }
                }
                None if deactivating => {
                    // Mid-stop the cgroup emptying is what ends the unit, not
                    // this exit — unless there is no cgroup to wait on.
                    if self.cgroups.get(&name).is_none() {
                        self.finish_deactivation(&name);
                    }
                }
                None => {
                    if oneshot && exit == Exit::Clean {
                        println!("oxinit: {name} completed");
                    } else {
                        println!("oxinit: {name} {exit} ({})", instance.state);
                    }
                }
            }
        }
    }

    /// A `forking` unit's initial process exited while it was activating.
    ///
    /// The daemon oxinit actually wants is whatever that process left behind,
    /// and there is no pid relationship to find it by — it was reparented to
    /// PID 1 like any other orphan. The cgroup is the only thing that still
    /// knows, so `populated` is the readiness condition.
    fn forking_parent_exited(&mut self, name: &str, exit: Exit) {
        let populated = match self.cgroups.get(name) {
            Some(cgroup) => cgroup.populated().unwrap_or(false),
            None => {
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.failed_to_start();
                }
                eprintln!(
                    "oxinit: {name}: type = \"forking\" needs a cgroup to tell whether \
                     anything is left running"
                );
                return;
            }
        };

        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };

        match (exit, populated) {
            (Exit::Clean, true) => {
                instance.forked_away();
                println!("oxinit: {name} forked and is ready");
                self.report_usage(name);
            }
            (Exit::Clean, false) => {
                instance.failed_to_start();
                eprintln!("oxinit: {name}: the initial process left nothing running");
            }
            (exit, _) => {
                instance.failed_to_start();
                eprintln!("oxinit: {name}: the initial process {exit}");
            }
        }
    }

    /// A service cgroup's `populated` key changed.
    pub fn on_cgroup(&mut self, id: u32) {
        // Reading is also what clears the EPOLLPRI condition, so this happens
        // before any decision about whether the event was interesting.
        let populated = self.cgroups.populated(id);
        let Some(name) = self.cgroups.unit(id).map(str::to_owned) else {
            return;
        };

        // A cgroup gaining its first process is not something to act on.
        if populated != Some(false) {
            return;
        }

        let Some((state, tracked)) = self
            .instances
            .get(&name)
            .map(|instance| (instance.state, instance.pid().is_some()))
        else {
            return;
        };

        match state {
            State::Deactivating => self.finish_deactivation(&name),

            // A unit that is up with no process of oxinit's own left to
            // watch is tracked by its cgroup alone — a `forking` service.
            // For anything else SIGCHLD owns the lifecycle, and acting here
            // as well would apply the restart policy twice.
            State::Active if !tracked => {
                println!("oxinit: {name}: nothing left in its cgroup");
                self.unit_vanished(&name);
            }

            _ => {}
        }
    }

    /// A unit oxinit could only see through its cgroup is gone.
    ///
    /// Reported as an abnormal end because that is all that can honestly be
    /// said: the process that exited was never a child of oxinit's, so there
    /// is no wait status anywhere to read.
    fn unit_vanished(&mut self, name: &str) {
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };

        match instance.on_exit(Exit::Abnormal) {
            Some(delay) => {
                println!(
                    "oxinit: {name} restarting in {:?} (restart {})",
                    delay, instance.restarts
                );
                let alarm = Alarm::Restart {
                    unit: name.to_owned(),
                };
                if let Err(e) = self.timers.schedule(delay, alarm) {
                    eprintln!("oxinit: {e}");
                }
            }
            None => println!("oxinit: {name} is {}", instance.state),
        }
    }

    /// The cgroup emptied while the unit was stopping. That ends the stop.
    fn finish_deactivation(&mut self, name: &str) {
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };
        if instance.state != State::Deactivating {
            return;
        }

        match instance.deactivated() {
            Some(delay) => {
                println!(
                    "oxinit: {name} stopped, restarting in {:?} (restart {})",
                    delay, instance.restarts
                );
                let alarm = Alarm::Restart {
                    unit: name.to_owned(),
                };
                if let Err(e) = self.timers.schedule(delay, alarm) {
                    eprintln!("oxinit: {e}");
                }
            }
            None => println!("oxinit: {name} is {}", instance.state),
        }
    }

    /// Stop a unit: `SIGTERM` now, `cgroup.kill` after `stop-sec`.
    ///
    /// The signal goes to every process in the cgroup, not just the one
    /// oxinit forked. A `forking` service has no process oxinit forked left
    /// to signal, and a service that spawned workers has more than one.
    fn stop(&mut self, name: &str, cause: StopCause) {
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };

        if matches!(
            instance.state,
            State::Inactive | State::Failed | State::Deactivating | State::Restarting
        ) {
            return;
        }

        let stop_sec = instance
            .unit
            .service()
            .map_or(Duration::from_secs(30), |service| service.stop_sec);

        let child = instance.pid();
        instance.entered_deactivating(cause);

        let signalled = match self.cgroups.get(name) {
            Some(cgroup) => terminate_cgroup(cgroup),
            // No cgroup to walk. The one pid oxinit knows about is all there
            // is to work with.
            None => child.map_or(0, |pid| usize::from(terminate(pid))),
        };

        println!("oxinit: stopping {name}: SIGTERM to {signalled} process(es)");

        let alarm = Alarm::StopTimeout {
            unit: name.to_owned(),
        };
        if let Err(e) = self.timers.schedule(stop_sec, alarm) {
            eprintln!("oxinit: {e}");
        }
    }

    /// `stop-sec` elapsed and the unit is still there.
    fn stop_timed_out(&mut self, name: &str) {
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };
        if instance.state != State::Deactivating {
            // It stopped on its own, in time.
            return;
        }

        eprintln!("oxinit: {name} did not stop in time; killing its cgroup");
        instance.stop_timed_out();
        let child = instance.pid();

        match self.cgroups.get(name) {
            Some(cgroup) => {
                if let Err(e) = cgroup.kill() {
                    eprintln!("oxinit: {name}: {e}");
                }
            }
            None => {
                // Without a cgroup this can only reach the one process oxinit
                // forked; anything it forked in turn survives. Saying so
                // beats pretending the unit is gone.
                eprintln!("oxinit: {name}: no cgroup; only its initial process can be killed");
                if let Some(pid) = child {
                    if let Some(pid) = Pid::from_raw(pid as i32) {
                        let _ = rustix::process::kill_process(pid, Signal::KILL);
                    }
                }
            }
        }
    }

    /// Apply readiness, status, and watchdog pings from the notify socket.
    pub fn on_notify(&mut self) {
        for message in self.notify.drain() {
            let Some(name) = self.unit_for_pid(message.pid) else {
                // A datagram from a process that is not a service main pid.
                // Dropped: attributing it to a unit would be a guess.
                continue;
            };

            let Some(instance) = self.instances.get_mut(&name) else {
                continue;
            };

            if let Some(status) = message.status {
                println!("oxinit: {name} status: {status}");
                instance.status = Some(status);
            }

            if message.watchdog {
                instance.pinged();
            }

            if message.ready && instance.state == State::Activating {
                instance.entered_active();
                let watchdog = instance.unit.service().and_then(|s| s.watchdog_sec);

                println!("oxinit: {name} is ready");
                self.report_usage(&name);

                if let Some(interval) = watchdog {
                    self.arm_watchdog(&name, interval);
                }
            }
        }
    }

    /// What the unit's cgroup is currently using.
    ///
    /// Read on demand and only where it is about to be printed. Nothing polls
    /// these files.
    fn report_usage(&self, name: &str) {
        if let Some(cgroup) = self.cgroups.get(name) {
            println!("oxinit: {name}: {}", cgroup.stats());
        }
    }

    /// Schedule the next watchdog check for a unit.
    fn arm_watchdog(&mut self, unit: &str, interval: Duration) {
        if let Err(e) = self.timers.schedule(
            interval,
            Alarm::WatchdogCheck {
                unit: unit.to_owned(),
            },
        ) {
            eprintln!("oxinit: {e}");
        }
    }

    /// Restart whatever the timers say is due.
    pub fn on_timer(&mut self) {
        let fired = match self.timers.expired() {
            Ok(fired) => fired,
            Err(e) => {
                eprintln!("oxinit: {e}");
                return;
            }
        };

        for alarm in fired {
            match alarm {
                Alarm::Restart { unit } => {
                    if self
                        .instances
                        .get(&unit)
                        .is_some_and(|i| i.state == State::Restarting)
                    {
                        self.start(&unit);
                    }
                }
                Alarm::WatchdogCheck { unit } => self.check_watchdog(&unit),
                Alarm::StopTimeout { unit } => self.stop_timed_out(&unit),
            }
        }
    }

    /// A missed watchdog deadline is a failure, not a warning.
    ///
    /// The point of a watchdog is to turn a hang — which nothing else detects
    /// — into something the restart policy can act on, so this goes through
    /// the same path as a crash.
    fn check_watchdog(&mut self, name: &str) {
        let Some(instance) = self.instances.get(name) else {
            return;
        };
        let Some(interval) = instance.unit.service().and_then(|s| s.watchdog_sec) else {
            return;
        };
        if instance.state != State::Active {
            return;
        }

        if !instance.watchdog_expired(interval) {
            // Still pinging. Check again next interval.
            self.arm_watchdog(name, interval);
            return;
        }

        eprintln!("oxinit: {name} missed its watchdog deadline");
        self.report_usage(name);

        // Through the ordinary stop path rather than a SIGKILL to the one pid
        // oxinit knows: a hung service may have forked, and killing only the
        // process that stopped answering leaves the rest of it running.
        self.stop(name, StopCause::Watchdog);
    }

    fn unit_for_pid(&self, pid: u32) -> Option<String> {
        self.instances
            .iter()
            .find(|(_, instance)| instance.pid() == Some(pid))
            .map(|(name, _)| name.clone())
    }
}

/// Write a unit's `[resources]` into its cgroup.
fn apply_resources(resources: &Resources, cgroup: Option<&Cgroup>) -> Result<(), String> {
    if *resources == Resources::default() {
        return Ok(());
    }

    let Some(cgroup) = cgroup else {
        return Err("[resources] declared but this unit has no cgroup".to_owned());
    };

    cgroup.apply(resources).map_err(|e| e.to_string())
}

/// `SIGTERM` to every process in a cgroup. Returns how many were signalled.
///
/// Reading `cgroup.procs` and signalling one at a time races a process that
/// forks while the list is being walked — which is exactly why the escalation
/// after `stop-sec` is `cgroup.kill` and not more of this.
fn terminate_cgroup(cgroup: &Cgroup) -> usize {
    cgroup
        .pids()
        .unwrap_or_default()
        .into_iter()
        .filter(|pid| terminate(*pid))
        .count()
}

fn terminate(pid: u32) -> bool {
    i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .is_some_and(|pid| rustix::process::kill_process(pid, Signal::TERM).is_ok())
}

/// Spawn one service's main process.
fn spawn(unit: &Unit, cgroup: Option<&Cgroup>) -> Result<Child, String> {
    let service = unit.service().ok_or_else(|| "not a service".to_owned())?;

    // Resolved here, in the parent, because the child cannot: between fork
    // and exec there is no reading /etc/passwd and no allocating. Everything
    // that can fail fails now, where it can be reported against the unit that
    // asked for it.
    let identity = if service.user == oxinit_user::ROOT {
        None
    } else {
        Some(oxinit_user::resolve_system(&service.user).map_err(|e| e.to_string())?)
    };

    let cgroup_procs = match cgroup {
        Some(cgroup) => Some(cgroup::open_procs(cgroup).map_err(|e| e.to_string())?),
        None => None,
    };

    let (program, args) = service
        .exec
        .split_first()
        .ok_or_else(|| "empty exec".to_owned())?;

    let mut command = Command::new(program);
    command.args(args);

    // The protocol is opt-in by type: a service that did not ask for it should
    // not find NOTIFY_SOCKET in its environment.
    if service.ty == ServiceType::Notify {
        command.env("NOTIFY_SOCKET", notify::NOTIFY_PATH);

        // Services are expected to ping at roughly half this, so the interval
        // is passed through unchanged and the halving is their business.
        if let Some(interval) = service.watchdog_sec {
            command.env("WATCHDOG_USEC", interval.as_micros().to_string());
        }
    }

    setup_child(
        &mut command,
        ChildSetup {
            cgroup_procs,
            identity,
        },
    );

    command.spawn().map_err(|e| format!("spawn {program}: {e}"))
}
