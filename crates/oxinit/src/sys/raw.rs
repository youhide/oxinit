//! The only `unsafe` in the crate.
//!
//! rustix covers every syscall oxinit needs except three: `sigprocmask`
//! (exposed only under rustix's unstable `runtime` feature), `signalfd`
//! (listed in rustix's own `not_implemented.rs`), and what a child does
//! between `fork` and `exec`, which is not a syscall at all but a closure the
//! standard library runs in a process that may only call async-signal-safe
//! functions. All three are here, wrapped, and nothing outside this module
//! touches libc.
//!
//! Every `unsafe` block below states its invariant.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::process::Command;
use std::ptr;

use rustix::io::Errno;

use oxinit_user::Identity;

/// Size of one `struct signalfd_siginfo`. Fixed by the kernel ABI at 128
/// bytes; `signalfd` reads and writes whole records of exactly this size.
const SIGINFO_SIZE: usize = 128;

/// Block every signal in the calling thread.
///
/// PID 1 runs single-threaded, so this mask is the whole process. Signals are
/// then read from a signalfd as ordinary event loop input, which is what makes
/// signal handlers — and the async-signal-safety rules that come with them —
/// unnecessary.
pub fn block_all_signals() -> Result<(), Errno> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();

    // SAFETY: `sigfillset` writes a complete sigset_t through the pointer and
    // reads nothing. The allocation is a correctly sized, correctly aligned
    // MaybeUninit<sigset_t> owned by this frame, so the write is in bounds and
    // initializes it. Return value is 0 on Linux for a non-null argument.
    unsafe {
        libc::sigfillset(set.as_mut_ptr());
    }

    // SAFETY: `set` was fully initialized by `sigfillset` above. The third
    // argument is the optional old-mask out-param, and null means "do not
    // report the previous mask", which is what we want.
    let rc = unsafe { libc::sigprocmask(libc::SIG_SETMASK, set.as_ptr(), ptr::null_mut()) };
    if rc != 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// Create a signalfd delivering every signal.
///
/// Call only after [`block_all_signals`]. A signal that is not blocked is
/// delivered normally and never reaches the fd.
pub fn signalfd_all() -> Result<OwnedFd, Errno> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();

    // SAFETY: identical to the call in `block_all_signals` — an in-bounds
    // write of a complete sigset_t into a correctly sized owned allocation.
    unsafe {
        libc::sigfillset(set.as_mut_ptr());
    }

    // SAFETY: `set` is initialized by the call above. `-1` requests a new fd
    // rather than modifying an existing one, so no other descriptor is
    // touched. SFD_CLOEXEC keeps the fd out of every child across exec, which
    // matters because PID 1 forks every service on the machine.
    let fd = unsafe { libc::signalfd(-1, set.as_ptr(), libc::SFD_CLOEXEC) };
    if fd < 0 {
        return Err(last_errno());
    }

    // SAFETY: `signalfd` returned a fresh, open descriptor that this function
    // is the sole owner of; it has not been registered anywhere else and is
    // not closed on any path above. Transferring it to OwnedFd makes that
    // ownership the type system's problem from here on.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Signal numbers pending on the signalfd.
///
/// Reads whole `signalfd_siginfo` records and returns just `ssi_signo`, the
/// first `u32` of each. The rest of the record is not decoded, which keeps
/// this free of any struct layout assumption beyond the offset of that field.
pub fn read_signals(fd: &OwnedFd, out: &mut Vec<u32>) -> Result<(), Errno> {
    let mut buf = [0u8; SIGINFO_SIZE * 8];
    let n = rustix::io::read(fd, buf.as_mut_slice())?;

    out.clear();
    for record in buf.get(..n).unwrap_or(&[]).chunks_exact(SIGINFO_SIZE) {
        if let Some(signo) = record.get(..4).and_then(|b| b.try_into().ok()) {
            out.push(u32::from_ne_bytes(signo));
        }
    }
    Ok(())
}

/// What a service's process does between `fork` and `exec`.
///
/// Both steps have to happen here, in that order, and neither can be left to
/// the standard library:
///
/// - The pid must be in the cgroup *before* the service binary starts, or
///   oxinit races the first thing the service forks.
/// - The privilege drop must come *after* the cgroup write, because writing
///   `cgroup.procs` needs the privilege being dropped.
///
/// `Command::uid`/`Command::gid` would run in the wrong order relative to the
/// first step, and would pass only the primary group to `setgroups`.
/// The default — no cgroup and no privilege drop — still resets the signal
/// mask, which is why even the fallback console shell goes through this.
#[derive(Default)]
pub struct ChildSetup {
    /// An open `cgroup.procs`, write-only and `CLOEXEC`.
    pub cgroup_procs: Option<OwnedFd>,
    /// Who to become. `None` leaves the child as root.
    pub identity: Option<Identity>,
}

/// Install [`ChildSetup`] as the child's `pre_exec` hook.
///
/// Everything the hook needs is resolved before this is called — the
/// descriptor is open, the uid and gid are numbers — because the hook runs in
/// a freshly forked process where only async-signal-safe calls are legal. No
/// allocation, no file opening, no name lookup. `write`, `setgroups`,
/// `setgid`, and `setuid` are all on the POSIX list; reading a `Vec`'s pointer
/// and length is not a call at all.
pub fn setup_child(command: &mut Command, setup: ChildSetup) {
    // SAFETY: the contract on `pre_exec` is that the closure only calls
    // async-signal-safe functions, because it runs between fork and exec in a
    // process that inherited every lock the parent held. The closure below
    // makes four syscalls and touches no allocator, no lock, and no libc
    // machinery beyond them. Its captured state — an open descriptor and a
    // `Vec<u32>` — was allocated in the parent before the fork and is only
    // read here.
    unsafe {
        command.pre_exec(move || {
            // First, and unconditionally. Everything below is optional; this
            // is not.
            reset_signal_mask()?;

            if let Some(fd) = setup.cgroup_procs.as_ref() {
                // The kernel reads `0` as "the process doing the writing",
                // which is the only value that cannot be stale by the time it
                // is parsed.
                write_all(fd.as_raw_fd(), b"0")?;
            }

            if let Some(identity) = setup.identity.as_ref() {
                drop_privilege(identity)?;
            }

            Ok(())
        });
    }
}

/// Unblock every signal in the child.
///
/// oxinit blocks all of them so it can read them from a signalfd instead. That
/// mask is inherited across `fork` and survives `exec`, so without this a
/// service starts life unable to receive `SIGTERM` — and a service manager
/// that cannot stop a service is not one. ARCHITECTURE.md states the reset as
/// a requirement; this is where it happens.
///
/// Not left to the standard library. `Command` makes no documented promise
/// about the child's signal mask, and the version that happens to reset it
/// does so before `pre_exec` runs, which is unobservable from here. A
/// requirement this load-bearing is not left to an implementation detail.
///
/// Runs in the child, between fork and exec. `sigemptyset` and `sigprocmask`
/// are both on the POSIX async-signal-safe list.
fn reset_signal_mask() -> io::Result<()> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();

    // SAFETY: `sigemptyset` writes a complete sigset_t through the pointer and
    // reads nothing. The allocation is a correctly sized, correctly aligned
    // MaybeUninit<sigset_t> owned by this frame, so the write is in bounds and
    // initializes it.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
    }

    // SAFETY: `set` was fully initialized above. The third argument is the
    // optional old-mask out-param, and null means "do not report it".
    let rc = unsafe { libc::sigprocmask(libc::SIG_SETMASK, set.as_ptr(), ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// `setgroups`, `setgid`, `setuid`, in that order.
///
/// The order is the whole of it. `setuid` first would drop the privilege the
/// other two need, and the failure is silent: the process keeps the groups it
/// was supposed to shed.
///
/// Runs in the child, between fork and exec.
fn drop_privilege(identity: &Identity) -> io::Result<()> {
    // SAFETY: `setgroups` reads `len` gid_t values from the pointer. The
    // pointer and length come from the same live `Vec<u32>`, and `gid_t` is
    // `u32` on Linux, so the read is in bounds and correctly typed. The Vec
    // is owned by the closure this runs inside and outlives the call.
    let rc = unsafe { libc::setgroups(identity.groups.len(), identity.groups.as_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: both take a scalar id and return 0 or -1. Neither reads memory
    // through a pointer, so there is no invariant beyond calling them in this
    // order — which is the reason this function exists.
    unsafe {
        if libc::setgid(identity.gid) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::setuid(identity.uid) != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

/// `write`, retried until the whole buffer is gone.
///
/// Runs in the child, between fork and exec, so this is `libc::write` rather
/// than anything that might allocate or take a lock.
fn write_all(fd: i32, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: `write` reads `buf.len()` bytes from `buf.as_ptr()`. Both
        // come from the same live slice, so the read is in bounds. The
        // descriptor is owned by the caller and open for writing.
        let written = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };

        if written < 0 {
            let e = io::Error::last_os_error();
            // The child's signal mask is empty by this point, so EINTR is
            // reachable in a way it is not elsewhere in oxinit.
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }

        // Not reachable for a write this small, and an infinite loop if it
        // ever were.
        let Ok(written @ 1..) = usize::try_from(written) else {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        };

        buf = buf.get(written..).unwrap_or(&[]);
    }
    Ok(())
}

/// The current `errno`, read through std rather than `__errno_location`, so
/// this stays one less `unsafe` block and one less libc-internal symbol.
fn last_errno() -> Errno {
    Errno::from_raw_os_error(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}
