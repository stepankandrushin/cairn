//! Telling the daemon that something under a root changed. Which paths does not matter: any event
//! leads to a whole sync, whose scan is what decides what changed, so that a lost event, a missed
//! directory or a wrong path here can only delay a change, never lose one.
//!
//! inotify watches one directory at a time. The order that keeps this honest is watch first, sync
//! after: a directory is watched before the wake that follows its creation, so whatever was written
//! before the watch is found by that sync, and whatever comes after it is an event of its own.
//!
//! Nothing here holds a descriptor in the root between walks, so a disk can still be unmounted, which
//! a sync then finds out about.

use crate::fsops::Dir;
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
    paths: HashMap<i32, Vec<Name>>,
    state_watch: Option<i32>,
    counts: WatchCounts,
    stopped: bool,
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
        let proc = format!(
            "/proc/self/fd/{}",
            rustix::fd::AsRawFd::as_raw_fd(&dir.fd())
        );
        match inotify::add_watch(&self.inotify, proc.as_str(), dir_mask()) {
            Ok(wd) => {
                self.paths.insert(wd, path.to_vec());
                seen.insert(wd);
            }
            Err(Errno::NOSPC) => {
                self.counts.limit_reached = true;
                self.counts.unwatched += 1;
            }
            Err(_) => self.counts.unwatched += 1,
        }
    }

    /// Watch a directory and all under it, stopping where a scan stops.
    fn walk(&mut self, top: Dir, top_path: Vec<Name>, seen: &mut HashSet<i32>) {
        let mut stack = vec![(top, top_path)];
        while let Some((dir, path)) = stack.pop() {
            self.add_watch(&dir, &path, seen);
            let Ok(subdirs) = dir.subdirs() else {
                continue;
            };
            let mnt = dir.stat_self().map_or(0, |s| s.mnt_id);
            for raw in subdirs {
                if path.is_empty() && raw == STATE_DIR {
                    continue;
                }
                let Ok(name) = Name::new(raw) else {
                    continue;
                };
                let Ok(st) = dir.stat(name.as_bytes()) else {
                    continue;
                };
                if st.file_type != FileType::Directory || (mnt != 0 && st.mnt_id != mnt) {
                    continue;
                }
                let mut child_path = path.clone();
                child_path.push(name);
                if self.rules.excludes(&child_path, true) {
                    continue;
                }
                match dir.open_dir(child_path.last().expect("just pushed").as_bytes()) {
                    Ok(child) => stack.push((child, child_path)),
                    Err(_) => self.counts.unwatched += 1,
                }
            }
        }
    }

    /// All of it, from the path of the root as it is now: after a remount that is another directory.
    fn walk_all(&mut self) {
        self.counts = WatchCounts::default();
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
        let stale: Vec<i32> = self
            .paths
            .keys()
            .filter(|wd| !seen.contains(wd))
            .copied()
            .collect();
        for wd in stale {
            let _ = inotify::remove_watch(&self.inotify, wd);
            self.paths.remove(&wd);
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
        if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
            // Events are gone, and with them possibly the news of new directories.
            hit("daemon.watch.overflow");
            batch.walk_all = true;
            batch.wake = true;
            return;
        }
        if Some(wd) == self.state_watch {
            if name == Some(IGNORE_FILE) {
                batch.walk_all = true;
                batch.wake = true;
            }
            return;
        }
        if flags.contains(ReadFlags::IGNORED) {
            let was_root = self.paths.remove(&wd).is_some_and(|p| p.is_empty());
            self.counts.watched = self.paths.len();
            batch.wake |= was_root;
            return;
        }
        let Some(path) = self.paths.get(&wd).cloned() else {
            return;
        };
        if flags.intersects(ReadFlags::UNMOUNT | ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF) {
            // A moved directory's path is stale, as is everything's under it. If it was the root,
            // the sync this leads to is what tells a person.
            batch.walk_all |= flags.contains(ReadFlags::MOVE_SELF);
            batch.wake |= path.is_empty();
            return;
        }
        let Some(raw) = name else {
            batch.wake = true;
            return;
        };
        if path.is_empty() && raw == STATE_DIR {
            batch.wake = true;
            return;
        }
        let Ok(name) = Name::new(raw.to_vec()) else {
            batch.wake = true;
            return;
        };
        let mut at = path;
        at.push(name);
        let is_dir = flags.contains(ReadFlags::ISDIR);
        // A busy log file that is ignored must not keep the daemon scanning.
        if self.rules.excludes(&at, is_dir) {
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
        let reader = rustix::io::dup(&fd)?;
        let inner = Arc::new(Mutex::new(Inner {
            inotify: fd,
            root: root.to_path_buf(),
            rules: Rules::none(),
            paths: HashMap::new(),
            state_watch: None,
            counts: WatchCounts::default(),
            stopped: false,
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
            match poll(&mut fds, None) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(_) => {
                    inner.lock().unwrap_or_else(|e| e.into_inner()).stopped = true;
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
