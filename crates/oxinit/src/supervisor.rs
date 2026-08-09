//! Loading units, starting them in order, and keeping them running.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::OFlags;
use rustix::process::{Pid, Signal};

use oxinit_cgroup::Cgroup;
use oxinit_ipc::{Request, Response, UnitStatus};
use oxinit_service::{Exit, Instance, State, StopCause};
use oxinit_unit::{Kind, Output, Resources, ServiceType, Unit};

use crate::cgroup::{self, Cgroups};
use crate::error::Error;
use crate::event::{EventLoop, Source};
use crate::listen::Sockets;
use crate::logs::Logs;
use crate::notify::{self, Notify};
use crate::reap;
use crate::shutdown::{Action, Shutdown};
use crate::sys::raw::{setup_child, ChildSetup, Image, FIRST_LISTEN_FD};
use crate::timer::{Alarm, Timers};
use oxinit_timer::Next;

pub struct Supervisor {
    instances: BTreeMap<String, Instance>,
    /// Kept for `reload`, which has to expand `%H` the same way the first
    /// load did or the same file would parse into a different unit.
    hostname: String,
    /// Resolved start order. Also the order things are reported in.
    order: Vec<String>,
    pub timers: Timers,
    pub notify: Notify,
    pub cgroups: Cgroups,
    pub sockets: Sockets,
    pub logs: Logs,
    /// Queued to start, in resolved order, and not started yet.
    ///
    /// What makes `after` mean "wait for it" rather than "issue it first".
    pending: Vec<String>,
    /// Units an `oxctl restart` is waiting to start again.
    ///
    /// Separate from the restart policy, and deliberately: a requested stop
    /// overrides the policy, so a unit stopped this way lands `Inactive` and
    /// the policy has nothing to say about it. Something else has to remember
    /// that a start was asked for, and this is it.
    pending_restart: BTreeSet<String>,
    /// When each armed timer unit next fires, on whichever clock its schedule
    /// chose. Reported by `oxctl`, and kept here rather than asked of the
    /// deadline heaps, which are `BinaryHeap`s and answer "the soonest" rather
    /// than "this one's".
    next_elapse: BTreeMap<String, Next>,
    /// Set once the machine is going down. Nothing starts after this, and
    /// every event is also a chance to notice that everything has stopped.
    shutdown: Option<Shutdown>,
}

impl Supervisor {
    /// Load and resolve. Starting is a separate step so the caller can
    /// register the timerfd with epoll first.
    pub fn load(
        hostname: &str,
        timers: Timers,
        notify: Notify,
        cgroups: Cgroups,
        logs: Logs,
    ) -> Self {
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
                logs,
                sockets: Sockets::new(),
                pending: Vec::new(),
                pending_restart: BTreeSet::new(),
                next_elapse: BTreeMap::new(),
                shutdown: None,
                hostname: hostname.to_owned(),
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
            logs,
            sockets: Sockets::new(),
            pending: Vec::new(),
            pending_restart: BTreeSet::new(),
            next_elapse: BTreeMap::new(),
            shutdown: None,
            hostname: hostname.to_owned(),
        };

        // An instance for every unit that loaded, not just the ones in the
        // start order. A socket-activated service is deliberately unreachable
        // from `default` — that is what stops it starting at boot — but it
        // still needs somewhere to keep its state and a cgroup to run in when
        // a connection finally arrives.
        for (name, unit) in &loaded.units {
            // Every cgroup is created now, before anything starts, so the
            // whole set of `cgroup.events` descriptors can be registered with
            // epoll in one pass and never touched again. A restart reuses the
            // cgroup rather than churning the hierarchy and the registration.
            if matches!(unit.kind, Kind::Service(_)) {
                if let Err(e) = supervisor.cgroups.add(name, &unit.full_name()) {
                    eprintln!("oxinit: {name}: {e}");
                }
            }

            supervisor
                .instances
                .insert(name.clone(), Instance::new(unit.clone()));
        }

        // Bound in start order, so `listen` order within a unit and resolved
        // order between units decides which descriptor is fd 3.
        for name in &supervisor.order.clone() {
            let Some(socket) = loaded.units.get(name).and_then(Unit::socket) else {
                continue;
            };

            for failure in supervisor.sockets.bind(name, socket) {
                eprintln!("oxinit: {name}: {failure}");
            }
        }

        supervisor
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Queue every unit in the resolved order, then start whatever is ready.
    ///
    /// Not a loop over `order` calling `start`. `start` returns as soon as the
    /// child is forked, and a `notify` service is `Activating` until `READY=1`
    /// arrives — which is an event, so a loop would have started everything
    /// else by then. `after` would mean "issued in this order", where the unit
    /// format says "not started until every named unit has finished
    /// activating".
    ///
    /// So boot is a queue drained by the same event loop as everything else,
    /// which is the shape shutdown already has and for the same reason: what
    /// it is waiting for arrives as events, and re-implementing the reaper and
    /// the timers to wait on them synchronously is how you strand a machine.
    pub fn start_all(&mut self) {
        self.pending = self.order.clone();
        self.pump();
    }

    /// Start every queued unit whose ordering is satisfied, until none is.
    ///
    /// Called after every event, like socket reconciliation, rather than from
    /// each of the dozen places a unit can finish activating. Idempotent, and
    /// it cannot be forgotten by a path added later.
    pub fn pump(&mut self) {
        // Nothing starts once the machine is going down, including a unit
        // whose turn arrives mid-shutdown.
        if self.shutting_down() {
            self.pending.clear();
            return;
        }

        while let Some(name) = self.next_startable() {
            self.pending.retain(|queued| *queued != name);

            // Checked here rather than at load: `requires` implies no
            // ordering, so at this moment the requirement may not have run
            // yet. Having already failed is the only thing that can be known,
            // and it is the thing the unit format promises to act on.
            if let Some(failed) = self.failed_requirement(&name) {
                eprintln!("oxinit: {name}: requires `{failed}`, which failed; not starting");
                if let Some(instance) = self.instances.get_mut(&name) {
                    instance.failed_to_start();
                }
                continue;
            }

            self.start(&name);
        }
    }

    /// The first queued unit with nothing left to wait for.
    fn next_startable(&self) -> Option<String> {
        self.pending
            .iter()
            .find(|name| self.ordering_satisfied(name))
            .cloned()
    }

    /// Whether every unit this one is `after` has finished activating.
    fn ordering_satisfied(&self, name: &str) -> bool {
        let Some(instance) = self.instances.get(name) else {
            return false;
        };

        instance.unit.deps.after.iter().all(|dep| {
            // Still queued: it is going to start, so wait for it.
            if self.pending.iter().any(|queued| queued == dep) {
                return false;
            }

            match self.instances.get(dep) {
                // `after` on a unit that is not being started at all imposes
                // no delay. It orders units that are already going to start;
                // it does not cause them to start.
                None => true,
                Some(dep) => dep.state != State::Activating,
            }
        })
    }

    /// A requirement that has already failed, if there is one.
    fn failed_requirement(&self, name: &str) -> Option<String> {
        let instance = self.instances.get(name)?;

        instance
            .unit
            .deps
            .requires
            .iter()
            .find(|dep| {
                self.instances
                    .get(*dep)
                    .is_some_and(|dep| dep.state == State::Failed)
            })
            .cloned()
    }

    /// Units that cannot run at the same time as this one.
    ///
    /// Symmetric even when only one side declares it, which is what the unit
    /// format says: a unit should not have to be edited to be excluded by
    /// something it has never heard of.
    fn conflicting_with(&self, name: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .instances
            .get(name)
            .map(|instance| instance.unit.deps.conflicts.clone())
            .unwrap_or_default();

        for (other, instance) in &self.instances {
            if instance.unit.deps.conflicts.iter().any(|c| c == name) && !names.contains(other) {
                names.push(other.clone());
            }
        }

        names
    }

    /// Begin an ordered shutdown.
    ///
    /// Units are asked to stop in the reverse of the order they started, so a
    /// service goes down before whatever it was ordered after. Nothing blocks
    /// here: each stop finishes on its own events, and [`Supervisor::settled`]
    /// is what notices when they all have.
    pub fn begin_shutdown(&mut self, action: Action) {
        if let Some(existing) = self.shutdown.as_ref() {
            println!("oxinit: already shutting down to {}", existing.action);
            return;
        }

        println!("oxinit: shutting down to {action}");
        self.shutdown = Some(Shutdown::new(action));

        for name in self.stop_order() {
            self.stop(&name, StopCause::Requested);
        }
    }

    /// Every unit, in the reverse of the order they started.
    ///
    /// The resolved order first, reversed, so a service goes down before
    /// whatever it was ordered after. Then everything else — a unit that is
    /// running but is not in that order, because nothing reachable from
    /// `default` wanted it and it was started by a connection or by `oxctl`.
    ///
    /// Iterating the resolved order alone left those running, and `settled`
    /// waits for every unit rather than for the ordered ones, so the machine
    /// would sit there until the whole-shutdown deadline expired and then go
    /// down anyway with a service still holding whatever it held. Nothing had
    /// caught it: the only such unit was socket-activated and exited on its
    /// own before anyone asked.
    fn stop_order(&self) -> Vec<String> {
        let mut names: Vec<String> = self.order.iter().rev().cloned().collect();

        for name in self.instances.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }

        names
    }

    pub fn shutting_down(&self) -> bool {
        self.shutdown.is_some()
    }

    /// What to do now, if anything: `Some(action)` once every unit has
    /// stopped, or once waiting for the stragglers has gone on long enough.
    pub fn settled(&self) -> Option<Action> {
        let shutdown = self.shutdown.as_ref()?;

        let busy: Vec<&str> = self
            .instances
            .iter()
            .filter(|(_, instance)| !is_idle(instance.state))
            .map(|(name, _)| name.as_str())
            .collect();

        if busy.is_empty() {
            return Some(shutdown.action);
        }

        if shutdown.overdue() {
            eprintln!(
                "oxinit: giving up waiting for {}; going down anyway",
                busy.join(", ")
            );
            return Some(shutdown.action);
        }

        None
    }

    fn start(&mut self, name: &str) {
        // Nothing starts once the machine is going down, including a unit
        // whose restart timer happens to come due mid-shutdown.
        if self.shutting_down() {
            return;
        }

        // Before anything else, and before the unit's own state changes: what
        // this unit excludes has to be on its way down before it is up, or
        // both are running at once for as long as the stop takes.
        for other in self.conflicting_with(name) {
            if self
                .instances
                .get(&other)
                .is_some_and(|instance| !is_idle(instance.state))
            {
                println!("oxinit: {name} conflicts with {other}; stopping it");
                self.stop(&other, StopCause::Requested);
            }
        }

        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };

        if matches!(instance.unit.kind, Kind::Target) {
            instance.entered_active();
            println!("oxinit: reached target {name}");
            return;
        }

        // Starting a timer unit means arming it. It runs no process, owns no
        // cgroup, and the service it names is deliberately not started now —
        // that is what a schedule is for.
        if let Kind::Timer(timer) = &instance.unit.kind {
            let schedule = timer.schedule.clone();
            let service = timer.service.clone();
            instance.entered_active();

            self.arm(name, &service, schedule.first(unix_now()));
            return;
        }

        // A socket unit's descriptors were bound before anything started, so
        // reaching it in the order is only the announcement. Units ordered
        // after it can rely on the addresses being bound, which is the point
        // of binding them this early.
        if matches!(instance.unit.kind, Kind::Socket(_)) {
            instance.entered_active();
            println!(
                "oxinit: listening for {name} on {}",
                self.sockets.addresses(name).join(", ")
            );
            return;
        }

        let ty = instance.unit.service().map(|service| service.ty);
        let resources = instance
            .unit
            .service()
            .map_or_else(Resources::default, |service| service.resources);

        // A limit that cannot be applied is not a degraded mode, it is a
        // different configuration. The unit asked to be capped; running it
        // uncapped is not a smaller version of that.
        if let Err(e) = apply_resources(&resources, self.cgroups.get(name)) {
            if let Some(instance) = self.instances.get_mut(name) {
                instance.failed_to_start();
            }
            eprintln!("oxinit: {name}: {e}");
            return;
        }

        // A pipe per start, so the shipper sees this generation end. A unit
        // whose output cannot be routed is not started with it going somewhere
        // else — `output` is a statement about where a service's output goes,
        // and quietly putting it on the console instead is a different one.
        let output = match self.output_for(name) {
            Ok(output) => output,
            Err(e) => {
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.failed_to_start();
                }
                eprintln!("oxinit: {name}: {e}");
                return;
            }
        };

        let cgroup = self.cgroups.get(name);
        let listen = self.sockets.for_service(name);
        let spawned = self
            .instances
            .get(name)
            .map(|instance| spawn(&instance.unit, cgroup, &listen, output));

        let Some(spawned) = spawned else {
            return;
        };
        drop(listen);

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

                // Only for a unit that is actually waiting on something. A
                // `simple` service is Active by the line above, and arming a
                // deadline for a state it has already left is an alarm that
                // can only ever be a false one.
                if !ready {
                    let start_sec = instance
                        .unit
                        .service()
                        .map_or(Duration::from_secs(90), |service| service.start_sec);
                    let alarm = Alarm::StartTimeout {
                        unit: name.to_owned(),
                    };
                    if let Err(e) = self.timers.schedule(start_sec, alarm) {
                        eprintln!("oxinit: {e}");
                    }
                }

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

    /// Schedule a timer unit's next firing.
    ///
    /// `None` means there is no next one — an `on-boot` with no `interval` and
    /// no calendar is a delayed one-shot, and the unit stays `Active` with
    /// nothing pending rather than re-arming into a loop nobody asked for.
    fn arm(&mut self, name: &str, service: &str, next: Option<Next>) {
        let Some(next) = next else {
            println!("oxinit: {name} has fired; nothing further scheduled");
            self.next_elapse.remove(name);
            return;
        };

        let alarm = Alarm::TimerElapsed {
            unit: name.to_owned(),
        };

        // The clock is chosen by the schedule, not by this. A monotonic delay
        // must not move when the clock is corrected; a wall-clock moment must
        // stay the moment it is.
        let armed = match next {
            Next::After(delay) => self.timers.schedule(delay, alarm),
            Next::At(at) => self.timers.schedule_at(at, alarm),
        };

        match armed {
            Ok(()) => {
                self.next_elapse.insert(name.to_owned(), next);
                match next {
                    Next::After(delay) => {
                        println!("oxinit: {name} will start {service} in {delay:?}");
                    }
                    Next::At(at) => {
                        let left = at.saturating_sub(unix_now());
                        println!("oxinit: {name} will start {service} at {at} ({left}s away)");
                    }
                }
            }
            Err(e) => eprintln!("oxinit: {name}: {e}"),
        }
    }

    /// A timer elapsed: start what it names, then arm it again.
    ///
    /// Re-armed before the service is started rather than after, so a service
    /// that fails to start does not also stop the schedule. A timer is a
    /// statement about when, not about whether the last run worked.
    fn timer_elapsed(&mut self, name: &str) {
        let Some(timer) = self
            .instances
            .get(name)
            .and_then(|instance| instance.unit.timer())
        else {
            return;
        };

        let service = timer.service.clone();
        let schedule = timer.schedule.clone();

        // Not re-armed once the machine is going down. The firing itself would
        // be refused by `start`, so this is only about not announcing a
        // schedule that will never be kept.
        if !self.shutting_down() {
            self.arm(name, &service, schedule.next(unix_now()));
        }

        if !self.instances.contains_key(&service) {
            eprintln!("oxinit: {name}: no unit named `{service}` to start");
            return;
        }

        // Already running: skipped rather than queued. A job that takes longer
        // than its own interval would otherwise accumulate copies of itself
        // until the machine fell over, and the next firing is one interval
        // away either way.
        let busy = self
            .instances
            .get(&service)
            .is_some_and(|instance| !is_idle(instance.state));

        if busy {
            eprintln!("oxinit: {name}: {service} is still running; skipping this firing");
            return;
        }

        println!("oxinit: {name} elapsed; starting {service}");
        self.start(&service);
    }

    /// The descriptor a unit's `stdout` and `stderr` should land on, if not
    /// oxinit's own.
    ///
    /// `Console` is `None` — inheriting is what oxinit already does and what
    /// costs nothing. The other two each open something, and a failure is the
    /// unit's failure: a service told to write to a log or to nothing at all
    /// has not been started correctly if its output goes to the console
    /// instead.
    fn output_for(&mut self, name: &str) -> Result<Option<OwnedFd>, Error> {
        let output = self
            .instances
            .get(name)
            .and_then(|instance| instance.unit.service())
            .map_or(Output::Console, |service| service.output);

        match output {
            Output::Console => Ok(None),
            Output::Log => self.logs.pipe(name).map(Some),
            Output::Null => rustix::fs::open(
                "/dev/null",
                OFlags::WRONLY | OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map(Some)
            .map_err(|source| Error::Open {
                path: "/dev/null".to_owned(),
                source,
            }),
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

        // An `oxctl restart` is a stop followed by a start, and this is where
        // the stop finished. Deliberately after the policy above rather than
        // instead of it: the two cannot both apply, because a requested stop
        // is what stops the policy applying at all.
        if self.pending_restart.remove(name) {
            println!("oxinit: {name} stopped; starting it again");
            self.start(name);
        }
    }

    /// Stop a unit: `SIGTERM` now, `cgroup.kill` after `stop-sec`.
    ///
    /// The signal goes to every process in the cgroup, not just the one
    /// oxinit forked. A `forking` service has no process oxinit forked left
    /// to signal, and a service that spawned workers has more than one.
    fn stop(&mut self, name: &str, cause: StopCause) {
        // Queued but never started. Dropping it from the queue is the whole of
        // stopping it, and leaving it there would have a stop be followed by a
        // start as soon as its ordering came good.
        self.pending.retain(|queued| queued != name);

        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };

        // A unit waiting out its backoff has no process to signal. Left
        // `Restarting` it would block a shutdown forever, waiting for
        // something that is not coming back.
        if instance.state == State::Restarting {
            instance.cancel_restart();
            return;
        }

        // A target or a socket unit runs no process at all. There is nothing
        // to signal and nothing to wait for, so it stops the moment it is
        // asked to. Sending it through the ordinary path would leave it
        // `Deactivating` forever, waiting on an event only a process can
        // produce — and a shutdown waiting on that never finishes.
        if instance.unit.service().is_none() {
            instance.entered_deactivating(cause);
            instance.deactivated();
            println!("oxinit: {name} is {}", instance.state);
            return;
        }

        if matches!(
            instance.state,
            State::Inactive | State::Failed | State::Deactivating
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

    /// `start-sec` elapsed and the unit is still `Activating`.
    ///
    /// Not cancelled when the unit becomes ready — the deadline is left in the
    /// heap and this checks the state when it fires. Removing an entry from a
    /// `BinaryHeap` costs a rebuild, and a stale alarm that finds nothing to
    /// do is cheaper than keeping the heap exact.
    ///
    /// The unit goes through the ordinary stop path rather than being
    /// abandoned: it has a process, and a service that never signalled
    /// readiness is still running.
    fn start_timed_out(&mut self, name: &str) {
        let Some(instance) = self.instances.get(name) else {
            return;
        };
        if instance.state != State::Activating {
            return;
        }

        eprintln!("oxinit: {name} did not start in time; stopping it");
        self.stop(name, StopCause::Watchdog);
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

    /// A connection arrived on a socket unit's descriptor.
    ///
    /// Nothing is accepted here. oxinit starts the service; the service does
    /// the accepting, and the connection is still queued in the kernel when it
    /// gets there.
    pub fn on_socket(&mut self, id: u32) {
        let Some(service) = self.sockets.service(id).map(str::to_owned) else {
            return;
        };
        let address = self.sockets.address(id).unwrap_or("?").to_owned();

        let idle = self
            .instances
            .get(&service)
            .is_some_and(|instance| is_idle(instance.state));

        if !idle {
            // The service is already coming up, and the descriptor stops being
            // watched a moment from now. Nothing to do.
            return;
        }

        println!("oxinit: connection on {address}; starting {service}");
        self.start(&service);
    }

    /// Watch a socket unit's descriptors exactly when its service is down.
    ///
    /// A reconciliation pass rather than an arm/disarm call at every place a
    /// service changes state. There are a dozen of those and the cost of
    /// forgetting one is either a service that can never be activated again or
    /// an event loop spinning at full speed on a connection nobody accepted.
    ///
    /// Cheap: a handful of descriptors, and epoll is only touched when the
    /// answer actually changed.
    pub fn sync_sockets(&mut self, events: &EventLoop) {
        for (id, service) in self.sockets.ids() {
            let want = self
                .instances
                .get(&service)
                .is_some_and(|instance| is_idle(instance.state));

            if want == self.sockets.armed(id) {
                continue;
            }

            let Some(fd) = self.sockets.fd(id) else {
                continue;
            };

            let result = if want {
                events.register(fd, Source::Socket(id))
            } else {
                // Not closed. The descriptor is the service's across every
                // restart it makes; connections queue in the kernel rather
                // than being refused.
                events.deregister(fd)
            };

            match result {
                Ok(()) => self.sockets.set_armed(id, want),
                Err(e) => eprintln!("oxinit: {service}: {e}"),
            }
        }
    }

    /// Answer one control request.
    ///
    /// Every answer is immediate. A stop is not over when this returns — it
    /// ends when the unit's cgroup empties — and oxinit does not hold a client
    /// connection open waiting for that. `Accepted` says what was asked for,
    /// not what has finished.
    pub fn handle(&mut self, request: Request) -> Response {
        match request {
            Request::List => Response::Units(
                self.instances
                    .values()
                    .map(|instance| self.describe(instance))
                    .collect(),
            ),

            Request::Status { unit } => match self.instances.get(&unit) {
                Some(instance) => Response::Units(vec![self.describe(instance)]),
                None => unknown(&unit),
            },

            Request::Start { unit } => {
                if !self.instances.contains_key(&unit) {
                    return unknown(&unit);
                }
                if self.shutting_down() {
                    return refused("the machine is shutting down");
                }
                self.start(&unit);
                accepted(format!("started {unit}"))
            }

            Request::Stop { unit } => {
                if !self.instances.contains_key(&unit) {
                    return unknown(&unit);
                }
                self.stop(&unit, StopCause::Requested);
                accepted(format!("stopping {unit}"))
            }

            Request::Restart { unit } => {
                if !self.instances.contains_key(&unit) {
                    return unknown(&unit);
                }
                if self.shutting_down() {
                    return refused("the machine is shutting down");
                }

                // Stop, then start once it is actually down. A restart that
                // started the new process before the old one released its
                // cgroup would be two processes, not one.
                self.stop(&unit, StopCause::Requested);
                self.restart_when_stopped(&unit);
                accepted(format!("restarting {unit}"))
            }

            Request::Reload => self.reload(),
        }
    }

    /// Re-read the unit directories.
    ///
    /// Applies to units that are not running. A unit that is up keeps the
    /// definition it was started with until it stops — rewriting it underneath
    /// a running process would describe something that is not what is running.
    ///
    /// A new unit becomes known and gets a cgroup. A socket unit is *not*
    /// re-bound: its descriptors were bound and registered with epoll once, at
    /// boot, and changing that safely is not something a reload can do.
    fn reload(&mut self) -> Response {
        let loaded = oxinit_unit::load_default(&self.hostname);

        if !loaded.errors.is_empty() {
            let problems: Vec<String> = loaded.errors.iter().map(ToString::to_string).collect();
            return Response::Error {
                message: problems.join("; "),
            };
        }

        let mut changed = Vec::new();
        let mut skipped = Vec::new();

        for (name, unit) in &loaded.units {
            match self.instances.get(name) {
                Some(existing) if existing.unit == *unit => {}
                Some(existing) if !is_idle(existing.state) => skipped.push(name.clone()),
                Some(_) | None => {
                    if matches!(unit.kind, Kind::Service(_)) {
                        if let Err(e) = self.cgroups.add(name, &unit.full_name()) {
                            eprintln!("oxinit: {name}: {e}");
                        }
                    }
                    self.instances
                        .insert(name.clone(), Instance::new(unit.clone()));
                    changed.push(name.clone());
                }
            }
        }

        // A unit whose file is gone, and which is not running, stops being
        // known. One that is still running is left alone; it will go when it
        // stops and a later reload notices.
        self.instances.retain(|name, instance| {
            let gone = !loaded.units.contains_key(name);
            if gone && !is_idle(instance.state) {
                skipped.push(name.clone());
                return true;
            }
            !gone
        });

        let mut message = format!("{} unit(s) reloaded", changed.len());
        if !skipped.is_empty() {
            message.push_str(&format!(
                "; {} left alone because they are running: {}",
                skipped.len(),
                skipped.join(", ")
            ));
        }
        accepted(message)
    }

    /// Start a unit again once it has finished stopping.
    ///
    /// Remembered rather than timed. A stop ends when the unit's cgroup
    /// empties, which is an event, and waiting `stop-sec` for it instead would
    /// hold a restart for thirty seconds on a unit that stopped in one — or,
    /// as this did before M7's tests reached it, never start it again at all:
    /// the alarm it scheduled only acted on a unit in `Restarting`, and a
    /// requested stop ends `Inactive` precisely because it overrides the
    /// restart policy.
    ///
    /// No backstop timer. A stop that does not finish on its own is escalated
    /// to `cgroup.kill` by `stop-sec`, and that path ends here too.
    fn restart_when_stopped(&mut self, name: &str) {
        let Some(instance) = self.instances.get(name) else {
            return;
        };

        // Already down: nothing to wait for.
        if is_idle(instance.state) {
            self.start(name);
            return;
        }

        self.pending_restart.insert(name.to_owned());
    }

    fn describe(&self, instance: &Instance) -> UnitStatus {
        let stats = self
            .cgroups
            .get(instance.name())
            .map(|cgroup| cgroup.stats())
            .unwrap_or_default();

        UnitStatus {
            name: instance.name().to_owned(),
            kind: match instance.unit.kind {
                Kind::Service(_) => "service",
                Kind::Target => "target",
                Kind::Socket(_) => "socket",
                Kind::Timer(_) => "timer",
            }
            .to_owned(),
            description: instance.unit.description.clone(),
            state: instance.state.to_string(),
            pid: instance.pid(),
            restarts: instance.restarts,
            status: instance.status.clone(),
            memory: stats.memory,
            tasks: stats.tasks,
            next_elapse: self
                .next_elapse
                .get(instance.name())
                .map(|next| next.in_seconds(unix_now()).max(1)),
        }
    }

    /// Print what every unit is doing.
    ///
    /// The whole state of the machine on the console, which until the control
    /// socket exists is the only way to ask.
    pub fn report(&self) {
        println!("oxinit: {} units", self.instances.len());

        for (name, instance) in &self.instances {
            let kind = match instance.unit.kind {
                Kind::Service(_) => "service",
                Kind::Target => "target",
                Kind::Socket(_) => "socket",
                Kind::Timer(_) => "timer",
            };

            print!("  {name} ({kind}): {}", instance.state);
            if let Some(pid) = instance.pid() {
                print!(", pid {pid}");
            }
            if instance.restarts > 0 {
                print!(", {} restart(s)", instance.restarts);
            }
            if let Some(status) = instance.status.as_deref() {
                print!(", {status}");
            }
            println!();
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
    /// The wall clock reached a calendar deadline, or was stepped.
    pub fn on_wall_timer(&mut self) {
        let fired = match self.timers.wall_expired(unix_now()) {
            Ok(fired) => fired,
            Err(e) => {
                eprintln!("oxinit: {e}");
                return;
            }
        };

        for alarm in fired {
            if let Alarm::TimerElapsed { unit } = alarm {
                self.timer_elapsed(&unit);
            }
        }
    }

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
                Alarm::TimerElapsed { unit } => self.timer_elapsed(&unit),
                Alarm::StartTimeout { unit } => self.start_timed_out(&unit),
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

fn accepted(message: String) -> Response {
    Response::Accepted { message }
}

fn unknown(unit: &str) -> Response {
    Response::Error {
        message: format!("no unit named `{unit}`"),
    }
}

fn refused(why: &str) -> Response {
    Response::Error {
        message: why.to_owned(),
    }
}

/// A unit that is not running, so a connection on its socket should start it.
///
/// `Restarting` is deliberately not idle: it is already on its way back, and a
/// second start would be a second process.
fn is_idle(state: State) -> bool {
    matches!(state, State::Inactive | State::Failed)
}

/// Spawn one service's main process.
///
/// `listen` is the descriptors it inherits, paired with the socket unit each
/// one came from, in the order it receives them.
fn spawn(
    unit: &Unit,
    cgroup: Option<&Cgroup>,
    listen: &[(&str, BorrowedFd<'_>)],
    output: Option<OwnedFd>,
) -> Result<Child, String> {
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

    let mut env: Vec<(String, String)> = Vec::new();

    // The protocol is opt-in by type: a service that did not ask for it should
    // not find NOTIFY_SOCKET in its environment.
    if service.ty == ServiceType::Notify {
        env.push(("NOTIFY_SOCKET".to_owned(), notify::NOTIFY_PATH.to_owned()));

        // Services are expected to ping at roughly half this, so the interval
        // is passed through unchanged and the halving is their business.
        if let Some(interval) = service.watchdog_sec {
            env.push(("WATCHDOG_USEC".to_owned(), interval.as_micros().to_string()));
        }
    }

    let listen_fds = duplicate_listen_fds(listen)?;

    if !listen_fds.is_empty() {
        let names: Vec<&str> = listen.iter().map(|(name, _)| *name).collect();
        env.push(("LISTEN_FDS".to_owned(), listen_fds.len().to_string()));
        env.push(("LISTEN_FDNAMES".to_owned(), names.join(":")));
    }

    // LISTEN_PID is not in that list, because its value is the child's pid and
    // nothing here knows it yet. The image reserves the slot; the child fills
    // it in after the fork. See `sys::raw::Image`.
    let image = Image::build(&service.exec, &env, !listen_fds.is_empty())
        .ok_or_else(|| "exec or environment contains a NUL byte".to_owned())?;

    let mut command = Command::new(program);
    command.args(args);

    setup_child(
        &mut command,
        ChildSetup {
            cgroup_procs,
            listen_fds,
            identity,
            image: Some(image),
            output,
            // Declared, never inferred. Every service on a machine that boots
            // to a console inherits a tty on stdin, and giving each one a
            // session would have them take the terminal from one another, one
            // start at a time.
            session: service.tty,
        },
    );

    command.spawn().map_err(|e| format!("spawn {program}: {e}"))
}

/// The wall clock, in seconds, for a calendar schedule.
///
/// Only calendar schedules consult it. Everything else is measured from an
/// event on the monotonic clock, which is what stops an NTP correction moving
/// a restart backoff or a stop timeout.
fn unix_now() -> oxinit_timer::Unix {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Copy the listening descriptors somewhere the child can move them from.
///
/// Duplicated to numbers at or above the range they are moving into, so the
/// child's `dup2` sequence cannot land on a descriptor it has not read yet,
/// and cannot be a no-op onto itself — which would leave `FD_CLOEXEC` set and
/// close the descriptor at exec.
///
/// Duplicates, not the originals: oxinit keeps those, open, across every
/// restart the service makes.
fn duplicate_listen_fds(listen: &[(&str, BorrowedFd<'_>)]) -> Result<Vec<OwnedFd>, String> {
    let above = FIRST_LISTEN_FD.saturating_add(i32::try_from(listen.len()).unwrap_or(i32::MAX));

    listen
        .iter()
        .map(|(name, fd)| {
            rustix::io::fcntl_dupfd_cloexec(fd, above)
                .map_err(|e| format!("duplicate the descriptor from {name}: {e}"))
        })
        .collect()
}
