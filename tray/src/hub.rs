//! What the tray knows of each root, kept current by a thread per daemon and one for the registry.

use crate::link::{self, LinkError};
use crate::model::now_secs;
use cairn_protocol::local::{
    registry_path, Activity, Change, DaemonStatus, DeletedItem, OpenConflict, Registry, Reply,
    Request, RootEntry, Secs,
};
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How often the registry is read again, for a root added or removed.
const REGISTRY_EVERY: Duration = Duration::from_secs(3);
/// The longest wait before trying a daemon that did not answer again.
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// A daemon reads the server's history to answer [`Request::Deleted`], all of it the first time: as
/// long as that takes.
pub const DELETED_WAIT: Duration = Duration::from_secs(600);
/// The soonest Recently deleted is asked for again after a delete. The daemon keeps what it read for 10
/// minutes after the last request, so one a minute during a run of deletes takes only what is new.
const DELETED_EVERY: Secs = 60;
/// The soonest a list that could not be had is asked for again, while the server answers the daemon.
const RETRY_EVERY: Secs = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Up(Box<DaemonStatus>),
    Down { since: Secs, why: LinkError },
}

#[derive(Clone, Debug)]
pub struct RootView {
    pub entry: RootEntry,
    pub link: Link,
    /// The last answer to [`Request::Conflicts`], and when it came.
    pub conflicts: Option<(Secs, Result<Vec<OpenConflict>, String>)>,
    /// The last answer to [`Request::Deleted`] for the days it takes when none are said, and when it
    /// came.
    pub deleted: Option<(Secs, Result<Vec<DeletedItem>, String>)>,
    conflicts_asked: Asked,
    deleted_asked: Asked,
    /// Counts the subscriptions its daemon answered, so a daemon that starts again is asked again.
    link_epoch: u64,
    alive: Arc<AtomicBool>,
}

/// When a list the daemon's status does not carry was last asked for, to tell when it is due again.
#[derive(Clone, Debug, Default)]
struct Asked {
    /// The subscription it was asked on; 0 before it ever was.
    epoch: u64,
    /// The daemon's newest activity then, `None` if it had none: what is listed above it came after.
    newest: Option<Activity>,
    /// When it was asked.
    at: Secs,
    /// It is being asked now.
    asking: bool,
    /// What the window did may have changed it, so it is asked for again.
    again: bool,
}

/// The daemon's activity since `newest` was its newest, newest first: all it lists if `newest` is no
/// longer among them, or was `None`.
fn since<'a>(recent: &'a [Activity], newest: Option<&Activity>) -> &'a [Activity] {
    match newest.and_then(|n| recent.iter().position(|a| a == n)) {
        Some(i) => &recent[..i],
        None => recent,
    }
}

/// `path` is `at` or inside it, both relative to the root as the daemon writes them.
fn at_or_under(path: &str, at: &str) -> bool {
    path.strip_prefix(at)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Which list a daemon is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum List {
    Conflicts,
    Deleted,
}

impl RootView {
    fn asked(&self, list: List) -> &Asked {
        match list {
            List::Conflicts => &self.conflicts_asked,
            List::Deleted => &self.deleted_asked,
        }
    }

    fn asked_mut(&mut self, list: List) -> &mut Asked {
        match list {
            List::Conflicts => &mut self.conflicts_asked,
            List::Deleted => &mut self.deleted_asked,
        }
    }

    fn failed(&self, list: List) -> bool {
        match list {
            List::Conflicts => matches!(self.conflicts, Some((_, Err(_)))),
            List::Deleted => matches!(self.deleted, Some((_, Err(_)))),
        }
    }

    /// Whether `list` is to be asked for now: once each time a daemon answers, as one that started
    /// again knows what happened meanwhile; again when its activity says the list changed, a
    /// conflict made or a copy dealt with, a delete; again when the window did something to it; and
    /// again a while after it could not be had. Never on a timer alone: the daemon has the server
    /// replay its whole journal to answer, but for no open conflicts at all, which one request says.
    fn due(&self, list: List, now: Secs) -> bool {
        let Link::Up(s) = &self.link else {
            return false;
        };
        let asked = self.asked(list);
        if asked.asking {
            return false;
        }
        if asked.epoch != self.link_epoch || asked.again {
            return true;
        }
        if self.failed(list) && s.feed_down.is_none() && now - asked.at >= RETRY_EVERY {
            return true;
        }
        let new = since(&s.recent, asked.newest.as_ref());
        match list {
            List::Conflicts => {
                let open: Vec<&str> = match &self.conflicts {
                    Some((_, Ok(open))) => open
                        .iter()
                        .flat_map(|c| {
                            [Some(c.copy_display.as_str()), c.original_display.as_deref()]
                        })
                        .flatten()
                        .collect(),
                    _ => Vec::new(),
                };
                new.iter().any(|a| match a.change {
                    Change::Conflict => true,
                    // Deleting, renaming or moving the copy is how one deals with it.
                    Change::Removed | Change::Moved => open.iter().any(|p| at_or_under(p, &a.path)),
                    _ => false,
                })
            }
            List::Deleted => {
                now - asked.at >= DELETED_EVERY && new.iter().any(|a| a.change == Change::Removed)
            }
        }
    }
}

impl RootView {
    /// A view outside the hub's threads, for `--probe`.
    pub fn detached(
        entry: RootEntry,
        link: Link,
        conflicts: Option<Result<Vec<OpenConflict>, String>>,
    ) -> RootView {
        RootView {
            entry,
            link,
            conflicts: conflicts.map(|c| (now_secs(), c)),
            deleted: None,
            conflicts_asked: Asked::default(),
            deleted_asked: Asked::default(),
            link_epoch: 0,
            alive: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct State {
    /// Bumped on every change, for whoever waits on one.
    pub version: u64,
    pub registry: Option<PathBuf>,
    pub registry_error: Option<String>,
    pub roots: Vec<RootView>,
    /// A window tab asked for from the menu, numbered so the page sees each request once.
    pub tab: (u64, String),
    /// Something a person should read: an action from the menu that failed, say. Numbered as `tab`.
    pub notice: (u64, String),
}

#[derive(Default)]
pub struct Hub {
    state: Mutex<State>,
    changed: Condvar,
}

impl Hub {
    pub fn start() -> Arc<Hub> {
        let hub = Arc::new(Hub::default());
        let h = hub.clone();
        std::thread::spawn(move || h.watch_registry());
        let h = hub.clone();
        std::thread::spawn(move || h.poll_lists());
        hub
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> State {
        self.lock().clone()
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        let mut s = self.lock();
        f(&mut s);
        s.version += 1;
        self.changed.notify_all();
    }

    /// Blocks until the state is past `version`, or `timeout` passes; the version now.
    pub fn wait_past(&self, version: u64, timeout: Duration) -> u64 {
        let s = self.lock();
        let (s, _) = self
            .changed
            .wait_timeout_while(s, timeout, |s| s.version <= version)
            .unwrap_or_else(|e| e.into_inner());
        s.version
    }

    pub fn notice(&self, text: impl Into<String>) {
        let text = text.into();
        self.update(|s| s.notice = (s.notice.0 + 1, text));
    }

    pub fn ask_tab(&self, tab: &str) {
        self.update(|s| s.tab = (s.tab.0 + 1, tab.to_string()));
    }

    pub fn root(&self, root_id: &str) -> Option<RootView> {
        self.lock()
            .roots
            .iter()
            .find(|r| r.entry.root_id == root_id)
            .cloned()
    }

    fn with_root(&self, root_id: &str, f: impl FnOnce(&mut RootView)) {
        self.update(|s| {
            if let Some(r) = s.roots.iter_mut().find(|r| r.entry.root_id == root_id) {
                f(r);
            }
        });
    }

    fn watch_registry(self: Arc<Self>) {
        loop {
            let path = registry_path();
            let read = match &path {
                None => Err("no home directory to find the registry in".to_string()),
                Some(p) => read_registry(p),
            };
            let mut start = Vec::new();
            {
                let mut s = self.lock();
                let mut changed = s.registry != path;
                s.registry = path.clone();
                match read {
                    Err(e) => {
                        changed |= s.registry_error.as_ref() != Some(&e);
                        s.registry_error = Some(e);
                    }
                    Ok(reg) => {
                        changed |= s.registry_error.take().is_some();
                        for gone in s
                            .roots
                            .iter()
                            .filter(|r| !reg.roots.iter().any(|e| e.root_id == r.entry.root_id))
                        {
                            gone.alive.store(false, Ordering::Relaxed);
                        }
                        let before = s.roots.len();
                        s.roots
                            .retain(|r| reg.roots.iter().any(|e| e.root_id == r.entry.root_id));
                        changed |= s.roots.len() != before;
                        for entry in reg.roots {
                            match s
                                .roots
                                .iter_mut()
                                .find(|r| r.entry.root_id == entry.root_id)
                            {
                                Some(r) => {
                                    if r.entry != entry {
                                        r.entry = entry;
                                        changed = true;
                                    }
                                }
                                None => {
                                    let alive = Arc::new(AtomicBool::new(true));
                                    start.push((entry.root_id.clone(), alive.clone()));
                                    s.roots.push(RootView {
                                        entry,
                                        link: Link::Connecting,
                                        conflicts: None,
                                        deleted: None,
                                        conflicts_asked: Asked::default(),
                                        deleted_asked: Asked::default(),
                                        link_epoch: 0,
                                        alive,
                                    });
                                    changed = true;
                                }
                            }
                        }
                    }
                }
                if changed {
                    s.version += 1;
                    self.changed.notify_all();
                }
            }
            for (root_id, alive) in start {
                let h = self.clone();
                std::thread::spawn(move || h.follow(root_id, alive));
            }
            std::thread::sleep(REGISTRY_EVERY);
        }
    }

    /// Subscribes to one daemon for as long as its root is in the registry, again after each drop.
    fn follow(self: Arc<Self>, root_id: String, alive: Arc<AtomicBool>) {
        let mut backoff = Duration::from_secs(1);
        while alive.load(Ordering::Relaxed) {
            let why = match self.subscribe(&root_id, &alive) {
                Ok(()) => {
                    backoff = Duration::from_secs(1);
                    LinkError::NotRunning("the daemon stopped".into())
                }
                Err(e) => e,
            };
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            self.with_root(&root_id, |r| {
                r.link = match &r.link {
                    Link::Down { since, why: old } if *old == why => {
                        Link::Down { since: *since, why }
                    }
                    _ => Link::Down {
                        since: now_secs(),
                        why,
                    },
                }
            });
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Ok once a daemon that answered has closed; an error if none did.
    fn subscribe(&self, root_id: &str, alive: &AtomicBool) -> Result<(), LinkError> {
        let mut stream = link::connect(root_id)?;
        link::send(&mut stream, &Request::Subscribe)
            .map_err(|e| LinkError::Other(format!("cannot ask the daemon: {e}")))?;
        let mut reader = BufReader::new(stream);
        let mut answered = false;
        loop {
            match link::read_reply(&mut reader) {
                Ok(Some(Reply::Status(status))) => {
                    let first = !answered;
                    answered = true;
                    if !alive.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    self.with_root(root_id, |r| {
                        r.link = Link::Up(status);
                        if first {
                            r.link_epoch += 1;
                        }
                    });
                }
                Ok(Some(Reply::Error { text })) => return Err(LinkError::Other(text)),
                Ok(Some(_)) => {}
                Ok(None) if answered => return Ok(()),
                Ok(None) => {
                    return Err(LinkError::Other(
                        "the daemon closed without saying anything".into(),
                    ))
                }
                Err(e) if answered => return Err(LinkError::NotRunning(e)),
                Err(e) => return Err(LinkError::Other(e)),
            }
        }
    }

    /// Asks a daemon for its open conflicts, and keeps the answer.
    pub fn fetch_conflicts(&self, root_id: &str) -> Result<Vec<OpenConflict>, String> {
        let r = match link::ask(root_id, Request::Conflicts, Duration::from_secs(60)) {
            Ok(Reply::Conflicts { open }) => Ok(open),
            Ok(Reply::Error { text }) => Err(text),
            Ok(other) => Err(format!("the daemon answered {other:?}")),
            Err(e) => Err(e.to_string()),
        };
        let kept = r.clone();
        self.with_root(root_id, |v| v.conflicts = Some((now_secs(), kept)));
        r
    }

    /// Asks a daemon what was deleted in the last [`DEFAULT_DELETED_DAYS`] days, and keeps the answer.
    ///
    /// [`DEFAULT_DELETED_DAYS`]: cairn_protocol::local::DEFAULT_DELETED_DAYS
    pub fn fetch_deleted(&self, root_id: &str) -> Result<Vec<DeletedItem>, String> {
        let r = match link::ask(root_id, Request::Deleted { days: None }, DELETED_WAIT) {
            Ok(Reply::Deleted { items, .. }) => Ok(items),
            Ok(Reply::Error { text }) => Err(text),
            Ok(other) => Err(format!("the daemon answered {other:?}")),
            Err(e) => Err(e.to_string()),
        };
        self.keep_deleted(root_id, r.clone());
        r
    }

    /// Keeps what the window was told was deleted, when it asked for the days the menu shows.
    pub fn keep_deleted(&self, root_id: &str, r: Result<Vec<DeletedItem>, String>) {
        self.with_root(root_id, |v| v.deleted = Some((now_secs(), r)));
    }

    /// The window brought something back, so Recently deleted is asked for again.
    pub fn deleted_changed(&self, root_id: &str) {
        self.with_root(root_id, |v| v.deleted_asked.again = true);
    }

    /// Asks each running daemon for its conflicts and what was deleted when they are due
    /// ([`RootView::due`]), each on a thread of its own, as a first Recently deleted can take a minute.
    /// The window asks too, when its tab is opened, and what it is told is kept for the menu.
    fn poll_lists(self: Arc<Self>) {
        loop {
            let now = now_secs();
            let due: Vec<(String, List)> = {
                let mut s = self.lock();
                let mut due = Vec::new();
                for r in s.roots.iter_mut() {
                    for list in [List::Conflicts, List::Deleted] {
                        if !r.due(list, now) {
                            continue;
                        }
                        let Link::Up(st) = &r.link else { continue };
                        let newest = st.recent.first().cloned();
                        let epoch = r.link_epoch;
                        let a = r.asked_mut(list);
                        *a = Asked {
                            epoch,
                            newest,
                            at: now,
                            asking: true,
                            again: false,
                        };
                        due.push((r.entry.root_id.clone(), list));
                    }
                }
                due
            };
            for (root_id, list) in due {
                let h = self.clone();
                std::thread::spawn(move || {
                    match list {
                        List::Conflicts => drop(h.fetch_conflicts(&root_id)),
                        List::Deleted => drop(h.fetch_deleted(&root_id)),
                    }
                    h.with_root(&root_id, |r| r.asked_mut(list).asking = false);
                });
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

pub fn read_registry(path: &std::path::Path) -> Result<Registry, String> {
    let bytes = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "no roots on this device yet: {} is not there",
                path.display()
            )
        } else {
            format!("cannot read {}: {e}", path.display())
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::local::Direction;

    fn act(time: Secs, change: Change, path: &str) -> Activity {
        Activity {
            time,
            direction: Direction::Down,
            change,
            path: path.into(),
            to: None,
        }
    }

    fn up(recent: Vec<Activity>) -> RootView {
        let entry = RootEntry {
            path: PathBuf::from("/nowhere/root"),
            root_id: "r".into(),
            server: String::new(),
            added: 0,
        };
        let status = DaemonStatus {
            recent,
            ..DaemonStatus::default()
        };
        let mut r = RootView::detached(entry, Link::Up(Box::new(status)), None);
        r.link_epoch = 1;
        r
    }

    fn status(r: &mut RootView) -> &mut DaemonStatus {
        match &mut r.link {
            Link::Up(s) => s,
            _ => unreachable!(),
        }
    }

    /// As `poll_lists` marks it once asked, and as the answer leaves it.
    fn asked(r: &mut RootView, list: List, at: Secs) {
        let newest = status(r).recent.first().cloned();
        let epoch = r.link_epoch;
        *r.asked_mut(list) = Asked {
            epoch,
            newest,
            at,
            asking: false,
            again: false,
        };
    }

    fn open_copy(r: &mut RootView, copy: &str) {
        r.conflicts = Some((
            0,
            Ok(vec![OpenConflict {
                seq: 7,
                time: 0,
                device_name: "b".into(),
                copy: None,
                original: None,
                copy_display: copy.into(),
                original_display: Some("docs/a.txt".into()),
            }]),
        ));
    }

    #[test]
    fn what_came_after_is_found_by_the_newest_then() {
        let a = act(5, Change::Added, "a");
        let b = act(5, Change::Added, "b");
        let c = act(6, Change::Removed, "c");
        let recent = vec![c.clone(), b.clone(), a.clone()];
        assert_eq!(since(&recent, Some(&b)), std::slice::from_ref(&c));
        assert_eq!(since(&recent, Some(&c)), &[] as &[Activity]);
        // Nothing then: all of it came after. Gone off the list: more came after than it holds.
        assert_eq!(since(&recent, None), recent.as_slice());
        assert_eq!(
            since(&recent, Some(&act(1, Change::Added, "old"))),
            recent.as_slice()
        );
    }

    #[test]
    fn a_path_is_under_itself_and_its_folders_only() {
        assert!(at_or_under("docs/a.txt", "docs/a.txt"));
        assert!(at_or_under("docs/a.txt", "docs"));
        assert!(!at_or_under("docs/a.txt", "doc"));
        assert!(!at_or_under("docs", "docs/a.txt"));
    }

    #[test]
    fn each_list_is_asked_for_once_a_daemon_answers_and_again_when_it_starts_again() {
        let mut r = up(vec![act(1, Change::Added, "a")]);
        for list in [List::Conflicts, List::Deleted] {
            assert!(r.due(list, 100), "{list:?}");
            asked(&mut r, list, 100);
            assert!(!r.due(list, 10_000), "{list:?}");
        }
        r.link_epoch += 1;
        assert!(r.due(List::Conflicts, 100));
        assert!(r.due(List::Deleted, 100));
    }

    #[test]
    fn nothing_is_asked_of_a_daemon_that_does_not_run_or_is_being_asked() {
        let mut r = up(Vec::new());
        r.conflicts_asked.asking = true;
        assert!(!r.due(List::Conflicts, 100));
        r.link = Link::Down {
            since: 0,
            why: LinkError::NotRunning("stopped".into()),
        };
        assert!(!r.due(List::Deleted, 100));
    }

    #[test]
    fn conflicts_are_asked_for_again_when_one_is_made_or_its_copy_dealt_with() {
        let mut r = up(vec![act(1, Change::Added, "a")]);
        open_copy(&mut r, "docs/a (conflict).txt");
        asked(&mut r, List::Conflicts, 100);
        status(&mut r)
            .recent
            .insert(0, act(2, Change::Removed, "docs/b.txt"));
        status(&mut r)
            .recent
            .insert(0, act(2, Change::Changed, "docs/a (conflict).txt"));
        assert!(!r.due(List::Conflicts, 101), "nothing done to the copy");
        for (change, path) in [
            (Change::Conflict, "elsewhere.txt"),
            (Change::Removed, "docs/a (conflict).txt"),
            (Change::Moved, "docs"),
            (Change::Removed, "docs/a.txt"),
        ] {
            let mut r = r.clone();
            status(&mut r).recent.insert(0, act(3, change, path));
            assert!(r.due(List::Conflicts, 101), "{change:?} {path}");
        }
    }

    #[test]
    fn deleted_is_asked_for_again_after_a_delete_at_most_once_a_minute() {
        let mut r = up(vec![act(1, Change::Added, "a")]);
        asked(&mut r, List::Deleted, 100);
        asked(&mut r, List::Conflicts, 100);
        status(&mut r)
            .recent
            .insert(0, act(2, Change::Changed, "a"));
        assert!(!r.due(List::Deleted, 1_000), "nothing deleted");
        status(&mut r)
            .recent
            .insert(0, act(3, Change::Removed, "b"));
        assert!(!r.due(List::Deleted, 100 + DELETED_EVERY - 1));
        assert!(r.due(List::Deleted, 100 + DELETED_EVERY));
        assert!(!r.due(List::Conflicts, 1_000), "no conflict open");
    }

    #[test]
    fn what_the_window_changed_is_asked_for_again() {
        let mut r = up(Vec::new());
        asked(&mut r, List::Deleted, 100);
        r.deleted_asked.again = true;
        assert!(r.due(List::Deleted, 101));
    }

    #[test]
    fn a_list_that_could_not_be_had_is_asked_for_again_once_the_server_answers() {
        let mut r = up(Vec::new());
        asked(&mut r, List::Deleted, 100);
        r.deleted = Some((100, Err("the server did not answer".into())));
        assert!(!r.due(List::Deleted, 100 + RETRY_EVERY - 1));
        status(&mut r).feed_down = Some("no route".into());
        assert!(!r.due(List::Deleted, 100 + RETRY_EVERY));
        status(&mut r).feed_down = None;
        assert!(r.due(List::Deleted, 100 + RETRY_EVERY));
    }
}
