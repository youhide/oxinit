//! Loading units and starting them in dependency order.
//!
//! M1 starts things. It does not yet supervise them: no readiness protocol, no
//! restart policy, no timeouts. Those are M2.

use std::process::{Child, Command};

use oxinit_graph::Plan;
use oxinit_unit::{Kind, Loaded, Unit};

/// A started service and the unit it came from.
pub struct Started {
    pub unit: String,
    pub child: Child,
}

/// Load, resolve, and start everything reachable from the `default` unit.
///
/// Returns what was started. Every failure is logged and survived: a machine
/// with three of four services running beats a machine that gave up on the
/// first one.
pub fn boot(hostname: &str) -> Vec<Started> {
    let loaded = oxinit_unit::load_default(hostname);

    for error in &loaded.errors {
        eprintln!("oxinit: {error}");
    }

    if loaded.units.is_empty() {
        eprintln!("oxinit: no units found; nothing to start");
        return Vec::new();
    }

    // A broken graph is refused whole rather than partially applied: a
    // half-started machine matches no configuration on disk.
    let plan = match oxinit_graph::resolve(&loaded.units) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("oxinit: {e}");
            eprintln!("oxinit: refusing to start with an unresolvable unit set");
            return Vec::new();
        }
    };

    for warning in &plan.warnings {
        eprintln!("oxinit: {warning}");
    }

    start_in_order(&plan, &loaded)
}

fn start_in_order(plan: &Plan, loaded: &Loaded) -> Vec<Started> {
    let mut started = Vec::new();

    for name in &plan.order {
        let Some(unit) = loaded.units.get(name) else {
            continue;
        };

        match &unit.kind {
            // A target is a grouping. Reaching it in the order is all it does.
            Kind::Target => println!("oxinit: reached target {name}"),

            Kind::Service(_) => match spawn(unit) {
                Ok(Some(child)) => {
                    println!("oxinit: started {name} as pid {}", child.id());
                    started.push(Started {
                        unit: name.clone(),
                        child,
                    });
                }
                Ok(None) => {}
                Err(e) => eprintln!("oxinit: {name}: {e}"),
            },
        }
    }

    started
}

/// Spawn one service.
///
/// `Ok(None)` means the unit was deliberately skipped.
fn spawn(unit: &Unit) -> Result<Option<Child>, String> {
    let Some(service) = unit.service() else {
        return Ok(None);
    };

    // `user` is parsed and validated but not yet applied: dropping privilege
    // needs setgid, initgroups, and setuid against /etc/passwd, which is M2
    // work. Running as root a service that asked to be someone else would be a
    // privilege escalation, so refuse instead of doing the wrong thing
    // silently.
    if service.user != "root" {
        return Err(format!(
            "user = \"{}\" is not supported yet; refusing to run it as root (M2)",
            service.user
        ));
    }

    let (program, args) = service
        .exec
        .split_first()
        .ok_or_else(|| "empty exec".to_owned())?;

    // std resets the child's signal mask to empty between fork and exec, which
    // matters because oxinit blocks everything.
    Command::new(program)
        .args(args)
        .spawn()
        .map(Some)
        .map_err(|e| format!("spawn {program}: {e}"))
}

/// Which unit a reaped pid belonged to, if any.
pub fn unit_for_pid(started: &[Started], pid: u32) -> Option<&str> {
    started
        .iter()
        .find(|s| s.child.id() == pid)
        .map(|s| s.unit.as_str())
}
