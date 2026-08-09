//! The epoll loop.
//!
//! M0 had one descriptor and read it directly. M2 has more than one, so this
//! is where multiplexing starts. Still one thread, still no async runtime.

use std::os::fd::{AsFd, OwnedFd};

use rustix::event::epoll;

use crate::error::{Error, Result};

/// Which descriptor woke the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A signal is pending on the signalfd.
    Signals,
    /// At least one deadline has expired.
    Timer,
    /// A service sent an sd_notify datagram.
    Notify,
    /// A service cgroup's `populated` key changed. Carries the id the
    /// supervisor assigned that cgroup, not a unit name — epoll hands back a
    /// `u64` and nothing else.
    Cgroup(u32),
    /// A connection arrived on a socket unit's descriptor. Carries the id of
    /// that descriptor.
    Socket(u32),
    /// Someone connected to the control socket.
    Control,
    /// A connected control client sent something, or hung up.
    Client(u32),
    /// `oxlogd` is connecting.
    LogSocket,
    /// The connected `oxlogd` hung up. It never sends anything, so that is the
    /// only reason its socket becomes readable.
    LogShipper,
}

/// epoll carries one `u64` per registration and hands it back unchanged, so
/// the token has to say both *what kind* of descriptor woke the loop and
/// *which one*. The high half is the kind, the low half is the id.
///
/// A flat numbering with reserved ranges would work too, and would silently
/// alias the moment one range outgrew its reservation.
const KIND_SHIFT: u32 = 32;
const KIND_FIXED: u64 = 0;
const KIND_CGROUP: u64 = 1;
const KIND_SOCKET: u64 = 2;
const KIND_CLIENT: u64 = 3;

impl Source {
    const fn token(self) -> u64 {
        let (kind, id) = match self {
            Source::Signals => (KIND_FIXED, 0),
            Source::Timer => (KIND_FIXED, 1),
            Source::Notify => (KIND_FIXED, 2),
            Source::Control => (KIND_FIXED, 3),
            Source::LogSocket => (KIND_FIXED, 4),
            Source::LogShipper => (KIND_FIXED, 5),
            Source::Cgroup(id) => (KIND_CGROUP, id),
            Source::Socket(id) => (KIND_SOCKET, id),
            Source::Client(id) => (KIND_CLIENT, id),
        };
        (kind << KIND_SHIFT) | id as u64
    }

    fn from_token(token: u64) -> Option<Self> {
        let id = token as u32;
        match token >> KIND_SHIFT {
            KIND_FIXED => match id {
                0 => Some(Source::Signals),
                1 => Some(Source::Timer),
                2 => Some(Source::Notify),
                3 => Some(Source::Control),
                4 => Some(Source::LogSocket),
                5 => Some(Source::LogShipper),
                _ => None,
            },
            KIND_CGROUP => Some(Source::Cgroup(id)),
            KIND_SOCKET => Some(Source::Socket(id)),
            KIND_CLIENT => Some(Source::Client(id)),
            _ => None,
        }
    }
}

pub struct EventLoop {
    epoll: OwnedFd,
    events: Vec<epoll::Event>,
}

impl EventLoop {
    pub fn new() -> Result<Self> {
        let epoll = epoll::create(epoll::CreateFlags::CLOEXEC).map_err(Error::Epoll)?;
        Ok(Self {
            epoll,
            // One entry per descriptor that can be ready at once: the three
            // fixed sources plus a service's worth of cgroups. Level-triggered
            // registration makes an undersized buffer correct but wasteful —
            // the leftovers come back on the next wait.
            events: Vec::with_capacity(32),
        })
    }

    /// Register a descriptor as level-triggered readable.
    ///
    /// Level-triggered on purpose: with edge-triggered, a handler that reads
    /// less than everything available silently stops being woken, which in
    /// PID 1 means a service that never gets reaped.
    pub fn register(&self, fd: impl AsFd, source: Source) -> Result<()> {
        self.add(fd, source, epoll::EventFlags::IN)
    }

    /// Register a descriptor for `EPOLLPRI` rather than readability.
    ///
    /// `cgroup.events` is always readable, so `EPOLLIN` on it would spin the
    /// loop. kernfs signals a *change* to the file with `EPOLLPRI` instead,
    /// and clears that condition only when the notified descriptor is read —
    /// which is why the read has to go through this same fd.
    pub fn register_pri(&self, fd: impl AsFd, source: Source) -> Result<()> {
        self.add(fd, source, epoll::EventFlags::PRI)
    }

    /// Stop watching a descriptor without closing it.
    ///
    /// This is how a socket unit stops waking the loop once its service is
    /// running: the pending connection keeps the descriptor readable until the
    /// service accepts it, and a level-triggered registration would spin on
    /// that. The descriptor stays open — it is the service's, and holding it
    /// across a restart is the point.
    pub fn deregister(&self, fd: impl AsFd) -> Result<()> {
        epoll::delete(&self.epoll, fd).map_err(Error::Epoll)
    }

    fn add(&self, fd: impl AsFd, source: Source, flags: epoll::EventFlags) -> Result<()> {
        epoll::add(
            &self.epoll,
            fd,
            epoll::EventData::new_u64(source.token()),
            flags,
        )
        .map_err(Error::Epoll)
    }

    /// Block until something is ready, then report what.
    ///
    /// `EINTR` cannot happen here — every signal is blocked — but it is
    /// handled rather than assumed away.
    pub fn wait(&mut self, out: &mut Vec<Source>) -> Result<()> {
        out.clear();
        self.events.clear();

        match epoll::wait(
            &self.epoll,
            rustix::buffer::spare_capacity(&mut self.events),
            None,
        ) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => return Ok(()),
            Err(e) => return Err(Error::Epoll(e)),
        }

        for event in &self.events {
            if let Some(source) = Source::from_token(event.data.u64()) {
                out.push(source);
            }
        }

        Ok(())
    }
}
