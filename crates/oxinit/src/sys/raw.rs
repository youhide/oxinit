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
/// The default — no cgroup, no descriptors, no privilege drop, and no image —
/// still resets the signal mask, which is why even the fallback console shell
/// goes through this.
#[derive(Default)]
pub struct ChildSetup {
    /// An open `cgroup.procs`, write-only and `CLOEXEC`.
    pub cgroup_procs: Option<OwnedFd>,
    /// Descriptors to place at fd 3 upward, in order.
    ///
    /// Already duplicated to numbers at or above the range they are moving
    /// into, so no `dup2` here can clobber a descriptor it has not used yet —
    /// and none of them is a no-op, which would leave `FD_CLOEXEC` set and
    /// close the descriptor at exec.
    pub listen_fds: Vec<OwnedFd>,
    /// Who to become. `None` leaves the child as root.
    pub identity: Option<Identity>,
    /// Start a session and claim stdin as its controlling terminal.
    ///
    /// For the console shell and nothing else. See [`start_session`].
    pub session: bool,
    /// What to exec. `None` leaves it to the standard library, which is what
    /// the console shell wants.
    pub image: Option<Image>,
}

/// Install [`ChildSetup`] as the child's `pre_exec` hook.
///
/// Everything the hook needs is resolved before this is called — the
/// descriptor is open, the uid and gid are numbers — because the hook runs in
/// a freshly forked process where only async-signal-safe calls are legal. No
/// allocation, no file opening, no name lookup. `write`, `setgroups`,
/// `setgid`, and `setuid` are all on the POSIX list; reading a `Vec`'s pointer
/// and length is not a call at all.
pub fn setup_child(command: &mut Command, mut setup: ChildSetup) {
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

            if setup.session {
                // Deliberately not `?`. A shell without a controlling terminal
                // is a shell with no job control, which is a nuisance; a shell
                // that failed to start is a machine with no way in.
                let _ = start_session();
            }

            if let Some(fd) = setup.cgroup_procs.as_ref() {
                // The kernel reads `0` as "the process doing the writing",
                // which is the only value that cannot be stale by the time it
                // is parsed.
                write_all(fd.as_raw_fd(), b"0")?;
            }

            place_descriptors(&setup.listen_fds)?;

            if let Some(identity) = setup.identity.as_ref() {
                drop_privilege(identity)?;
            }

            match setup.image.as_mut() {
                // Does not return unless it fails.
                Some(image) => Err(image.exec()),
                None => Ok(()),
            }
        });
    }
}

/// Move the inherited listening descriptors to fd 3, 4, 5 …
///
/// `dup2` clears `FD_CLOEXEC` on the descriptor it creates, which is what
/// makes them survive the exec — every other descriptor oxinit holds is
/// `CLOEXEC` and closes.
///
/// Runs in the child, between fork and exec.
fn place_descriptors(fds: &[OwnedFd]) -> io::Result<()> {
    for (index, fd) in fds.iter().enumerate() {
        let Ok(offset) = i32::try_from(index) else {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        };

        // SAFETY: `dup2` takes two descriptor numbers and returns one. The
        // source is owned by the caller and open. The target is `3 + index`,
        // which the caller guarantees differs from every source — a `dup2`
        // onto itself is a no-op that would leave FD_CLOEXEC set, and the
        // descriptor would vanish at exec.
        if unsafe { libc::dup2(fd.as_raw_fd(), FIRST_LISTEN_FD + offset) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
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

/// Become a session leader, and claim stdin as the session's controlling
/// terminal.
///
/// Without this the console shell has no controlling terminal, and busybox
/// says so on every boot: "can't access tty; job control turned off". Ctrl-C
/// reaches nothing, `fg` and `bg` do not exist, and every program the operator
/// runs from that shell inherits the same. It is the oldest known gap in this
/// project — noted in M0, deferred twice — and it is two syscalls.
///
/// `setsid` first, because `TIOCSCTTY` is only legal for a session leader that
/// does not already have a controlling terminal, and the second condition is
/// what `setsid` guarantees. It also detaches the shell from PID 1's session,
/// which is the point: the terminal's signals should reach the shell's process
/// group, not oxinit's.
///
/// Whether stdin is a terminal at all is decided in the parent — see
/// [`crate::shell`]. Here it is assumed.
///
/// Runs in the child, between fork and exec. `setsid` is on the POSIX
/// async-signal-safe list. `ioctl` is not on it, and is used anyway: on both
/// musl and glibc it is a thin wrapper around the syscall that allocates
/// nothing and takes no lock, and the list is a statement about what the C
/// library does, not about the kernel.
fn start_session() -> io::Result<()> {
    // SAFETY: `setsid` takes no arguments and reads no memory. It fails only
    // if the caller is already a process group leader, which a freshly forked
    // child of PID 1 is not — its pgid is inherited and is not its own pid.
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `TIOCSCTTY` reads its argument as an integer, not as a pointer,
    // so no memory is dereferenced. `0` means "do not steal this terminal from
    // another session", which is the conservative half of the two behaviours
    // and the only one that is anyone's to ask for. Descriptor 0 is stdin,
    // open and a terminal by the caller's precondition; if it is neither, the
    // call fails with an errno and changes nothing.
    if unsafe { libc::ioctl(0, libc::TIOCSCTTY as _, 0) } < 0 {
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

/// The first descriptor a socket-activated service is handed. Fixed by the
/// protocol: 0, 1 and 2 are stdio, and the listening sockets start after them.
pub const FIRST_LISTEN_FD: i32 = 3;

/// The variable whose value only the child knows.
const LISTEN_PID: &str = "LISTEN_PID=";

/// Room for any pid the kernel will hand out. `pid_max` is bounded by
/// `PID_MAX_LIMIT`, which is well under ten digits.
const PID_DIGITS: usize = 10;

/// The child's exec image: program, argv, and environment.
///
/// Built entirely in the parent. That is the point of it — `LISTEN_PID` has to
/// name the process the descriptors were passed to, and that pid does not
/// exist until after the fork, at which point nothing may allocate. So the
/// storage is laid out beforehand with a gap the child fills in, and the child
/// execs this image itself rather than the one the standard library would have
/// built.
///
/// It also means oxinit decides exactly what a service's environment is,
/// rather than inheriting whatever `Command` would have assembled.
pub struct Image {
    /// NUL-terminated strings. `argv` and `envp` point into these, so nothing
    /// may be pushed after they are built — but the bytes may be overwritten,
    /// which is how `LISTEN_PID` gets its value.
    storage: Vec<Vec<u8>>,
    /// NULL-terminated pointer arrays, held as `usize` rather than as raw
    /// pointers so the whole struct stays `Send + Sync` and can be moved into
    /// the `pre_exec` closure.
    argv: Vec<usize>,
    envp: Vec<usize>,
    /// Which storage entry holds `LISTEN_PID`, and where its digits start.
    listen_pid: Option<(usize, usize)>,
}

impl Image {
    /// Lay out the image.
    ///
    /// `exec` is the argv, program included. `env` is what oxinit adds on top
    /// of its own environment; a variable named in both is oxinit's.
    ///
    /// `listen_pid` reserves the slot the child fills in. Set it only when the
    /// service is actually being given descriptors — a `LISTEN_PID` with no
    /// `LISTEN_FDS` is noise, and a service that trusted it would be reading a
    /// descriptor table it does not own.
    ///
    /// `None` if any string contains a NUL, which cannot survive as a C
    /// string, or if `exec` is empty.
    pub fn build(exec: &[String], env: &[(String, String)], listen_pid: bool) -> Option<Self> {
        let mut storage: Vec<Vec<u8>> = Vec::new();

        for arg in exec {
            storage.push(terminate(arg.as_bytes().to_vec())?);
        }
        let argc = storage.len();
        if argc == 0 {
            return None;
        }

        // oxinit's own environment, minus anything it is about to set and
        // anything a service must never inherit second-hand. A stale
        // LISTEN_FDS would have a service reading descriptors that belong to
        // something else entirely.
        for (key, value) in std::env::vars_os() {
            let key = key.into_encoded_bytes();
            if RESERVED.iter().any(|name| name.as_bytes() == key) {
                continue;
            }
            if env.iter().any(|(name, _)| name.as_bytes() == key) {
                continue;
            }

            let mut entry = key;
            entry.push(b'=');
            entry.extend_from_slice(&value.into_encoded_bytes());
            storage.push(terminate(entry)?);
        }

        for (key, value) in env {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            storage.push(terminate(entry)?);
        }

        let listen_pid = listen_pid.then(|| {
            // Zeroes now, digits and a NUL later. The entry only has to be
            // long enough; the NUL the child writes is what ends the string,
            // so a shorter pid simply leaves unused bytes behind it.
            let mut entry = LISTEN_PID.as_bytes().to_vec();
            entry.resize(LISTEN_PID.len() + PID_DIGITS + 1, 0);
            storage.push(entry);

            (storage.len().saturating_sub(1), LISTEN_PID.len())
        });

        let mut argv: Vec<usize> = storage
            .get(..argc)?
            .iter()
            .map(|entry| entry.as_ptr() as usize)
            .collect();
        argv.push(0);

        let mut envp: Vec<usize> = storage
            .get(argc..)?
            .iter()
            .map(|entry| entry.as_ptr() as usize)
            .collect();
        envp.push(0);

        Some(Self {
            storage,
            argv,
            envp,
            listen_pid,
        })
    }

    /// Replace the calling process with this image.
    ///
    /// Runs in the child, between fork and exec. Returns only on failure, and
    /// the standard library reports that error to the parent through the pipe
    /// it already set up.
    fn exec(&mut self) -> io::Error {
        if let Some((index, offset)) = self.listen_pid {
            // SAFETY: `getpid` takes no arguments, touches no memory, and is
            // on the POSIX async-signal-safe list.
            let pid = unsafe { libc::getpid() };

            if let Some(entry) = self.storage.get_mut(index) {
                write_pid(entry, offset, pid);
            }
        }

        let Some(program) = self.argv.first().copied() else {
            return io::Error::from(io::ErrorKind::InvalidInput);
        };

        // SAFETY: `execve` reads a NUL-terminated path and two
        // NULL-terminated arrays of NUL-terminated strings. Every string lives
        // in `self.storage`, which outlives this call; both arrays were built
        // from those same entries and end in a 0. `usize` and `*const c_char`
        // have the same size and alignment on every target oxinit supports, so
        // the pointer arrays are laid out as the kernel expects. On success it
        // does not return.
        unsafe {
            libc::execve(
                program as *const libc::c_char,
                self.argv.as_ptr().cast(),
                self.envp.as_ptr().cast(),
            );
        }

        io::Error::last_os_error()
    }
}

/// Variables oxinit never passes through from its own environment.
///
/// A service sees the values oxinit set for it, or nothing. Inheriting any of
/// these second-hand would point it at another process's descriptors or
/// another process's notify socket.
const RESERVED: &[&str] = &[
    "LISTEN_FDS",
    "LISTEN_PID",
    "LISTEN_FDNAMES",
    "NOTIFY_SOCKET",
    "WATCHDOG_USEC",
];

/// Append the NUL that makes a byte string a C string.
///
/// `None` if it already contains one: the kernel would read a truncated
/// argument, which is worse than refusing to start the unit.
fn terminate(mut bytes: Vec<u8>) -> Option<Vec<u8>> {
    if bytes.contains(&0) {
        return None;
    }
    bytes.push(0);
    Some(bytes)
}

/// Write `pid` as decimal digits at `offset`, followed by a NUL.
///
/// Runs in the child, into a buffer allocated before the fork. Digits are
/// produced least-significant first into a fixed array and then copied, which
/// is the whole reason this is not `format!`.
fn write_pid(entry: &mut [u8], offset: usize, pid: i32) {
    let mut digits = [0u8; PID_DIGITS];
    let mut value = pid.unsigned_abs();
    let mut first = PID_DIGITS;

    while first > 0 {
        first -= 1;
        if let Some(slot) = digits.get_mut(first) {
            *slot = b'0' + (value % 10) as u8;
        }
        value /= 10;
        if value == 0 {
            break;
        }
    }

    let mut at = offset;
    for byte in digits.get(first..).unwrap_or_default() {
        if let Some(slot) = entry.get_mut(at) {
            *slot = *byte;
        }
        at = at.saturating_add(1);
    }
    if let Some(slot) = entry.get_mut(at) {
        *slot = 0;
    }
}

/// The current `errno`, read through std rather than `__errno_location`, so
/// this stays one less `unsafe` block and one less libc-internal symbol.
fn last_errno() -> Errno {
    Errno::from_raw_os_error(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}
