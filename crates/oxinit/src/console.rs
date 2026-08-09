//! Wire stdio to `/dev/console`, but only where there is nothing there
//! already.
//!
//! Until stdio works, everything oxinit prints goes nowhere, which makes early
//! boot failures invisible. It is the third thing that happens, right after the
//! mounts that make `/dev/console` exist.
//!
//! **What oxinit inherited comes first.** A container runtime starts PID 1 with
//! fds 0, 1 and 2 already connected to the pipes it will read the logs from,
//! and those images have a `/dev/console` too — so redirecting onto it
//! unconditionally sent every line oxinit ever wrote to a device nobody was
//! reading. That was the whole of "no output at all from a container".
//!
//! The test is not "am I in a container". It is whether the descriptor is
//! already open, which is the exact question, has an exact answer, and gives
//! the same result on a machine: the kernel hands init `/dev/console` on 0, 1
//! and 2 when the initramfs has one, and this keeps those. It opens the console
//! itself in the case that motivated the code in the first place — an image
//! where the kernel found no console to open, and left the descriptors closed.

use std::mem;
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};

use rustix::fs::{Mode, OFlags};
use rustix::stdio::{stderr, stdin, stdout};

use crate::error::{Error, Result};

/// Fill in whichever of fd 0, 1 and 2 is not already open.
///
/// Not reported either way. When oxinit keeps what it inherited, the caller's
/// pipes are where the log is and that is where every line after this one
/// lands; when it opens the console instead, a line saying so would go to the
/// console, which is the one place it would tell nobody anything.
pub fn attach() -> Result<()> {
    let missing = [!usable(stdin()), !usable(stdout()), !usable(stderr())];

    if !missing.iter().any(|missing| *missing) {
        return Ok(());
    }

    let console = rustix::fs::open("/dev/console", OFlags::RDWR | OFlags::NOCTTY, Mode::empty())
        .map_err(Error::Console)?;

    // NOCTTY above: PID 1 does not want the console as its controlling
    // terminal. The console shell does, and takes it for itself — see
    // `sys::raw::start_session`.
    if missing.first() == Some(&true) {
        rustix::stdio::dup2_stdin(&console).map_err(Error::Console)?;
    }
    if missing.get(1) == Some(&true) {
        rustix::stdio::dup2_stdout(&console).map_err(Error::Console)?;
    }
    if missing.get(2) == Some(&true) {
        rustix::stdio::dup2_stderr(&console).map_err(Error::Console)?;
    }

    keep_if_stdio(console);
    Ok(())
}

/// Whether a descriptor is open at all.
///
/// `fstat` is the cheapest question the kernel answers with `EBADF`, and
/// `EBADF` is the whole of what this needs to know.
fn usable(fd: BorrowedFd<'_>) -> bool {
    rustix::fs::fstat(fd).is_ok()
}

/// Do not close the console if it *is* stdio.
///
/// `open` returns the lowest free descriptor. When the kernel left 0, 1 and 2
/// closed — the exact case this function is reached in — that number is 0, the
/// `dup2` onto stdin is a no-op, and dropping the `OwnedFd` at the end of the
/// scope would close the standard input it had just installed.
fn keep_if_stdio(console: OwnedFd) {
    if console.as_raw_fd() <= 2 {
        mem::forget(console);
    }
}
