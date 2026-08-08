//! Reaping orphans.
//!
//! PID 1 inherits every orphaned process on the machine. If it does not reap
//! them, the process table fills with zombies.

use rustix::process::{wait, Pid, WaitOptions, WaitStatus};

/// Reap every child that has exited, and report what was reaped.
///
/// `SIGCHLD` does not queue: if four children exit while signals are blocked,
/// the signalfd reports one `SIGCHLD`. Reaping a single child per signal leaks
/// the rest, so this loops until the kernel says there is nothing more.
///
/// Stops on:
/// - `Ok(None)` — children remain, but none have exited.
/// - `Err(ECHILD)` — no children at all.
///
/// Any other error also stops the loop; retrying it would spin.
pub fn reap_all() -> Vec<(Pid, WaitStatus)> {
    let mut reaped = Vec::new();
    loop {
        match wait(WaitOptions::NOHANG) {
            Ok(Some(result)) => reaped.push(result),
            Ok(None) => return reaped,
            Err(_) => return reaped,
        }
    }
}

/// How a child ended, for logging.
pub fn describe(status: &WaitStatus) -> String {
    if let Some(code) = status.exit_status() {
        format!("exit status {code}")
    } else if let Some(signal) = status.terminating_signal() {
        format!("killed by signal {signal}")
    } else {
        "stopped or continued".to_owned()
    }
}
