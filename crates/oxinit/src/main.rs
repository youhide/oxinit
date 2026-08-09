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

mod cgroup;
mod console;
mod control;
mod error;
mod event;
mod listen;
mod mounts;
mod notify;
mod reap;
mod shell;
mod shutdown;
mod supervisor;
mod sys;
mod timer;

use std::os::fd::OwnedFd;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Child, ExitCode};

use crate::error::{Error, Result};

/// The signals oxinit acts on. Everything else is read and discarded so it
/// cannot accumulate on the signalfd.
///
/// PID 1 has no default dispositions — the kernel applies none — so a signal
/// that is not in this table does nothing at all. That is why the table is
/// written down rather than left implicit.
const SIGCHLD: u32 = libc::SIGCHLD as u32;

/// Stop everything and power off.
///
/// This is what a container runtime means by `SIGTERM`, and it is the whole
/// of the contract `docker stop` and a Kubernetes pod deletion rely on. On a
/// machine it is not something anyone types by accident.
const SIGTERM: u32 = libc::SIGTERM as u32;

/// Stop everything and reboot. The kernel sends this to PID 1 for
/// Ctrl-Alt-Del, which is the one keystroke that has to mean something.
const SIGINT: u32 = libc::SIGINT as u32;

/// Stop everything and halt, without cutting power.
const SIGUSR1: u32 = libc::SIGUSR1 as u32;

/// Report what every unit is doing. Changes nothing, and is the only way to
/// ask before the control socket exists.
const SIGUSR2: u32 = libc::SIGUSR2 as u32;

/// Lost power. Stop everything and power off while there is still power to do
/// it with.
const SIGPWR: u32 = libc::SIGPWR as u32;

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

    let (hostname, failure) = mounts::set_hostname();
    if let Some(e) = failure {
        eprintln!("oxinit: {e}; using the name already set, {hostname}");
    }

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

    // Not fatal. A machine with no control socket still boots and still
    // supervises; it just cannot be asked anything until the next boot, which
    // beats not booting.
    let mut control = match control::Control::bind() {
        Ok(control) => Some(control),
        Err(e) => {
            eprintln!("oxinit: {e}");
            eprintln!("oxinit: continuing without a control socket");
            None
        }
    };

    // Without a cgroup hierarchy the machine still boots. It loses process
    // tracking, resource limits, `forking` readiness, and `cgroup.kill`, and
    // says so where each is asked for — which beats refusing to start.
    let cgroups = match cgroup::Cgroups::open(oxinit_cgroup::CGROUP_ROOT) {
        Ok(cgroups) => cgroups,
        Err(e) => {
            eprintln!("oxinit: {e}");
            eprintln!("oxinit: continuing without cgroups");
            cgroup::Cgroups::unavailable()
        }
    };

    let mut supervisor = supervisor::Supervisor::load(&hostname, timers, notify, cgroups);

    let registered = events
        .register(&signals, event::Source::Signals)
        .and_then(|()| events.register(supervisor.timers.as_fd(), event::Source::Timer))
        .and_then(|()| events.register(supervisor.notify.as_fd(), event::Source::Notify));

    if let Some(control) = control.as_ref() {
        if let Err(e) = events.register(control.as_fd(), event::Source::Control) {
            eprintln!("oxinit: {e}");
        }
    }

    if let Err(e) = registered {
        eprintln!("oxinit: {e}");
        fallback(Some(&signals));
    }

    // Every service cgroup, registered once and never touched again. The
    // hierarchy is built before anything starts precisely so this is one pass
    // rather than an epoll call on every start and stop.
    for (id, fd) in supervisor.cgroups.events() {
        if let Err(e) = events.register_pri(fd, event::Source::Cgroup(id)) {
            eprintln!("oxinit: {e}");
        }
    }

    // With nothing to supervise, keep the machine usable the way M0 did.
    let mut shell = if supervisor.is_empty() {
        eprintln!("oxinit: nothing to start; falling back to a console shell");
        shell::spawn().map_err(|e| eprintln!("oxinit: {e}")).ok()
    } else {
        supervisor.start_all();
        None
    };

    // Socket units are watched from here on. Done after the initial start
    // rather than alongside the cgroups, so a service that boot started
    // eagerly is never activated a second time by a connection.
    supervisor.sync_sockets(&events);

    run(
        &mut events,
        &signals,
        &mut supervisor,
        &mut shell,
        &mut control,
    )
}

/// The event loop.
fn run(
    events: &mut event::EventLoop,
    signals: &OwnedFd,
    supervisor: &mut supervisor::Supervisor,
    shell: &mut Option<Child>,
    control: &mut Option<control::Control>,
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
                event::Source::Cgroup(id) => supervisor.on_cgroup(*id),
                event::Source::Socket(id) => supervisor.on_socket(*id),
                event::Source::Control => on_connect(control, events),
                event::Source::Client(id) => on_request(control, events, supervisor, *id),
            }));

            if result.is_err() {
                eprintln!("oxinit: handler panicked; continuing");
            }
        }

        // One reconciliation per wake-up rather than an arm/disarm call inside
        // every handler that can change a service's state. Idempotent, and it
        // cannot be forgotten in a path added later.
        let result = catch_unwind(AssertUnwindSafe(|| supervisor.sync_sockets(events)));
        if result.is_err() {
            eprintln!("oxinit: socket reconciliation panicked; continuing");
        }

        // A shutdown finishes on the same events everything else does: a unit
        // stops, its cgroup empties, and this is where that turns out to have
        // been the last one.
        if let Some(action) = supervisor.settled() {
            if let Err(e) = shutdown::finalize(action) {
                // The kernel refused. In a container that is expected —
                // rebooting is not oxinit's to do there, and exiting instead
                // is M6. Anywhere else PID 1 exiting is a kernel panic, so
                // the only option is to stay up and say so.
                eprintln!("oxinit: {action}: {e}");
                eprintln!("oxinit: cannot go down; staying up with everything stopped");
                fallback(Some(signals));
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
        match *signal {
            SIGCHLD => {
                // The fallback shell is not a unit, so it is checked
                // separately and respawned directly.
                if let Some(child) = shell.as_mut() {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        // Not during a shutdown: respawning a shell on the
                        // console is exactly what stops the machine going
                        // down.
                        if supervisor.shutting_down() {
                            *shell = None;
                        } else {
                            eprintln!("oxinit: {} exited; respawning", shell::SHELL);
                            *shell = shell::spawn().map_err(|e| eprintln!("oxinit: {e}")).ok();
                        }
                    }
                }

                supervisor.on_sigchld();
            }

            SIGTERM | SIGPWR => supervisor.begin_shutdown(shutdown::Action::PowerOff),
            SIGINT => supervisor.begin_shutdown(shutdown::Action::Reboot),
            SIGUSR1 => supervisor.begin_shutdown(shutdown::Action::Halt),
            SIGUSR2 => supervisor.report(),

            // Read and dropped. PID 1 has no default dispositions, so a
            // signal oxinit does not handle does nothing — the failure mode
            // is an unresponsive init, not a dead one.
            _ => {}
        }
    }
}

/// Someone connected to the control socket.
///
/// The connection is registered with epoll rather than read here, because a
/// client that connects and then says nothing must cost PID 1 no time at all.
fn on_connect(control: &mut Option<control::Control>, events: &event::EventLoop) {
    let Some(control) = control.as_mut() else {
        return;
    };
    let Some(id) = control.accept() else {
        return;
    };

    let registered = control
        .fd(id)
        .map(|fd| events.register(fd, event::Source::Client(id)));

    if !matches!(registered, Some(Ok(()))) {
        control.close(id);
    }
}

/// A control client sent a request, or hung up.
///
/// One request per connection. The reply goes back and the connection closes,
/// which is why nothing here has to remember anything between messages.
fn on_request(
    control: &mut Option<control::Control>,
    events: &event::EventLoop,
    supervisor: &mut supervisor::Supervisor,
    id: u32,
) {
    let Some(control) = control.as_mut() else {
        return;
    };

    if let Some(message) = control.recv(id) {
        let response = match oxinit_ipc::decode::<oxinit_ipc::Request>(&message) {
            Ok(request) => supervisor.handle(request),
            Err(e) => oxinit_ipc::Response::Error {
                message: e.to_string(),
            },
        };

        match oxinit_ipc::encode(&response) {
            Ok(bytes) => control.reply(id, &bytes),
            Err(e) => eprintln!("oxinit: control: {e}"),
        }
    }

    // Deregistered before the descriptor closes, or epoll is left holding one
    // that no longer exists.
    let _ = control.fd(id).map(|fd| events.deregister(fd));
    control.close(id);
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
