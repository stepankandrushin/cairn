//! Where a watcher saw something happen, so that a daemon's sync lists those folders and takes the rest of
//! the tree from the last agreement, and what it takes to trust that.
//!
//! A watcher marks the folder an event was in, stamped when the event was read, and a folder that is new
//! to it, made or moved in, with everything under it. A sync takes every mark stamped since the scan the
//! last agreement rests on began (`since`), and once it agrees, from a scan begun at `t`, the marks from
//! before `t` can go: whatever they said, that scan listed. An event read after a scan began is stamped
//! after it, so a change made before the scan whose event comes late is the next sync's, never lost
//! between the two.
//!
//! Whatever a watcher cannot place, or may have missed (a queue overflowed, events dropped, a stream that
//! says to look at everything, a path outside the root), it says with `lose`, and no scope stands on
//! anything before that: the sync after it takes the whole tree. A watcher that has stopped, a folder
//! that is not watched, a reconcile, a daemon's first sync and the daily check take the whole tree too, so
//! that a lost event the watcher could not know of is found within the hour, and within a day whatever
//! the bookkeeping missed.

use cairn_protocol::Name;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What a watcher has seen, and since when it can be trusted.
#[derive(Debug)]
pub struct Marks {
    /// Each folder something happened in, by its path when the event was read: when it last did, and
    /// whether everything under it is new to the watcher.
    folders: HashMap<Vec<Name>, (Instant, bool)>,
    /// When the watcher last lost track, from its start on.
    lost: Instant,
}

impl Default for Marks {
    fn default() -> Self {
        Self {
            folders: HashMap::new(),
            lost: Instant::now(),
        }
    }
}

/// The folders to list, by path, and whether everything under each is to be listed too.
pub type Folders = BTreeMap<Vec<Name>, bool>;

impl Marks {
    /// Something happened in this folder; with `whole`, in everything under it.
    pub fn mark(&mut self, folder: &[Name], whole: bool) {
        let now = Instant::now();
        let e = self.folders.entry(folder.to_vec()).or_insert((now, false));
        e.0 = now;
        e.1 |= whole;
    }

    /// The folder an entry is in, and with `whole` the entry itself and all under it, as for a folder made
    /// or moved in.
    pub fn mark_entry(&mut self, path: &[Name], whole: bool) {
        match path.split_last() {
            Some((_, parent)) => self.mark(parent, false),
            None => self.mark(&[], false),
        }
        if whole && !path.is_empty() {
            self.mark(path, true);
        }
    }

    /// Something may have been missed: no scope stands on anything before now.
    pub fn lose(&mut self) {
        cairn_failpoint::hit("daemon.watch.lost");
        self.lost = Instant::now();
    }

    /// Every folder marked since `since`, or `None` if the watcher lost track since then.
    pub fn since(&self, since: Instant) -> Option<Folders> {
        if self.lost >= since {
            return None;
        }
        Some(
            self.folders
                .iter()
                .filter(|(_, (at, _))| *at >= since)
                .map(|(p, (_, whole))| (p.clone(), *whole))
                .collect(),
        )
    }

    /// What a scan begun at `t` has listed can go.
    pub fn prune(&mut self, t: Instant) {
        self.folders.retain(|_, (at, _)| *at >= t);
    }

    pub fn len(&self) -> usize {
        self.folders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }
}

/// A watcher's marks, shared with the daemon.
pub type Shared = Arc<Mutex<Marks>>;

pub fn lock(m: &Shared) -> std::sync::MutexGuard<'_, Marks> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a daemon's sync may narrow its scans by: the watcher's marks since the scan the last agreement in
/// this process rests on began.
#[derive(Clone)]
pub struct Source {
    pub marks: Shared,
    pub since: Instant,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("since", &self.since)
            .finish()
    }
}

impl Source {
    /// The folders to list now, or `None` for the whole tree.
    pub fn take(&self) -> Option<Folders> {
        lock(&self.marks).since(self.since)
    }
}

/// FSEvents' flags, as `watch_fsevents.rs` gets them; here so that what each means is tested everywhere.
pub mod fsevents {
    pub const MUST_SCAN: u32 = 0x01;
    pub const USER_DROPPED: u32 = 0x02;
    pub const KERNEL_DROPPED: u32 = 0x04;
    pub const ROOT_CHANGED: u32 = 0x20;
    pub const MOUNT: u32 = 0x40;
    pub const UNMOUNT: u32 = 0x80;
    pub const ITEM_CREATED: u32 = 0x100;
    pub const ITEM_RENAMED: u32 = 0x800;
    pub const IS_DIR: u32 = 0x0002_0000;
}

/// One FSEvents event, its path relative to the root's real path split into names (`None` if it is not
/// under the root or a name in it is not one), into the marks. A stream that dropped events, says to
/// look at a folder and all under it, changed above the root or saw a mount is a loss of track: must-scan
/// names a folder whose events were folded together, but comes of a stream under strain, and the whole
/// tree costs one walk.
pub fn mark_fsevent(marks: &mut Marks, rel: Option<&[Name]>, flags: u32) {
    use fsevents::*;
    if flags & (MUST_SCAN | USER_DROPPED | KERNEL_DROPPED | ROOT_CHANGED | MOUNT | UNMOUNT) != 0 {
        marks.lose();
        return;
    }
    let Some(rel) = rel else {
        marks.lose();
        return;
    };
    let new_dir = flags & IS_DIR != 0 && flags & (ITEM_CREATED | ITEM_RENAMED) != 0;
    marks.mark_entry(rel, new_dir);
    // A folder's own event, as for a change of its metadata or what is in it, is about it too.
    if flags & IS_DIR != 0 {
        marks.mark(rel, false);
    }
}

/// The data volume, as macOS mounts it under the system volume.
const DATA_VOLUME: &str = "/System/Volumes/Data";

/// The path FSEvents names a root's events by, from the root's real path. A root named through the data
/// volume, `/System/Volumes/Data/Users/...`, stays so in `canonicalize`, but its events come by the
/// path the system volume shows it at, `/Users/...`: so that one, if `same` says it is the same folder
/// (by device and inode). Matched against the longer, no event would be under the root, and each would
/// be a loss of track and a wake.
pub fn fsevents_root(real: &Path, same: impl Fn(&Path, &Path) -> bool) -> PathBuf {
    if let Ok(rest) = real.strip_prefix(DATA_VOLUME) {
        let short = Path::new("/").join(rest);
        if !rest.as_os_str().is_empty() && same(real, &short) {
            return short;
        }
    }
    real.to_path_buf()
}

/// `ReadDirectoryChangesW`'s actions.
pub mod rdcw {
    pub const ADDED: u32 = 1;
    pub const RENAMED_NEW_NAME: u32 = 5;
}

/// One `ReadDirectoryChangesW` entry, its path relative to the root split into names (`None` if a name
/// in it is not one), into the marks. The entry does not say whether it is a folder, so one added or
/// renamed in is listed whole in case it is: a file there lists nothing.
pub fn mark_rdcw(marks: &mut Marks, rel: Option<&[Name]>, action: u32) {
    let Some(rel) = rel else {
        marks.lose();
        return;
    };
    let new = matches!(action, rdcw::ADDED | rdcw::RENAMED_NEW_NAME);
    marks.mark_entry(rel, new);
    // It may be a folder whose metadata changed, which its own listing shows nothing of but itself.
    marks.mark(rel, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(p: &str) -> Vec<Name> {
        p.split('/')
            .filter(|s| !s.is_empty())
            .map(|s| Name::new(s.as_bytes().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn marks_since_a_scan_began_are_all_and_only_what_came_after_it() {
        let mut m = Marks::default();
        let before = Instant::now();
        assert_eq!(m.since(before - std::time::Duration::from_secs(1)), None);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t0 = Instant::now();
        m.mark_entry(&names("a/b/f"), false);
        m.mark_entry(&names("c/new"), true);
        let got = m.since(t0).unwrap();
        assert_eq!(got.get(&names("a/b")), Some(&false));
        assert_eq!(got.get(&names("c")), Some(&false));
        assert_eq!(got.get(&names("c/new")), Some(&true));
        assert_eq!(got.len(), 3);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t1 = Instant::now();
        m.mark_entry(&names("d/g"), false);
        m.prune(t1);
        assert_eq!(m.since(t0).unwrap().len(), 1);
        // A loss after the scan began leaves nothing to narrow it by.
        m.lose();
        assert_eq!(m.since(t1), None);
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(m.since(Instant::now()).is_some());
    }

    #[test]
    fn an_fsevent_that_dropped_must_scan_or_cannot_be_placed_is_a_loss() {
        use fsevents::*;
        for flags in [
            MUST_SCAN | IS_DIR,
            USER_DROPPED,
            KERNEL_DROPPED,
            ROOT_CHANGED,
            MOUNT,
            UNMOUNT,
        ] {
            let mut m = Marks::default();
            std::thread::sleep(std::time::Duration::from_millis(2));
            let t = Instant::now();
            mark_fsevent(&mut m, Some(&names("a/f")), flags);
            assert_eq!(m.since(t), None, "flags {flags:#x}");
        }
        let mut m = Marks::default();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t = Instant::now();
        mark_fsevent(&mut m, None, 0);
        assert_eq!(m.since(t), None, "a path outside the root");

        let mut m = Marks::default();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t = Instant::now();
        mark_fsevent(&mut m, Some(&names("x/f")), 0x1000);
        mark_fsevent(&mut m, Some(&names("y/d")), ITEM_CREATED | IS_DIR);
        let got = m.since(t).unwrap();
        assert_eq!(got.get(&names("x")), Some(&false));
        assert_eq!(got.get(&names("y/d")), Some(&true));
    }

    #[test]
    fn a_windows_entry_added_is_listed_whole_and_one_not_understood_is_a_loss() {
        let mut m = Marks::default();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t = Instant::now();
        mark_rdcw(&mut m, Some(&names("a/new")), rdcw::ADDED);
        mark_rdcw(&mut m, Some(&names("b/f")), 3);
        let got = m.since(t).unwrap();
        assert_eq!(got.get(&names("a")), Some(&false));
        assert_eq!(got.get(&names("a/new")), Some(&true));
        assert_eq!(got.get(&names("b")), Some(&false));
        assert_eq!(got.get(&names("b/f")), Some(&false));
        mark_rdcw(&mut m, None, 3);
        assert_eq!(m.since(t), None);
    }

    #[test]
    fn a_root_named_through_the_data_volume_is_matched_by_the_path_fsevents_gives() {
        let p = |s: &str| PathBuf::from(s);
        let same = |_: &Path, _: &Path| true;
        assert_eq!(
            fsevents_root(&p("/System/Volumes/Data/Users/u/tree"), same),
            p("/Users/u/tree")
        );
        assert_eq!(
            fsevents_root(&p("/System/Volumes/Data/private/var/t"), same),
            p("/private/var/t")
        );
        // Not the same folder at the shorter path: the real one, and the seam was asked of the two.
        let asked = std::cell::RefCell::new(Vec::new());
        let other = |a: &Path, b: &Path| {
            asked.borrow_mut().push((a.to_path_buf(), b.to_path_buf()));
            false
        };
        assert_eq!(
            fsevents_root(&p("/System/Volumes/Data/Volumes/x"), other),
            p("/System/Volumes/Data/Volumes/x")
        );
        assert_eq!(
            asked.into_inner(),
            [(p("/System/Volumes/Data/Volumes/x"), p("/Volumes/x"))]
        );
        // Anything else is left as it is, the data volume itself and a name that only begins like it.
        let never = |_: &Path, _: &Path| -> bool { panic!("asked") };
        for real in [
            "/Users/u/tree",
            "/System/Volumes/Data",
            "/System/Volumes/DataX/u",
            "/System/Volumes/Preboot/u",
            "/private/var/t",
        ] {
            assert_eq!(fsevents_root(&p(real), never), p(real), "{real}");
        }
    }
}
