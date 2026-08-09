//! The unit model, and TOML into it.

use std::net::SocketAddr;
use std::time::Duration;

use oxinit_timer::{Calendar, Schedule};
use serde::Deserialize;

use crate::error::UnitError;
use crate::exec::{self, Specifiers};
use crate::value::{DurationValue, SizeValue};

/// Default delay before a restart, before backoff scales it.
const DEFAULT_RESTART_SEC: Duration = Duration::from_millis(100);

/// Default bound on `Activating`.
///
/// Generous, because it is a backstop rather than a schedule: a service that
/// legitimately takes a minute to come up must not be killed for it, and one
/// that is never coming up must not hold the boot open forever.
const DEFAULT_START_SEC: Duration = Duration::from_secs(90);

/// Default grace period between `SIGTERM` and `cgroup.kill`.
const DEFAULT_STOP_SEC: Duration = Duration::from_secs(30);

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
    /// Listening descriptors, and the service they belong to. Runs no process
    /// and owns no cgroup either; the service it activates does both.
    Socket(Socket),
    /// A schedule, and the service it starts. Runs no process and owns no
    /// cgroup; being armed is the whole of what it does.
    Timer(Timer),
}

/// Default `listen(2)` backlog.
const DEFAULT_BACKLOG: u32 = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socket {
    /// One descriptor per entry, in this order, which is the order the service
    /// receives them in.
    pub listen: Vec<Listen>,
    /// The service to start when a connection arrives.
    pub service: String,
    pub ty: SocketType,
    pub backlog: u32,
}

/// When to start something, and what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timer {
    pub schedule: Schedule,
    /// The service to start when it elapses.
    ///
    /// Required rather than inferred from the timer's own name, for the reason
    /// a socket unit's is: unit names are unique across kinds, which is what
    /// lets `requires`, `after` and `oxctl` take a bare name — so a timer and
    /// its service cannot share one.
    pub service: String,
}

/// One address to bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// A literal IP and port. Never a hostname: PID 1 does not do DNS, and a
    /// boot that waits on a resolver is a boot that hangs when it is down.
    Inet(SocketAddr),
    /// An absolute path.
    Unix(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SocketType {
    #[default]
    Stream,
    Datagram,
    Seqpacket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub ty: ServiceType,
    /// Already expanded and split. Never passed to a shell.
    pub exec: Vec<String>,
    pub restart: Restart,
    pub restart_sec: Duration,
    /// How long the service may stay `Activating` before it has failed.
    ///
    /// Unbounded activation is what makes one service that never signals
    /// readiness stall every unit ordered after it, with nothing to break the
    /// deadlock.
    pub start_sec: Duration,
    /// How long the service has between `SIGTERM` and `cgroup.kill`.
    pub stop_sec: Duration,
    /// How long the service may go without a `WATCHDOG=1` ping. `None`
    /// disables the watchdog.
    pub watchdog_sec: Option<Duration>,
    pub user: String,
    /// Start the process in its own session and make its standard input the
    /// session's controlling terminal.
    ///
    /// For the one unit that is a login shell or a getty. Without it a shell
    /// on the console has no job control — no Ctrl-C, no `fg`, no `bg` — and
    /// passes that on to everything run from it. With it on more than one unit
    /// the units take the terminal from each other, which is why it is
    /// declared rather than inferred from stdin happening to be a tty.
    pub tty: bool,
    /// Where the service's standard output and error go.
    pub output: Output,
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

/// Where a service's `stdout` and `stderr` go.
///
/// They share one destination, and for `log` one pipe. The order in which a
/// service wrote to its two streams is information, and two pipes would
/// destroy it — they would be read independently and interleaved by whichever
/// the reader reached first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Output {
    /// The descriptors oxinit itself has: the console on a machine, the
    /// runtime's pipes in a container.
    ///
    /// The default, and deliberately not `log`. oxinit does not move a
    /// service's output somewhere the operator did not ask for, and on an
    /// image with no `oxlogd` a `log` default would be a pipe with no reader.
    #[default]
    Console,
    /// A pipe to `oxlogd`, which writes `/var/log/oxinit/<unit>.log`.
    Log,
    /// `/dev/null`.
    Null,
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
            Kind::Socket(_) => "socket",
            Kind::Timer(_) => "timer",
        };
        format!("{}.{}", self.name, suffix)
    }

    pub fn timer(&self) -> Option<&Timer> {
        match &self.kind {
            Kind::Timer(timer) => Some(timer),
            _ => None,
        }
    }

    pub fn service(&self) -> Option<&Service> {
        match &self.kind {
            Kind::Service(service) => Some(service),
            _ => None,
        }
    }

    pub fn socket(&self) -> Option<&Socket> {
        match &self.kind {
            Kind::Socket(socket) => Some(socket),
            _ => None,
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
    socket: Option<RawSocket>,
    timer: Option<RawTimer>,
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
    start_sec: Option<DurationValue>,
    stop_sec: Option<DurationValue>,
    watchdog_sec: Option<DurationValue>,
    user: Option<String>,
    tty: Option<bool>,
    output: Option<Output>,
}

/// Targets take no keys. The section's presence is the whole of it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawSocket {
    listen: Vec<String>,
    service: String,
    #[serde(rename = "type")]
    ty: Option<SocketType>,
    backlog: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawTimer {
    service: String,
    on_boot: Option<DurationValue>,
    interval: Option<DurationValue>,
    on_calendar: Option<String>,
}

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

    // Exactly one kind section, counted rather than matched: three optional
    // sections is eight cases, and seven of them are the same error.
    let sections = usize::from(raw.service.is_some())
        + usize::from(raw.target.is_some())
        + usize::from(raw.socket.is_some())
        + usize::from(raw.timer.is_some());

    if sections == 0 {
        return Err(UnitError::NoKind {
            unit: name.to_owned(),
        });
    }
    if sections > 1 {
        return Err(UnitError::MultipleKinds {
            unit: name.to_owned(),
        });
    }

    // Only a service has a process to limit, so anything else carrying
    // [resources] is an error rather than a silent no-op.
    if raw.service.is_none() && raw.resources.is_some() {
        return Err(UnitError::ResourcesOnNonService {
            unit: name.to_owned(),
        });
    }

    let kind = match (raw.service, raw.socket, raw.timer) {
        (Some(service), None, None) => {
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
                start_sec: service.start_sec.map_or(DEFAULT_START_SEC, |value| value.0),
                stop_sec: service.stop_sec.map_or(DEFAULT_STOP_SEC, |value| value.0),
                watchdog_sec,
                user,
                tty: service.tty.unwrap_or(false),
                output: service.output.unwrap_or_default(),
                resources: match raw.resources {
                    Some(res) => Resources {
                        memory_max: res.memory_max,
                        tasks_max: res.tasks_max,
                    },
                    None => Resources::default(),
                },
            })
        }

        (None, Some(socket), None) => {
            let ty = socket.ty.unwrap_or_default();

            if socket.listen.is_empty() {
                return Err(UnitError::EmptyListen {
                    unit: name.to_owned(),
                });
            }

            // A datagram socket is never listen()ed on, so a backlog for it
            // is a value that would be read and thrown away.
            if socket.backlog.is_some() && ty == SocketType::Datagram {
                return Err(UnitError::BacklogOnDatagram {
                    unit: name.to_owned(),
                });
            }

            let listen = socket
                .listen
                .iter()
                .map(|address| {
                    parse_listen(address).ok_or_else(|| UnitError::ListenAddress {
                        unit: name.to_owned(),
                        address: address.clone(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            Kind::Socket(Socket {
                listen,
                service: socket.service,
                ty,
                backlog: socket.backlog.unwrap_or(DEFAULT_BACKLOG),
            })
        }

        (None, None, Some(timer)) => {
            let calendar = timer
                .on_calendar
                .as_deref()
                .map(Calendar::parse)
                .transpose()
                .map_err(|source| UnitError::Calendar {
                    unit: name.to_owned(),
                    source,
                })?;

            let schedule = Schedule {
                on_boot: timer.on_boot.map(|value| value.0),
                interval: timer.interval.map(|value| value.0),
                calendar,
            };

            // A timer with nothing to fire on is a unit that would sit armed
            // forever. Refused at load, where it can be reported against the
            // file that says it, rather than at boot where it looks like a
            // service that never ran.
            if schedule.is_empty() {
                return Err(UnitError::EmptySchedule {
                    unit: name.to_owned(),
                });
            }

            Kind::Timer(Timer {
                schedule,
                service: timer.service,
            })
        }

        // Counted above, so the only combination left is the target.
        _ => Kind::Target,
    };

    Ok(Unit {
        name: name.to_owned(),
        description: meta.description.unwrap_or_else(|| name.to_owned()),
        deps,
        kind,
    })
}

/// One `listen` entry: an absolute path, or a literal address and port.
///
/// Hostnames are rejected rather than resolved. PID 1 does not do DNS, and a
/// boot that waits on a resolver is a boot that hangs when the resolver is
/// down — which, on a machine this is the init of, it will be.
///
/// A bare port is rejected too. `0.0.0.0:22` and `[::]:22` are different
/// requests, and guessing which one `22` meant is how a service ends up
/// reachable on an interface nobody intended.
fn parse_listen(address: &str) -> Option<Listen> {
    if address.starts_with('/') {
        return Some(Listen::Unix(address.to_owned()));
    }

    // `SocketAddr` accepts exactly what is documented: `1.2.3.4:80` and
    // `[::1]:80`, and nothing that needs resolving.
    address.parse().ok().map(Listen::Inet)
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
stop-sec    = "20s"
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
        assert_eq!(service.stop_sec, Duration::from_secs(20));
        assert_eq!(service.user, "root");
        assert_eq!(
            service.resources.memory_max,
            Some(SizeValue::Bytes(256 * 1024 * 1024))
        );
        assert_eq!(service.resources.tasks_max, Some(512));
    }

    #[test]
    fn parses_a_timer() {
        let unit = parse_ok(
            "nightly",
            "[timer]\nservice = \"backup\"\non-boot = \"30s\"\ninterval = \"1h\"\non-calendar = \"03:30\"\n",
        );

        assert_eq!(unit.full_name(), "nightly.timer");

        let timer = unit.timer().unwrap();
        assert_eq!(timer.service, "backup");
        assert_eq!(timer.schedule.on_boot, Some(Duration::from_secs(30)));
        assert_eq!(timer.schedule.interval, Some(Duration::from_secs(3600)));
        assert_eq!(
            timer.schedule.calendar,
            Some(oxinit_timer::Calendar::At {
                hour: 3,
                minute: 30,
                second: 0
            })
        );
    }

    #[test]
    fn a_timer_needs_something_to_fire_on() {
        // Otherwise it is a unit that sits armed forever, and looks from the
        // outside like a service that never ran.
        assert!(parse("t", "[timer]\nservice = \"backup\"\n", "h").is_err());
    }

    #[test]
    fn a_timer_rejects_a_schedule_it_cannot_read() {
        assert!(parse(
            "t",
            "[timer]\nservice = \"b\"\non-calendar = \"*/5\"\n",
            "h"
        )
        .is_err());
    }

    #[test]
    fn a_unit_is_one_kind() {
        assert!(
            parse(
                "t",
                "[timer]\nservice = \"b\"\ninterval = \"1h\"\n[target]\n",
                "h"
            )
            .is_err(),
            "a timer and a target in one file is not a unit"
        );
    }

    #[test]
    fn tty_is_opt_in() {
        let unit = parse_ok("console", "[service]\nexec = \"/bin/sh\"\ntty = true\n");
        assert!(unit.service().unwrap().tty);
    }

    #[test]
    fn output_is_opt_in() {
        let unit = parse_ok("x", "[service]\nexec = \"/bin/true\"\noutput = \"log\"\n");
        assert_eq!(unit.service().unwrap().output, Output::Log);

        assert!(
            parse(
                "x",
                "[service]\nexec = \"/bin/true\"\noutput = \"journal\"\n",
                "h"
            )
            .is_err(),
            "an unknown destination is a load error, not a silent console"
        );
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
        assert_eq!(service.start_sec, DEFAULT_START_SEC);
        assert_eq!(service.stop_sec, DEFAULT_STOP_SEC);
        assert_eq!(service.watchdog_sec, None, "watchdog is off by default");
        assert_eq!(service.user, "root");
        assert!(!service.tty, "no controlling terminal by default");
        assert_eq!(
            service.output,
            Output::Console,
            "output is not moved anywhere the operator did not ask for"
        );
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

        let socket = "[socket]\nlisten = [\"/run/x\"]\nservice = \"y\"\n";
        assert!(parse("x", &format!("{socket}[target]\n"), "h").is_err());
        assert!(
            parse(
                "x",
                &format!("{socket}[service]\nexec = \"/bin/true\"\n"),
                "h"
            )
            .is_err(),
            "a socket unit is not also a service unit"
        );
    }

    #[test]
    fn parses_the_documented_socket() {
        let text = "\
[unit]
description = \"OpenSSH socket\"

[socket]
listen  = [\"0.0.0.0:22\", \"[::]:22\", \"/run/sshd.sock\"]
service = \"sshd\"
type    = \"stream\"
backlog = 64
";
        let unit = parse_ok("sshd-socket", text);
        assert_eq!(unit.full_name(), "sshd-socket.socket");
        assert!(unit.service().is_none());

        let socket = unit.socket().unwrap();
        assert_eq!(socket.service, "sshd");
        assert_eq!(socket.ty, SocketType::Stream);
        assert_eq!(socket.backlog, 64);
        assert_eq!(
            socket.listen,
            [
                Listen::Inet("0.0.0.0:22".parse().unwrap()),
                Listen::Inet("[::]:22".parse().unwrap()),
                Listen::Unix("/run/sshd.sock".to_owned()),
            ],
            "order is preserved: it is the order the service gets the fds in"
        );
    }

    #[test]
    fn socket_defaults() {
        let unit = parse_ok("s", "[socket]\nlisten = [\"/run/x\"]\nservice = \"x\"\n");
        let socket = unit.socket().unwrap();
        assert_eq!(socket.ty, SocketType::Stream);
        assert_eq!(socket.backlog, DEFAULT_BACKLOG);
    }

    #[test]
    fn a_socket_needs_somewhere_to_listen_and_something_to_start() {
        assert!(parse("s", "[socket]\nlisten = []\nservice = \"x\"\n", "h").is_err());
        assert!(parse("s", "[socket]\nservice = \"x\"\n", "h").is_err());
        assert!(parse("s", "[socket]\nlisten = [\"/run/x\"]\n", "h").is_err());
    }

    #[test]
    fn rejects_listen_addresses_that_would_have_to_be_guessed_at() {
        let bad = |address: &str| {
            let text = format!("[socket]\nlisten = [\"{address}\"]\nservice = \"x\"\n");
            parse("s", &text, "h").is_err()
        };

        assert!(bad("22"), "a bare port does not say which interface");
        assert!(bad("localhost:22"), "no DNS in PID 1");
        assert!(bad("example.com:80"));
        assert!(bad("run/x.sock"), "a unix path must be absolute");
        assert!(bad("0.0.0.0"), "no port");
        assert!(bad("::1:22"), "an IPv6 address needs brackets");
        assert!(bad(""));
    }

    #[test]
    fn rejects_a_backlog_on_a_datagram_socket() {
        let text = "[socket]\nlisten = [\"0.0.0.0:69\"]\nservice = \"x\"\n\
                    type = \"datagram\"\nbacklog = 32\n";
        assert!(
            parse("s", text, "h").is_err(),
            "nothing listens on a datagram socket"
        );

        // Without the backlog it is fine.
        let ok = "[socket]\nlisten = [\"0.0.0.0:69\"]\nservice = \"x\"\ntype = \"datagram\"\n";
        assert_eq!(parse_ok("s", ok).socket().unwrap().ty, SocketType::Datagram);
    }

    #[test]
    fn rejects_resources_on_anything_but_a_service() {
        assert!(parse("x", "[target]\n[resources]\ntasks-max = 10\n", "h").is_err());
        assert!(parse(
            "x",
            "[socket]\nlisten = [\"/run/x\"]\nservice = \"y\"\n[resources]\ntasks-max = 10\n",
            "h"
        )
        .is_err());
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
