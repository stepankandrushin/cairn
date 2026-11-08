//! Telling the daemon that something under a root changed, and in which folders. The scan decides what
//! changed: a daemon's sync lists the folders marked here and takes the rest from the last agreement
//! (scope.rs), and whatever this cannot place or may have missed, an overflow above all, it says, and the
//! sync after it lists the whole tree. A folder that cannot be watched, or a watcher that has stopped,
//! has every sync list the whole tree. So a lost event the kernel says nothing of, as a write through a
//! mapping sends none, waits for the reconcile, within the hour, and can only delay a change.
//!
//! inotify watches one directory at a time. The order that keeps this honest is watch first, sync
//! after: a directory is watched before the wake that follows its creation, so whatever was written
//! before the watch is found by that sync, and whatever comes after it is an event of its own.
//!
//! Nothing here holds a descriptor in the root between walks, so a disk can still be unmounted, which
//! a sync then finds out about.

use crate::fsops::Dir;
use crate::scan::display_path;
use crate::selection::{Rules, IGNORE_FILE};
use cairn_failpoint::hit;
use cairn_protocol::{Name, STATE_DIR};
use rustix::event::{poll, PollFd, PollFlags};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::fs::FileType;
use rustix::io::Errno;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A batch of events is cut here, so that a storm cannot hold the wake back for long.
const MAX_BATCH: usize = 4096;
/// Folders that could not be watched are named, up to this many, for the daemon to say.
const MAX_MISSED: usize = 10;

fn dir_mask() -> WatchFlags {
    WatchFlags::CREATE
        | WatchFlags::DELETE
        | WatchFlags::MOVED_FROM
        | WatchFlags::MOVED_TO
        | WatchFlags::MODIFY
        | WatchFlags::CLOSE_WRITE
        | WatchFlags::ATTRIB
        | WatchFlags::DELETE_SELF
        | WatchFlags::MOVE_SELF
        | WatchFlags::ONLYDIR
}

/// How much of the tree events can be expected from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchCounts {
    pub watched: usize,
    /// Directories that could not be watched, whose changes wait for the next full sync.
    pub unwatched: usize,
    /// The kernel's limit on watches was reached (`fs.inotify.max_user_watches`).
    pub limit_reached: bool,
    /// Reading events failed, and no more will come.
    pub stopped: bool,
}

struct Inner {
    inotify: OwnedFd,
    root: PathBuf,
    rules: Rules,
    /// Each watch and the directory it is on. A path goes stale when a parent is renamed, until the
    /// walk that a rename leads to.
    paths: WatchPaths,
    state_watch: Option<i32>,
    counts: WatchCounts,
    stopped: bool,
    /// Files the last sync found still being written, by their printable paths: their writes wake
    /// nothing, as the daemon comes back for them anyway, and their close does.
    in_motion: HashSet<String>,
    /// The first folders that could not be watched, each with why.
    missed: Vec<String>,
    /// Where something happened, for the daemon's syncs.
    marks: crate::scope::Shared,
    /// Watches whose folder was deleted, whose end is no news.
    deleted: HashSet<i32>,
}

/// Each watch's path, joined (`scope::join`), all in one buffer. As a vector of names each, a tree's
/// paths are hundreds of thousands of small blocks, made again by every walk among a sync's own; spread
/// over the heap that way they kept it from being given back between syncs: on 65k folders, 54 to 87 MB
/// held between syncs for 24 in use, and 23 to 25 held like this.
#[derive(Default)]
struct WatchPaths {
    /// Where in `bytes` each watch's path is, and how long.
    at: HashMap<i32, (usize, usize)>,
    bytes: Vec<u8>,
    /// How much of `bytes` is paths since replaced or removed.
    dead: usize,
}

impl WatchPaths {
    fn len(&self) -> usize {
        self.at.len()
    }

    fn joined(&self, wd: i32) -> Option<&[u8]> {
        let &(start, len) = self.at.get(&wd)?;
        Some(&self.bytes[start..start + len])
    }

    fn get(&self, wd: i32) -> Option<Vec<Name>> {
        self.joined(wd).map(crate::scope::split)
    }

    /// Make `path` the watch's, and say whether it was not that already: a walk that finds a path as it
    /// was allocates nothing.
    fn set(&mut self, wd: i32, path: &[Name]) -> bool {
        if let Some(joined) = self.joined(wd) {
            if crate::scope::is_joined(joined, path) {
                return false;
            }
            self.dead += joined.len();
        }
        let start = self.bytes.len();
        self.bytes.extend_from_slice(&crate::scope::join(path));
        self.at.insert(wd, (start, self.bytes.len() - start));
        self.compact();
        true
    }

    /// Take the watch off, with the path it had.
    fn remove(&mut self, wd: i32) -> Option<Vec<Name>> {
        let path = self.get(wd)?;
        let (_, len) = self.at.remove(&wd).expect("just read");
        self.dead += len;
        self.compact();
        Some(path)
    }

    fn wds(&self) -> impl Iterator<Item = i32> + '_ {
        self.at.keys().copied()
    }

    /// Once most of the buffer is paths no watch has, only the ones they have, in a buffer of their own.
    fn compact(&mut self) {
        if self.dead < 4096 || self.dead * 2 < self.bytes.len() {
            return;
        }
        let mut bytes = Vec::with_capacity(self.bytes.len() - self.dead);
        for (start, len) in self.at.values_mut() {
            let moved = bytes.len();
            bytes.extend_from_slice(&self.bytes[*start..*start + *len]);
            *start = moved;
        }
        self.bytes = bytes;
        self.dead = 0;
    }
}

/// A folder being walked, with the subfolders of it still to go.
struct Level {
    dir: Dir,
    path: Vec<Name>,
    mnt: u64,
    subdirs: std::vec::IntoIter<Vec<u8>>,
}

fn load_rules(root: &Dir) -> Rules {
    // A sync refuses rules it cannot read. Here the worst of no rules is some extra watches.
    root.open_dir(STATE_DIR)
        .ok()
        .and_then(|state| Rules::load(&state).ok())
        .unwrap_or_else(Rules::none)
}

impl Inner {
    /// Through the descriptor, which was opened refusing symlinks, so a directory swapped for a link
    /// a moment ago cannot put a watch somewhere else.
    fn add_watch(&mut self, dir: &Dir, path: &[Name], seen: &mut HashSet<i32>) {
        if path
            .last()
            .is_some_and(|n| cairn_failpoint::unwatchable(n.as_bytes()))
        {
            return self.missed(path, "refused by the harness, as past the kernel's limit");
        }
        let proc = format!(
            "/proc/self/fd/{}",
            rustix::fd::AsRawFd::as_raw_fd(&dir.fd())
        );
        match inotify::add_watch(&self.inotify, proc.as_str(), dir_mask()) {
            Ok(wd) => {
                // A folder watched only now may have changed while it was not.
                if self.paths.set(wd, path) {
                    crate::scope::lock(&self.marks).mark(path, false);
                }
                seen.insert(wd);
            }
            Err(Errno::NOSPC) => {
                self.counts.limit_reached = true;
                self.missed(path, "the kernel's limit on inotify watches");
            }
            Err(e) => self.missed(path, &e.to_string()),
        }
    }

    fn missed(&mut self, path: &[Name], why: &str) {
        self.counts.unwatched += 1;
        if self.missed.len() < MAX_MISSED {
            let shown = match path {
                [] => ".".to_string(),
                _ => display_path(path),
            };
            self.missed.push(format!("{shown} ({why})"));
        }
    }

    /// Watch a directory and all under it, stopping where a scan stops. Depth first, with one folder
    /// open for each level down and none for the folders waiting their turn: a folder of thousands of
    /// subfolders would otherwise hold them all open at once, past the limit on open files.
    fn walk(&mut self, top: Dir, top_path: Vec<Name>, seen: &mut HashSet<i32>) {
        let mut stack: Vec<Level> = self.enter(top, top_path, seen).into_iter().collect();
        while let Some(level) = stack.last_mut() {
            let Some(raw) = level.subdirs.next() else {
                stack.pop();
                continue;
            };
            if let Some((child, path)) = self.child(level, raw) {
                stack.extend(self.enter(child, path, seen));
            }
        }
    }

    /// Watch this folder, and list what to walk into.
    fn enter(&mut self, dir: Dir, path: Vec<Name>, seen: &mut HashSet<i32>) -> Option<Level> {
        self.add_watch(&dir, &path, seen);
        let subdirs = dir.subdirs().ok()?;
        let mnt = dir.stat_self().map_or(0, |s| s.mnt_id);
        Some(Level {
            dir,
            path,
            mnt,
            subdirs: subdirs.into_iter(),
        })
    }

    /// A subfolder to walk into, opened now, if a scan would go there too.
    fn child(&mut self, level: &Level, raw: Vec<u8>) -> Option<(Dir, Vec<Name>)> {
        if level.path.is_empty() && raw == STATE_DIR {
            return None;
        }
        let name = Name::new(raw).ok()?;
        let st = level.dir.stat(name.as_bytes()).ok()?;
        if st.file_type != FileType::Directory || (level.mnt != 0 && st.mnt_id != level.mnt) {
            return None;
        }
        let mut path = level.path.clone();
        path.push(name);
        if self.rules.excludes(&path, true) {
            return None;
        }
        match level
            .dir
            .open_dir(path.last().expect("just pushed").as_bytes())
        {
            Ok(child) => Some((child, path)),
            Err(e) => {
                self.missed(&path, &e.to_string());
                None
            }
        }
    }

    /// All of it, from the path of the root as it is now: after a remount that is another directory.
    fn walk_all(&mut self) {
        self.counts = WatchCounts::default();
        self.missed.clear();
        let mut seen = HashSet::new();
        self.state_watch = None;
        if let Ok(root) = Dir::open_root(&self.root) {
            self.rules = load_rules(&root);
            self.walk(root.dup().unwrap_or(root), Vec::new(), &mut seen);
            let state = self.root.join(OsStr::from_bytes(STATE_DIR));
            // Only for the rules file. Everything else in there is the sync's own, and moves all the time.
            let state_mask = WatchFlags::CREATE
                | WatchFlags::MOVED_TO
                | WatchFlags::MOVED_FROM
                | WatchFlags::CLOSE_WRITE
                | WatchFlags::DELETE
                | WatchFlags::ONLYDIR
                | WatchFlags::DONT_FOLLOW;
            if let Ok(wd) = inotify::add_watch(&self.inotify, state.as_path(), state_mask) {
                seen.insert(wd);
                self.state_watch = Some(wd);
            }
        }
        let stale: Vec<i32> = self.paths.wds().filter(|wd| !seen.contains(wd)).collect();
        for wd in stale {
            let _ = inotify::remove_watch(&self.inotify, wd);
            self.paths.remove(wd);
        }
        self.counts.watched = self.paths.len();
    }

    /// Watch one new directory and what is already in it.
    fn walk_new(&mut self, path: Vec<Name>) {
        let Ok(root) = Dir::open_root(&self.root) else {
            return;
        };
        let Ok(dir) = root.open_path(path.iter()) else {
            return;
        };
        let mut seen = HashSet::new();
        self.walk(dir, path, &mut seen);
        self.counts.watched = self.paths.len();
    }
}

#[derive(Default)]
struct Batch {
    wake: bool,
    walk_all: bool,
}

impl Inner {
    fn event(&mut self, wd: i32, flags: ReadFlags, name: Option<&[u8]>, batch: &mut Batch) {
        let marks = self.marks.clone();
        let mut marks = crate::scope::lock(&marks);
        if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
            // Events are gone, and with them possibly the news of new directories.
            hit("daemon.watch.overflow");
            marks.lose();
            batch.walk_all = true;
            batch.wake = true;
            return;
        }
        if Some(wd) == self.state_watch {
            if name == Some(IGNORE_FILE) {
                marks.lose();
                batch.walk_all = true;
                batch.wake = true;
            }
            return;
        }
        if flags.contains(ReadFlags::IGNORED) {
            let gone = self.paths.remove(wd);
            let was_root = gone.as_ref().is_some_and(|p| p.is_empty());
            let deleted = self.deleted.remove(&wd);
            match &gone {
                // A walk took it off, as its folder had gone from the tree.
                None => {}
                Some(p) if deleted && !p.is_empty() => marks.mark_entry(p, false),
                // A watch that ended with its folder still there: nothing more comes from it, and
                // what came before may be missing too. Watched again by a walk.
                Some(_) => {
                    marks.lose();
                    batch.walk_all = true;
                }
            }
            self.counts.watched = self.paths.len();
            batch.wake |= was_root;
            return;
        }
        let Some(path) = self.paths.get(wd) else {
            return;
        };
        if flags.intersects(ReadFlags::UNMOUNT | ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF) {
            // A moved directory's path is stale, as is everything's under it. If it was the root,
            // the sync this leads to is what tells a person.
            if path.is_empty() || flags.contains(ReadFlags::UNMOUNT) {
                marks.lose();
            } else {
                marks.mark_entry(&path, false);
            }
            if flags.contains(ReadFlags::DELETE_SELF) {
                self.deleted.insert(wd);
            }
            batch.walk_all |= flags.contains(ReadFlags::MOVE_SELF);
            batch.wake |= path.is_empty();
            return;
        }
        let Some(raw) = name else {
            // The folder itself, as for a change of its metadata.
            marks.mark_entry(&path, false);
            marks.mark(&path, false);
            batch.wake = true;
            return;
        };
        if cairn_failpoint::event_lost(raw) {
            return;
        }
        if path.is_empty() && raw == STATE_DIR {
            batch.wake = true;
            return;
        }
        let Ok(name) = Name::new(raw.to_vec()) else {
            marks.lose();
            batch.wake = true;
            return;
        };
        let is_dir = flags.contains(ReadFlags::ISDIR);
        // The folder it is in, whatever the rules say of it: a sync that lists that folder sees it as a
        // scan of the whole tree would, rules and all. A folder made or moved in is new all through.
        marks.mark(&path, false);
        if is_dir && flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO) {
            let mut at = path.clone();
            at.push(name.clone());
            marks.mark(&at, true);
        }
        drop(marks);
        let mut at = path;
        at.push(name);
        // A busy log file that is ignored must not keep the daemon scanning.
        if self.rules.excludes(&at, is_dir) {
            return;
        }
        let only_written = !flags.intersects(
            ReadFlags::CREATE
                | ReadFlags::DELETE
                | ReadFlags::MOVED_FROM
                | ReadFlags::MOVED_TO
                | ReadFlags::CLOSE_WRITE,
        );
        if only_written && !is_dir && self.in_motion.contains(&display_path(&at)) {
            return;
        }
        if is_dir {
            if flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO) {
                self.walk_new(at);
            }
            batch.walk_all |= flags.contains(ReadFlags::MOVED_FROM);
        }
        batch.wake = true;
    }
}

/// Watches a root until the process ends.
pub struct Watcher {
    inner: Arc<Mutex<Inner>>,
}

impl Watcher {
    pub fn start(root: &Path, wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        // Closed on exec, as the daemon restarts into an update in the same process: one carried over
        // would keep the old watches, a whole tree's, counted against the kernel's limit for good.
        let reader = rustix::io::fcntl_dupfd_cloexec(&fd, 0)?;
        let inner = Arc::new(Mutex::new(Inner {
            inotify: fd,
            root: root.to_path_buf(),
            rules: Rules::none(),
            paths: WatchPaths::default(),
            state_watch: None,
            counts: WatchCounts::default(),
            stopped: false,
            in_motion: HashSet::new(),
            missed: Vec::new(),
            marks: crate::scope::Shared::default(),
            deleted: HashSet::new(),
        }));
        let shared = inner.clone();
        std::thread::spawn(move || read_events(reader, shared, wake));
        Ok(Self { inner })
    }

    /// Make the watches match the tree. Watches are added before this returns, so a sync that starts
    /// afterwards misses nothing.
    pub fn walk_all(&self) {
        self.lock().walk_all();
    }

    /// The files the last sync found still being written, by their printable paths.
    pub fn set_in_motion(&self, paths: HashSet<String>) {
        self.lock().in_motion = paths;
    }

    /// The first folders that could not be watched, each with why.
    pub fn missed(&self) -> Vec<String> {
        self.lock().missed.clone()
    }

    /// Where something happened, for the daemon's syncs to list.
    pub fn marks(&self) -> crate::scope::Shared {
        self.lock().marks.clone()
    }

    pub fn counts(&self) -> WatchCounts {
        let inner = self.lock();
        WatchCounts {
            stopped: inner.stopped,
            ..inner.counts
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn read_events(fd: OwnedFd, inner: Arc<Mutex<Inner>>, wake: impl Fn()) {
    let mut buf = [MaybeUninit::<u8>::uninit(); 65536];
    // Kept across batches, since it may hold events already read from the kernel.
    let mut reader = inotify::Reader::new(&fd, &mut buf);
    let mut drained = true;
    loop {
        if drained {
            let mut fds = [PollFd::from_borrowed_fd(fd.as_fd(), PollFlags::IN)];
            let polled = poll(&mut fds, None);
            // A harness build can have the reader fail here, as one that can read no more does.
            let died = cairn_failpoint::unsupported("daemon.watch.reader");
            match polled {
                Ok(_) | Err(Errno::INTR) if !died => {}
                _ => {
                    let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
                    inner.stopped = true;
                    crate::scope::lock(&inner.marks).lose();
                    drop(inner);
                    wake();
                    return;
                }
            }
            hit("daemon.watch.read");
        }
        let mut batch = Batch::default();
        let mut count = 0;
        drained = false;
        while count < MAX_BATCH {
            match reader.next() {
                Ok(event) => {
                    let name = event.file_name().map(|n| n.to_bytes());
                    inner.lock().unwrap_or_else(|e| e.into_inner()).event(
                        event.wd(),
                        event.events(),
                        name,
                        &mut batch,
                    );
                    count += 1;
                }
                Err(Errno::INTR) => {}
                Err(_) => {
                    drained = true;
                    break;
                }
            }
        }
        if batch.walk_all {
            inner.lock().unwrap_or_else(|e| e.into_inner()).walk_all();
        }
        // Only now: every directory this batch spoke of is watched.
        if batch.wake {
            wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(names: &[&str]) -> Vec<Name> {
        names
            .iter()
            .map(|n| Name::new(n.as_bytes()).unwrap())
            .collect()
    }

    #[test]
    fn paths_come_back_as_they_were_put() {
        let mut paths = WatchPaths::default();
        assert!(paths.set(1, &[]));
        assert!(paths.set(2, &path(&["a"])));
        let odd = vec![
            Name::new(b"\xff\\x".to_vec()).unwrap(),
            Name::new(b" .b ".to_vec()).unwrap(),
        ];
        assert!(paths.set(3, &odd));
        assert_eq!(paths.get(1), Some(Vec::new()));
        assert_eq!(paths.get(2), Some(path(&["a"])));
        assert_eq!(paths.get(3), Some(odd.clone()));
        assert_eq!(paths.get(4), None);
        assert_eq!(paths.len(), 3);
        let mut wds: Vec<i32> = paths.wds().collect();
        wds.sort();
        assert_eq!(wds, [1, 2, 3]);
        assert_eq!(paths.remove(3), Some(odd));
        assert_eq!(paths.remove(3), None);
        assert_eq!(paths.get(3), None);
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn a_path_set_again_as_it_was_says_so_and_takes_nothing() {
        let mut paths = WatchPaths::default();
        assert!(paths.set(1, &path(&["a", "b"])));
        let used = paths.bytes.len();
        assert!(!paths.set(1, &path(&["a", "b"])));
        assert_eq!(paths.bytes.len(), used);
        // The same bytes cut another way, a prefix, and the root are all other paths.
        assert!(paths.set(1, &path(&["a", "b", "c"])));
        assert!(paths.set(1, &path(&["a"])));
        assert!(paths.set(1, &[]));
        assert!(!paths.set(1, &[]));
        assert!(paths.set(1, &path(&["a"])));
        assert_eq!(paths.get(1), Some(path(&["a"])));
    }

    #[test]
    fn paths_replaced_and_removed_are_given_back() {
        let mut paths = WatchPaths::default();
        let long = "n".repeat(200);
        for round in 0..50 {
            for wd in 0..100 {
                let name = format!("{long}-{round}");
                assert!(paths.set(wd, &path(&["top", &name, &wd.to_string()])));
            }
            for wd in 100..110 {
                paths.set(wd, &path(&["gone", &round.to_string()]));
                paths.remove(wd);
            }
        }
        let live: usize = (0..100).map(|wd| paths.joined(wd).unwrap().len()).sum();
        assert!(
            paths.bytes.len() <= 2 * live + 4096,
            "{} for {live}",
            paths.bytes.len()
        );
        for wd in 0..100 {
            let name = format!("{long}-49");
            assert_eq!(paths.get(wd), Some(path(&["top", &name, &wd.to_string()])));
        }
        assert_eq!(paths.len(), 100);
    }
}
