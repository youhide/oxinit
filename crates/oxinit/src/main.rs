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
mod event;
mod mounts;
mod notify;
mod reap;
mod shell;
mod supervisor;
mod sys;
mod timer;

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
        // left stderr usable.
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

    // Signals become event loop input rather than interruptions. Blocking must
    // happen before the signalfd is created: a signal that is not blocked is
    // delivered normally and never reaches the fd.
    let signals = match open_signalfd() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("oxinit: {e}");
            fallback(None)
        }
    };

    let timers = match timer::Timers::new() {
        Ok(timers) => timers,
        Err(e) => {
            eprintln!("oxinit: {e}");
            fallback(Some(&signals))
        }
    };

    let mut events = match event::EventLoop::new() {
        Ok(events) => events,
        Err(e) => {
            eprintln!("oxinit: {e}");
            fallback(Some(&signals))
        }
    };

    let notify = match notify::Notify::bind() {
        Ok(notify) => notify,
        Err(e) => {
            eprintln!("oxinit: {e}");
            fallback(Some(&signals))
        }
    };

    let mut supervisor = supervisor::Supervisor::load(&hostname, timers, notify);

    let registered = events
        .register(&signals, event::Source::Signals)
        .and_then(|()| events.register(supervisor.timers.as_fd(), event::Source::Timer))
        .and_then(|()| events.register(supervisor.notify.as_fd(), event::Source::Notify));

    if let Err(e) = registered {
        eprintln!("oxinit: {e}");
        fallback(Some(&signals));
    }

    // With nothing to supervise, keep the machine usable the way M0 did.
    let mut shell = if supervisor.is_empty() {
        eprintln!("oxinit: nothing to start; falling back to a console shell");
        shell::spawn().map_err(|e| eprintln!("oxinit: {e}")).ok()
    } else {
        supervisor.start_all();
        None
    };

    run(&mut events, &signals, &mut supervisor, &mut shell)
}

/// The event loop.
fn run(
    events: &mut event::EventLoop,
    signals: &OwnedFd,
    supervisor: &mut supervisor::Supervisor,
    shell: &mut Option<Child>,
) -> ! {
    let mut sources = Vec::new();
    let mut pending = Vec::new();

    loop {
        if let Err(e) = events.wait(&mut sources) {
            eprintln!("oxinit: {e}");
            // A failing epoll would spin this loop at full speed.
            fallback(Some(signals));
        }

        for source in &sources {
            // A panic inside a handler unwinds to here, gets logged, and the
            // loop continues with the next event. AssertUnwindSafe because the
            // handlers hold &mut state across the boundary: a panic mid-update
            // can leave that state stale, which is survivable — the next event
            // corrects it — where letting the panic escape is not.
            let result = catch_unwind(AssertUnwindSafe(|| match source {
                event::Source::Signals => on_signals(signals, supervisor, shell, &mut pending),
                event::Source::Timer => supervisor.on_timer(),
                event::Source::Notify => supervisor.on_notify(),
            }));

            if result.is_err() {
                eprintln!("oxinit: handler panicked; continuing");
            }
        }
    }
}

fn on_signals(
    signals: &OwnedFd,
    supervisor: &mut supervisor::Supervisor,
    shell: &mut Option<Child>,
    pending: &mut Vec<u32>,
) {
    if let Err(e) = sys::raw::read_signals(signals, pending).map_err(Error::ReadSignalFd) {
        eprintln!("oxinit: {e}");
        return;
    }

    for signal in pending.iter() {
        if *signal != SIGCHLD {
            // PID 1 has no default dispositions, so everything else is read
            // and dropped rather than ignored by accident. Acting on SIGTERM
            // and friends is M5.
            continue;
        }

        // The fallback shell is not a unit, so it is checked separately and
        // respawned directly.
        if let Some(child) = shell.as_mut() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                eprintln!("oxinit: {} exited; respawning", shell::SHELL);
                *shell = shell::spawn().map_err(|e| eprintln!("oxinit: {e}")).ok();
            }
        }

        supervisor.on_sigchld();
    }
}

/// Block every signal, then open the signalfd that replaces them.
fn open_signalfd() -> Result<OwnedFd> {
    sys::raw::block_all_signals().map_err(Error::BlockSignals)?;
    sys::raw::signalfd_all().map_err(Error::SignalFd)
}

/// Last resort. The loop cannot continue, so give an operator a shell and keep
/// reaping. PID 1 exiting is a kernel panic, so this never returns.
fn fallback(signals: Option<&OwnedFd>) -> ! {
    eprintln!("oxinit: event loop unavailable; falling back to a console shell");
    let mut child = shell::spawn().map_err(|e| eprintln!("oxinit: {e}")).ok();
    let mut pending = Vec::new();

    loop {
        // Prefer the signalfd if there is one, so this still sleeps in the
        // kernel rather than spinning.
        match signals {
            Some(fd) => {
                let _ = sys::raw::read_signals(fd, &mut pending);
            }
            None => std::thread::sleep(std::time::Duration::from_secs(1)),
        }

        let dead = reap::reap_all().into_iter().any(|(pid, _)| {
            child
                .as_ref()
                .is_some_and(|c| c.id() == pid.as_raw_nonzero().get() as u32)
        });

        if dead || child.is_none() {
            child = shell::spawn().map_err(|e| eprintln!("oxinit: {e}")).ok();
        }
    }
}
