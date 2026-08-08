//! oxinit — PID 1.
//!
//! Milestone 0: mount the pseudo-filesystems, wire the console, block every
//! signal and read them from a signalfd, supervise a shell, reap orphans.
//! No units, no dependency graph, no cgroups. See ROADMAP.md.
//!
//! The rule that shapes all of this: PID 1 cannot exit. If it does, the kernel
//! panics. Every failure below is logged and survived.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

// Without this, building on a non-Linux host fails deep inside the signalfd
// code with errors about missing libc items, which says nothing about the
// actual problem.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "oxinit is Linux-only. Cross-compile it: \
     cargo build --target x86_64-unknown-linux-musl -p oxinit, \
     or use `cargo xtask boot`. See CONTRIBUTING.md."
);

mod console;
mod error;
mod mounts;
mod reap;
mod shell;
mod sys;
mod units;

use std::os::fd::OwnedFd;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Child, ExitCode};

use crate::error::{Error, Result};

/// `SIGCHLD` is the only signal M0 acts on. The rest are read and discarded so
/// they cannot accumulate on the signalfd.
const SIGCHLD: u32 = libc::SIGCHLD as u32;

fn main() -> ExitCode {
    if !rustix::process::getpid().is_init() {
        eprintln!(
            "oxinit: running as pid {}, not 1. oxinit is an init system; \
             it is not meant to be run from a shell.",
            std::process::id()
        );
        return ExitCode::FAILURE;
    }

    boot()
}

/// Never returns. Not "returns on shutdown" — never.
fn boot() -> ! {
    for failure in mounts::mount_all() {
        // Nothing has a console yet, so this is only visible if the kernel
        // left stderr usable. Logged again below once the console is attached.
        eprintln!("oxinit: {failure}");
    }

    if let Err(e) = console::attach() {
        eprintln!("oxinit: {e}");
    }

    let hostname = match mounts::set_hostname() {
        Ok(name) => name,
        Err(e) => {
            eprintln!("oxinit: {e}");
            "localhost".to_owned()
        }
    };

    // From here on, signals are event loop input rather than interruptions.
    // Blocking must happen before the signalfd is created: a signal that is
    // not blocked is delivered normally and never reaches the fd.
    let signals = match open_signalfd() {
        Ok(fd) => Some(fd),
        Err(e) => {
            eprintln!("oxinit: {e}; running without signal handling");
            None
        }
    };

    let started = units::boot(&hostname);

    // With no units to supervise there is nothing to do but keep the machine
    // usable, which is what M0 did. Once units exist the shell is no longer
    // spawned automatically; a unit can ask for one.
    let mut child = if started.is_empty() {
        eprintln!("oxinit: nothing started; falling back to a console shell");
        match shell::spawn() {
            Ok(child) => Some(child),
            Err(e) => {
                eprintln!("oxinit: {e}");
                None
            }
        }
    } else {
        None
    };

    match signals {
        Some(fd) => supervise(fd, &mut child, started),
        // Without a signalfd there is nothing to wait on, so fall back to
        // polling. Degraded, but still a running init rather than a panic.
        None => poll_forever(&mut child, &started),
    }
}

/// Block every signal, then open the signalfd that replaces them.
///
/// Order matters: a signal that is not blocked is delivered normally and never
/// reaches the fd.
fn open_signalfd() -> Result<OwnedFd> {
    sys::raw::block_all_signals().map_err(Error::BlockSignals)?;
    sys::raw::signalfd_all().map_err(Error::SignalFd)
}

/// The M0 event loop.
///
/// One descriptor, so a blocking read is the whole of it. epoll arrives in M1
/// with the second descriptor; multiplexing one fd would be ceremony.
fn supervise(signals: OwnedFd, child: &mut Option<Child>, started: Vec<units::Started>) -> ! {
    let mut pending = Vec::new();

    loop {
        if let Err(e) = sys::raw::read_signals(&signals, &mut pending).map_err(Error::ReadSignalFd)
        {
            eprintln!("oxinit: {e}");
            // A failing signalfd would spin this loop at full speed. Drop to
            // the degraded path instead, which at least paces itself.
            poll_forever(child, &started);
        }

        for signal in &pending {
            if *signal != SIGCHLD {
                continue;
            }

            // A panic inside the handler unwinds to here, gets logged, and the
            // loop continues with the next event. The closure holds `&mut
            // Option<Child>` across the boundary, which is why this needs
            // AssertUnwindSafe: if the handler panics mid-update, `child` may
            // be stale. Stale is survivable — the next SIGCHLD corrects it.
            if catch_unwind(AssertUnwindSafe(|| on_sigchld(child, &started))).is_err() {
                eprintln!("oxinit: SIGCHLD handler panicked; continuing");
            }
        }
    }
}

/// Reap everything that exited, and put the shell back if it was the one that
/// died.
fn on_sigchld(child: &mut Option<Child>, started: &[units::Started]) {
    let mut dead_shell = false;

    for (pid, status) in reap::reap_all() {
        let raw = pid.as_raw_nonzero().get() as u32;

        if child.as_ref().is_some_and(|c| c.id() == raw) {
            eprintln!("oxinit: {} {}", shell::SHELL, reap::describe(&status));
            dead_shell = true;
            continue;
        }

        // A pid belonging to no unit is an orphan the machine reparented here;
        // reaping it is the whole job. For a unit, M1 reports and moves on —
        // restart policy, backoff, and marking it failed are M2.
        if let Some(unit) = units::unit_for_pid(started, raw) {
            println!("oxinit: {unit} {}", reap::describe(&status));
        }
    }

    if dead_shell {
        *child = match shell::spawn() {
            Ok(new) => Some(new),
            Err(e) => {
                eprintln!("oxinit: {e}");
                None
            }
        };
    }
}

/// Degraded loop for when the signalfd is gone.
///
/// Blocking `wait` with no `NOHANG`, so this sleeps in the kernel rather than
/// spinning, and still reaps whatever the machine orphans.
fn poll_forever(child: &mut Option<Child>, started: &[units::Started]) -> ! {
    loop {
        match rustix::process::wait(rustix::process::WaitOptions::empty()) {
            // Blocking wait, so this only returns when something actually
            // exited. Reap the rest and restore the shell if it was the one.
            Ok(Some(_)) => on_sigchld(child, started),

            // No children to wait for, or the wait itself failed. Either way
            // there is nothing to block on, so pace the loop before retrying;
            // without the sleep this spins a core at 100%.
            _ => {
                if child.is_none() {
                    *child = shell::spawn().ok();
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}
