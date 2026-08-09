//! The service cgroups, and the descriptors the event loop watches them with.
//!
//! `oxinit-cgroup` knows the layout and the file formats. This module owns the
//! part that needs a kernel: one open `cgroup.events` per service, registered
//! with `EPOLLPRI`, and the reads through those descriptors that both report
//! the `populated` key and clear the notification.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags};

use oxinit_cgroup::{parse_populated, Cgroup, CgroupError, Hierarchy};

use crate::error::{Error, Result};

/// One service's cgroup, plus the descriptor epoll watches.
struct Handle {
    unit: String,
    cgroup: Cgroup,
    /// Held open for the lifetime of the process. Closing it would drop the
    /// epoll registration, and reopening would lose the kernfs notification
    /// state that makes `EPOLLPRI` edge-like.
    events: OwnedFd,
}

/// Every cgroup oxinit manages.
///
/// A machine whose hierarchy could not be prepared still boots. Placement,
/// limits, `forking` readiness, and `cgroup.kill` are then unavailable, and
/// each is reported at the point something asks for it rather than as one
/// fatal error at startup — PID 1 logs and continues.
pub struct Cgroups {
    hierarchy: Option<Hierarchy>,
    handles: Vec<Handle>,
}

impl Cgroups {
    pub fn open(root: &str) -> std::result::Result<Self, CgroupError> {
        Ok(Self {
            hierarchy: Some(Hierarchy::new(root)?),
            handles: Vec::new(),
        })
    }

    /// The degraded mode: no hierarchy, every operation reporting why.
    pub fn unavailable() -> Self {
        Self {
            hierarchy: None,
            handles: Vec::new(),
        }
    }

    /// Create `unit`'s cgroup and open its `cgroup.events`.
    ///
    /// Does nothing when there is no hierarchy to create it in.
    pub fn add(&mut self, unit: &str, full_name: &str) -> Result<()> {
        let Some(hierarchy) = self.hierarchy.as_ref() else {
            return Ok(());
        };

        let cgroup = hierarchy.cgroup(full_name);
        cgroup.create().map_err(Error::Cgroup)?;

        let events = open_read(&cgroup.events())?;

        // A cgroup's id is its index here. Nothing is ever removed, so an id
        // stays valid — and stays registered with epoll — for the life of the
        // process, including across restarts of the unit.
        self.handles.push(Handle {
            unit: unit.to_owned(),
            cgroup,
            events,
        });

        Ok(())
    }

    /// The descriptors to register, paired with their ids.
    pub fn events(&self) -> impl Iterator<Item = (u32, BorrowedFd<'_>)> {
        self.handles
            .iter()
            .enumerate()
            .filter_map(|(index, handle)| {
                let id = u32::try_from(index).ok()?;
                Some((id, handle.events.as_fd()))
            })
    }

    pub fn unit(&self, id: u32) -> Option<&str> {
        self.handle(id).map(|handle| handle.unit.as_str())
    }

    pub fn get(&self, unit: &str) -> Option<&Cgroup> {
        self.handles
            .iter()
            .find(|handle| handle.unit == unit)
            .map(|handle| &handle.cgroup)
    }

    /// Read `populated` through the registered descriptor.
    ///
    /// This read is not just how the value is obtained, it is what tells
    /// kernfs the notification was consumed. Reading a freshly opened
    /// descriptor instead would leave `EPOLLPRI` asserted on the registered
    /// one and spin the event loop forever.
    ///
    /// `pread` rather than `read` because the descriptor is never seeked and
    /// every read has to start at the beginning of the file.
    pub fn populated(&self, id: u32) -> Option<bool> {
        let handle = self.handle(id)?;

        let mut buf = [0u8; 256];
        let read = rustix::io::pread(&handle.events, buf.as_mut_slice(), 0).ok()?;

        let text = std::str::from_utf8(buf.get(..read)?).ok()?;
        parse_populated(text)
    }

    fn handle(&self, id: u32) -> Option<&Handle> {
        self.handles.get(usize::try_from(id).ok()?)
    }
}

/// Open a cgroup interface file for reading.
fn open_read(path: &Path) -> Result<OwnedFd> {
    open(path, OFlags::RDONLY)
}

/// Open a unit's `cgroup.procs` for the child to write itself into.
///
/// `CLOEXEC` matters: the descriptor exists only for the window between fork
/// and exec, and the service must not inherit a writable handle on the file
/// that decides which cgroup it lives in.
pub fn open_procs(cgroup: &Cgroup) -> Result<OwnedFd> {
    open(&cgroup.procs(), OFlags::WRONLY)
}

fn open(path: &Path, flags: OFlags) -> Result<OwnedFd> {
    rustix::fs::open(path, flags | OFlags::CLOEXEC, Mode::empty()).map_err(|source| Error::Open {
        path: path.display().to_string(),
        source,
    })
}
