//! The unit model, and TOML into it.

use std::time::Duration;

use serde::de::IgnoredAny;
use serde::Deserialize;

use crate::error::UnitError;
use crate::exec::{self, Specifiers};
use crate::value::{DurationValue, SizeValue};

/// Default delay before a restart, before backoff scales it.
const DEFAULT_RESTART_SEC: Duration = Duration::from_millis(100);

/// A parsed, validated unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    pub description: String,
    pub deps: Deps,
    pub kind: Kind,
}

/// Requirement and ordering are separate axes. `requires` implies no ordering;
/// `after` creates no dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deps {
    pub requires: Vec<String>,
    pub wants: Vec<String>,
    pub after: Vec<String>,
    pub before: Vec<String>,
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Service(Service),
    /// A named group of dependencies. Runs no process, owns no cgroup.
    Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub ty: ServiceType,
    /// Already expanded and split. Never passed to a shell.
    pub exec: Vec<String>,
    pub restart: Restart,
    pub restart_sec: Duration,
    /// How long the service may go without a `WATCHDOG=1` ping. `None`
    /// disables the watchdog.
    pub watchdog_sec: Option<Duration>,
    pub user: String,
    pub resources: Resources,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceType {
    #[default]
    Simple,
    Forking,
    Oneshot,
    Notify,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    #[default]
    No,
    Always,
    OnFailure,
    OnAbnormal,
}

/// An absent limit is the kernel default, which is unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resources {
    pub memory_max: Option<SizeValue>,
    pub tasks_max: Option<u64>,
}

impl Unit {
    /// The fully-qualified name, `<name>.<kind>`.
    pub fn full_name(&self) -> String {
        let suffix = match self.kind {
            Kind::Service(_) => "service",
            Kind::Target => "target",
        };
        format!("{}.{}", self.name, suffix)
    }

    pub fn service(&self) -> Option<&Service> {
        match &self.kind {
            Kind::Service(service) => Some(service),
            Kind::Target => None,
        }
    }
}

// The TOML shape. `deny_unknown_fields` everywhere: an unknown key is an
// error, because a typo in a resource limit must not silently mean "no limit".

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    unit: Option<RawUnit>,
    service: Option<RawService>,
    target: Option<RawTarget>,
    socket: Option<IgnoredAny>,
    resources: Option<RawResources>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawUnit {
    description: Option<String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    wants: Vec<String>,
    #[serde(default)]
    after: Vec<String>,
    #[serde(default)]
    before: Vec<String>,
    #[serde(default)]
    conflicts: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawService {
    #[serde(rename = "type")]
    ty: Option<ServiceType>,
    exec: String,
    restart: Option<Restart>,
    restart_sec: Option<DurationValue>,
    watchdog_sec: Option<DurationValue>,
    user: Option<String>,
}

/// Targets take no keys. The section's presence is the whole of it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawResources {
    memory_max: Option<SizeValue>,
    tasks_max: Option<u64>,
}

/// Parse one unit file.
///
/// `name` is the filename without `.toml`. `hostname` feeds the `%H`
/// specifier; this crate does not read it from the system itself.
pub fn parse(name: &str, text: &str, hostname: &str) -> Result<Unit, UnitError> {
    validate_name(name)?;

    let raw: RawFile = basic_toml::from_str(text).map_err(|source| UnitError::Toml {
        unit: name.to_owned(),
        message: source.to_string(),
    })?;

    if raw.socket.is_some() {
        return Err(UnitError::SocketReserved {
            unit: name.to_owned(),
        });
    }

    let meta = raw.unit.unwrap_or_default();
    let deps = Deps {
        requires: meta.requires,
        wants: meta.wants,
        after: meta.after,
        before: meta.before,
        conflicts: meta.conflicts,
    };

    if let Some(other) = deps.requires.iter().find(|r| deps.conflicts.contains(r)) {
        return Err(UnitError::RequiresAndConflicts {
            unit: name.to_owned(),
            other: other.clone(),
        });
    }

    let kind = match (raw.service, raw.target) {
        (Some(service), None) => {
            let user = service.user.unwrap_or_else(|| "root".to_owned());
            let ty = service.ty.unwrap_or_default();

            let specifiers = Specifiers {
                name: name.to_owned(),
                full_name: format!("{name}.service"),
                hostname: hostname.to_owned(),
                user: user.clone(),
            };

            let exec =
                exec::parse(&service.exec, &specifiers).map_err(|source| UnitError::Exec {
                    unit: name.to_owned(),
                    source,
                })?;

            let watchdog_sec = service.watchdog_sec.map(|value| value.0);
            if watchdog_sec.is_some() && ty != ServiceType::Notify {
                return Err(UnitError::WatchdogWithoutNotify {
                    unit: name.to_owned(),
                });
            }

            Kind::Service(Service {
                ty,
                exec,
                restart: service.restart.unwrap_or_default(),
                restart_sec: service
                    .restart_sec
                    .map_or(DEFAULT_RESTART_SEC, |value| value.0),
                watchdog_sec,
                user,
                resources: match raw.resources {
                    Some(res) => Resources {
                        memory_max: res.memory_max,
                        tasks_max: res.tasks_max,
                    },
                    None => Resources::default(),
                },
            })
        }

        (None, Some(RawTarget {})) => {
            // A target has no process, so a resource limit on it would be a
            // limit on nothing. An error rather than a silent no-op.
            if raw.resources.is_some() {
                return Err(UnitError::ResourcesOnNonService {
                    unit: name.to_owned(),
                });
            }
            Kind::Target
        }

        (None, None) => {
            return Err(UnitError::NoKind {
                unit: name.to_owned(),
            })
        }
        (Some(_), Some(_)) => {
            return Err(UnitError::MultipleKinds {
                unit: name.to_owned(),
            })
        }
    };

    Ok(Unit {
        name: name.to_owned(),
        description: meta.description.unwrap_or_else(|| name.to_owned()),
        deps,
        kind,
    })
}

/// Names must be usable as a filename component and as a cgroup path segment.
fn validate_name(name: &str) -> Result<(), UnitError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));

    if ok {
        Ok(())
    } else {
        Err(UnitError::Name {
            unit: name.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSHD: &str = r#"
[unit]
description = "OpenSSH daemon"
after       = ["network-online"]
requires    = ["network-online"]

[service]
type        = "notify"
exec        = "/usr/sbin/sshd -D"
restart     = "on-failure"
restart-sec = "5s"
user        = "root"

[resources]
memory-max = "256M"
tasks-max  = 512
"#;

    fn parse_ok(name: &str, text: &str) -> Unit {
        parse(name, text, "testhost").unwrap()
    }

    #[test]
    fn parses_the_documented_example() {
        let unit = parse_ok("sshd", SSHD);

        assert_eq!(unit.name, "sshd");
        assert_eq!(unit.description, "OpenSSH daemon");
        assert_eq!(unit.full_name(), "sshd.service");
        assert_eq!(unit.deps.after, ["network-online"]);
        assert_eq!(unit.deps.requires, ["network-online"]);

        let service = unit.service().unwrap();
        assert_eq!(service.ty, ServiceType::Notify);
        assert_eq!(service.exec, ["/usr/sbin/sshd", "-D"]);
        assert_eq!(service.restart, Restart::OnFailure);
        assert_eq!(service.restart_sec, Duration::from_secs(5));
        assert_eq!(service.user, "root");
        assert_eq!(
            service.resources.memory_max,
            Some(SizeValue::Bytes(256 * 1024 * 1024))
        );
        assert_eq!(service.resources.tasks_max, Some(512));
    }

    #[test]
    fn watchdog_requires_type_notify() {
        let ok = "[service]\ntype = \"notify\"\nexec = \"/bin/true\"\nwatchdog-sec = \"30s\"\n";
        let unit = parse_ok("x", ok);
        assert_eq!(
            unit.service().unwrap().watchdog_sec,
            Some(Duration::from_secs(30))
        );

        // Nothing but notify has a channel to ping on.
        let bad = "[service]\nexec = \"/bin/true\"\nwatchdog-sec = \"30s\"\n";
        assert!(parse("x", bad, "h").is_err());
    }

    #[test]
    fn applies_documented_defaults() {
        let unit = parse_ok("x", "[service]\nexec = \"/bin/true\"\n");
        let service = unit.service().unwrap();

        assert_eq!(unit.description, "x", "description defaults to the name");
        assert_eq!(service.ty, ServiceType::Simple);
        assert_eq!(service.restart, Restart::No);
        assert_eq!(service.restart_sec, DEFAULT_RESTART_SEC);
        assert_eq!(service.watchdog_sec, None, "watchdog is off by default");
        assert_eq!(service.user, "root");
        assert_eq!(service.resources, Resources::default());
        assert_eq!(unit.deps, Deps::default());
    }

    #[test]
    fn parses_a_target() {
        let unit = parse_ok("multi-user", "[target]\n[unit]\nwants = [\"sshd\"]\n");
        assert_eq!(unit.kind, Kind::Target);
        assert_eq!(unit.full_name(), "multi-user.target");
        assert!(unit.service().is_none());
    }

    #[test]
    fn rejects_unknown_key_and_section() {
        assert!(parse("x", "[service]\nexec = \"/bin/true\"\nnope = 1\n", "h").is_err());
        assert!(parse("x", "[service]\nexec = \"/bin/true\"\n[nope]\n", "h").is_err());
        // A typo in a limit must not silently mean "no limit".
        assert!(parse(
            "x",
            "[service]\nexec = \"/bin/true\"\n[resources]\nmemory_max = \"1G\"\n",
            "h"
        )
        .is_err());
    }

    #[test]
    fn requires_exactly_one_kind_section() {
        assert!(parse("x", "[unit]\ndescription = \"nothing\"\n", "h").is_err());
        assert!(parse("x", "", "h").is_err(), "empty file has no kind");
        assert!(parse("x", "[service]\nexec = \"/bin/true\"\n[target]\n", "h").is_err());
    }

    #[test]
    fn rejects_socket_units_until_m4() {
        assert!(parse("x", "[socket]\nlisten = \"0.0.0.0:22\"\n", "h").is_err());
    }

    #[test]
    fn rejects_resources_on_a_target() {
        assert!(parse("x", "[target]\n[resources]\ntasks-max = 10\n", "h").is_err());
    }

    #[test]
    fn requires_exec_in_a_service() {
        assert!(parse("x", "[service]\ntype = \"simple\"\n", "h").is_err());
    }

    #[test]
    fn rejects_wrong_types() {
        // tasks-max takes a bare integer; a size string is a type error.
        assert!(parse(
            "x",
            "[service]\nexec = \"/bin/true\"\n[resources]\ntasks-max = \"512\"\n",
            "h"
        )
        .is_err());
        // restart-sec takes a duration string; a bare number is not one.
        assert!(parse(
            "x",
            "[service]\nexec = \"/bin/true\"\nrestart-sec = 5\n",
            "h"
        )
        .is_err());
        assert!(parse(
            "x",
            "[service]\nexec = \"/bin/true\"\ntype = \"weird\"\n",
            "h"
        )
        .is_err());
        assert!(parse(
            "x",
            "[service]\nexec = \"/bin/true\"\nrestart = \"maybe\"\n",
            "h"
        )
        .is_err());
    }

    #[test]
    fn rejects_requires_and_conflicts_on_the_same_unit() {
        let text = "[service]\nexec = \"/bin/true\"\n\
                    [unit]\nrequires = [\"a\"]\nconflicts = [\"a\"]\n";
        assert!(parse("x", text, "h").is_err());
    }

    #[test]
    fn rejects_bad_names() {
        let body = "[service]\nexec = \"/bin/true\"\n";
        assert!(parse("../escape", body, "h").is_err());
        assert!(parse("with/slash", body, "h").is_err());
        assert!(parse("", body, "h").is_err());
        assert!(parse("ok.name-1_2", body, "h").is_ok());
    }

    #[test]
    fn expands_specifiers_in_exec() {
        let unit = parse("web", "[service]\nexec = \"/bin/x %n %N %H\"\n", "myhost").unwrap();
        assert_eq!(
            unit.service().unwrap().exec,
            ["/bin/x", "web", "web.service", "myhost"]
        );
    }
}
