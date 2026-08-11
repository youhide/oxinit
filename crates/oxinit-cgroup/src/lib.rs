//! cgroup v2.
//!
//! Every service gets a cgroup under `/sys/fs/cgroup/oxinit.slice/`. That is
//! how oxinit tracks processes, and it is reliable in ways pid tracking is
//! not: a daemon that double-forks cannot escape it.
//!
//! A cgroup is a directory of small text files, so this crate is `std::fs` and
//! nothing more. That is deliberate — it means the hierarchy layout, every
//! value written to a limit file, and every value parsed back out are all
//! exercised on the host against a temporary directory, with no VM and no
//! kernel. The two things that genuinely need a kernel — registering
//! `cgroup.events` with `EPOLLPRI`, and writing `cgroup.procs` between fork
//! and exec — stay in the `oxinit` binary.
//!
//! This runs inside PID 1, so a panic here is a panic in PID 1.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use thiserror::Error;

use oxinit_unit::Resources;

/// Where the cgroup v2 hierarchy is mounted. `mounts.rs` puts it there.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Everything oxinit manages lives under this, so a machine's service cgroups
/// are one subtree an operator can look at, and so oxinit never writes to a
/// cgroup it did not create.
pub const SLICE: &str = "oxinit.slice";

/// Where PID 1 puts itself, so that the cgroup it started in holds no
/// processes.
///
/// A cgroup may not both contain processes and delegate controllers to its
/// children. The root cgroup is the one exception, which is the only reason
/// delegation worked while PID 1 sat in it — and it stops being true the
/// moment the mount root is not really the root, as inside a cgroup
/// namespace. Moving out first removes the dependency on that exemption, and
/// stops PID 1's own memory being accounted against the root.
pub const INIT_SCOPE: &str = "init.scope";

/// Controllers the service cgroups need. `memory` backs `memory-max` and
/// `memory.current`; `pids` backs `tasks-max` and `pids.current`.
///
/// A controller is only usable in a child cgroup if the parent enabled it in
/// `cgroup.subtree_control`, so this list is written twice on the way down:
/// once at the root, once on the slice.
const CONTROLLERS: &[&str] = &["memory", "pids"];

#[derive(Debug, Error)]
pub enum CgroupError {
    #[error("{path} is not a cgroup v2 hierarchy: {source}")]
    NotCgroup2 {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("create cgroup {path}: {source}")]
    Create {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },
}

pub type Result<T> = std::result::Result<T, CgroupError>;

/// The cgroup v2 mount and the slice oxinit owns inside it.
pub struct Hierarchy {
    slice: PathBuf,
}

impl Hierarchy {
    /// `root` is the cgroup2 mount point, `init_pid` the process to move into
    /// [`INIT_SCOPE`] before delegating anything.
    ///
    /// Both are parameters rather than constants so the tests can point this
    /// at a temporary directory and check what was written where.
    pub fn new(root: impl AsRef<Path>, init_pid: u32) -> Result<Self> {
        let root = root.as_ref();

        // `cgroup.controllers` exists in every cgroup v2 directory and in no
        // v1 one, so reading it both proves the mount is what it claims and
        // tells us which controllers the kernel has.
        let controllers_path = root.join("cgroup.controllers");
        let controllers = read(&controllers_path).map_err(|source| CgroupError::NotCgroup2 {
            path: display(root),
            source,
        })?;

        let available: Vec<&str> = controllers.split_whitespace().collect();
        let wanted: Vec<&str> = CONTROLLERS
            .iter()
            .copied()
            .filter(|c| available.contains(c))
            .collect();

        // Out of the way first: a cgroup holding processes cannot delegate
        // controllers to its children.
        //
        // Best-effort, and deliberately not fatal. On a real root the
        // exemption still applies, so a failure here costs nothing but PID 1's
        // memory being accounted one level up. Where it does matter, the
        // `enable` below fails with EBUSY and reports the situation in the
        // terms that actually explain it.
        let _ = park_init(root, init_pid);

        enable(root, &wanted)?;

        let slice = root.join(SLICE);
        create_dir(&slice)?;

        // The slice holds only child cgroups, never a process, which is what
        // makes this second delegation legal.
        enable(&slice, &wanted)?;

        Ok(Self { slice })
    }

    pub fn slice(&self) -> &Path {
        &self.slice
    }

    /// The cgroup for one unit. `name` is the fully-qualified unit name, so
    /// the directory is `oxinit.slice/sshd.service/`.
    pub fn cgroup(&self, name: &str) -> Cgroup {
        Cgroup {
            path: self.slice.join(name),
        }
    }
}

/// One service's cgroup.
pub struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `cgroup.procs`. The child writes itself into this between fork and
    /// exec; see `oxinit::sys::raw`.
    pub fn procs(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    /// `cgroup.events`. Registered with `EPOLLPRI` by the event loop, which
    /// is how oxinit learns the cgroup emptied without polling for it.
    pub fn events(&self) -> PathBuf {
        self.path.join("cgroup.events")
    }

    /// Create the directory. Already existing is not an error: a restarting
    /// service reuses its cgroup rather than churning the hierarchy.
    pub fn create(&self) -> Result<()> {
        create_dir(&self.path)
    }

    /// Write the `[resources]` limits.
    ///
    /// An absent key is left alone rather than written as `max`, so a limit
    /// set out of band by an operator is not silently reset on every restart.
    pub fn apply(&self, resources: &Resources) -> Result<()> {
        if let Some(memory_max) = resources.memory_max {
            write(&self.path.join("memory.max"), &memory_max.to_string())?;
        }
        if let Some(tasks_max) = resources.tasks_max {
            write(&self.path.join("pids.max"), &tasks_max.to_string())?;
        }
        Ok(())
    }

    /// Whether any process remains in the cgroup.
    ///
    /// This reads the file directly. The event loop instead reads through the
    /// descriptor it registered with epoll, because that read is also what
    /// clears the `EPOLLPRI` condition — see [`parse_populated`].
    pub fn populated(&self) -> Result<bool> {
        let path = self.events();
        let text = read(&path).map_err(|source| CgroupError::Read {
            path: display(&path),
            source,
        })?;

        Ok(parse_populated(&text).unwrap_or(false))
    }

    /// `memory.current` and `pids.current`, read on demand.
    ///
    /// Nothing polls these. A missing or unreadable file reads as `None`
    /// rather than an error: accounting is reporting, and a service does not
    /// fail because its usage could not be printed.
    pub fn stats(&self) -> Stats {
        Stats {
            memory: read_number(&self.path.join("memory.current")),
            tasks: read_number(&self.path.join("pids.current")),
        }
    }

    /// Every process in the cgroup, from `cgroup.procs`.
    ///
    /// A snapshot, and racy by construction: a process may fork between the
    /// read and whatever the caller does with the list. That is tolerable for
    /// `SIGTERM`, where the point is to ask politely, and is exactly why the
    /// escalation from there is [`Cgroup::kill`] rather than more of this.
    pub fn pids(&self) -> Result<Vec<u32>> {
        let path = self.path.join("cgroup.procs");
        let text = read(&path).map_err(|source| CgroupError::Read {
            path: display(&path),
            source,
        })?;

        Ok(text
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect())
    }

    /// Kill every process in the cgroup, atomically.
    ///
    /// Needs Linux 5.14. The alternative — reading `cgroup.procs` and
    /// signalling each pid — races against a process that forks while the
    /// list is being walked, which is exactly the case this exists for.
    pub fn kill(&self) -> Result<()> {
        write(&self.path.join("cgroup.kill"), "1")
    }
}

/// What the cgroup is currently using. `None` where the controller is not
/// enabled or the file could not be read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub memory: Option<u64>,
    pub tasks: Option<u64>,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.memory {
            Some(bytes) => write!(f, "memory {}", Bytes(bytes))?,
            None => f.write_str("memory unknown")?,
        }
        match self.tasks {
            Some(tasks) => write!(f, ", tasks {tasks}"),
            None => f.write_str(", tasks unknown"),
        }
    }
}

/// A byte count in binary multiples, for log lines.
struct Bytes(u64);

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u64, &str); 4] = [(1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K"), (1, "B")];

        for (scale, suffix) in UNITS {
            if self.0 >= scale {
                // One decimal place, computed in integers: this crate is in
                // PID 1 and floats buy nothing here.
                let whole = self.0 / scale;
                let tenths = (self.0 % scale) * 10 / scale;
                return write!(f, "{whole}.{tenths}{suffix}");
            }
        }
        f.write_str("0B")
    }
}

/// The `populated` key of a `cgroup.events` body.
///
/// `1` while any process remains in the cgroup, `0` once the last one exits.
/// `None` when the key is absent, which should not happen and is treated by
/// callers as "not populated" rather than as a reason to fail a unit.
///
/// Exposed because the event loop reads `cgroup.events` through the very
/// descriptor it registered with epoll — kernfs only clears the `EPOLLPRI`
/// condition when the notified descriptor is read, so reading a fresh one
/// would leave the loop spinning on an event it never consumed.
pub fn parse_populated(text: &str) -> Option<bool> {
    text.lines()
        .filter_map(|line| line.split_once(' '))
        .find(|(key, _)| *key == "populated")
        .map(|(_, value)| value.trim() != "0")
}

/// Move `pid` into [`INIT_SCOPE`], so the cgroup it was in holds no processes.
///
/// Its own pid rather than `0`: this runs in the parent, long before any fork,
/// and naming the process explicitly is what makes it testable.
fn park_init(root: &Path, pid: u32) -> Result<()> {
    let scope = root.join(INIT_SCOPE);
    create_dir(&scope)?;
    write(&scope.join("cgroup.procs"), &pid.to_string())
}

/// Enable `controllers` in this cgroup's `cgroup.subtree_control`, so that its
/// children get the matching interface files.
fn enable(cgroup: &Path, controllers: &[&str]) -> Result<()> {
    if controllers.is_empty() {
        return Ok(());
    }

    let line = controllers
        .iter()
        .map(|c| format!("+{c}"))
        .collect::<Vec<_>>()
        .join(" ");

    write(&cgroup.join("cgroup.subtree_control"), &line)
}

fn create_dir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(CgroupError::Create {
            path: display(path),
            source,
        }),
    }
}

/// Write one value to a cgroup interface file.
///
/// Deliberately without `O_CREAT`: these are kernfs nodes that already exist,
/// and asking the kernel to create one would turn a typo in a filename into a
/// new regular file rather than an error. `O_TRUNC` is a no-op on kernfs,
/// where a write is a whole command, and is asked for anyway so the same
/// function behaves on an ordinary file — which is what the tests use.
fn write(path: &Path, value: &str) -> Result<()> {
    let attempt = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .and_then(|mut file| file.write_all(value.as_bytes()));

    attempt.map_err(|source| CgroupError::Write {
        path: display(path),
        source,
    })
}

fn read(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
}

fn read_number(path: &Path) -> Option<u64> {
    read(path).ok()?.trim().parse().ok()
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use oxinit_unit::SizeValue;

    /// A fake cgroup2 mount: the files the kernel would have put there, and
    /// nothing else. Enough to exercise every path this crate takes.
    ///
    /// The one thing it cannot imitate is kernfs conjuring a cgroup's
    /// interface files the instant the directory is created — `fs::create_dir`
    /// makes an empty directory. So the fake writes those files itself, before
    /// or after the code under test makes the directory.
    struct Fake {
        root: PathBuf,
    }

    impl Fake {
        fn new(name: &str) -> Self {
            let fake = Self::bare(name, "cpuset cpu io memory hugetlb pids rdma\n");

            // As if the mkdirs had already happened.
            for name in [SLICE, INIT_SCOPE] {
                let cgroup = fake.root.join(name);
                fs::create_dir(&cgroup).unwrap();
                fs::write(cgroup.join("cgroup.subtree_control"), "").unwrap();
                fs::write(cgroup.join("cgroup.procs"), "").unwrap();
            }

            fake
        }

        /// A mount with no `oxinit.slice` yet.
        fn bare(name: &str, controllers: &str) -> Self {
            let root = std::env::temp_dir().join(format!("oxinit-cgroup-test-{name}"));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();

            fs::write(root.join("cgroup.controllers"), controllers).unwrap();
            fs::write(root.join("cgroup.subtree_control"), "").unwrap();

            Self { root }
        }

        /// The interface files inside one service cgroup.
        fn interface(&self, cgroup: &Path, populated: bool) {
            fs::write(cgroup.join("cgroup.subtree_control"), "").unwrap();
            fs::write(
                cgroup.join("cgroup.events"),
                format!("populated {}\nfrozen 0\n", u8::from(populated)),
            )
            .unwrap();
            fs::write(cgroup.join("memory.max"), "max\n").unwrap();
            fs::write(cgroup.join("pids.max"), "max\n").unwrap();
            fs::write(cgroup.join("cgroup.kill"), "0\n").unwrap();
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn read_back(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn refuses_a_directory_that_is_not_cgroup2() {
        let dir = std::env::temp_dir().join("oxinit-cgroup-test-notcgroup");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // No cgroup.controllers: a cgroup v1 mount, or any other directory.
        assert!(matches!(
            Hierarchy::new(&dir, 1),
            Err(CgroupError::NotCgroup2 { .. })
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parks_pid_1_before_delegating() {
        let fake = Fake::new("initscope");
        Hierarchy::new(&fake.root, 1).unwrap();

        assert_eq!(
            read_back(&fake.root.join(INIT_SCOPE).join("cgroup.procs")),
            "1",
            "PID 1 has to leave the cgroup that is about to delegate"
        );
    }

    #[test]
    fn a_root_that_will_not_take_init_scope_still_delegates() {
        // No init.scope interface files, so the move fails — which is what a
        // read-only or otherwise unwilling root looks like.
        let fake = Fake::bare("noscope", "memory pids\n");
        fs::create_dir(fake.root.join(SLICE)).unwrap();
        fs::write(fake.root.join(SLICE).join("cgroup.subtree_control"), "").unwrap();

        let hierarchy = Hierarchy::new(&fake.root, 1).unwrap();

        // Not fatal: on a real root the exemption still applies, so the only
        // cost is where PID 1's own memory is accounted.
        assert_eq!(
            read_back(&hierarchy.slice().join("cgroup.subtree_control")),
            "+memory +pids"
        );
    }

    #[test]
    fn creates_the_slice() {
        // No controllers, so the delegation writes are skipped and this is a
        // test of the mkdir alone.
        let fake = Fake::bare("slice", "\n");
        let hierarchy = Hierarchy::new(&fake.root, 1).unwrap();

        assert!(hierarchy.slice().is_dir());
        assert!(hierarchy.slice().ends_with(SLICE));
    }

    #[test]
    fn delegates_controllers_down_to_the_slice() {
        let fake = Fake::new("delegate");
        let hierarchy = Hierarchy::new(&fake.root, 1).unwrap();

        assert_eq!(
            read_back(&fake.root.join("cgroup.subtree_control")),
            "+memory +pids",
            "the root must delegate, or the slice has no memory.max to write"
        );
        assert_eq!(
            read_back(&hierarchy.slice().join("cgroup.subtree_control")),
            "+memory +pids",
            "and the slice must delegate, or the services have none"
        );
    }

    #[test]
    fn enables_only_controllers_the_kernel_has() {
        let fake = Fake::new("partial");
        fs::write(fake.root.join("cgroup.controllers"), "cpuset cpu io pids\n").unwrap();

        let hierarchy = Hierarchy::new(&fake.root, 1).unwrap();

        assert_eq!(
            read_back(&hierarchy.slice().join("cgroup.subtree_control")),
            "+pids",
            "a kernel built without the memory controller must still boot"
        );
    }

    #[test]
    fn a_service_cgroup_is_named_for_its_unit() {
        let fake = Fake::new("naming");
        let hierarchy = Hierarchy::new(&fake.root, 1).unwrap();
        let cgroup = hierarchy.cgroup("sshd.service");

        assert_eq!(cgroup.path(), fake.root.join("oxinit.slice/sshd.service"));
        assert!(cgroup.procs().ends_with("cgroup.procs"));
        assert!(cgroup.events().ends_with("cgroup.events"));
    }

    #[test]
    fn creating_an_existing_cgroup_is_not_an_error() {
        let fake = Fake::new("recreate");
        let cgroup = Hierarchy::new(&fake.root, 1).unwrap().cgroup("x.service");

        cgroup.create().unwrap();
        // A restarting service reuses its cgroup rather than churning it.
        cgroup.create().unwrap();
    }

    #[test]
    fn limits_land_in_the_documented_files() {
        let fake = Fake::new("limits");
        let cgroup = Hierarchy::new(&fake.root, 1).unwrap().cgroup("x.service");
        cgroup.create().unwrap();
        fake.interface(cgroup.path(), false);

        cgroup
            .apply(&Resources {
                memory_max: Some(SizeValue::Bytes(256 * 1024 * 1024)),
                tasks_max: Some(512),
            })
            .unwrap();

        assert_eq!(read_back(&cgroup.path().join("memory.max")), "268435456");
        assert_eq!(read_back(&cgroup.path().join("pids.max")), "512");
    }

    #[test]
    fn an_absent_limit_is_left_alone() {
        let fake = Fake::new("absent");
        let cgroup = Hierarchy::new(&fake.root, 1).unwrap().cgroup("x.service");
        cgroup.create().unwrap();
        fake.interface(cgroup.path(), false);

        cgroup.apply(&Resources::default()).unwrap();

        // Not rewritten as "max": an operator's out-of-band limit survives a
        // restart of a unit that declares no limit of its own.
        assert_eq!(read_back(&cgroup.path().join("memory.max")), "max\n");
        assert_eq!(read_back(&cgroup.path().join("pids.max")), "max\n");
    }

    #[test]
    fn memory_max_written_as_the_kernel_spells_it() {
        let fake = Fake::new("max");
        let cgroup = Hierarchy::new(&fake.root, 1).unwrap().cgroup("x.service");
        cgroup.create().unwrap();
        fake.interface(cgroup.path(), false);

        cgroup
            .apply(&Resources {
                memory_max: Some(SizeValue::Max),
                tasks_max: None,
            })
            .unwrap();

        assert_eq!(read_back(&cgroup.path().join("memory.max")), "max");
    }

    #[test]
    fn populated_is_read_from_cgroup_events() {
        let fake = Fake::new("populated");
        let cgroup = Hierarchy::new(&fake.root, 1).unwrap().cgroup("x.service");
        cgroup.create().unwrap();

        fake.interface(cgroup.path(), true);
        assert!(cgroup.populated().unwrap());

        fake.interface(cgroup.path(), false);
        assert!(!cgroup.populated().unwrap());
    }

    #[test]
    fn parses_the_events_file_the_kernel_writes() {
        assert_eq!(parse_populated("populated 1\nfrozen 0\n"), Some(true));
        assert_eq!(parse_populated("populated 0\nfrozen 0\n"), Some(false));
        // Key order is not guaranteed, and new keys get added over time.
        assert_eq!(parse_populated("frozen 0\npopulated 1\n"), Some(true));
        assert_eq!(parse_populated("frozen 0\n"), None);
        assert_eq!(parse_populated(""), None);
    }

    #[test]
    fn pids_reads_cgroup_procs() {
        let fake = Fake::new("pids");
        let cgroup = Hierarchy::new(&fake.root, 1).unwrap().cgroup("x.service");
        cgroup.create().unwrap();

        fs::write(cgroup.path().join("cgroup.procs"), "412\n413\n\n").unwrap();
        assert_eq!(cgroup.pids().unwrap(), [412, 413]);

        // An emptied cgroup, which is the file's normal end state.
        fs::write(cgroup.path().join("cgroup.procs"), "").unwrap();
        assert!(cgroup.pids().unwrap().is_empty());
    }

    #[test]
    fn kill_writes_the_documented_value() {
        let fake = Fake::new("kill");
        let cgroup = Hierarchy::new(&fake.root, 1).unwrap().cgroup("x.service");
        cgroup.create().unwrap();
        fake.interface(cgroup.path(), true);

        cgroup.kill().unwrap();
        assert_eq!(read_back(&cgroup.path().join("cgroup.kill")), "1");
    }

    #[test]
    fn stats_read_the_current_files() {
        let fake = Fake::new("stats");
        let cgroup = Hierarchy::new(&fake.root, 1).unwrap().cgroup("x.service");
        cgroup.create().unwrap();

        // Neither controller enabled: reporting, not a failure.
        assert_eq!(cgroup.stats(), Stats::default());

        fs::write(cgroup.path().join("memory.current"), "1572864\n").unwrap();
        fs::write(cgroup.path().join("pids.current"), "3\n").unwrap();

        let stats = cgroup.stats();
        assert_eq!(stats.memory, Some(1_572_864));
        assert_eq!(stats.tasks, Some(3));
        assert_eq!(stats.to_string(), "memory 1.5M, tasks 3");
    }

    #[test]
    fn byte_counts_scale_to_binary_units() {
        assert_eq!(Bytes(0).to_string(), "0B");
        assert_eq!(Bytes(512).to_string(), "512.0B");
        assert_eq!(Bytes(1024).to_string(), "1.0K");
        assert_eq!(Bytes(1536).to_string(), "1.5K");
        assert_eq!(Bytes(3 << 30).to_string(), "3.0G");
    }

    #[test]
    fn writing_a_missing_interface_file_is_an_error() {
        let fake = Fake::new("missing");
        let cgroup = Hierarchy::new(&fake.root, 1).unwrap().cgroup("x.service");
        cgroup.create().unwrap();

        // No memory.max, because the controller was not delegated. This must
        // report rather than create a regular file of the same name.
        let result = cgroup.apply(&Resources {
            memory_max: Some(SizeValue::Bytes(1)),
            tasks_max: None,
        });

        assert!(matches!(result, Err(CgroupError::Write { .. })));
        assert!(!cgroup.path().join("memory.max").exists());
    }
}
