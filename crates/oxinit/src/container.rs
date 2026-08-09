//! Whether oxinit booted a machine or was handed a container.
//!
//! One thing changes as a direct consequence: on a machine the end of a
//! shutdown is `reboot(2)`, and in a container it is exiting, because the
//! machine is not oxinit's to reboot. See [`crate::shutdown`].
//!
//! Nothing else keys off this. The console and the mounts each decide on their
//! own local evidence — whether stdio is already usable, whether a filesystem
//! is already mounted — because those questions have exact answers and "am I in
//! a container" does not. A privileged container has `/dev/console` and can
//! mount; an initramfs may arrive with `/proc` already there. Guessing the
//! specific from the general would be wrong in both directions.

use std::fmt;

/// Where oxinit is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Environment {
    /// PID 1 of a machine. Owns the hardware, and shutdown means `reboot(2)`.
    Machine,
    /// PID 1 of a container, as named by whatever evidence found it.
    Container(Runtime),
}

impl Environment {
    /// Look for evidence, in descending order of how much it proves.
    pub fn detect() -> Self {
        for (runtime, found) in EVIDENCE {
            if let Some(runtime) = found(runtime) {
                return Environment::Container(runtime);
            }
        }

        Environment::Machine
    }

    pub fn is_container(&self) -> bool {
        matches!(self, Environment::Container(_))
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Environment::Machine => f.write_str("a machine"),
            Environment::Container(runtime) => write!(f, "a container ({runtime})"),
        }
    }
}

/// What said so, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    /// The runtime's own name for itself, where it gave one.
    name: String,
    /// Which check matched.
    evidence: &'static str,
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}, by {}", self.name, self.evidence)
    }
}

type Check = fn(&'static str) -> Option<Runtime>;

/// The checks, in order. The first match wins, so the ones that name the
/// runtime come before the one that only proves there is one.
const EVIDENCE: &[(&str, Check)] = &[
    ("$container", from_env),
    ("/run/.containerenv", from_marker),
    ("/.dockerenv", from_marker),
    ("CapEff", from_capabilities),
];

/// `container=podman`, `container=lxc`, `container=systemd-nspawn`.
///
/// The convention systemd established and the one every runtime that bothers
/// to identify itself follows. It is also the only check that names the
/// runtime on the runtime's own authority rather than by inference.
fn from_env(evidence: &'static str) -> Option<Runtime> {
    let name = std::env::var("container").ok()?;
    if name.is_empty() {
        return None;
    }

    Some(Runtime { name, evidence })
}

/// A file a runtime drops in the image to say it is there.
///
/// Docker writes `/.dockerenv`; podman writes `/run/.containerenv`, which is
/// an environment file and may be empty. Neither is documented as an
/// interface, and both have been relied on long enough to be one.
fn from_marker(path: &'static str) -> Option<Runtime> {
    if !std::path::Path::new(path).exists() {
        return None;
    }

    let name = match path {
        "/.dockerenv" => "docker",
        "/run/.containerenv" => "podman",
        other => other,
    };

    Some(Runtime {
        name: name.to_owned(),
        evidence: path,
    })
}

/// PID 1 without `CAP_SYS_ADMIN`.
///
/// The backstop for a runtime that leaves no marker and sets no variable. PID
/// 1 of a machine has every capability there is — the kernel starts it with a
/// full set and nothing has run yet to take any away. One that is missing
/// `CAP_SYS_ADMIN` was started by something that dropped it, and the things
/// that do that are container runtimes.
///
/// It proves there is a runtime; it cannot say which, so it is last. It is
/// also incomplete in the other direction, which is why it is not the only
/// check: `--privileged` keeps the full set, and that container looks exactly
/// like a machine here.
fn from_capabilities(evidence: &'static str) -> Option<Runtime> {
    /// Bit 21 of the capability set. Fixed by the kernel ABI.
    const CAP_SYS_ADMIN: u32 = 21;

    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(str::trim)
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())?;

    if effective & (1u64 << CAP_SYS_ADMIN) != 0 {
        return None;
    }

    Some(Runtime {
        name: "unidentified".to_owned(),
        evidence,
    })
}
