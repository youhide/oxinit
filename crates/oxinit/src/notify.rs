//! The `sd_notify` protocol.
//!
//! A runtime contract, not a config format — the distinction argued in
//! README and ARCHITECTURE. Daemons already speak this, so implementing it
//! makes unmodified software report readiness correctly.
//!
//! Services get `NOTIFY_SOCKET` in their environment and send newline
//! separated `KEY=value` datagrams.

use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SocketAddrUnix, SocketType,
};

use oxinit_service::notify::{parse, Message};

use crate::error::{Error, Result};

/// Where services send readiness. Also the value of `NOTIFY_SOCKET`.
pub const NOTIFY_PATH: &str = "/run/oxinit/notify";

pub struct Notify {
    socket: OwnedFd,
}

impl Notify {
    pub fn bind() -> Result<Self> {
        if let Some(parent) = Path::new(NOTIFY_PATH).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // A stale socket from a previous boot would make bind fail with
        // EADDRINUSE. /run is a fresh tmpfs so this is belt and braces.
        let _ = std::fs::remove_file(NOTIFY_PATH);

        let socket = rustix::net::socket(AddressFamily::UNIX, SocketType::DGRAM, None)
            .map_err(Error::Notify)?;

        let addr = SocketAddrUnix::new(NOTIFY_PATH).map_err(Error::Notify)?;
        rustix::net::bind(&socket, &addr).map_err(Error::Notify)?;

        // Without SO_PASSCRED the kernel attaches no credentials and every
        // datagram is anonymous. This is the whole authentication story for
        // this socket.
        rustix::net::sockopt::set_socket_passcred(&socket, true).map_err(Error::Notify)?;

        Ok(Self { socket })
    }

    pub fn as_fd(&self) -> impl AsFd + '_ {
        self.socket.as_fd()
    }

    /// Read every datagram currently queued.
    ///
    /// Datagrams arriving without credentials are dropped: the payload is
    /// attacker-controlled, so the sender's identity has to come from the
    /// kernel or not at all.
    pub fn drain(&self) -> Vec<Message> {
        let mut messages = Vec::new();

        loop {
            let mut payload = [0u8; 4096];
            let mut ancillary_space = [MaybeUninit::<u8>::uninit(); 128];
            let mut ancillary = RecvAncillaryBuffer::new(&mut ancillary_space);

            let received = rustix::net::recvmsg(
                &self.socket,
                &mut [IoSliceMut::new(&mut payload)],
                &mut ancillary,
                RecvFlags::DONTWAIT,
            );

            let Ok(received) = received else {
                // EAGAIN once the queue is empty, which is the normal exit.
                return messages;
            };
            if received.bytes == 0 {
                return messages;
            }

            let mut pid = None;
            for message in ancillary.drain() {
                if let RecvAncillaryMessage::ScmCredentials(creds) = message {
                    pid = Some(creds.pid.as_raw_nonzero().get() as u32);
                }
            }

            let Some(pid) = pid else {
                continue;
            };

            if let Some(parsed) = parse(pid, payload.get(..received.bytes).unwrap_or(&[])) {
                messages.push(parsed);
            }
        }
    }
}
