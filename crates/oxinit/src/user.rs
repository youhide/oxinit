//! `oxinit --user`: a user's own service manager.
//!
//! The same supervisor as PID 1 — the same units, graph, state machine,
//! timers, sockets, `sd_notify`, control protocol and log pipes — run by a
//! user for their own services: PipeWire, WirePlumber, anything a desktop
//! session expects to find running. One per logged-in user.
//!
//! What it is not is the rest of PID 1. It mounts nothing, owns no console,
//! sets no hostname, feeds no watchdog and reboots nothing; it changes no
//! one's identity, because it has no one else to become; and its shutdown
//! ends by exiting, like a container's. Starting it is not its business
//! either: oxinit does not manage logins or sessions, so whatever starts a
//! session starts this — and keeps it to one per user, which the control
//! socket enforces. See ARCHITECTURE.md.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rustix::fs::Mode;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

use oxinit_paths::Paths;
use oxinit_unit::Scope;

use crate::container::Environment;
use crate::supervisor::{Supervisor, UnitSource};
use crate::{cgroup, control, event, logs, notify, timer};

/// Start a user manager. Returns only if it cannot start, or if one is
/// already running for this user.
///
/// Until the loop starts, every failure is an exit with a message: this is
/// not PID 1, and a manager that could not be set up is better reported to
/// whatever started it than left running half-built.
///
/// Never called as PID 1; `main` sees to that.
pub fn boot() -> ExitCode {
    let uid = rustix::process::getuid();
    if uid.is_root() {
        // Root's services are system units. A root user manager would be a
        // second system manager with none of PID 1's guarantees.
        eprintln!("oxinit: --user is for users; root's services are system units");
        return ExitCode::FAILURE;
    }

    let paths = match Paths::user_from_env() {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("oxinit: {e}");
            return ExitCode::FAILURE;
        }
    };
    let name = oxinit_user::name_of_system(uid.as_raw());

    if let Err(e) = private_dir(&paths.runtime_dir) {
        eprintln!("oxinit: {}: {e}", paths.runtime_dir.display());
        return ExitCode::FAILURE;
    }

    // One per user. A second session for the same user starts this again,
    // and finding the first one answering is success, not an error: the
    // services that session wants are already running.
    if answering(&paths.control()) {
        println!("oxinit: the user manager for {name} is already running");
        return ExitCode::SUCCESS;
    }

    // Orphans of this user's services come here rather than to PID 1. A
    // `forking` daemon's real process is one, and it has to be reaped by the
    // manager that is supervising it.
    if let Err(e) = rustix::process::set_child_subreaper(Some(rustix::process::getpid())) {
        eprintln!("oxinit: become a subreaper: {e}; orphans will go to pid 1");
    }

    let signals = match crate::open_signalfd() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("oxinit: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (timers, mut events, notify) = match (
        timer::Timers::new(),
        event::EventLoop::new(),
        notify::Notify::bind(&paths.notify()),
    ) {
        (Ok(timers), Ok(events), Ok(notify)) => (timers, events, notify),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            eprintln!("oxinit: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Required here, where PID 1 can do without it: it is how the user
    // reaches their manager at all, and what tells the next session one is
    // already running.
    let control = match control::Control::bind(&paths.control()) {
        Ok(control) => control,
        Err(e) => {
            eprintln!("oxinit: {e}");
            return ExitCode::FAILURE;
        }
    };

    let logs = match logs::Logs::bind(&paths.log_socket()) {
        Ok(logs) => logs,
        Err(e) => {
            eprintln!("oxinit: {e}; continuing without log shipping");
            logs::Logs::unavailable()
        }
    };

    let cgroups = own_cgroups();

    // `%H` in a user unit means what it means in a system one.
    let hostname = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();

    let config_home = paths.config_home.clone().unwrap_or_default();
    let source = UnitSource {
        dirs: oxinit_unit::user_dirs(&config_home),
        scope: Scope::User { name: name.clone() },
        notify: paths.notify(),
    };

    let environment = Environment::User(name);
    println!(
        "oxinit: {environment}, units from {}",
        source
            .dirs
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut supervisor = Supervisor::load(&hostname, source, timers, notify, cgroups, logs);

    if let Err(e) = crate::wire(&events, &signals, &supervisor, Some(&control)) {
        eprintln!("oxinit: {e}");
        return ExitCode::FAILURE;
    }

    // Nothing to start is not a reason to leave. The user may add a unit and
    // `oxctl --user reload`, and the next session would only start this
    // again.
    supervisor.start_all();
    supervisor.sync_sockets(&events);

    crate::run(
        &mut events,
        &signals,
        &mut supervisor,
        &mut None,
        &mut Some(control),
        &environment,
        &mut None,
    )
}

/// The loop cannot go on. A user manager is not PID 1: there is no kernel
/// panic to avoid and no console to fall back to, so it exits, and whatever
/// started it — the session — sees that it did.
///
/// Its services are left running, reparented to PID 1. Stopping them here
/// would mean running the shutdown this loop can no longer run.
pub fn give_up() -> ! {
    eprintln!("oxinit: the user manager cannot continue; exiting");
    std::process::exit(1)
}

/// The runtime directory, private to the user. `$XDG_RUNTIME_DIR` is `0700`
/// already, but this one is the user manager's, and saying so costs nothing.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    rustix::fs::chmod(dir, Mode::from_raw_mode(0o700))?;
    Ok(())
}

/// Whether a manager is already listening on this control socket.
///
/// A socket file nobody answers on is a manager that died without cleaning
/// up — the next bind removes it.
fn answering(control: &Path) -> bool {
    let Ok(socket) = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    ) else {
        return false;
    };
    let Ok(addr) = SocketAddrUnix::new(control) else {
        return false;
    };
    rustix::net::connect(&socket, &addr).is_ok()
}

/// cgroups, if the user was given a subtree of their own to manage.
///
/// A user manager can only create cgroups under one it may write to, which
/// means something above it delegated one — chowned the directory to the
/// user. Without that it runs as PID 1 does without a hierarchy: every
/// service still runs, and what needs a cgroup — `[resources]`, `forking`
/// readiness, `cgroup.kill` — fails on the unit that asked for it, saying
/// why.
fn own_cgroups() -> cgroup::Cgroups {
    let own = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("0::"))
                .map(|path| path.trim().trim_start_matches('/').to_owned())
        })
        .map(|path| PathBuf::from(oxinit_cgroup::CGROUP_ROOT).join(path));

    let Some(own) = own else {
        println!("oxinit: no cgroup v2 placement; running without cgroups");
        return cgroup::Cgroups::unavailable();
    };

    match cgroup::Cgroups::open(&own.to_string_lossy()) {
        Ok(cgroups) => cgroups,
        Err(e) => {
            println!(
                "oxinit: {} is not delegated to this user ({e}); running without cgroups",
                own.display()
            );
            cgroup::Cgroups::unavailable()
        }
    }
}
