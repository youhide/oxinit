//! Resolving a service's `user` to the ids `setuid` needs.
//!
//! `/etc/passwd` and `/etc/group` are colon-separated text, and this crate
//! parses them. It does not call `getpwnam`: glibc resolves that through NSS,
//! which means `dlopen`, arbitrary third-party code, and a lookup that can
//! block on a network — none of which belongs in PID 1, and none of which can
//! be tested without the developer's own user database.
//!
//! The split matters for a second reason. Resolution happens in the parent,
//! before `fork`; the `setgroups`/`setgid`/`setuid` calls happen in the child,
//! between fork and exec, where allocating or reading a file is not allowed.
//! Everything that could fail is therefore done here, early, where it can be
//! reported against the unit that asked for it.
//!
//! This runs inside PID 1, so a panic here is a panic in PID 1.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use thiserror::Error;

pub const PASSWD_PATH: &str = "/etc/passwd";
pub const GROUP_PATH: &str = "/etc/group";

/// The default `user`, and the one case that needs no lookup.
pub const ROOT: &str = "root";

#[derive(Debug, Error)]
pub enum UserError {
    #[error("no user named {name} in {PASSWD_PATH}")]
    Unknown { name: String },

    #[error("{PASSWD_PATH}: entry for {name} is malformed")]
    Malformed { name: String },

    #[error("read {path}: {source}")]
    Read {
        path: &'static str,
        #[source]
        source: std::io::Error,
    },
}

/// Who a service runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// The supplementary set passed to `setgroups`, primary gid included.
    ///
    /// `initgroups` includes the primary group, and dropping it here would
    /// silently narrow what the service can reach compared to a login shell
    /// for the same account.
    pub groups: Vec<u32>,
}

impl Identity {
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }
}

/// Resolve `name` against the real `/etc/passwd` and `/etc/group`.
///
/// At start time, not at load time: a unit may name a user that an earlier
/// service in the boot creates.
pub fn resolve_system(name: &str) -> Result<Identity, UserError> {
    let passwd = read(PASSWD_PATH)?;

    // A missing /etc/group is survivable — the user still has a primary gid —
    // where a missing /etc/passwd is not.
    let group = std::fs::read_to_string(GROUP_PATH).unwrap_or_default();

    resolve(name, &passwd, &group)
}

/// Resolve `name` against the contents of a passwd and a group file.
pub fn resolve(name: &str, passwd: &str, group: &str) -> Result<Identity, UserError> {
    let entry = passwd
        .lines()
        .filter_map(fields)
        .find(|fields| fields.first() == Some(&name))
        .ok_or_else(|| UserError::Unknown {
            name: name.to_owned(),
        })?;

    // name:password:uid:gid:gecos:home:shell
    let uid = entry.get(2).and_then(|v| v.parse().ok());
    let gid = entry.get(3).and_then(|v| v.parse().ok());

    let (Some(uid), Some(gid)) = (uid, gid) else {
        return Err(UserError::Malformed {
            name: name.to_owned(),
        });
    };

    let mut groups = vec![gid];
    // name:password:gid:member,member,...
    for fields in group.lines().filter_map(fields) {
        let Some(members) = fields.get(3) else {
            continue;
        };
        if !members.split(',').any(|member| member == name) {
            continue;
        }
        if let Some(gid) = fields.get(2).and_then(|v| v.parse().ok()) {
            groups.push(gid);
        }
    }

    // Sorted and deduplicated so the set is deterministic and a user listed
    // in their own primary group is not passed to `setgroups` twice.
    groups.sort_unstable();
    groups.dedup();

    Ok(Identity {
        name: name.to_owned(),
        uid,
        gid,
        groups,
    })
}

/// The colon-separated fields of one line, or `None` if the line is not an
/// entry.
///
/// A malformed line elsewhere in the file is skipped rather than fatal: one
/// bad entry must not make every account on the machine unresolvable. A
/// malformed entry for the user actually being looked up is still an error.
fn fields(line: &str) -> Option<Vec<&str>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let fields: Vec<&str> = line.split(':').collect();
    if fields.len() < 4 {
        return None;
    }
    Some(fields)
}

fn read(path: &'static str) -> Result<String, UserError> {
    std::fs::read_to_string(path).map_err(|source| UserError::Read { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
daemon:x:1:1:daemon:/usr/sbin:/sbin/nologin
oxinit:x:1000:1000:oxinit service account:/var/empty:/sbin/nologin
nobody:x:65534:65534:nobody:/:/sbin/nologin
";

    const GROUP: &str = "\
root:x:0:
daemon:x:1:
adm:x:4:oxinit,syslog
oxinit:x:1000:
kvm:x:34:oxinit
nobody:x:65534:
";

    fn resolved(name: &str) -> Identity {
        resolve(name, PASSWD, GROUP).unwrap()
    }

    #[test]
    fn resolves_the_documented_fields() {
        let identity = resolved("nobody");
        assert_eq!(identity.name, "nobody");
        assert_eq!(identity.uid, 65534);
        assert_eq!(identity.gid, 65534);
        assert!(!identity.is_root());
    }

    #[test]
    fn root_is_uid_zero() {
        assert!(resolved("root").is_root());
        assert_eq!(resolved("root").uid, 0);
    }

    #[test]
    fn collects_supplementary_groups() {
        // Primary 1000, plus adm (4) and kvm (34) by membership.
        assert_eq!(resolved("oxinit").groups, [4, 34, 1000]);
    }

    #[test]
    fn the_primary_group_is_always_in_the_set() {
        // `initgroups` includes it, and dropping it would quietly give the
        // service less than a login shell for the same account has.
        assert_eq!(resolved("nobody").groups, [65534]);
    }

    #[test]
    fn a_user_listed_in_their_own_primary_group_is_not_duplicated() {
        let group = "oxinit:x:1000:oxinit\n";
        assert_eq!(resolve("oxinit", PASSWD, group).unwrap().groups, [1000]);
    }

    #[test]
    fn membership_matches_whole_names_only() {
        let passwd = "ox:x:5:5::/:/sbin/nologin\n";
        let group = "wheel:x:10:oxinit,proxy\n";
        // "ox" is a prefix of "oxinit" and a substring of "proxy". Neither is
        // membership.
        assert_eq!(resolve("ox", passwd, group).unwrap().groups, [5]);
    }

    #[test]
    fn unknown_user_is_an_error() {
        assert!(matches!(
            resolve("ghost", PASSWD, GROUP),
            Err(UserError::Unknown { .. })
        ));
    }

    #[test]
    fn a_malformed_entry_for_the_named_user_is_an_error() {
        let passwd = "broken:x:notanumber:0::/:/bin/sh\n";
        assert!(matches!(
            resolve("broken", passwd, ""),
            Err(UserError::Malformed { .. })
        ));

        let truncated = "short:x:1\n";
        // Too few fields to be an entry at all, so the name is never found.
        assert!(matches!(
            resolve("short", truncated, ""),
            Err(UserError::Unknown { .. })
        ));
    }

    #[test]
    fn a_malformed_line_elsewhere_does_not_hide_a_good_one() {
        let passwd = "garbage\n\n# a comment\nnobody:x:65534:65534::/:/sbin/nologin\n";
        assert_eq!(resolve("nobody", passwd, "").unwrap().uid, 65534);
    }

    #[test]
    fn a_group_line_without_members_is_skipped() {
        let group = "empty:x:7\nadm:x:4:nobody\n";
        assert_eq!(resolve("nobody", PASSWD, group).unwrap().groups, [4, 65534]);
    }

    #[test]
    fn missing_group_file_still_yields_the_primary_group() {
        assert_eq!(resolve("nobody", PASSWD, "").unwrap().groups, [65534]);
    }
}
