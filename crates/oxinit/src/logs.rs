//! Handing service output to `oxlogd`.
//!
//! PID 1 makes the pipe, gives the write end to the child, and passes the read
//! end to `oxlogd` over a unix socket with `SCM_RIGHTS`. **It never reads a
//! byte of it.** A service writing a megabyte a second must not be able to
//! make PID 1 do work, and a bug in a log writer must kill a log writer — the
//! same argument that put `oxctl` out of process.
//!
//! **The read end is kept, not given away.** `oxlogd` is a supervised service
//! and can restart. If PID 1 handed over its only copy, the last close would
//! break every running service's stdout, and a service killed by `SIGPIPE`
//! because the log daemon bounced is worse than a service with no logs.
//! Keeping it means a reconnecting `oxlogd` is handed every descriptor again
//! and the services never notice there was a gap.
//!
//! The cost, stated rather than hidden: with nothing connected, output backs
//! up in the pipe and a service that fills it blocks on write. That is why the
//! pipe is widened below, and why `output` defaults to `console`.

use std::io::IoSlice;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::Mode;
use rustix::net::{
    AddressFamily, SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix,
    SocketFlags, SocketType,
};

use oxinit_log::SOCKET_PATH;

use crate::error::{Error, Result};

/// How much output may sit in a pipe before the service writing to it blocks.
///
/// The kernel default is 64 KiB. This widens the window in which `oxlogd` can
/// be down — restarting, or not started yet — without a chatty service
/// stalling on a write. Best-effort: an unprivileged oxinit cannot raise a
/// pipe past `/proc/sys/fs/pipe-max-size`, and 64 KiB is not nothing.
const PIPE_SIZE: usize = 1024 * 1024;

/// Backlog of one. A second shipper is refused rather than queued: there is
/// nothing for it to do, and leaving it waiting would look like it had
/// connected.
const BACKLOG: i32 = 1;

/// The read end of one service's pipe, held open so a reconnecting shipper can
/// be given it again.
struct Held {
    unit: String,
    read: OwnedFd,
}

pub struct Logs {
    /// `None` when the socket could not be bound. A machine still boots and
    /// still supervises; `output = "log"` is what stops working, and it says
    /// so where it is asked for.
    listener: Option<OwnedFd>,
    shipper: Option<OwnedFd>,
    held: Vec<Held>,
}

impl Logs {
    pub fn bind() -> Result<Self> {
        if let Some(parent) = std::path::Path::new(SOCKET_PATH).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(SOCKET_PATH);

        let listener = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .map_err(Error::Logs)?;

        let addr = SocketAddrUnix::new(SOCKET_PATH).map_err(Error::Logs)?;
        rustix::net::bind(&listener, &addr).map_err(Error::Logs)?;

        // Same story as the control socket: whoever can open this receives
        // every service's output, and the mode is the whole of what stops
        // them.
        rustix::fs::chmod(SOCKET_PATH, Mode::from_raw_mode(0o600)).map_err(Error::Logs)?;
        rustix::net::listen(&listener, BACKLOG).map_err(Error::Logs)?;

        Ok(Self {
            listener: Some(listener),
            shipper: None,
            held: Vec::new(),
        })
    }

    /// No socket. Every operation reports why where it is asked for.
    pub fn unavailable() -> Self {
        Self {
            listener: None,
            shipper: None,
            held: Vec::new(),
        }
    }

    pub fn listener(&self) -> Option<BorrowedFd<'_>> {
        self.listener.as_ref().map(AsFd::as_fd)
    }

    pub fn shipper(&self) -> Option<BorrowedFd<'_>> {
        self.shipper.as_ref().map(AsFd::as_fd)
    }

    /// Take the pending connection, and hand it everything already running.
    ///
    /// Returns whether there is now a shipper to register with epoll. One at a
    /// time: a second would race the first for the same descriptors, and each
    /// would get an arbitrary half of every service's output.
    pub fn accept(&mut self) -> bool {
        let Some(listener) = self.listener.as_ref() else {
            return false;
        };
        let Ok(fd) = rustix::net::accept_with(listener, SocketFlags::CLOEXEC) else {
            return false;
        };

        if self.shipper.is_some() {
            eprintln!("oxinit: logs: a shipper is already connected; refusing");
            return false;
        }

        self.shipper = Some(fd);
        println!("oxinit: logs: shipper connected");

        // This is what makes an `oxlogd` restart invisible to the services it
        // was shipping. Their pipes were never closed; the new process is
        // simply given them.
        for held in &self.held {
            offer(self.shipper.as_ref(), &held.unit, held.read.as_fd());
        }

        true
    }

    /// The shipper hung up. The caller deregisters it from epoll first.
    ///
    /// Nothing else happens: the pipes stay open and the services writing into
    /// them are not told, because there is nothing they could usefully do and
    /// a replacement is a restart away.
    pub fn disconnect(&mut self) {
        if self.shipper.take().is_some() {
            eprintln!("oxinit: logs: shipper gone; output is buffering in the pipes");
        }
    }

    /// Whether the shipper is still there.
    ///
    /// It never sends anything, so the only reason its socket becomes readable
    /// is that it closed.
    pub fn still_connected(&self) -> bool {
        let Some(shipper) = self.shipper.as_ref() else {
            return false;
        };

        let mut byte = [0u8; 1];
        !matches!(
            rustix::net::recv(
                shipper,
                byte.as_mut_slice(),
                rustix::net::RecvFlags::DONTWAIT
            ),
            Ok((0, _))
        )
    }

    /// A fresh pipe for `unit`. The write end is the caller's, for the child.
    ///
    /// One per start, not one per unit. A restarted service gets a new pipe,
    /// the shipper sees EOF on the old descriptor and closes it, and is handed
    /// this one. Reusing a pipe across restarts would mean a descriptor
    /// outliving the process it was made for, and no EOF to say a service had
    /// gone.
    pub fn pipe(&mut self, unit: &str) -> Result<OwnedFd> {
        if self.listener.is_none() {
            return Err(Error::NoLogSocket);
        }

        let (read, write) =
            rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(Error::Logs)?;

        // Best-effort, and only ever an improvement: the failure mode is the
        // kernel default, which is what every other pipe on the machine has.
        let _ = rustix::pipe::fcntl_setpipe_size(&read, PIPE_SIZE);

        offer(self.shipper.as_ref(), unit, read.as_fd());

        // Replaces the previous generation's read end, which the shipper
        // already has its own copy of and will drain to EOF.
        self.held.retain(|held| held.unit != unit);
        self.held.push(Held {
            unit: unit.to_owned(),
            read,
        });

        Ok(write)
    }
}

/// One message: the unit name, with the descriptor attached.
///
/// `SOCK_SEQPACKET` puts the two in the same message with no framing, and the
/// name is the only field there is — encoding one string through a serializer
/// would be ceremony around a `send`.
///
/// `DONTWAIT` because this is PID 1. If the shipper is so far behind that its
/// socket buffer is full, the descriptor is not delivered and that unit's
/// output waits in its pipe until the next start; blocking the event loop on
/// a log writer is not a trade worth making.
fn offer(shipper: Option<&OwnedFd>, unit: &str, fd: BorrowedFd<'_>) {
    let Some(shipper) = shipper else {
        return;
    };

    let mut space = [MaybeUninit::<u8>::uninit(); 128];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);

    // Bound to a name rather than passed inline: `ScmRights` borrows the
    // slice, and a temporary array would be dropped before the `sendmsg` that
    // reads it.
    let rights = [fd];

    if !ancillary.push(SendAncillaryMessage::ScmRights(&rights)) {
        eprintln!("oxinit: logs: {unit}: no room for the descriptor");
        return;
    }

    if let Err(e) = rustix::net::sendmsg(
        shipper,
        &[IoSlice::new(unit.as_bytes())],
        &mut ancillary,
        SendFlags::DONTWAIT,
    ) {
        eprintln!("oxinit: logs: {unit}: {e}");
    }
}
