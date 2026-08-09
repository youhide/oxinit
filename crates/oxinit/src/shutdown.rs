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
pub fn finalize(action: Action) -> rustix::io::Result<()> {
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
