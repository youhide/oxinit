//! The control socket.
//!
//! `SOCK_SEQPACKET` preserves message boundaries and is connection-oriented,
//! so a request is exactly one `recv` and a reply is exactly one `send`. No
//! framing layer, no length prefix, no partial-message state to get wrong.
//! `SOCK_STREAM` would need all three; `SOCK_DGRAM` would lose the connection
//! that the reply goes back on.
//!
//! Authorization is the socket's mode. `0600`, owned by root, in a root-owned
//! directory. There is no in-protocol authentication and no per-command
//! permissions: access to this socket is full control of the machine.
//!
//! Nothing here blocks. A client connection is registered with epoll like
//! every other descriptor, so a client that connects and says nothing costs a
//! slot and no time at all.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::Mode;
use rustix::net::{AddressFamily, SendFlags, SocketAddrUnix, SocketFlags, SocketType};

use oxinit_ipc::{CONTROL_PATH, MAX_MESSAGE};

use crate::error::{Error, Result};

/// How many clients may be connected at once.
///
/// A client that connects and never sends holds its slot until it goes away.
/// That is bounded, and only reachable by someone who can already open this
/// socket — which is to say someone who already has full control of the
/// machine. A timer to evict them would be machinery guarding nothing.
const MAX_CLIENTS: usize = 8;

/// Backlog. Larger than `MAX_CLIENTS` on purpose: a connection waiting in the
/// backlog costs nothing, and refusing it outright would make `oxctl` fail
/// rather than wait.
const BACKLOG: i32 = 32;

struct Client {
    id: u32,
    fd: OwnedFd,
}

pub struct Control {
    listener: OwnedFd,
    clients: Vec<Client>,
    next_id: u32,
}

impl Control {
    pub fn bind() -> Result<Self> {
        if let Some(parent) = std::path::Path::new(CONTROL_PATH).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // /run is a fresh tmpfs, so a leftover should be impossible. Removed
        // anyway: the alternative failure is EADDRINUSE on a socket nothing is
        // listening to, at boot, which is a miserable thing to debug.
        let _ = std::fs::remove_file(CONTROL_PATH);

        let listener = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .map_err(Error::Control)?;

        let addr = SocketAddrUnix::new(CONTROL_PATH).map_err(Error::Control)?;
        rustix::net::bind(&listener, &addr).map_err(Error::Control)?;

        // After bind, because the file does not exist until then. This is the
        // whole of the authorization story for the socket.
        rustix::fs::chmod(CONTROL_PATH, Mode::from_raw_mode(0o600)).map_err(Error::Control)?;

        rustix::net::listen(&listener, BACKLOG).map_err(Error::Control)?;

        Ok(Self {
            listener,
            clients: Vec::new(),
            next_id: 0,
        })
    }

    pub fn as_fd(&self) -> impl AsFd + '_ {
        self.listener.as_fd()
    }

    /// Take one pending connection.
    ///
    /// Returns the id to register with epoll. `None` when there was nothing to
    /// accept, or when there are already too many clients — in which case the
    /// connection is closed rather than queued, so the caller finds out now.
    pub fn accept(&mut self) -> Option<u32> {
        let fd = rustix::net::accept_with(&self.listener, SocketFlags::CLOEXEC).ok()?;

        if self.clients.len() >= MAX_CLIENTS {
            eprintln!("oxinit: control: {MAX_CLIENTS} clients already connected; refusing");
            return None;
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.clients.push(Client { id, fd });

        Some(id)
    }

    pub fn fd(&self, id: u32) -> Option<BorrowedFd<'_>> {
        self.client(id).map(|client| client.fd.as_fd())
    }

    /// One whole request, or `None` if the client sent nothing usable.
    pub fn recv(&self, id: u32) -> Option<Vec<u8>> {
        let client = self.client(id)?;

        let mut buf = vec![0u8; MAX_MESSAGE];

        // `read` is what landed in the buffer; `sent` is how big the message
        // actually was. They differ only when the other side ignored the
        // shared limit, and acting on the fragment that did fit would be
        // acting on something nobody sent.
        let (read, sent) = rustix::net::recv(
            &client.fd,
            buf.as_mut_slice(),
            rustix::net::RecvFlags::empty(),
        )
        .ok()?;

        if read == 0 {
            return None;
        }
        if sent > buf.len() {
            eprintln!("oxinit: control: {sent} byte request over the {MAX_MESSAGE} byte limit");
            return None;
        }

        buf.truncate(read);
        Some(buf)
    }

    pub fn reply(&self, id: u32, message: &[u8]) {
        let Some(client) = self.client(id) else {
            return;
        };

        // One send, one message. A short write cannot happen on a datagram
        // socket: it either fits or it is refused, and the size was checked
        // when the message was encoded.
        if let Err(e) = rustix::net::send(&client.fd, message, SendFlags::empty()) {
            eprintln!("oxinit: control: reply: {e}");
        }
    }

    /// Forget a client. The caller deregisters it from epoll first — the
    /// descriptor closes here, and epoll must not be left holding it.
    pub fn close(&mut self, id: u32) {
        self.clients.retain(|client| client.id != id);
    }

    fn client(&self, id: u32) -> Option<&Client> {
        self.clients.iter().find(|client| client.id == id)
    }
}
