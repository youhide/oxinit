//! Stopping the machine.
//!
//! Shutdown is not a function that runs to completion. Stopping a unit means
//! signalling it and waiting — for its cgroup to empty, or for its stop
//! timeout — and both of those arrive as events. So a shutdown is a *state*
//! the supervisor is in: every unit is asked to stop, the ordinary event loop
//! keeps running, and when nothing is left the machine goes down.
//!
//! Written that way rather than as a blocking loop because the blocking loop
//! would have to re-implement the reaper, the timers, and the cgroup
//! notifications it is waiting on — during the one part of the boot where
//! getting it wrong strands the machine.

use std::fmt;
use std::time::{Duration, Instant};

use rustix::system::RebootCommand;

use crate::container::Environment;

/// How long the whole shutdown may take before oxinit stops waiting for units
/// and goes down anyway.
///
/// Every unit already has its own `stop-sec`, so this is not the normal
/// mechanism — it is the backstop for a unit whose stop path is itself broken.
/// A machine that will not power off is worse than one that loses a service's
/// last write.
const DEADLINE: Duration = Duration::from_secs(90);

/// What to do once everything has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    PowerOff,
    Reboot,
    /// Stop everything and stay stopped, without cutting power. What you want
    /// when something else is about to cut it for you.
    Halt,
}

impl Action {
    pub fn command(self) -> RebootCommand {
        match self {
            Action::PowerOff => RebootCommand::PowerOff,
            Action::Reboot => RebootCommand::Restart,
            Action::Halt => RebootCommand::Halt,
        }
    }

    /// What a container manager reads out of PID 1's exit status.
    ///
    /// `0` is a clean stop, which is what `docker stop` and a Kubernetes pod
    /// deletion are waiting for — the alternative is the ten-second grace
    /// period expiring and `SIGKILL`, reported as exit 137.
    ///
    /// `133` is the convention systemd established for "restart me", honoured
    /// by podman and by anything reading a container's exit code against a
    /// restart policy. oxinit cannot reboot a container itself; the most it
    /// can do is go down and say which way.
    pub fn status(self) -> i32 {
        match self {
            Action::PowerOff | Action::Halt => 0,
            Action::Reboot => 133,
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Action::PowerOff => "power off",
            Action::Reboot => "reboot",
            Action::Halt => "halt",
        })
    }
}

/// A shutdown in progress.
pub struct Shutdown {
    pub action: Action,
    started: Instant,
}

impl Shutdown {
    pub fn new(action: Action) -> Self {
        Self {
            action,
            started: Instant::now(),
        }
    }

    /// Whether to stop waiting for units that have not stopped.
    pub fn overdue(&self) -> bool {
        self.started.elapsed() > DEADLINE
    }
}

/// Flush, seal, and go down. Does not return unless the kernel refuses.
///
/// The order matters and is the whole of it: `sync` first, because a
/// filesystem that has not been flushed is what makes a reboot lose data;
/// then remount read-only, so anything still holding a write cannot make more.
pub fn finalize(action: Action, environment: &Environment) -> rustix::io::Result<()> {
    if environment.is_container() {
        exit(action)
    }

    println!("oxinit: syncing");
    rustix::fs::sync();

    // Best-effort. On an initramfs the root is a rootfs that cannot be
    // remounted read-only, and failing here would strand a machine that was
    // about to shut down cleanly.
    if let Err(e) = rustix::mount::mount_remount("/", rustix::mount::MountFlags::RDONLY, "") {
        eprintln!("oxinit: remount / read-only: {e}");
    }

    println!("oxinit: {action}");
    rustix::system::reboot(action.command())
}

/// The end of a shutdown in a container.
///
/// **The one place oxinit exits, and the reason the rule is written as an
/// absolute everywhere else.** PID 1 exiting is a kernel panic — on a machine.
/// In a container it is the entire contract: the container is up for exactly as
/// long as its PID 1 is, so a supervisor that has stopped every unit and then
/// stays running is a container that will not stop, and `docker stop` ends it
/// with `SIGKILL` ten seconds later.
///
/// Reached only from the end of an ordered shutdown, with every unit already
/// stopped. No error path leads here; a container whose event loop has failed
/// still falls back to a shell, because that failure is not a reason to take
/// the whole thing down.
///
/// There is no `sync` and no remount. The filesystems belong to the runtime,
/// oxinit is not permitted to seal them, and flushing the host's disks from
/// inside a container is not its call to make.
fn exit(action: Action) -> ! {
    let status = action.status();
    println!("oxinit: {action}: exiting with status {status}");

    std::process::exit(status)
}
