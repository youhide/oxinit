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

/// What became of the mounts.
#[derive(Default)]
pub struct Mounts {
    /// Targets something else had already mounted, and oxinit left alone.
    pub inherited: Vec<&'static str>,
    /// Targets oxinit was not allowed to mount.
    pub refused: Vec<&'static str>,
    /// Everything that went wrong for a reason an operator can act on.
    pub failures: Vec<Error>,
}

/// Mount everything, in order.
///
/// Nothing here stops at the first problem. A machine with `/proc` but no
/// `/sys` is worth more than one that gave up.
///
/// The two categories that are not failures are separated out because they are
/// not actionable and there are a lot of them. A container runtime mounts these
/// filesystems itself before it execs PID 1 and then denies the capability to
/// mount anything, so oxinit used to open every container log with seven
/// `EPERM` lines describing work that was already done. `inherited` is that
/// work; `refused` is the rest, collapsed into one line by the caller, because
/// seven copies of "not permitted" say nothing the first one did not.
pub fn mount_all() -> Mounts {
    let mut mounts = Mounts::default();

    for spec in SPECS {
        // Asked before mounting rather than inferred from the error, because
        // the error does not distinguish them: mounting over an existing mount
        // is `EBUSY` where oxinit is allowed to mount and `EPERM` where it is
        // not, and `EPERM` is also what a genuinely missing filesystem returns.
        if is_mounted(spec.target) {
            mounts.inherited.push(spec.target);
            continue;
        }

        match mount_one(spec) {
            Ok(()) => {}
            Err(Error::Mount {
                target,
                source: Errno::PERM,
            }) => mounts.refused.push(target),
            Err(e) => mounts.failures.push(e),
        }
    }

    mounts
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
        // Already mounted, and `is_mounted` did not see it. Reachable for a
        // bind mount of the same filesystem, which the device comparison below
        // cannot detect.
        Err(Errno::BUSY) => Ok(()),
        Err(source) => Err(Error::Mount {
            target: spec.target,
            source,
        }),
    }
}

/// Whether something is mounted at `target`.
///
/// A directory and its parent are on the same device unless a filesystem is
/// mounted between them, so two `stat` calls answer this — with no `/proc`,
/// which matters because `/proc` is the first thing on the list and cannot be
/// used to decide whether to mount itself.
///
/// A target that does not exist yet is not mounted, which is the same answer
/// and the same next step.
fn is_mounted(target: &'static str) -> bool {
    let (Ok(here), Ok(parent)) = (
        rustix::fs::stat(target),
        rustix::fs::stat(format!("{target}/..")),
    ) else {
        return false;
    };

    here.st_dev != parent.st_dev
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
