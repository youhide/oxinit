//! Graph behaviour, stated in terms of the format document.

use std::collections::BTreeMap;

use oxinit_graph::{resolve, resolve_from, GraphError, Warning};
use oxinit_unit::Unit;

/// Build a unit set from `(name, toml)` pairs.
fn units(specs: &[(&str, &str)]) -> BTreeMap<String, Unit> {
    specs
        .iter()
        .map(|(name, text)| {
            let unit = oxinit_unit::parse(name, text, "testhost")
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            ((*name).to_owned(), unit)
        })
        .collect()
}

/// A service with the given `[unit]` body.
fn service(deps: &str) -> String {
    format!("[service]\nexec = \"/bin/true\"\n[unit]\n{deps}")
}

fn target(deps: &str) -> String {
    format!("[target]\n[unit]\n{deps}")
}

fn position(order: &[String], name: &str) -> usize {
    order
        .iter()
        .position(|n| n == name)
        .unwrap_or_else(|| panic!("{name} not in {order:?}"))
}

#[test]
fn requires_pulls_a_unit_in_without_ordering_it() {
    let set = units(&[
        ("default", &target("requires = [\"a\"]")),
        ("a", &service("")),
    ]);

    let plan = resolve(&set).unwrap();

    // Both start. Which one first is unconstrained, because `requires`
    // creates no ordering — that is the whole point of the two axes.
    assert_eq!(plan.order.len(), 2);
    assert!(plan.order.contains(&"a".to_owned()));
}

#[test]
fn after_orders_without_pulling_anything_in() {
    let set = units(&[
        ("default", &target("requires = [\"web\"]")),
        ("web", &service("after = [\"db\"]")),
        ("db", &service("")),
    ]);

    let plan = resolve(&set).unwrap();

    // `db` is ordered before `web` but nothing requires or wants it, so it is
    // not started at all and imposes no delay.
    assert!(!plan.order.contains(&"db".to_owned()));
    assert_eq!(plan.order.len(), 2);
}

#[test]
fn requires_plus_after_is_the_common_case() {
    let set = units(&[
        ("default", &target("requires = [\"web\"]")),
        ("web", &service("requires = [\"db\"]\nafter = [\"db\"]")),
        ("db", &service("")),
    ]);

    let plan = resolve(&set).unwrap();
    assert!(position(&plan.order, "db") < position(&plan.order, "web"));
}

#[test]
fn before_is_the_inverse_of_after() {
    let with_after = units(&[
        ("default", &target("requires = [\"a\", \"b\"]")),
        ("a", &service("")),
        ("b", &service("after = [\"a\"]")),
    ]);
    let with_before = units(&[
        ("default", &target("requires = [\"a\", \"b\"]")),
        ("a", &service("before = [\"b\"]")),
        ("b", &service("")),
    ]);

    for set in [with_after, with_before] {
        let plan = resolve(&set).unwrap();
        assert!(position(&plan.order, "a") < position(&plan.order, "b"));
    }
}

#[test]
fn ordering_is_transitive() {
    let set = units(&[
        ("default", &target("requires = [\"a\", \"b\", \"c\"]")),
        ("a", &service("")),
        ("b", &service("after = [\"a\"]")),
        ("c", &service("after = [\"b\"]")),
    ]);

    let plan = resolve(&set).unwrap();
    assert!(position(&plan.order, "a") < position(&plan.order, "b"));
    assert!(position(&plan.order, "b") < position(&plan.order, "c"));
}

#[test]
fn missing_requires_is_fatal() {
    let set = units(&[("default", &target("requires = [\"ghost\"]"))]);

    match resolve(&set) {
        Err(GraphError::MissingRequires { missing, .. }) => assert_eq!(missing, "ghost"),
        other => panic!("expected MissingRequires, got {other:?}"),
    }
}

#[test]
fn missing_wants_is_only_a_warning() {
    let set = units(&[("default", &target("wants = [\"ghost\"]"))]);

    let plan = resolve(&set).unwrap();
    assert_eq!(plan.order, ["default"]);
    assert!(matches!(
        plan.warnings.first(),
        Some(Warning::MissingWants { .. })
    ));
}

#[test]
fn wants_failure_does_not_block_the_wanting_unit() {
    let set = units(&[
        ("default", &target("wants = [\"optional\"]")),
        ("optional", &service("")),
    ]);

    let plan = resolve(&set).unwrap();
    assert_eq!(plan.order.len(), 2);
}

#[test]
fn cycle_is_refused_and_reports_its_path() {
    let set = units(&[
        ("default", &target("requires = [\"a\", \"b\", \"c\"]")),
        ("a", &service("after = [\"c\"]")),
        ("b", &service("after = [\"a\"]")),
        ("c", &service("after = [\"b\"]")),
    ]);

    match resolve(&set) {
        Err(GraphError::Cycle { path }) => {
            // Closed: the first and last entries are the same unit.
            assert!(path.len() >= 4, "path was {path:?}");
            assert_eq!(path.first(), path.last());
            for name in ["a", "b", "c"] {
                assert!(
                    path.contains(&name.to_owned()),
                    "{name} missing from {path:?}"
                );
            }
        }
        other => panic!("expected Cycle, got {other:?}"),
    }
}

#[test]
fn self_cycle_is_refused() {
    let set = units(&[
        ("default", &target("requires = [\"a\"]")),
        ("a", &service("after = [\"a\"]")),
    ]);

    assert!(matches!(resolve(&set), Err(GraphError::Cycle { .. })));
}

#[test]
fn conflicting_units_cannot_both_be_started() {
    let set = units(&[
        ("default", &target("requires = [\"a\", \"b\"]")),
        ("a", &service("conflicts = [\"b\"]")),
        ("b", &service("")),
    ]);

    assert!(matches!(
        resolve(&set),
        Err(GraphError::ConflictingRequired { .. })
    ));
}

#[test]
fn conflicts_is_symmetric_even_declared_on_one_side() {
    let set = units(&[
        ("default", &target("requires = [\"a\", \"b\"]")),
        ("a", &service("")),
        ("b", &service("conflicts = [\"a\"]")),
    ]);

    assert!(matches!(
        resolve(&set),
        Err(GraphError::ConflictingRequired { .. })
    ));
}

#[test]
fn conflicts_between_unstarted_units_is_fine() {
    let set = units(&[
        ("default", &target("requires = [\"a\"]")),
        ("a", &service("conflicts = [\"b\"]")),
        ("b", &service("")),
    ]);

    let plan = resolve(&set).unwrap();
    assert!(!plan.order.contains(&"b".to_owned()));
}

#[test]
fn no_default_unit_is_an_error() {
    let set = units(&[("a", &service(""))]);
    assert!(matches!(resolve(&set), Err(GraphError::NoDefault)));
}

#[test]
fn a_diamond_resolves_once_per_unit() {
    let set = units(&[
        ("default", &target("requires = [\"left\", \"right\"]")),
        (
            "left",
            &service("requires = [\"base\"]\nafter = [\"base\"]"),
        ),
        (
            "right",
            &service("requires = [\"base\"]\nafter = [\"base\"]"),
        ),
        ("base", &service("")),
    ]);

    let plan = resolve(&set).unwrap();
    assert_eq!(
        plan.order.len(),
        4,
        "base must appear once: {:?}",
        plan.order
    );
    assert!(position(&plan.order, "base") < position(&plan.order, "left"));
    assert!(position(&plan.order, "base") < position(&plan.order, "right"));
}

#[test]
fn order_is_stable_across_runs() {
    let set = units(&[
        ("default", &target("requires = [\"a\", \"b\", \"c\"]")),
        ("a", &service("")),
        ("b", &service("")),
        ("c", &service("")),
    ]);

    let first = resolve(&set).unwrap().order;
    for _ in 0..5 {
        assert_eq!(resolve(&set).unwrap().order, first);
    }
}

#[test]
fn resolves_from_an_arbitrary_root() {
    let set = units(&[
        ("default", &target("")),
        ("web", &service("requires = [\"db\"]\nafter = [\"db\"]")),
        ("db", &service("")),
    ]);

    let plan = resolve_from(&set, "web").unwrap();
    assert_eq!(plan.order, ["db", "web"]);
}

/// A socket unit listening on one path and activating `service`.
fn socket(service: &str, deps: &str) -> String {
    format!("[socket]\nlisten = [\"/run/x.sock\"]\nservice = \"{service}\"\n[unit]\n{deps}")
}

#[test]
fn a_socket_does_not_pull_its_service_in() {
    let set = units(&[
        ("default", &target("wants = [\"web-socket\"]")),
        ("web-socket", &socket("web", "")),
        ("web", &service("")),
    ]);

    let plan = resolve(&set).unwrap();

    // The whole point of socket activation: the socket binds at boot, the
    // service waits for a connection.
    assert!(plan.order.contains(&"web-socket".to_owned()));
    assert!(
        !plan.order.contains(&"web".to_owned()),
        "a service reachable only through its socket must not start at boot"
    );
}

#[test]
fn a_socket_is_ordered_before_its_service_without_being_asked() {
    let set = units(&[
        // Both started at boot, which an operator may well want.
        ("default", &target("wants = [\"web-socket\", \"web\"]")),
        ("web-socket", &socket("web", "")),
        ("web", &service("")),
    ]);

    let plan = resolve(&set).unwrap();

    assert!(
        position(&plan.order, "web-socket") < position(&plan.order, "web"),
        "the descriptors must be bound before the service that inherits them"
    );
}

#[test]
fn a_socket_activating_a_missing_unit_is_fatal() {
    let set = units(&[
        ("default", &target("wants = [\"web-socket\"]")),
        ("web-socket", &socket("web", "")),
    ]);

    // Unlike a missing `wants`. Binding a port nothing can ever answer is
    // worse than not binding it.
    assert!(matches!(
        resolve(&set),
        Err(GraphError::MissingSocketService { .. })
    ));
}

#[test]
fn a_socket_cannot_activate_a_target() {
    let set = units(&[
        ("default", &target("wants = [\"web-socket\"]")),
        ("web-socket", &socket("web", "")),
        ("web", &target("")),
    ]);

    assert!(matches!(
        resolve(&set),
        Err(GraphError::SocketServiceNotAService { .. })
    ));
}

#[test]
fn the_implicit_socket_edge_can_close_a_cycle_and_is_reported() {
    let set = units(&[
        ("default", &target("wants = [\"web-socket\", \"web\"]")),
        ("web-socket", &socket("web", "after = [\"web\"]")),
        ("web", &service("")),
    ]);

    // socket -> service implicitly, service -> socket by the `after`. A cycle
    // is a cycle however one of its edges got there.
    assert!(matches!(resolve(&set), Err(GraphError::Cycle { .. })));
}
