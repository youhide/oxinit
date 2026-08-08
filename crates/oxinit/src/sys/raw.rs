//! The only `unsafe` in the crate.
//!
//! rustix covers every syscall oxinit needs except two: `sigprocmask` (exposed
//! only under rustix's unstable `runtime` feature) and `signalfd` (listed in
//! rustix's own `not_implemented.rs`). Both are here, wrapped, and nothing
//! outside this module touches libc.
//!
//! Every `unsafe` block below states its invariant.

use std::mem::MaybeUninit;
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::ptr;

use rustix::io::Errno;

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

/// The current `errno`, read through std rather than `__errno_location`, so
/// this stays one less `unsafe` block and one less libc-internal symbol.
fn last_errno() -> Errno {
    Errno::from_raw_os_error(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}
