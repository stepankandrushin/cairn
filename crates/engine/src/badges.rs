//! The badges a file manager shows on the entries of a root (`cairn_protocol::local::Badge`): Explorer,
//! through cairn-explorer, asks the daemon on its local API for a folder's, and is told when they change.
//!
//! Nothing here is looked up on disk but the folder listed: a badge is what the daemon knows already.
//! Changed is what the watcher has named since the last sync that agreed with the server (scope.rs keeps
//! the entries beside the folders a sync lists), and everything until the daemon's first agreement, or
//! after the watcher lost track, since until then it does not know. What the last sync skipped is an
//! error, or changing if it was still being written; metadata this filesystem refused is a warning. A
//! folder shows what is under it: changing if anything is, a warning if anything is not synced. Left out
//! is what this device does not sync: the rules, folders excluded by identity and the copies they left,
//! and the state directory, which have no badge. So a badge says no more than `cairn status` does, in
//! the place the entry is: a file shown synced has been seen synced, and not seen to change since.

use crate::fsops::{is_not_found, Dir};
use crate::scope::{split, Shared};
use crate::selection::Rules;
use cairn_protocol::local::{Badge, BadgeEntry, Badges, NotApplied, Reply};
use cairn_protocol::name::display_bytes;
use cairn_protocol::{Name, Skip};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Changes come in bursts, a save or a build: told once they have gone quiet this long, or at least
/// this often while they go on.
const SETTLE: Duration = Duration::from_millis(250);
/// More changed paths than this are told as everything.
const MAX_TOLD: usize = 4096;
/// Notices waiting for one program. One that falls this far behind is dropped, and its next connection
/// starts with everything.
const QUEUE: usize = 16;

/// What this device does not sync, as a sync read it: the rules, and the copies `cairn exclude` left
/// here, by where they were last seen.
#[derive(Clone, Default)]
pub struct Exclusions {
    pub rules: Rules,
    pub copies: Vec<String>,
}

impl std::fmt::Debug for Exclusions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exclusions")
            .field("rules", &!self.rules.is_empty())
            .field("copies", &self.copies)
            .finish()
    }
}

/// What the daemon tells the board.
#[derive(Default)]
struct Inputs {
    marks: Option<Shared>,
    /// When the scan began that the last agreement in this process rests on.
    agreed_at: Option<Instant>,
    skips: Vec<Skip>,
    not_applied: Vec<String>,
    /// The root's own trouble: the last sync failed, or the daemon is paused.
    trouble: Option<Badge>,
    exclusions: Arc<Exclusions>,
    /// Topmost paths on the server this device does not sync.
    not_synced: Vec<String>,
    /// Bumped on every change, so an index knows it is out of date.
    version: u64,
}

/// Everything's badge, as of one version of the inputs and one look at the marks.
#[derive(Default)]
struct Index {
    /// The inputs' version and the watcher's count of touches it was made from.
    version: (u64, u64),
    known: bool,
    trouble: Option<Badge>,
    /// A path's own badge, where it is not simply synced.
    own: HashMap<String, Badge>,
    /// For each folder above something changing or not synced: whether anything under it is changing,
    /// and whether anything is not synced.
    below: HashMap<String, (bool, bool)>,
    /// Folders new all through, and with entries overflowed, every folder marked, whose entries all count
    /// as changing.
    whole: HashSet<String>,
    marked: HashSet<String>,
    exclusions: Arc<Exclusions>,
    left_out: HashSet<String>,
}

/// The parent of a printed path, `""` for an entry of the root; `None` for the root itself.
fn parent(path: &str) -> Option<&str> {
    if path.is_empty() {
        return None;
    }
    Some(path.rsplit_once('/').map_or("", |(p, _)| p))
}

/// Every folder above a printed path, nearest first, the root last.
fn above(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(parent(path), |p| parent(p))
}

/// A printed path and every path it is under, itself first.
fn and_above(path: &str) -> impl Iterator<Item = &str> {
    std::iter::once(path).chain(above(path).filter(|p| !p.is_empty()))
}

fn join_printed(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.to_string()
    } else {
        format!("{folder}/{name}")
    }
}

/// The names of a path printed with `/`, as the local API takes it. A name with escapes in it cannot
/// be taken back from its print: it is the bytes printed, which no file has, so it lists nothing.
pub fn names_of(path: &str) -> Option<Vec<Name>> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| match s {
            "." | ".." => None,
            _ => Name::new(s.as_bytes().to_vec()).ok(),
        })
        .collect()
}

impl Index {
    /// Whether this device leaves this out. `is_dir` is what the entry is, where that is known; each
    /// name above it is a folder.
    fn leaves_out(&self, names: &[Name], is_dir: Option<bool>) -> bool {
        if names
            .first()
            .is_some_and(|n| n.as_bytes() == cairn_protocol::STATE_DIR)
        {
            return true;
        }
        let rules = &self.exclusions.rules;
        if !rules.is_empty() {
            for i in 1..=names.len() {
                let p = &names[..i];
                let out = match (i < names.len(), is_dir) {
                    (true, _) => rules.excludes(p, true),
                    (false, Some(d)) => rules.excludes(p, d),
                    (false, None) => rules.excludes(p, false) || rules.excludes(p, true),
                };
                if out {
                    return true;
                }
            }
        }
        if self.left_out.is_empty() {
            return false;
        }
        let printed = crate::scan::display_path(names);
        let out = and_above(&printed).any(|p| self.left_out.contains(p));
        out
    }

    fn badge(&self, path: &str, is_dir: bool) -> Badge {
        let own = self.own.get(path).copied();
        let trouble = if path.is_empty() { self.trouble } else { None };
        if own == Some(Badge::Error) || trouble == Some(Badge::Error) {
            return Badge::Error;
        }
        let (changing, failing) = match is_dir {
            true => self.below.get(path).copied().unwrap_or_default(),
            false => (false, false),
        };
        let in_whole = and_above(path).any(|p| self.whole.contains(p));
        let in_marked = parent(path).is_some_and(|p| self.marked.contains(p))
            || (is_dir && self.marked.contains(path));
        if !self.known || own == Some(Badge::Syncing) || changing || in_whole || in_marked {
            return Badge::Syncing;
        }
        if own == Some(Badge::Warning) || failing || trouble == Some(Badge::Warning) {
            return Badge::Warning;
        }
        Badge::Synced
    }

    /// The paths whose badges may differ between two indexes, or `None` for all of them.
    fn differences(&self, other: &Index) -> Option<Vec<String>> {
        if self.known != other.known
            || !Arc::ptr_eq(&self.exclusions, &other.exclusions)
            || self.left_out != other.left_out
        {
            return None;
        }
        let mut out: HashSet<&str> = HashSet::new();
        // The root's own, whatever is under it.
        if self.trouble != other.trouble {
            out.insert("");
        }
        for (a, b) in [(self, other), (other, self)] {
            for (k, v) in &a.own {
                if b.own.get(k) != Some(v) {
                    out.insert(k);
                }
            }
            for (k, v) in &a.below {
                if b.below.get(k) != Some(v) {
                    out.insert(k);
                }
            }
            for k in a.whole.difference(&b.whole) {
                out.insert(k);
            }
            // Every entry of a folder marked counts, and the folder itself.
            for k in a.marked.difference(&b.marked) {
                out.insert(k);
            }
            if out.len() > MAX_TOLD {
                return None;
            }
        }
        let mut paths: Vec<String> = out.into_iter().map(str::to_string).collect();
        paths.sort();
        Some(paths)
    }
}

/// The daemon's side: what it knows, an index of it, and who is told when it changes.
pub struct Board {
    root: PathBuf,
    inputs: Mutex<Inputs>,
    index: Mutex<Arc<Index>>,
    watchers: Mutex<Vec<SyncSender<Arc<str>>>>,
    /// How many times the watcher has marked something.
    touches: AtomicU64,
    /// When something last changed that the teller has not told yet.
    touched: Mutex<Option<Instant>>,
    wake: Condvar,
    teller: std::sync::Once,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn line(reply: &Reply) -> Arc<str> {
    let mut s = serde_json::to_string(reply).unwrap_or_default();
    s.push('\n');
    s.into()
}

impl Board {
    pub fn new(root: &Path) -> Arc<Self> {
        Arc::new(Self {
            root: root.to_path_buf(),
            inputs: Mutex::new(Inputs {
                version: 1,
                ..Inputs::default()
            }),
            index: Mutex::new(Arc::new(Index::default())),
            watchers: Mutex::new(Vec::new()),
            touches: AtomicU64::new(0),
            touched: Mutex::new(None),
            wake: Condvar::new(),
            teller: std::sync::Once::new(),
        })
    }

    fn change(&self, f: impl FnOnce(&mut Inputs)) {
        {
            let mut inputs = lock(&self.inputs);
            f(&mut inputs);
            inputs.version += 1;
        }
        self.touch();
    }

    /// The watcher's marks, once it runs.
    pub fn watch_marks(&self, marks: Shared) {
        self.change(|i| i.marks = Some(marks));
    }

    /// The watcher has marked something: from its thread, so it only says so.
    pub fn touch(&self) {
        self.touches.fetch_add(1, Ordering::SeqCst);
        *lock(&self.touched) = Some(Instant::now());
        self.wake.notify_all();
    }

    /// A sync ended: what it agreed from, if it did, what it skipped, and what it left out. Marks from
    /// before its agreement are pruned by then.
    pub fn synced(&self, report: &crate::sync::SyncReport, not_applied: &[NotApplied]) {
        self.change(|i| {
            if report.agreed_since.is_some() {
                i.agreed_at = report.agreed_since;
            }
            i.skips.clone_from(&report.skips);
            i.not_applied = not_applied.iter().map(|n| n.path.clone()).collect();
            // Kept as they were when they read the same, so a sync that changes none of it tells
            // nobody to look at everything again.
            if let Some(e) = &report.exclusions {
                let same = e.rules.text_hash() == i.exclusions.rules.text_hash()
                    && e.copies == i.exclusions.copies;
                if !same {
                    i.exclusions = e.clone();
                }
                i.not_synced.clone_from(&report.not_synced_here);
            }
        });
    }

    /// The daemon's state, as its status says it: the root's own badge when it is in trouble.
    pub fn set_state(&self, state: &str) {
        let trouble = match state {
            "error" => Some(Badge::Error),
            "paused" => Some(Badge::Warning),
            _ => None,
        };
        if lock(&self.inputs).trouble != trouble {
            self.change(|i| i.trouble = trouble);
        }
    }

    /// The index as things are now, made again if anything has changed.
    fn index(&self) -> Arc<Index> {
        let mut index = lock(&self.index);
        let inputs = lock(&self.inputs);
        let version = (inputs.version, self.touches.load(Ordering::SeqCst));
        if index.version == version {
            return index.clone();
        }
        let built = Arc::new(build(&inputs, version));
        drop(inputs);
        *index = built.clone();
        built
    }

    /// One folder's badges: the local API's [`cairn_protocol::local::Request::Badges`].
    pub fn badges(&self, folder: &str) -> Reply {
        let Some(names) = names_of(folder) else {
            return Reply::Error {
                text: format!("{folder:?} is not a path in this root"),
            };
        };
        let index = self.index();
        let printed = crate::scan::display_path(&names);
        let mut out = Badges {
            folder: folder.to_string(),
            ..Badges::default()
        };
        if index.leaves_out(&names, Some(true)) {
            return Reply::Badges(Box::new(out));
        }
        let listed = Dir::open_root(&self.root).and_then(|root| {
            let dir = root.open_path(names.iter())?;
            Ok((dir.list()?, dir.subdirs()?))
        });
        let (list, subdirs) = match listed {
            Ok(l) => l,
            Err(e) if is_not_found(&e) || e.raw_os_error() == Some(NOT_A_DIRECTORY) => {
                return Reply::Badges(Box::new(out));
            }
            Err(e) => {
                return Reply::Error {
                    text: format!("cannot list {printed:?}: {e}"),
                }
            }
        };
        out.badge = Some(index.badge(&printed, true));
        let subdirs: HashSet<Vec<u8>> = subdirs.into_iter().collect();
        let mut at = names;
        for raw in list {
            let Ok(name) = Name::new(raw.clone()) else {
                continue;
            };
            let is_dir = subdirs.contains(&raw);
            at.push(name);
            if !index.leaves_out(&at, Some(is_dir)) {
                let shown = display_bytes(&raw);
                let badge = index.badge(&join_printed(&printed, &shown), is_dir);
                out.entries.push(BadgeEntry { name: shown, badge });
            }
            at.pop();
        }
        Reply::Badges(Box::new(out))
    }

    /// A program to be told of changes: the line it is sent first, everything, and the rest to come.
    pub fn watch(self: &Arc<Self>) -> (Arc<str>, Receiver<Arc<str>>) {
        let board = self.clone();
        self.teller.call_once(|| {
            std::thread::spawn(move || board.tell());
        });
        let (tx, rx) = sync_channel(QUEUE);
        lock(&self.watchers).push(tx);
        let first = line(&Reply::BadgesChanged {
            all: true,
            paths: Vec::new(),
        });
        (first, rx)
    }

    /// Tells each program watching what changed, once things settle: a thread of its own, which sleeps
    /// while nothing does.
    fn tell(self: Arc<Self>) {
        let mut told = self.index();
        loop {
            {
                let mut touched = lock(&self.touched);
                while touched.is_none() {
                    touched = self.wake.wait(touched).unwrap_or_else(|e| e.into_inner());
                }
            }
            // Until it has been quiet a moment, or a burst has gone on long enough to tell it so far.
            let first = Instant::now();
            loop {
                std::thread::sleep(SETTLE);
                let quiet = lock(&self.touched).is_none_or(|t| t.elapsed() >= SETTLE);
                if quiet || first.elapsed() >= 4 * SETTLE {
                    break;
                }
            }
            *lock(&self.touched) = None;
            let now = self.index();
            let reply = match now.differences(&told) {
                Some(paths) if paths.is_empty() => continue,
                Some(paths) => Reply::BadgesChanged { all: false, paths },
                None => Reply::BadgesChanged {
                    all: true,
                    paths: Vec::new(),
                },
            };
            told = now;
            let line = line(&reply);
            lock(&self.watchers).retain(|w| match w.try_send(line.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
            });
        }
    }
}

/// What a listing that reaches a file says on each platform.
#[cfg(unix)]
const NOT_A_DIRECTORY: i32 = 20;
#[cfg(windows)]
const NOT_A_DIRECTORY: i32 = 267;

fn build(inputs: &Inputs, version: (u64, u64)) -> Index {
    let mut index = Index {
        version,
        known: inputs.agreed_at.is_some(),
        trouble: inputs.trouble,
        exclusions: inputs.exclusions.clone(),
        left_out: inputs
            .exclusions
            .copies
            .iter()
            .chain(&inputs.not_synced)
            .cloned()
            .collect(),
        ..Index::default()
    };
    // Copied out, so the watcher waits on none of what follows.
    let (entries, whole, marked) = match &inputs.marks {
        Some(m) => {
            let marks = crate::scope::lock(m);
            if inputs.agreed_at.is_some_and(|t| marks.lost_since(t)) {
                index.known = false;
            }
            let changed = marks.changed();
            let own = |v: Vec<&[u8]>| -> Vec<Box<[u8]>> { v.into_iter().map(Box::from).collect() };
            (
                own(changed.entries),
                own(changed.whole),
                changed.folders.map(own).unwrap_or_default(),
            )
        }
        None => Default::default(),
    };
    if !index.known {
        // Everything is changing: nothing more to say but what is not synced.
        index.whole.insert(String::new());
    }
    let changing = |index: &mut Index, path: String| {
        for p in above(&path) {
            index.below.entry(p.to_string()).or_default().0 = true;
        }
        index.own.entry(path).or_insert(Badge::Syncing);
    };
    for joined in entries.iter().chain(whole.iter()) {
        let names = split(joined);
        if names.is_empty() || index.leaves_out(&names, None) {
            continue;
        }
        changing(&mut index, display_bytes(joined));
    }
    for joined in &whole {
        let names = split(joined);
        if !index.leaves_out(&names, Some(true)) {
            index.whole.insert(display_bytes(joined));
        }
    }
    for joined in &marked {
        let names = split(joined);
        if !index.leaves_out(&names, Some(true)) {
            let printed = display_bytes(joined);
            for p in above(&printed) {
                index.below.entry(p.to_string()).or_default().0 = true;
            }
            index.below.entry(printed.clone()).or_default().0 = true;
            index.marked.insert(printed);
        }
    }
    // What is not synced, after what is changing: a file skipped is that, however it changed.
    let failing = |index: &mut Index, path: &str, badge: Badge| {
        for p in above(path) {
            let b = index.below.entry(p.to_string()).or_default();
            match badge {
                Badge::Syncing => b.0 = true,
                _ => b.1 = true,
            }
        }
        let own = index.own.entry(path.to_string()).or_insert(badge);
        // The worse of two: an error over a warning over changing.
        let rank = |b: Badge| match b {
            Badge::Error => 3,
            Badge::Warning => 2,
            Badge::Syncing => 1,
            _ => 0,
        };
        if rank(badge) > rank(*own) {
            *own = badge;
        }
    };
    for s in &inputs.skips {
        let badge = match s.kind.in_motion() {
            true => Badge::Syncing,
            false => Badge::Error,
        };
        failing(&mut index, &s.path, badge);
    }
    for p in &inputs.not_applied {
        failing(&mut index, p, Badge::Warning);
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::SkipKind;

    fn names(p: &str) -> Vec<Name> {
        names_of(p).unwrap()
    }

    fn board_with(marks: &Shared) -> Inputs {
        Inputs {
            marks: Some(marks.clone()),
            agreed_at: Some(Instant::now()),
            version: 1,
            ..Inputs::default()
        }
    }

    #[test]
    fn paths_above_are_every_folder_up_to_the_root() {
        assert_eq!(above("a/b/c").collect::<Vec<_>>(), ["a/b", "a", ""]);
        assert_eq!(above("a").collect::<Vec<_>>(), [""]);
        assert_eq!(above("").count(), 0);
        assert_eq!(and_above("a/b").collect::<Vec<_>>(), ["a/b", "a"]);
        assert_eq!(names_of("a//b/"), Some(names("a/b")));
        assert_eq!(names_of("a/../b"), None);
    }

    #[test]
    fn a_changed_file_shows_changing_up_to_the_root_and_nothing_else_does() {
        let marks = Shared::default();
        let mut inputs = board_with(&marks);
        std::thread::sleep(Duration::from_millis(2));
        crate::scope::lock(&marks).mark_entry(&names("a/b/f.txt"), false);
        let index = build(&inputs, (0, 0));
        assert_eq!(index.badge("a/b/f.txt", false), Badge::Syncing);
        assert_eq!(index.badge("a/b/g.txt", false), Badge::Synced);
        assert_eq!(index.badge("a/b", true), Badge::Syncing);
        assert_eq!(index.badge("a", true), Badge::Syncing);
        assert_eq!(index.badge("", true), Badge::Syncing);
        assert_eq!(index.badge("c", true), Badge::Synced);
        // Synced: the marks before the agreement go, and with them what showed.
        let t = Instant::now();
        crate::scope::lock(&marks).prune(t);
        inputs.agreed_at = Some(t);
        inputs.version += 1;
        let after = build(&inputs, (0, 0));
        assert_eq!(after.badge("a/b/f.txt", false), Badge::Synced);
        assert_eq!(after.badge("", true), Badge::Synced);
        let told = after.differences(&index).unwrap();
        assert_eq!(told, ["", "a", "a/b", "a/b/f.txt"]);
    }

    #[test]
    fn before_the_first_agreement_or_after_a_loss_everything_is_changing() {
        let marks = Shared::default();
        let mut inputs = board_with(&marks);
        inputs.agreed_at = None;
        let index = build(&inputs, (0, 0));
        assert_eq!(index.badge("x/y", false), Badge::Syncing);
        assert_eq!(index.badge("", true), Badge::Syncing);
        inputs.agreed_at = Some(Instant::now());
        assert_eq!(build(&inputs, (0, 0)).badge("x/y", false), Badge::Synced);
        crate::scope::lock(&marks).lose();
        let lost = build(&inputs, (0, 0));
        assert_eq!(lost.badge("x/y", false), Badge::Syncing);
        assert_eq!(
            lost.differences(&build(&board_with(&Shared::default()), (0, 0))),
            None
        );
    }

    #[test]
    fn a_skipped_file_is_an_error_and_each_folder_above_it_a_warning() {
        let marks = Shared::default();
        let mut inputs = board_with(&marks);
        let skip = |path: &str, kind| Skip {
            path: path.into(),
            kind,
            detail: None,
        };
        inputs.skips = vec![
            skip("a/locked.db", SkipKind::PermissionDenied),
            skip("b/log.txt", SkipKind::KeepsChanging),
        ];
        inputs.not_applied = vec!["c/owned".into()];
        let index = build(&inputs, (0, 0));
        assert_eq!(index.badge("a/locked.db", false), Badge::Error);
        assert_eq!(index.badge("a", true), Badge::Warning);
        assert_eq!(index.badge("a/other", false), Badge::Synced);
        assert_eq!(index.badge("b/log.txt", false), Badge::Syncing);
        assert_eq!(index.badge("b", true), Badge::Syncing);
        assert_eq!(index.badge("c/owned", false), Badge::Warning);
        assert_eq!(index.badge("c", true), Badge::Warning);
        // Changing outranks a warning below, never an error of its own.
        assert_eq!(index.badge("", true), Badge::Syncing);
        crate::scope::lock(&marks).mark_entry(&names("a/locked.db"), false);
        let changed = build(&inputs, (0, 0));
        assert_eq!(changed.badge("a/locked.db", false), Badge::Error);
        assert_eq!(changed.badge("a", true), Badge::Syncing);
        // The root shows a failed sync or a pause, whatever is under it.
        let mut inputs = board_with(&Shared::default());
        inputs.trouble = Some(Badge::Error);
        assert_eq!(build(&inputs, (0, 0)).badge("", true), Badge::Error);
        inputs.trouble = Some(Badge::Warning);
        let paused = build(&inputs, (0, 0));
        assert_eq!(paused.badge("", true), Badge::Warning);
        assert_eq!(paused.badge("a", true), Badge::Synced);
    }

    #[test]
    fn what_this_device_leaves_out_has_no_badge_and_its_changes_show_nowhere() {
        let marks = Shared::default();
        let mut inputs = board_with(&marks);
        inputs.exclusions = Arc::new(Exclusions {
            rules: Rules::parse(b"*.log\nbuild/\n").unwrap(),
            copies: vec!["kept/old".into()],
        });
        inputs.not_synced = vec!["big".into()];
        {
            let mut m = crate::scope::lock(&marks);
            m.mark_entry(&names("src/app.log"), false);
            m.mark_entry(&names("src/build/out.o"), false);
            m.mark_entry(&names("kept/old/x"), false);
            m.mark_entry(&names("big/y"), false);
            m.mark_entry(&names(".cairn/state"), false);
        }
        let index = build(&inputs, (0, 0));
        for (p, dir) in [
            ("src/app.log", Some(false)),
            ("src/build", Some(true)),
            ("src/build/out.o", Some(false)),
            ("kept/old", Some(true)),
            ("kept/old/x", None),
            ("big", Some(true)),
            (".cairn", Some(true)),
        ] {
            assert!(index.leaves_out(&names(p), dir), "{p}");
        }
        assert!(!index.leaves_out(&names("src/build"), Some(false)));
        assert!(!index.leaves_out(&names("kept/other"), Some(true)));
        assert_eq!(index.badge("src", true), Badge::Synced);
        assert_eq!(index.badge("kept", true), Badge::Synced);
        assert_eq!(index.badge("", true), Badge::Synced);
    }

    #[test]
    fn a_folder_new_all_through_is_changing_all_through() {
        let marks = Shared::default();
        let inputs = board_with(&marks);
        crate::scope::lock(&marks).mark_entry(&names("new"), true);
        let index = build(&inputs, (0, 0));
        assert_eq!(index.badge("new", true), Badge::Syncing);
        assert_eq!(index.badge("new/deep/file", false), Badge::Syncing);
        assert_eq!(index.badge("old", true), Badge::Synced);
    }

    #[test]
    fn past_the_entries_kept_every_entry_of_a_marked_folder_is_changing() {
        let marks = Shared::default();
        let inputs = board_with(&marks);
        {
            let mut m = crate::scope::lock(&marks);
            for i in 0..crate::scope::MAX_ENTRIES {
                m.entry(&names(&format!("gen/{i}")));
            }
            m.mark_entry(&names("other/one-too-many"), false);
        }
        let index = build(&inputs, (0, 0));
        assert_eq!(index.badge("other/anything", false), Badge::Syncing);
        assert_eq!(index.badge("other", true), Badge::Syncing);
        assert_eq!(index.badge("gen/1", false), Badge::Syncing);
        assert_eq!(index.badge("quiet/file", false), Badge::Synced);
    }
}
