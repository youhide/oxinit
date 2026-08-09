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
}

/// Where the per-cgroup tokens start. The fixed sources sit below it, with
/// room to add more without renumbering anything.
const CGROUP_TOKEN_BASE: u64 = 16;

impl Source {
    /// epoll carries a `u64` per registration and hands it back on wake-up.
    /// These are those tokens.
    const fn token(self) -> u64 {
        match self {
            Source::Signals => 0,
            Source::Timer => 1,
            Source::Notify => 2,
            Source::Cgroup(id) => CGROUP_TOKEN_BASE + id as u64,
        }
    }

    fn from_token(token: u64) -> Option<Self> {
        match token {
            0 => Some(Source::Signals),
            1 => Some(Source::Timer),
            2 => Some(Source::Notify),
            _ => token
                .checked_sub(CGROUP_TOKEN_BASE)
                .and_then(|id| u32::try_from(id).ok())
                .map(Source::Cgroup),
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
