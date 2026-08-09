//! The pseudo-filesystems PID 1 mounts before anything else works.

use std::ffi::CStr;

use rustix::fs::Mode;
use rustix::io::Errno;
use rustix::mount::{mount, MountFlags};

use crate::error::{Error, Result};

struct Spec {
    target: &'static str,
    fstype: &'static str,
    flags: MountFlags,
    data: Option<&'static CStr>,
}

const NOSUID_NODEV: MountFlags = MountFlags::NOSUID.union(MountFlags::NODEV);
const NOSUID_NODEV_NOEXEC: MountFlags = NOSUID_NODEV.union(MountFlags::NOEXEC);

/// Order matters. `/dev/pts` needs `/dev`, `/sys/fs/cgroup` needs `/sys`.
const SPECS: &[Spec] = &[
    Spec {
        target: "/proc",
        fstype: "proc",
        flags: NOSUID_NODEV_NOEXEC,
        data: None,
    },
    Spec {
        target: "/sys",
        fstype: "sysfs",
        flags: NOSUID_NODEV_NOEXEC,
        data: None,
    },
    Spec {
        target: "/dev",
        fstype: "devtmpfs",
        flags: MountFlags::NOSUID,
        data: Some(c"mode=0755"),
    },
    Spec {
        target: "/dev/pts",
        fstype: "devpts",
        flags: MountFlags::NOSUID.union(MountFlags::NOEXEC),
        data: Some(c"gid=5,mode=620"),
    },
    Spec {
        target: "/dev/shm",
        fstype: "tmpfs",
        flags: NOSUID_NODEV,
        data: Some(c"mode=1777"),
    },
    Spec {
        target: "/run",
        fstype: "tmpfs",
        flags: NOSUID_NODEV,
        data: Some(c"mode=0755"),
    },
    Spec {
        target: "/sys/fs/cgroup",
        fstype: "cgroup2",
        flags: NOSUID_NODEV_NOEXEC,
        data: None,
    },
];

/// Mount everything, in order.
///
/// Returns the failures rather than stopping at the first one. A machine with
/// `/proc` but no `/sys` is worth more than one that gave up.
pub fn mount_all() -> Vec<Error> {
    let mut failures = Vec::new();
    for spec in SPECS {
        if let Err(e) = mount_one(spec) {
            failures.push(e);
        }
    }
    failures
}

fn mount_one(spec: &Spec) -> Result<()> {
    // devtmpfs does not bring its own subdirectories, and the initramfs may
    // not have them either.
    if let Err(e) = rustix::fs::mkdir(spec.target, Mode::from_raw_mode(0o755)) {
        if e != Errno::EXIST {
            return Err(Error::Mkdir {
                path: spec.target,
                source: e,
            });
        }
    }

    match mount(spec.fstype, spec.target, spec.fstype, spec.flags, spec.data) {
        Ok(()) => Ok(()),
        // Already mounted. The initramfs or a previous run got here first.
        Err(Errno::BUSY) => Ok(()),
        Err(source) => Err(Error::Mount {
            target: spec.target,
            source,
        }),
    }
}

/// Set the hostname from `/etc/hostname`, and report the name that is
/// actually in effect.
///
/// The returned name feeds the `%H` unit specifier, so it has to be what the
/// machine is called, not what oxinit wanted to call it. When `sethostname` is
/// refused — which is what happens without `CAP_SYS_ADMIN`, so on every
/// container — the name already set is read back rather than assumed. Assuming
/// it was `localhost` made `%H` a lie exactly where it was most likely to be
/// wrong.
pub fn set_hostname() -> (String, Option<Error>) {
    let raw = std::fs::read("/etc/hostname").unwrap_or_default();
    let wanted = raw
        .split(|b| *b == b'\n')
        .next()
        .filter(|line| !line.is_empty())
        .unwrap_or(b"localhost");

    match rustix::system::sethostname(wanted) {
        Ok(()) => (String::from_utf8_lossy(wanted).into_owned(), None),
        Err(e) => (current_hostname(), Some(Error::Hostname(e))),
    }
}

/// Whatever the machine is called right now.
fn current_hostname() -> String {
    let uname = rustix::system::uname();
    let name = uname.nodename().to_string_lossy().into_owned();

    if name.is_empty() {
        "localhost".to_owned()
    } else {
        name
    }
}
