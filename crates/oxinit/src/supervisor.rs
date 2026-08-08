//! Loading units, starting them in order, and keeping them running.

use std::collections::BTreeMap;
use std::process::{Child, Command};
use std::time::Duration;

use oxinit_service::{Exit, Instance, State};
use oxinit_unit::{Kind, ServiceType, Unit};

use crate::notify::{self, Notify};
use crate::reap;
use crate::timer::{Alarm, Timers};

pub struct Supervisor {
    instances: BTreeMap<String, Instance>,
    /// Resolved start order. Also the order things are reported in.
    order: Vec<String>,
    pub timers: Timers,
    pub notify: Notify,
}

impl Supervisor {
    /// Load and resolve. Starting is a separate step so the caller can
    /// register the timerfd with epoll first.
    pub fn load(hostname: &str, timers: Timers, notify: Notify) -> Self {
        let loaded = oxinit_unit::load_default(hostname);
        for error in &loaded.errors {
            eprintln!("oxinit: {error}");
        }

        let mut instances = BTreeMap::new();
        let mut order = Vec::new();

        if loaded.units.is_empty() {
            eprintln!("oxinit: no units found");
            return Self {
                instances,
                order,
                timers,
                notify,
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

        for name in &order {
            if let Some(unit) = loaded.units.get(name) {
                instances.insert(name.clone(), Instance::new(unit.clone()));
            }
        }

        Self {
            instances,
            order,
            timers,
            notify,
        }
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
        // Ends the immutable borrow of `instance` before the state changes.
        let spawned = spawn(&instance.unit);

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

                    // Needs the cgroup to tell us the initial process left a
                    // populated cgroup behind. Until M3 there is no way to
                    // know, so treat it as simple and say so.
                    Some(ServiceType::Forking) => {
                        eprintln!("oxinit: {name}: type = \"forking\" needs cgroups (M3); treating as simple");
                        instance.entered_active();
                    }

                    None => {}
                }
                println!("oxinit: started {name} as pid {pid}");
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

            let oneshot = instance
                .unit
                .service()
                .is_some_and(|s| s.ty == ServiceType::Oneshot);

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
                println!("oxinit: {name} is ready");

                if let Some(interval) = instance.unit.service().and_then(|s| s.watchdog_sec) {
                    self.arm_watchdog(&name, interval);
                }
            }
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
            }
        }
    }

    /// A missed watchdog deadline is a failure, not a warning.
    ///
    /// The point of a watchdog is to turn a hang — which nothing else detects
    /// — into something the restart policy can act on, so this goes through
    /// the same path as a crash.
    fn check_watchdog(&mut self, name: &str) {
        let Some(instance) = self.instances.get_mut(name) else {
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

        eprintln!("oxinit: {name} missed its watchdog deadline; killing");

        if let Some(child) = instance.child.as_mut() {
            // SIGKILL rather than SIGTERM: the service already proved it is not
            // responding. Graceful stop is for units that are still answering.
            let _ = child.kill();
        }

        // The reaper sees the exit and applies the restart policy from there.
    }

    fn unit_for_pid(&self, pid: u32) -> Option<String> {
        self.instances
            .iter()
            .find(|(_, instance)| instance.pid() == Some(pid))
            .map(|(name, _)| name.clone())
    }
}

/// Spawn one service's main process.
fn spawn(unit: &Unit) -> Result<Child, String> {
    let service = unit.service().ok_or_else(|| "not a service".to_owned())?;

    // `user` is parsed and validated but not applied yet. Running as root a
    // service that asked to be someone else is a privilege escalation, not a
    // degraded mode, so refuse instead.
    if service.user != "root" {
        return Err(format!(
            "user = \"{}\" is not supported yet; refusing to run it as root",
            service.user
        ));
    }

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

    // std resets the child's signal mask to empty between fork and exec, which
    // matters because oxinit blocks every signal.
    command.spawn().map_err(|e| format!("spawn {program}: {e}"))
}
