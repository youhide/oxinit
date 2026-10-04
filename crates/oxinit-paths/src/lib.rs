//! Where oxinit's sockets and logs are.
//!
//! Two scopes. The **system** manager is PID 1 and its paths are fixed:
//! `/run/oxinit` and `/var/log/oxinit`. A **user** manager — `oxinit --user`,
//! one per logged-in user — keeps the same layout under that user's own
//! directories, found the way every other per-user program finds them: the
//! XDG base directory variables its session was started with.
//!
//! Shared by `oxinit`, `oxctl` and `oxlogd`. A client that looked for the
//! control socket somewhere the manager did not put it would fail with a
//! message about a missing file, which says nothing about why.
//!
//! Nothing here touches the filesystem. Resolving a user's paths takes the
//! environment as a function, so every rule below is a host test.
//!
//! `oxinit` depends on this crate, so a panic here is a panic in PID 1.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// The system manager's paths. Fixed: PID 1 has no environment worth
/// consulting, and every client has to find them without asking.
pub mod system {
    /// Every socket PID 1 owns.
    pub const RUNTIME_DIR: &str = "/run/oxinit";
    /// `oxctl` connects here. Mode `0600`, owned by root.
    pub const CONTROL: &str = "/run/oxinit/control.sock";
    /// Services send `sd_notify` datagrams here; the value of
    /// `NOTIFY_SOCKET`.
    pub const NOTIFY: &str = "/run/oxinit/notify";
    /// `oxlogd` connects here for the read ends of the service pipes.
    pub const LOG_SOCKET: &str = "/run/oxinit/log.sock";
    /// Where `oxlogd` writes, and where `oxctl logs` reads.
    pub const LOG_DIR: &str = "/var/log/oxinit";
}

/// The socket file names, the same in every scope.
pub const CONTROL_NAME: &str = "control.sock";
pub const NOTIFY_NAME: &str = "notify";
pub const LOG_SOCKET_NAME: &str = "log.sock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// PID 1.
    System,
    /// `oxinit --user`.
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PathsError {
    #[error(
        "XDG_RUNTIME_DIR is not set; a user manager keeps its sockets there, \
         and a login session sets it (pam_elogind, pam_rundir, pam_systemd)"
    )]
    NoRuntimeDir,

    #[error("XDG_RUNTIME_DIR is `{0}`, which is not an absolute path")]
    RelativeRuntimeDir(String),

    #[error("HOME is not set or not absolute, and {0} is not set either")]
    NoHome(&'static str),
}

/// Where everything is, for one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub scope: Scope,
    /// The directory holding the control, notify and log sockets.
    pub runtime_dir: PathBuf,
    /// Where `oxlogd` writes `<unit>.log`.
    pub log_dir: PathBuf,
    /// `$XDG_CONFIG_HOME`, for a user: the base the user's own unit
    /// directory is under. `None` for the system.
    pub config_home: Option<PathBuf>,
}

impl Paths {
    pub fn system() -> Self {
        Self {
            scope: Scope::System,
            runtime_dir: PathBuf::from(system::RUNTIME_DIR),
            log_dir: PathBuf::from(system::LOG_DIR),
            config_home: None,
        }
    }

    /// A user manager's paths, from the process's own environment.
    pub fn user_from_env() -> Result<Self, PathsError> {
        Self::user(|name| std::env::var_os(name))
    }

    /// A user manager's paths, from an environment given as a lookup.
    ///
    /// - Sockets: `$XDG_RUNTIME_DIR/oxinit/`. Required, and absolute: it is
    ///   the per-user, per-boot, `0700` directory the session was given, and
    ///   there is nowhere else a user's sockets can go that is both private
    ///   and gone at logout.
    /// - Logs: `$XDG_STATE_HOME/oxinit/log/`, defaulting to
    ///   `~/.local/state/oxinit/log/`. State, not cache: logs are worth
    ///   keeping across a reboot, and not configuration either.
    /// - Units: under `$XDG_CONFIG_HOME`, defaulting to `~/.config`.
    ///
    /// A relative `XDG_*_HOME` is ignored, as the XDG specification says:
    /// relative to what would depend on the directory oxinit was started in.
    pub fn user(env: impl Fn(&str) -> Option<OsString>) -> Result<Self, PathsError> {
        let runtime = match env("XDG_RUNTIME_DIR") {
            None => return Err(PathsError::NoRuntimeDir),
            Some(value) if value.is_empty() => return Err(PathsError::NoRuntimeDir),
            Some(value) => PathBuf::from(value),
        };
        if !runtime.is_absolute() {
            return Err(PathsError::RelativeRuntimeDir(
                runtime.display().to_string(),
            ));
        }

        let home = env("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
        let base = |name: &'static str, fallback: &str| -> Result<PathBuf, PathsError> {
            match env(name).map(PathBuf::from).filter(|dir| dir.is_absolute()) {
                Some(dir) => Ok(dir),
                None => home
                    .as_deref()
                    .map(|home| home.join(fallback))
                    .ok_or(PathsError::NoHome(name)),
            }
        };

        let config_home = base("XDG_CONFIG_HOME", ".config")?;
        let state_home = base("XDG_STATE_HOME", ".local/state")?;

        Ok(Self {
            scope: Scope::User,
            runtime_dir: runtime.join("oxinit"),
            log_dir: state_home.join("oxinit").join("log"),
            config_home: Some(config_home),
        })
    }

    pub fn control(&self) -> PathBuf {
        self.runtime_dir.join(CONTROL_NAME)
    }

    pub fn notify(&self) -> PathBuf {
        self.runtime_dir.join(NOTIFY_NAME)
    }

    pub fn log_socket(&self) -> PathBuf {
        self.runtime_dir.join(LOG_SOCKET_NAME)
    }

    pub fn log(&self, unit: &str) -> PathBuf {
        self.log_dir.join(format!("{unit}.log"))
    }

    pub fn is_user(&self) -> bool {
        self.scope == Scope::User
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn the_system_paths_are_the_fixed_ones() {
        let paths = Paths::system();
        assert_eq!(paths.control(), Path::new(system::CONTROL));
        assert_eq!(paths.notify(), Path::new(system::NOTIFY));
        assert_eq!(paths.log_socket(), Path::new(system::LOG_SOCKET));
        assert_eq!(paths.log_dir, Path::new(system::LOG_DIR));
        assert_eq!(paths.config_home, None);
        assert!(!paths.is_user());
    }

    #[test]
    fn a_user_keeps_the_same_layout_under_its_own_directories() {
        let paths = Paths::user(env(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HOME", "/home/ana"),
        ]))
        .unwrap();

        assert_eq!(
            paths.control(),
            Path::new("/run/user/1000/oxinit/control.sock")
        );
        assert_eq!(paths.notify(), Path::new("/run/user/1000/oxinit/notify"));
        assert_eq!(
            paths.log_socket(),
            Path::new("/run/user/1000/oxinit/log.sock")
        );
        assert_eq!(
            paths.log_dir,
            Path::new("/home/ana/.local/state/oxinit/log")
        );
        assert_eq!(
            paths.log("pipewire"),
            Path::new("/home/ana/.local/state/oxinit/log/pipewire.log")
        );
        assert_eq!(
            paths.config_home.as_deref(),
            Some(Path::new("/home/ana/.config"))
        );
        assert!(paths.is_user());
    }

    #[test]
    fn the_xdg_homes_win_over_home() {
        let paths = Paths::user(env(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HOME", "/home/ana"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_STATE_HOME", "/state"),
        ]))
        .unwrap();

        assert_eq!(paths.config_home.as_deref(), Some(Path::new("/cfg")));
        assert_eq!(paths.log_dir, Path::new("/state/oxinit/log"));
    }

    #[test]
    fn a_relative_xdg_home_is_ignored() {
        let paths = Paths::user(env(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HOME", "/home/ana"),
            ("XDG_CONFIG_HOME", "cfg"),
        ]))
        .unwrap();
        assert_eq!(
            paths.config_home.as_deref(),
            Some(Path::new("/home/ana/.config"))
        );
    }

    #[test]
    fn no_home_is_fine_when_the_xdg_homes_are_set() {
        let paths = Paths::user(env(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_STATE_HOME", "/state"),
        ]));
        assert!(paths.is_ok());
    }

    #[test]
    fn a_user_manager_needs_a_runtime_dir() {
        assert_eq!(
            Paths::user(env(&[("HOME", "/home/ana")])),
            Err(PathsError::NoRuntimeDir)
        );
        assert_eq!(
            Paths::user(env(&[("XDG_RUNTIME_DIR", ""), ("HOME", "/home/ana")])),
            Err(PathsError::NoRuntimeDir)
        );
        assert_eq!(
            Paths::user(env(&[
                ("XDG_RUNTIME_DIR", "run/user/1000"),
                ("HOME", "/home/ana")
            ])),
            Err(PathsError::RelativeRuntimeDir("run/user/1000".to_owned()))
        );
    }

    #[test]
    fn and_a_home_when_the_xdg_homes_are_unset() {
        assert_eq!(
            Paths::user(env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])),
            Err(PathsError::NoHome("XDG_CONFIG_HOME"))
        );
        assert_eq!(
            Paths::user(env(&[
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("HOME", "relative")
            ])),
            Err(PathsError::NoHome("XDG_CONFIG_HOME"))
        );
    }
}
