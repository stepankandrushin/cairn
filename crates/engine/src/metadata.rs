//! Reading metadata from an entry, and applying a stored record under a client's policy.
//!
//! What is stored and what is applied are separate (invariant 2): the policy decides what reaches
//! this filesystem, and whatever does not is kept on the server, untouched.

use crate::fsops::{Dir, Stat};
use cairn_protocol::{Bytes, Metadata, ObjectKind, Owner, RecordKey, NS_LINUX_XATTR};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerPolicy {
    /// Leave ownership to the local user. The stored owner is kept.
    #[default]
    None,
    /// Map the stored names to local IDs, falling back to the numbers.
    ByName,
    /// Apply the stored numbers as they are.
    ByNumber,
}

/// What a client applies to its filesystem. Never affects what is stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyPolicy {
    #[serde(default)]
    pub owners: OwnerPolicy,
    /// Apply permission bits at all. Off models a destination that cannot hold them.
    #[serde(default = "yes")]
    pub mode: bool,
    /// Apply setuid and setgid.
    #[serde(default)]
    pub setid: bool,
    /// Apply Linux capabilities (`security.capability`).
    #[serde(default)]
    pub capabilities: bool,
    /// Prefixes of xattr names to apply.
    #[serde(default = "default_xattr_allow")]
    pub xattr_allow: Vec<String>,
    /// Prefixes never applied, whatever is allowed.
    #[serde(default = "default_xattr_deny")]
    pub xattr_deny: Vec<String>,
}

fn yes() -> bool {
    true
}
fn default_xattr_allow() -> Vec<String> {
    [
        "user.",
        "system.posix_acl_access",
        "system.posix_acl_default",
    ]
    .map(String::from)
    .to_vec()
}
fn default_xattr_deny() -> Vec<String> {
    vec!["security.selinux".into()]
}

impl Default for ApplyPolicy {
    fn default() -> Self {
        Self {
            owners: OwnerPolicy::None,
            mode: true,
            setid: false,
            capabilities: false,
            xattr_allow: default_xattr_allow(),
            xattr_deny: default_xattr_deny(),
        }
    }
}

impl ApplyPolicy {
    /// Everything this platform can apply, for an explicit restore. `security.selinux` stays out:
    /// labels belong to the destination's policy.
    pub fn exact() -> Self {
        Self {
            owners: OwnerPolicy::ByNumber,
            mode: true,
            setid: true,
            capabilities: true,
            xattr_allow: ["user.", "system.posix_acl_", "security.", "trusted."]
                .map(String::from)
                .to_vec(),
            xattr_deny: default_xattr_deny(),
        }
    }

    pub fn allows_xattr(&self, name: &[u8]) -> bool {
        let has = |prefixes: &[String]| prefixes.iter().any(|p| name.starts_with(p.as_bytes()));
        if has(&self.xattr_deny) {
            return false;
        }
        if name == b"security.capability" {
            return self.capabilities;
        }
        has(&self.xattr_allow)
    }

    fn mode_bits(&self) -> u32 {
        if self.setid {
            0o7777
        } else {
            0o1777
        }
    }
}

/// `/etc/passwd` and `/etc/group`, read once. Static binaries have no NSS, so accounts that exist
/// only in a directory service keep their numbers and no name.
#[derive(Debug, Default)]
pub struct Accounts {
    users: HashMap<u32, Vec<u8>>,
    groups: HashMap<u32, Vec<u8>>,
    user_ids: HashMap<Vec<u8>, u32>,
    group_ids: HashMap<Vec<u8>, u32>,
}

fn parse_ids(path: &str) -> (HashMap<u32, Vec<u8>>, HashMap<Vec<u8>, u32>) {
    let mut by_id = HashMap::new();
    let mut by_name = HashMap::new();
    if let Ok(data) = std::fs::read(path) {
        for line in data.split(|&b| b == b'\n') {
            let mut fields = line.split(|&b| b == b':');
            let (Some(name), Some(_), Some(id)) = (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Some(id) = std::str::from_utf8(id)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            by_id.entry(id).or_insert_with(|| name.to_vec());
            by_name.entry(name.to_vec()).or_insert(id);
        }
    }
    (by_id, by_name)
}

impl Accounts {
    pub fn load() -> Self {
        let (users, user_ids) = parse_ids("/etc/passwd");
        let (groups, group_ids) = parse_ids("/etc/group");
        Self {
            users,
            groups,
            user_ids,
            group_ids,
        }
    }
}

/// Read one entry's metadata. A value that cannot be read goes into `problems`, not into silence.
pub fn capture(
    dir: &Dir,
    name: &[u8],
    st: &Stat,
    accounts: &Accounts,
    precision_ns: u32,
    problems: &mut Vec<String>,
) -> Metadata {
    let mut records = BTreeMap::new();
    match dir.list_xattrs(name) {
        Ok(keys) => {
            for key in keys {
                match dir.get_xattr(name, &key) {
                    Ok(value) => {
                        records.insert(
                            RecordKey {
                                ns: NS_LINUX_XATTR.into(),
                                name: Bytes(key),
                            },
                            Bytes(value),
                        );
                    }
                    Err(e) => problems.push(format!(
                        "xattr {}: {e}",
                        cairn_protocol::name::display_bytes(&key)
                    )),
                }
            }
        }
        Err(e) => problems.push(format!("listing xattrs: {e}")),
    }
    Metadata {
        mode: st.mode,
        owner: Some(Owner {
            uid: st.uid,
            gid: st.gid,
            user: accounts.users.get(&st.uid).cloned().map(Bytes),
            group: accounts.groups.get(&st.gid).cloned().map(Bytes),
        }),
        mtime: st.mtime,
        btime: st.btime,
        time_precision_ns: precision_ns,
        records,
        streams: Vec::new(),
        unknown: BTreeMap::new(),
    }
}

/// How far metadata on its way from `from` to `to` has come, going by what is on disk now: the stored
/// state, field by field, and its pair of what the filesystem holds. A field still as it was is simply
/// still to be applied, and one that is neither has been changed here since.
pub fn arrived_fields(
    from: &Metadata,
    applied: &Metadata,
    to: &Metadata,
    now: &Metadata,
) -> (Metadata, Metadata) {
    let (mut stored, mut held) = (from.clone(), applied.clone());
    if to.mode != from.mode && now.mode == to.mode {
        stored.mode = to.mode;
        held.mode = now.mode;
    }
    if to.owner != from.owner && now.owner == to.owner {
        stored.owner.clone_from(&to.owner);
        held.owner.clone_from(&now.owner);
    }
    if (to.mtime, to.time_precision_ns) != (from.mtime, from.time_precision_ns)
        && now.mtime == to.mtime
    {
        stored.mtime = to.mtime;
        stored.time_precision_ns = to.time_precision_ns;
        held.mtime = now.mtime;
        held.time_precision_ns = now.time_precision_ns;
    }
    if to.btime != from.btime && now.btime == to.btime {
        stored.btime = to.btime;
        held.btime = now.btime;
    }
    let keys: std::collections::BTreeSet<&RecordKey> =
        from.records.keys().chain(to.records.keys()).collect();
    for key in keys {
        let want = to.records.get(key);
        if want != from.records.get(key) && now.records.get(key) == want {
            for side in [&mut stored, &mut held] {
                match want {
                    Some(v) => {
                        side.records.insert(key.clone(), v.clone());
                    }
                    None => {
                        side.records.remove(key);
                    }
                }
            }
        }
    }
    // Nothing on this filesystem says how far these have come, and nothing here changes them.
    stored.streams.clone_from(&to.streams);
    (stored, held)
}

/// The stored record after this client's local changes: start from what is stored, and carry over
/// only what changed on this filesystem since the last sync. A field this client could not apply
/// (`applied` differs from `stored`) but did not change stays as stored.
pub fn rebase_local_changes(stored: &Metadata, applied: &Metadata, scanned: &Metadata) -> Metadata {
    let mut out = stored.clone();
    let changed_bits = (applied.mode ^ scanned.mode) & 0o7777;
    out.mode = (stored.mode & !changed_bits) | (scanned.mode & changed_bits);

    let default_owner = Owner::default();
    let a = applied.owner.as_ref().unwrap_or(&default_owner);
    let s = scanned.owner.as_ref().unwrap_or(&default_owner);
    let mut owner = stored.owner.clone().or_else(|| scanned.owner.clone());
    if let Some(o) = owner.as_mut() {
        if a.uid != s.uid || a.user != s.user {
            o.uid = s.uid;
            o.user = s.user.clone();
        }
        if a.gid != s.gid || a.group != s.group {
            o.gid = s.gid;
            o.group = s.group.clone();
        }
    }
    out.owner = owner;

    if applied.mtime != scanned.mtime {
        out.mtime = scanned.mtime;
        out.time_precision_ns = scanned.time_precision_ns;
    }
    if applied.btime != scanned.btime {
        out.btime = scanned.btime;
    }

    let keys: std::collections::BTreeSet<&RecordKey> = applied
        .records
        .keys()
        .chain(scanned.records.keys())
        .collect();
    for key in keys {
        if applied.records.get(key) != scanned.records.get(key) {
            match scanned.records.get(key) {
                Some(v) => {
                    out.records.insert(key.clone(), v.clone());
                }
                None => {
                    out.records.remove(key);
                }
            }
        }
    }
    out
}

/// Apply a stored record to an entry. `current` is what the entry holds now in stored terms, so only
/// differences are touched; `None` means a new entry, where everything applies. Returns the fields
/// this client could not apply, for the report; they stay stored.
pub fn apply(
    dir: &Dir,
    name: &[u8],
    kind: ObjectKind,
    want: &Metadata,
    current: Option<&Metadata>,
    policy: &ApplyPolicy,
    accounts: &Accounts,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut note = |what: &str, e: io::Error| problems.push(format!("{what}: {e}"));

    // chown clears setuid, setgid and capabilities, so they are set after it, and a chown forces them.
    let owner_differs = current.is_none_or(|c| c.owner != want.owner);
    let mut owner_applied = false;
    if policy.owners != OwnerPolicy::None && owner_differs {
        if let Some(o) = &want.owner {
            let (uid, gid) = match policy.owners {
                OwnerPolicy::ByName => (
                    o.user
                        .as_ref()
                        .and_then(|u| accounts.user_ids.get(&u.0))
                        .copied()
                        .unwrap_or(o.uid),
                    o.group
                        .as_ref()
                        .and_then(|g| accounts.group_ids.get(&g.0))
                        .copied()
                        .unwrap_or(o.gid),
                ),
                _ => (o.uid, o.gid),
            };
            match dir.chown(name, uid, gid) {
                Ok(()) => owner_applied = true,
                Err(e) => note("owner", e),
            }
        }
    }

    let empty = BTreeMap::new();
    let current_records = current.map_or(&empty, |c| &c.records);
    for (key, value) in &want.records {
        if key.ns != NS_LINUX_XATTR || !policy.allows_xattr(&key.name.0) {
            continue;
        }
        let same = current_records.get(key) == Some(value);
        let is_capability = key.name.0 == b"security.capability";
        if same && !(owner_applied && is_capability) {
            continue;
        }
        if let Err(e) = dir.set_xattr(name, &key.name.0, &value.0) {
            note(
                &format!("xattr {}", cairn_protocol::name::display_bytes(&key.name.0)),
                e,
            );
        }
    }
    for key in current_records.keys() {
        if key.ns == NS_LINUX_XATTR
            && policy.allows_xattr(&key.name.0)
            && !want.records.contains_key(key)
        {
            match dir.remove_xattr(name, &key.name.0) {
                Ok(()) => {}
                // Stored but never applied here: already absent, which is what was asked for.
                Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NODATA.raw_os_error()) => {}
                Err(e) => note(
                    &format!(
                        "removing xattr {}",
                        cairn_protocol::name::display_bytes(&key.name.0)
                    ),
                    e,
                ),
            }
        }
    }

    if kind != ObjectKind::Symlink && policy.mode {
        let bits = policy.mode_bits();
        let differs = current.is_none_or(|c| (c.mode ^ want.mode) & bits != 0);
        if differs || owner_applied {
            if let Err(e) = dir.chmod(name, want.mode & bits) {
                note("mode", e);
            }
        }
    }

    // Times last: everything above moves ctime, and a write to a directory moves its mtime.
    if current.is_none_or(|c| c.mtime != want.mtime) {
        if let Err(e) = dir.set_mtime(name, want.mtime) {
            note("mtime", e);
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::Timestamp;

    fn with_mode(mode: u32) -> Metadata {
        Metadata {
            mode,
            ..Metadata::default()
        }
    }

    /// One field of what is on its way, each way it can stand on disk.
    #[test]
    fn a_field_has_arrived_only_if_the_disk_says_so() {
        let (from, to) = (with_mode(0o644), with_mode(0o600));
        // It got there: shared, and no local change.
        let (stored, held) = arrived_fields(&from, &from, &to, &to);
        assert_eq!(stored.mode, 0o600);
        assert_eq!(rebase_local_changes(&stored, &held, &to).mode, 0o600);
        // Not yet: still to be applied.
        let (stored, held) = arrived_fields(&from, &from, &to, &from);
        assert_eq!(stored.mode, 0o644);
        assert_eq!(rebase_local_changes(&stored, &held, &from).mode, 0o644);
        // Someone changed it since: that stays a change here.
        let now = with_mode(0o640);
        let (stored, held) = arrived_fields(&from, &from, &to, &now);
        assert_eq!(stored.mode, 0o644);
        assert_eq!(rebase_local_changes(&stored, &held, &now).mode, 0o640);
    }

    /// A field nothing was bringing is not touched by what arrives alongside it.
    #[test]
    fn a_change_here_stays_beside_what_arrived() {
        let from = with_mode(0o644);
        let mut to = with_mode(0o644);
        to.mtime = Timestamp { sec: 9, nsec: 0 };
        let mut now = to.clone();
        now.mode = 0o600;
        let (stored, held) = arrived_fields(&from, &from, &to, &now);
        assert_eq!(stored.mtime, to.mtime, "the time arrived");
        let local = rebase_local_changes(&stored, &held, &now);
        assert_eq!((local.mode, local.mtime), (0o600, to.mtime));
    }

    #[test]
    fn a_record_arrives_and_goes_by_itself() {
        let key = record("user.a");
        let gone = record("user.b");
        let value = Bytes(b"v".to_vec());
        let mut from = with_mode(0o644);
        from.records.insert(gone.clone(), value.clone());
        let mut to = with_mode(0o644);
        to.records.insert(key.clone(), value.clone());
        let (stored, held) = arrived_fields(&from, &from, &to, &to);
        assert_eq!(stored.records, to.records);
        assert_eq!(held.records, to.records);
        // Neither has happened on disk yet.
        let (stored, _) = arrived_fields(&from, &from, &to, &from);
        assert_eq!(stored.records, from.records);
    }

    fn record(k: &str) -> RecordKey {
        RecordKey {
            ns: NS_LINUX_XATTR.into(),
            name: Bytes(k.into()),
        }
    }

    /// Invariant 2, from the plan: Linux uploads at 0755, a client that cannot hold the bit edits the
    /// file, the new revision keeps 0755.
    #[test]
    fn a_client_that_cannot_hold_a_field_does_not_erase_it() {
        let stored = Metadata {
            mode: 0o755,
            ..Default::default()
        };
        let applied = Metadata {
            mode: 0o644,
            ..Default::default()
        };
        let scanned = Metadata {
            mode: 0o644,
            mtime: Timestamp { sec: 9, nsec: 0 },
            ..Default::default()
        };
        let out = rebase_local_changes(&stored, &applied, &scanned);
        assert_eq!(out.mode, 0o755);
        assert_eq!(out.mtime.sec, 9, "the edit itself is still taken");
    }

    #[test]
    fn only_the_bits_that_changed_here_change_in_the_record() {
        // setuid is stored, not applied here; a local chmod g+w must not lose it.
        let stored = Metadata {
            mode: 0o4755,
            ..Default::default()
        };
        let applied = Metadata {
            mode: 0o755,
            ..Default::default()
        };
        let scanned = Metadata {
            mode: 0o775,
            ..Default::default()
        };
        assert_eq!(
            rebase_local_changes(&stored, &applied, &scanned).mode,
            0o4775
        );
    }

    #[test]
    fn an_unapplied_owner_stays_and_a_real_change_is_taken() {
        let owner = |uid| {
            Some(Owner {
                uid,
                gid: uid,
                user: None,
                group: None,
            })
        };
        let stored = Metadata {
            owner: owner(1000),
            ..Default::default()
        };
        let applied = Metadata {
            owner: owner(501),
            ..Default::default()
        };
        assert_eq!(
            rebase_local_changes(&stored, &applied, &applied).owner,
            owner(1000),
            "numeric IDs differ between machines"
        );
        let scanned = Metadata {
            owner: Some(Owner {
                uid: 501,
                gid: 20,
                user: None,
                group: None,
            }),
            ..Default::default()
        };
        let out = rebase_local_changes(&stored, &applied, &scanned)
            .owner
            .unwrap();
        assert_eq!((out.uid, out.gid), (1000, 20));
    }

    #[test]
    fn xattrs_merge_per_key_and_unapplied_ones_stay() {
        let mut stored = Metadata::default();
        stored
            .records
            .insert(record("security.selinux"), Bytes(b"from-a".to_vec()));
        stored
            .records
            .insert(record("user.keep"), Bytes(b"1".to_vec()));
        stored
            .records
            .insert(record("user.drop"), Bytes(b"1".to_vec()));
        let mut applied = Metadata::default();
        applied
            .records
            .insert(record("user.keep"), Bytes(b"1".to_vec()));
        applied
            .records
            .insert(record("user.drop"), Bytes(b"1".to_vec()));
        let mut scanned = Metadata::default();
        scanned
            .records
            .insert(record("user.keep"), Bytes(b"1".to_vec()));
        scanned
            .records
            .insert(record("user.new"), Bytes(b"2".to_vec()));
        let out = rebase_local_changes(&stored, &applied, &scanned);
        assert!(out.records.contains_key(&record("security.selinux")));
        assert!(out.records.contains_key(&record("user.new")));
        assert!(!out.records.contains_key(&record("user.drop")));
    }

    #[test]
    fn unknown_fields_survive_a_local_change() {
        let mut stored = Metadata {
            mode: 0o644,
            ..Default::default()
        };
        stored
            .unknown
            .insert("from_the_future".into(), serde_json::json!(1));
        let scanned = Metadata {
            mode: 0o600,
            ..Default::default()
        };
        let out = rebase_local_changes(&stored, &stored, &scanned);
        assert_eq!(out.mode, 0o600);
        assert_eq!(out.unknown["from_the_future"], 1);
    }

    #[test]
    fn the_default_policy_withholds_what_needs_an_explicit_choice() {
        let p = ApplyPolicy::default();
        assert!(p.allows_xattr(b"user.x"));
        assert!(p.allows_xattr(b"system.posix_acl_access"));
        assert!(!p.allows_xattr(b"security.selinux"));
        assert!(!p.allows_xattr(b"security.capability"));
        assert!(!p.allows_xattr(b"trusted.x"));
        assert_eq!(p.mode_bits() & 0o6000, 0);

        let exact = ApplyPolicy::exact();
        assert!(exact.allows_xattr(b"security.capability"));
        assert!(exact.allows_xattr(b"trusted.x"));
        assert!(
            !exact.allows_xattr(b"security.selinux"),
            "labels stay with the destination"
        );
        assert_eq!(exact.mode_bits(), 0o7777);
    }
}
