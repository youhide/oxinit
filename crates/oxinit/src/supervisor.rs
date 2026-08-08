//! Loading units, starting them in order, and keeping them running.

use std::collections::BTreeMap;
use std::process::{Child, Command};

use oxinit_service::{Exit, Instance, State};
use oxinit_unit::{Kind, ServiceType, Unit};

use crate::reap;
use crate::timer::{Alarm, Timers};

pub struct Supervisor {
    instances: BTreeMap<String, Instance>,
    /// Resolved start order. Also the order things are reported in.
    order: Vec<String>,
    pub timers: Timers,
}

impl Supervisor {
    /// Load and resolve. Starting is a separate step so the caller can
    /// register the timerfd with epoll first.
    pub fn load(hostname: &str, timers: Timers) -> Self {
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

                // M2a treats every type as `simple`: ready as soon as exec
                // succeeds. `notify` waits for READY=1 and `forking` waits for
                // the cgroup to settle, which are M2b and M3.
                if ty != Some(ServiceType::Oneshot) {
                    instance.entered_active();
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
            let Alarm::Restart { unit } = alarm;
            if self
                .instances
                .get(&unit)
                .is_some_and(|i| i.state == State::Restarting)
            {
                self.start(&unit);
            }
        }
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

    // std resets the child's signal mask to empty between fork and exec, which
    // matters because oxinit blocks every signal.
    Command::new(program)
        .args(args)
        .spawn()
        .map_err(|e| format!("spawn {program}: {e}"))
}
