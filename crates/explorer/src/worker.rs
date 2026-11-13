//! The threads that ask the daemons, never Explorer's own: one asks for folders in turn, and one for each
//! root shown watches its daemon, so that what changes is asked for again and shown.

use crate::cache::{self, Cache, Root};
use cairn_engine::control::{self, Client};
use cairn_protocol::local::{registry_path, Badge, Registry, Reply, Request};
use std::collections::{HashSet, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, Once, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use windows_sys::Win32::UI::Shell::{
    SHChangeNotify, SHCNE_UPDATEITEM, SHCNF_FLUSHNOWAIT, SHCNF_PATHW,
};

/// How often the registry of roots is looked at, to see whether it has changed.
const ROOTS_EVERY: Duration = Duration::from_secs(2);
/// For a folder's answer. A daemon in a sync answers between its steps, from what it holds.
const ASK_WAIT: Duration = Duration::from_secs(10);
/// A daemon says something at least every 30 s; past this, it has gone without closing.
const WATCH_WAIT: Duration = Duration::from_secs(90);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Roots {
    list: Vec<Root>,
    checked: Option<Instant>,
    /// The registry's time and size when it was read.
    stamp: Option<(SystemTime, u64)>,
}

struct Shared {
    roots: Mutex<Roots>,
    cache: Mutex<Cache>,
    jobs: Mutex<VecDeque<(Root, String)>>,
    wake: Condvar,
    asker: Once,
    watched: Mutex<HashSet<String>>,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared {
        roots: Mutex::default(),
        cache: Mutex::default(),
        jobs: Mutex::default(),
        wake: Condvar::new(),
        asker: Once::new(),
        watched: Mutex::default(),
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The registry read again if it has changed. One that cannot be read lists no roots.
fn refresh(roots: &mut Roots) {
    if roots.checked.is_some_and(|t| t.elapsed() < ROOTS_EVERY) {
        return;
    }
    roots.checked = Some(Instant::now());
    let Some(path) = registry_path() else {
        roots.list.clear();
        return;
    };
    let stamp = std::fs::metadata(&path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())));
    if stamp == roots.stamp {
        return;
    }
    roots.stamp = stamp;
    roots.list = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Registry>(&b).ok())
        .map(|r| cache::roots(&r.roots))
        .unwrap_or_default();
}

/// The badge Explorer is to show on a path now, if it is known: on Explorer's thread, so it only looks.
pub fn badge_of(path: &str) -> Option<Badge> {
    let s = shared();
    let (root, rel) = {
        let mut roots = lock(&s.roots);
        refresh(&mut roots);
        let (root, rel) = cache::locate(&roots.list, path)?;
        (root.clone(), rel)
    };
    watch(&root);
    let (badge, ask) = lock(&s.cache).look(&root, &rel, path);
    if let Some(folder) = ask {
        queue(root, folder);
    }
    badge
}

fn queue(root: Root, folder: String) {
    let s = shared();
    {
        let mut jobs = lock(&s.jobs);
        if !jobs.iter().any(|(r, f)| r.id == root.id && *f == folder) {
            jobs.push_back((root, folder));
        }
    }
    s.wake.notify_one();
    s.asker.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("cairn-badges".into())
            .spawn(ask_in_turn);
    });
}

/// Asks for each folder queued, and tells Explorer to look again at what its answer changed.
fn ask_in_turn() {
    let s = shared();
    loop {
        let (root, folder) = {
            let mut jobs = lock(&s.jobs);
            loop {
                if let Some(job) = jobs.pop_front() {
                    break job;
                }
                jobs = s.wake.wait(jobs).unwrap_or_else(|e| e.into_inner());
            }
        };
        // A panic ends this folder's answer, not the thread: Explorer goes on being answered.
        let asked = std::panic::catch_unwind(|| {
            let request = Request::Badges {
                folder: folder.clone(),
            };
            match control::ask(&root.id, &request, ASK_WAIT) {
                Ok(Reply::Badges(badges)) => {
                    let tell = lock(&s.cache).answered(&root, *badges);
                    tell_explorer(&tell);
                    true
                }
                _ => false,
            }
        });
        if !matches!(asked, Ok(true)) {
            lock(&s.cache).failed(&root, &folder);
        }
    }
}

/// Explorer looks at each of these again, and asks the handlers about it.
fn tell_explorer(paths: &[String]) {
    for p in paths {
        let wide: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a terminated path, which Explorer copies before this returns.
        unsafe {
            SHChangeNotify(
                SHCNE_UPDATEITEM as i32,
                SHCNF_PATHW | SHCNF_FLUSHNOWAIT,
                wide.as_ptr().cast(),
                std::ptr::null(),
            )
        };
    }
}

/// A thread that watches this root's daemon, once for each root shown.
fn watch(root: &Root) {
    if !lock(&shared().watched).insert(root.id.clone()) {
        return;
    }
    let root = root.clone();
    let _ = std::thread::Builder::new()
        .name("cairn-badges-watch".into())
        .spawn(move || watch_daemon(root));
}

/// Until the root leaves the registry. Each connection begins with everything changed, so a daemon
/// that has started again, or one that was not running, has every folder known asked for again.
fn watch_daemon(root: Root) {
    let s = shared();
    let mut backoff = Duration::from_secs(1);
    loop {
        // A panic ends this connection, as a daemon going away does, and another is made.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Ok(mut c) = Client::connect(&root.id) else {
                return;
            };
            if c.send(&Request::WatchBadges).is_err() {
                return;
            }
            while let Ok(Some(reply)) = c.next(Some(WATCH_WAIT)) {
                // An older daemon answers with an error and closes.
                let Reply::BadgesChanged { all, paths } = reply else {
                    break;
                };
                backoff = Duration::from_secs(1);
                if all || !paths.is_empty() {
                    let folders = lock(&s.cache).changed(&root, all, &paths);
                    for folder in folders {
                        queue(root.clone(), folder);
                    }
                }
            }
        }));
        let listed = {
            let mut roots = lock(&s.roots);
            refresh(&mut roots);
            roots.list.iter().any(|r| r.id == root.id)
        };
        if !listed {
            lock(&s.watched).remove(&root.id);
            return;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
