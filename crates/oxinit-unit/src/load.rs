//! Loading units from disk.
//!
//! Two directories, in order. A file in `/etc` replaces the file of the same
//! name in `/usr/lib` completely — no per-key merging, no drop-ins. The
//! effective unit is always a file you can `cat`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::UnitError;
use crate::unit::{self, Scope, Unit};

/// Packaged units. Lowest precedence.
pub const VENDOR_DIR: &str = "/usr/lib/oxinit/units";

/// Operator units. Replaces a vendor file of the same name.
pub const ETC_DIR: &str = "/etc/oxinit/units";

/// Packaged units for every user's manager. Lowest precedence.
pub const USER_VENDOR_DIR: &str = "/usr/lib/oxinit/user-units";

/// The operator's units for every user's manager.
pub const USER_ETC_DIR: &str = "/etc/oxinit/user-units";

/// A user's own units, under their `$XDG_CONFIG_HOME`. Highest precedence:
/// the user is the operator of their own manager.
pub const USER_CONFIG_SUBDIR: &str = "oxinit/user-units";

/// A user manager's unit directories, lowest precedence first.
///
/// Three rather than two because a user manager has two operators: the
/// machine's, in `/etc`, and the user, in their own configuration. Each
/// replaces a file of the same name wholly, as `/etc` does for the system.
pub fn user_dirs(config_home: &Path) -> Vec<PathBuf> {
    vec![
        PathBuf::from(USER_VENDOR_DIR),
        PathBuf::from(USER_ETC_DIR),
        config_home.join(USER_CONFIG_SUBDIR),
    ]
}

/// Units found on disk, plus whatever went wrong finding them.
#[derive(Debug, Default)]
pub struct Loaded {
    /// Sorted by name, so start order is stable when the graph leaves it free.
    pub units: BTreeMap<String, Unit>,
    /// Files that failed to parse or validate.
    pub errors: Vec<UnitError>,
}

/// Load from the standard directories, `/etc` winning over `/usr/lib`.
pub fn load_default(hostname: &str) -> Loaded {
    load_dirs(&[Path::new(VENDOR_DIR), Path::new(ETC_DIR)], hostname)
}

/// Load from the given directories, each overriding the ones before it.
///
/// A missing directory is not an error: a machine with no `/etc/oxinit/units`
/// is a machine with no operator overrides.
pub fn load_dirs(dirs: &[&Path], hostname: &str) -> Loaded {
    load_dirs_in(dirs, hostname, &Scope::System)
}

/// As [`load_dirs`], for the manager `scope` says.
pub fn load_dirs_in(dirs: &[&Path], hostname: &str, scope: &Scope) -> Loaded {
    let mut loaded = Loaded::default();

    for dir in dirs {
        match unit_files(dir) {
            Ok(files) => {
                for (name, path) in files {
                    match read_and_parse(&name, &path, hostname, scope) {
                        // Replaces any unit of the same name from an earlier
                        // directory, wholly.
                        Ok(unit) => {
                            loaded.units.insert(name, unit);
                        }
                        Err(e) => loaded.errors.push(e),
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => loaded.errors.push(UnitError::Directory {
                path: dir.display().to_string(),
                message: source.to_string(),
            }),
        }
    }

    loaded
}

/// `*.toml` in a directory, as (unit name, path), sorted by name.
fn unit_files(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut found = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
            found.push((name.to_owned(), path));
        }
    }

    found.sort();
    Ok(found)
}

fn read_and_parse(
    name: &str,
    path: &Path,
    hostname: &str,
    scope: &Scope,
) -> Result<Unit, UnitError> {
    let text = std::fs::read_to_string(path).map_err(|source| UnitError::Read {
        path: path.display().to_string(),
        message: source.to_string(),
    })?;

    unit::parse_in(name, &text, hostname, scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans up after itself.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("oxinit-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn write(&self, name: &str, text: &str) {
            std::fs::write(self.0.join(name), text).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SERVICE: &str = "[service]\nexec = \"/bin/true\"\n";

    #[test]
    fn etc_replaces_vendor_wholly() {
        let vendor = TempDir::new("vendor");
        let etc = TempDir::new("etc");

        vendor.write(
            "sshd.toml",
            "[unit]\ndescription = \"packaged\"\n[service]\nexec = \"/bin/vendor\"\nuser = \"nobody\"\n",
        );
        // No description and no user: replacement is total, so both fall back
        // to their defaults rather than inheriting from the vendor file.
        etc.write("sshd.toml", "[service]\nexec = \"/bin/local\"\n");

        let loaded = load_dirs(&[&vendor.0, &etc.0], "h");
        let unit = loaded.units.get("sshd").unwrap();

        assert!(loaded.errors.is_empty());
        assert_eq!(unit.service().unwrap().exec, ["/bin/local"]);
        assert_eq!(unit.description, "sshd", "no per-key merge");
        assert_eq!(unit.service().unwrap().user, "root", "no per-key merge");
    }

    #[test]
    fn vendor_units_survive_when_not_overridden() {
        let vendor = TempDir::new("v2");
        let etc = TempDir::new("e2");
        vendor.write("a.toml", SERVICE);
        vendor.write("b.toml", SERVICE);
        etc.write("b.toml", SERVICE);

        let loaded = load_dirs(&[&vendor.0, &etc.0], "h");
        assert_eq!(loaded.units.len(), 2);
        assert!(loaded.units.contains_key("a"));
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let loaded = load_dirs(&[Path::new("/nonexistent/oxinit/units")], "h");
        assert!(loaded.units.is_empty());
        assert!(loaded.errors.is_empty());
    }

    #[test]
    fn ignores_non_toml_files() {
        let dir = TempDir::new("mixed");
        dir.write("a.toml", SERVICE);
        dir.write("README", "not a unit");
        dir.write("b.toml.bak", "not a unit either");

        let loaded = load_dirs(&[&dir.0], "h");
        assert_eq!(loaded.units.len(), 1);
        assert!(loaded.errors.is_empty());
    }

    #[test]
    fn a_bad_file_is_reported_without_losing_the_good_ones() {
        let dir = TempDir::new("bad");
        dir.write("good.toml", SERVICE);
        dir.write("bad.toml", "[service]\nexec = \"/bin/true\"\nnope = 1\n");

        let loaded = load_dirs(&[&dir.0], "h");
        assert_eq!(loaded.units.len(), 1);
        assert_eq!(loaded.errors.len(), 1);
    }

    #[test]
    fn a_user_manager_reads_three_directories_in_order() {
        let dirs = user_dirs(Path::new("/home/ana/.config"));
        assert_eq!(
            dirs,
            [
                PathBuf::from("/usr/lib/oxinit/user-units"),
                PathBuf::from("/etc/oxinit/user-units"),
                PathBuf::from("/home/ana/.config/oxinit/user-units"),
            ]
        );
    }

    #[test]
    fn the_users_own_unit_replaces_the_machines() {
        let vendor = TempDir::new("uvendor");
        let user = TempDir::new("uconfig");
        vendor.write("pipewire.toml", "[service]\nexec = \"/usr/bin/pipewire\"\n");
        user.write(
            "pipewire.toml",
            "[service]\nexec = \"/home/ana/bin/pipewire\"\n",
        );

        let scope = Scope::User {
            name: "ana".to_owned(),
        };
        let loaded = load_dirs_in(&[&vendor.0, &user.0], "h", &scope);
        let unit = loaded.units.get("pipewire").unwrap();
        assert_eq!(unit.service().unwrap().exec, ["/home/ana/bin/pipewire"]);
        assert_eq!(unit.service().unwrap().user, "ana");
    }

    #[test]
    fn a_system_unit_in_a_user_directory_is_reported() {
        let dir = TempDir::new("usys");
        dir.write(
            "sshd.toml",
            "[service]\nexec = \"/usr/bin/sshd\"\nuser = \"root\"\n",
        );

        let scope = Scope::User {
            name: "ana".to_owned(),
        };
        let loaded = load_dirs_in(&[&dir.0], "h", &scope);
        assert!(loaded.units.is_empty());
        assert_eq!(
            loaded.errors,
            [UnitError::UserInUserUnit {
                unit: "sshd".to_owned()
            }]
        );
    }

    #[test]
    fn empty_file_has_no_kind_and_fails() {
        let dir = TempDir::new("empty");
        dir.write("masked.toml", "");

        let loaded = load_dirs(&[&dir.0], "h");
        assert!(loaded.units.is_empty());
        assert_eq!(loaded.errors.len(), 1);
    }
}
