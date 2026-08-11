//! Dependency resolution and start ordering.
//!
//! Requirement and ordering are separate axes, and this crate keeps them
//! separate. `requires` and `wants` decide *what* gets started; `after` and
//! `before` decide *in what order*. Neither implies the other.
//!
//! No Linux dependencies. Tests run on any host.
//!
//! This code runs inside PID 1, so the same failure policy applies: a panic
//! here is a panic in PID 1.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use oxinit_unit::Unit;
use thiserror::Error;

/// The unit started at the end of boot.
pub const DEFAULT_UNIT: &str = "default";

/// A resolved start plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Units to start, in an order that satisfies every `after`/`before` edge.
    pub order: Vec<String>,
    /// Non-fatal problems. A `wants` naming a unit that does not exist is the
    /// common one, and is deliberately not an error.
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Warning {
    #[error("unit `{unit}` wants `{missing}`, which does not exist")]
    MissingWants { unit: String, missing: String },

    #[error("unit `{unit}` is ordered after `{missing}`, which does not exist")]
    MissingOrdering { unit: String, missing: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GraphError {
    #[error("unit `{unit}` requires `{missing}`, which does not exist")]
    MissingRequires { unit: String, missing: String },

    #[error("no `{DEFAULT_UNIT}` unit; nothing to start")]
    NoDefault,

    #[error("dependency cycle: {}", .path.join(" -> "))]
    Cycle { path: Vec<String> },

    #[error("units `{a}` and `{b}` conflict, and both are required")]
    ConflictingRequired { a: String, b: String },

    #[error("socket `{unit}` activates `{missing}`, which does not exist")]
    MissingSocketService { unit: String, missing: String },

    #[error("socket `{unit}` activates `{other}`, which is not a service")]
    SocketServiceNotAService { unit: String, other: String },
}

/// Resolve the units reachable from `default` into a start order.
pub fn resolve(units: &BTreeMap<String, Unit>) -> Result<Plan, GraphError> {
    resolve_from(units, DEFAULT_UNIT)
}

/// Resolve from an arbitrary root, which is what the tests and `oxctl start`
/// need.
pub fn resolve_from(units: &BTreeMap<String, Unit>, root: &str) -> Result<Plan, GraphError> {
    if !units.contains_key(root) {
        return Err(GraphError::NoDefault);
    }

    let mut warnings = Vec::new();
    let wanted = reachable(units, root, &mut warnings)?;

    check_conflicts(units, &wanted)?;
    check_sockets(units, &wanted)?;
    let order = topological_order(units, &wanted, &mut warnings)?;

    Ok(Plan { order, warnings })
}

/// A socket unit must activate a service that exists.
///
/// Fatal, like a missing `requires` and unlike a missing `wants`: a socket
/// whose service is not installed binds a port that can never be answered,
/// which is worse than not binding it. The service is looked up in the whole
/// unit set rather than in `wanted`, because a socket-activated service is
/// normally *not* reachable from `default` — that is the point of it.
fn check_sockets(
    units: &BTreeMap<String, Unit>,
    wanted: &BTreeSet<String>,
) -> Result<(), GraphError> {
    for name in wanted {
        let Some(socket) = units.get(name).and_then(Unit::socket) else {
            continue;
        };

        match units.get(&socket.service) {
            None => {
                return Err(GraphError::MissingSocketService {
                    unit: name.clone(),
                    missing: socket.service.clone(),
                })
            }
            Some(unit) if unit.service().is_none() => {
                return Err(GraphError::SocketServiceNotAService {
                    unit: name.clone(),
                    other: socket.service.clone(),
                })
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Everything pulled in from `root` through `requires` and `wants`.
///
/// `after` and `before` are not traversed: ordering does not cause a unit to
/// start. A unit ordered after something that is not being started imposes no
/// delay and pulls in nothing.
fn reachable(
    units: &BTreeMap<String, Unit>,
    root: &str,
    warnings: &mut Vec<Warning>,
) -> Result<BTreeSet<String>, GraphError> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(root.to_owned());

    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }

        let Some(unit) = units.get(&name) else {
            continue;
        };

        // A missing `requires` is fatal: the unit cannot run without it.
        for dep in &unit.deps.requires {
            if !units.contains_key(dep) {
                return Err(GraphError::MissingRequires {
                    unit: name.clone(),
                    missing: dep.clone(),
                });
            }
            queue.push_back(dep.clone());
        }

        // A missing `wants` is a warning, so an optional unit can be listed
        // without requiring it to be installed.
        for dep in &unit.deps.wants {
            if units.contains_key(dep) {
                queue.push_back(dep.clone());
            } else {
                warnings.push(Warning::MissingWants {
                    unit: name.clone(),
                    missing: dep.clone(),
                });
            }
        }
    }

    Ok(seen)
}

/// Two units that conflict cannot both be in the start set.
fn check_conflicts(
    units: &BTreeMap<String, Unit>,
    wanted: &BTreeSet<String>,
) -> Result<(), GraphError> {
    for name in wanted {
        let Some(unit) = units.get(name) else {
            continue;
        };
        for other in &unit.deps.conflicts {
            if wanted.contains(other) {
                return Err(GraphError::ConflictingRequired {
                    a: name.clone(),
                    b: other.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Kahn's algorithm over the `after`/`before` edges.
///
/// An edge `a -> b` means "a starts before b". `after` and `before` are
/// inverses of each other and both become edges here.
fn topological_order(
    units: &BTreeMap<String, Unit>,
    wanted: &BTreeSet<String>,
    warnings: &mut Vec<Warning>,
) -> Result<Vec<String>, GraphError> {
    let mut successors: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut in_degree: BTreeMap<&str, usize> = wanted.iter().map(|n| (n.as_str(), 0)).collect();

    for name in wanted {
        let Some(unit) = units.get(name) else {
            continue;
        };

        for dep in &unit.deps.after {
            if !units.contains_key(dep) {
                warnings.push(Warning::MissingOrdering {
                    unit: name.clone(),
                    missing: dep.clone(),
                });
                continue;
            }
            add_edge(wanted, dep, name, &mut successors, &mut in_degree);
        }

        for dep in &unit.deps.before {
            if !units.contains_key(dep) {
                warnings.push(Warning::MissingOrdering {
                    unit: name.clone(),
                    missing: dep.clone(),
                });
                continue;
            }
            add_edge(wanted, name, dep, &mut successors, &mut in_degree);
        }

        // A socket is implicitly ordered before the service it activates. If
        // both are started at boot, the descriptors have to be bound before
        // the service runs, or handing them to it would mean nothing.
        //
        // Implicit, unlike everything else here, because there is no
        // configuration in which the other order is what anyone wanted. The
        // edge is inert when the service is not being started at boot, which
        // is the normal case for socket activation.
        if let Some(socket) = unit.socket() {
            add_edge(
                wanted,
                name,
                &socket.service,
                &mut successors,
                &mut in_degree,
            );
        }
    }

    // BTreeMap iteration is sorted, so units left free by the ordering
    // constraints start in a stable, reproducible order rather than whatever
    // the hasher decided that boot.
    let mut ready: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(name, _)| *name)
        .collect();

    let mut order = Vec::with_capacity(wanted.len());
    let mut remaining = in_degree.clone();

    while let Some(name) = ready.pop_front() {
        order.push(name.to_owned());

        for next in successors.get(name).into_iter().flatten() {
            if let Some(degree) = remaining.get_mut(*next) {
                *degree = degree.saturating_sub(1);
                if *degree == 0 {
                    ready.push_back(next);
                }
            }
        }
    }

    if order.len() != wanted.len() {
        return Err(GraphError::Cycle {
            path: find_cycle(&successors, &remaining),
        });
    }

    Ok(order)
}

/// Recover one cycle for the error message.
///
/// Everything still holding a non-zero in-degree after Kahn terminates is in
/// or downstream of a cycle. Walking successors from any of them must revisit
/// a node, and the revisited node closes the cycle.
fn find_cycle(
    successors: &BTreeMap<&str, BTreeSet<&str>>,
    remaining: &BTreeMap<&str, usize>,
) -> Vec<String> {
    let start = remaining
        .iter()
        .find(|(_, degree)| **degree > 0)
        .map(|(name, _)| *name);

    let Some(start) = start else {
        return Vec::new();
    };

    let mut path: Vec<&str> = Vec::new();
    let mut current = start;

    loop {
        if let Some(at) = path.iter().position(|seen| *seen == current) {
            // Close the loop so the message reads a -> b -> c -> a.
            let mut cycle: Vec<String> = path
                .get(at..)
                .unwrap_or_default()
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
            cycle.push(current.to_owned());
            return cycle;
        }

        path.push(current);

        // Follow any successor that is itself still blocked.
        let next = successors
            .get(current)
            .into_iter()
            .flatten()
            .find(|next| remaining.get(*next).is_some_and(|d| *d > 0));

        match next {
            Some(next) => current = next,
            None => return path.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

/// Record `from` starts before `to`.
///
/// Names are looked back up in `wanted` so the stored borrows live as long as
/// the set, and so edges to units that are not being started are dropped.
fn add_edge<'a>(
    wanted: &'a BTreeSet<String>,
    from: &str,
    to: &str,
    successors: &mut BTreeMap<&'a str, BTreeSet<&'a str>>,
    in_degree: &mut BTreeMap<&'a str, usize>,
) {
    let (Some(from), Some(to)) = (get_str(wanted, from), get_str(wanted, to)) else {
        return;
    };
    if successors.entry(from).or_default().insert(to) {
        *in_degree.entry(to).or_insert(0) += 1;
    }
}

/// The `&str` stored in the set, so the borrow lives as long as the set.
fn get_str<'a>(set: &'a BTreeSet<String>, name: &str) -> Option<&'a str> {
    set.get(name).map(String::as_str)
}
