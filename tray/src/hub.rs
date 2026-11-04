//! What the tray knows of each root, kept current by a thread per daemon and one for the registry.

use crate::link::{self, LinkError};
use crate::model::now_secs;
use cairn_protocol::local::{
    registry_path, Change, DaemonStatus, OpenConflict, Registry, Reply, Request, RootEntry, Secs,
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
/// How old a root's conflict list may grow before it is asked for again.
const CONFLICTS_EVERY: Secs = 300;

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
    /// The newest conflict activity the list above has seen.
    conflicts_seen: Secs,
    alive: Arc<AtomicBool>,
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
            conflicts_seen: 0,
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
        std::thread::spawn(move || h.poll_conflicts());
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
                                        conflicts_seen: 0,
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
                    answered = true;
                    if !alive.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    self.with_root(root_id, |r| r.link = Link::Up(status));
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

    /// Keeps each running daemon's conflict list fresh: every few minutes, and at once when its
    /// activity shows a new conflict.
    fn poll_conflicts(self: Arc<Self>) {
        loop {
            let due: Vec<(String, Secs)> = {
                let now = now_secs();
                self.lock()
                    .roots
                    .iter()
                    .filter_map(|r| {
                        let Link::Up(s) = &r.link else { return None };
                        let newest = s
                            .recent
                            .iter()
                            .filter(|a| a.change == Change::Conflict)
                            .map(|a| a.time)
                            .max()
                            .unwrap_or(0);
                        let stale = match &r.conflicts {
                            None => true,
                            Some((at, _)) => now - at >= CONFLICTS_EVERY,
                        };
                        (stale || newest > r.conflicts_seen)
                            .then(|| (r.entry.root_id.clone(), newest))
                    })
                    .collect()
            };
            for (root_id, newest) in due {
                let _ = self.fetch_conflicts(&root_id);
                self.with_root(&root_id, |r| r.conflicts_seen = newest);
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
