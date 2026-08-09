//! Socket units: the listening descriptors oxinit owns.
//!
//! They are bound before anything starts and never closed. That is what makes
//! socket activation worth doing — a unit ordered after the socket can rely on
//! the port being *bound*, which is a fact, where "the service started" is a
//! guess; and a service restart does not refuse connections, because the
//! listening descriptor outlives the process that was serving them.
//!
//! Nothing here accepts a connection. oxinit only learns that one arrived and
//! starts the service, which does the accepting.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::Mode;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

use oxinit_unit::{Listen, Socket};

use crate::error::{Error, Result};

/// Mode set on a unix socket after binding.
///
/// The process umask would otherwise decide, and PID 1's umask makes a socket
/// only root can reach — which for most of the services worth activating is
/// the same as not binding it. Restricting this is a deliberate choice an
/// operator should make, so it will get a `mode` key when someone needs one.
const UNIX_MODE: Mode = Mode::from_raw_mode(0o666);

/// One bound descriptor.
struct Bound {
    /// The socket unit that created it. Also its `LISTEN_FDNAMES` entry.
    unit: String,
    /// The service it activates.
    service: String,
    /// As written in `listen`, for log lines.
    address: String,
    fd: OwnedFd,
    /// Whether it is currently registered with epoll.
    armed: bool,
}

/// Every descriptor every socket unit bound, in `listen` order within a unit
/// and unit order between them. The index is the epoll token id.
#[derive(Default)]
pub struct Sockets {
    bound: Vec<Bound>,
}

impl Sockets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind everything one socket unit asks for.
    ///
    /// Returns what went wrong, rather than stopping at the first failure: a
    /// unit listening on both IPv4 and IPv6 on a kernel without IPv6 should
    /// still get its IPv4 descriptor.
    pub fn bind(&mut self, unit: &str, socket: &Socket) -> Vec<Error> {
        let mut failures = Vec::new();

        for address in &socket.listen {
            match bind_one(address, socket) {
                Ok(fd) => self.bound.push(Bound {
                    unit: unit.to_owned(),
                    service: socket.service.clone(),
                    address: describe(address),
                    fd,
                    armed: false,
                }),
                Err(e) => failures.push(e),
            }
        }

        failures
    }

    /// Every descriptor, with the service it activates. The supervisor uses
    /// this to decide which should be watched.
    pub fn ids(&self) -> Vec<(u32, String)> {
        self.bound
            .iter()
            .enumerate()
            .filter_map(|(index, bound)| {
                let id = u32::try_from(index).ok()?;
                Some((id, bound.service.clone()))
            })
            .collect()
    }

    pub fn service(&self, id: u32) -> Option<&str> {
        self.get(id).map(|bound| bound.service.as_str())
    }

    pub fn address(&self, id: u32) -> Option<&str> {
        self.get(id).map(|bound| bound.address.as_str())
    }

    /// Everything one socket unit bound, for a log line.
    pub fn addresses(&self, unit: &str) -> Vec<&str> {
        self.bound
            .iter()
            .filter(|bound| bound.unit == unit)
            .map(|bound| bound.address.as_str())
            .collect()
    }

    /// The descriptors to hand a service, in the order it will receive them,
    /// paired with the name each one is announced under in `LISTEN_FDNAMES`.
    pub fn for_service(&self, service: &str) -> Vec<(&str, BorrowedFd<'_>)> {
        self.bound
            .iter()
            .filter(|bound| bound.service == service)
            .map(|bound| (bound.unit.as_str(), bound.fd.as_fd()))
            .collect()
    }

    pub fn armed(&self, id: u32) -> bool {
        self.get(id).is_some_and(|bound| bound.armed)
    }

    pub fn set_armed(&mut self, id: u32, armed: bool) {
        if let Some(bound) = usize::try_from(id).ok().and_then(|i| self.bound.get_mut(i)) {
            bound.armed = armed;
        }
    }

    pub fn fd(&self, id: u32) -> Option<BorrowedFd<'_>> {
        self.get(id).map(|bound| bound.fd.as_fd())
    }

    fn get(&self, id: u32) -> Option<&Bound> {
        self.bound.get(usize::try_from(id).ok()?)
    }
}

/// Create, bind, and — for connection-oriented types — listen.
fn bind_one(address: &Listen, socket: &Socket) -> Result<OwnedFd> {
    let kind = match socket.ty {
        oxinit_unit::SocketType::Stream => SocketType::STREAM,
        oxinit_unit::SocketType::Datagram => SocketType::DGRAM,
        oxinit_unit::SocketType::Seqpacket => SocketType::SEQPACKET,
    };

    let fd = match address {
        Listen::Inet(addr) => {
            let family = if addr.is_ipv4() {
                AddressFamily::INET
            } else {
                AddressFamily::INET6
            };

            let fd = create(family, kind, address)?;

            // Or a restart is refused for as long as the kernel holds the old
            // socket in TIME_WAIT — which is exactly the window a service
            // manager restarts things in.
            set(
                rustix::net::sockopt::set_socket_reuseaddr(&fd, true),
                address,
            )?;

            if addr.is_ipv6() {
                // Each `listen` entry means the address it names and nothing
                // else. Without this a dual-stack `[::]` also claims every
                // IPv4 address, so a unit listing both would fail its second
                // bind with EADDRINUSE and the reason would be invisible.
                set(rustix::net::sockopt::set_ipv6_v6only(&fd, true), address)?;
            }

            bind_to(&fd, addr, address)?;
            fd
        }

        Listen::Unix(path) => {
            let fd = create(AddressFamily::UNIX, kind, address)?;

            // /run is a fresh tmpfs, so a leftover from a previous boot should
            // be impossible. It is removed anyway: the failure mode otherwise
            // is EADDRINUSE on a file nothing is listening on, which is a
            // miserable thing to debug at boot.
            let _ = std::fs::remove_file(path);

            let addr = SocketAddrUnix::new(path).map_err(|source| Error::Listen {
                address: describe(address),
                source,
            })?;
            bind_to(&fd, &addr, address)?;

            // After bind, because the file does not exist until then.
            set(rustix::fs::chmod(Path::new(path), UNIX_MODE), address)?;
            fd
        }
    };

    // A datagram socket is never listened on. The datagram that starts the
    // service is still queued when it gets the descriptor.
    if socket.ty != oxinit_unit::SocketType::Datagram {
        let backlog = i32::try_from(socket.backlog).unwrap_or(i32::MAX);
        set(rustix::net::listen(&fd, backlog), address)?;
    }

    Ok(fd)
}

fn create(family: AddressFamily, kind: SocketType, address: &Listen) -> Result<OwnedFd> {
    // CLOEXEC because oxinit forks every service on the machine, and a
    // listening socket must reach exactly the service it belongs to. The
    // descriptors that service gets are duplicates made deliberately.
    rustix::net::socket_with(family, kind, SocketFlags::CLOEXEC, None).map_err(|source| {
        Error::Listen {
            address: describe(address),
            source,
        }
    })
}

fn bind_to(
    fd: &OwnedFd,
    addr: &impl rustix::net::addr::SocketAddrArg,
    address: &Listen,
) -> Result<()> {
    set(rustix::net::bind(fd, addr), address)
}

fn set(result: rustix::io::Result<()>, address: &Listen) -> Result<()> {
    result.map_err(|source| Error::Listen {
        address: describe(address),
        source,
    })
}

fn describe(address: &Listen) -> String {
    match address {
        Listen::Inet(addr) => addr.to_string(),
        Listen::Unix(path) => path.clone(),
    }
}
