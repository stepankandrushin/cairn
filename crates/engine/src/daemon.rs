//! Keeping a root in sync without being run by hand. All the work is `sync()`: the same state-based
//! run a person starts, so the daemon adds no way of changing anything. What it adds is when to run:
//!
//! - a change under the root, from the watcher (watch.rs), once things have been quiet for a moment;
//! - a change on the server, from a long-poll of `GET /changes`;
//! - every so often regardless, since either of those can miss something: this is the safety net;
//! - after a failure, with growing waits, and while paused, since nothing else announces a disk being
//!   mounted again.
//!
//! A kill at any moment is the crash `sync()` already recovers from, so no signal is handled.
//!
//! It also serves the local API (control.rs) to `cairn status` and the tray: its status, and three
//! requests that change no tree. Look now is an event like the watcher's; a pause stops it starting a
//! sync until a resume, and outlives a restart, being kept in `.cairn`.

use crate::client::Client;
use crate::control::{Handler, Hub};
use crate::fsops::Dir;
use crate::root::{read_config, read_root_id, Busy};
use crate::scan::{display_path, STATE_FILE_DAEMON};
use crate::sync::{sync, SyncOptions, SyncOutcome, SyncReport};
use crate::watch::{WatchCounts, Watcher};
use anyhow::{bail, Result};
use cairn_failpoint::hit;
use cairn_protocol::local::{OpenConflict, Reply, Request, MAX_RECENT};
use cairn_protocol::SkipKind;
use std::collections::BTreeSet;
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::linux::net::SocketAddrExt;
#[cfg(target_os = "linux")]
use std::os::unix::net::{SocketAddr, UnixListener};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long after the last event to wait, for a save or a build to finish.
const QUIET: Duration = Duration::from_millis(300);
/// And the most that waiting can add up to, while something keeps writing.
const MOST_QUIET: Duration = Duration::from_secs(3);
/// Between the end of one sync and the next, at the very least.
const MIN_GAP: Duration = Duration::from_secs(1);
/// And at the most: a change written as a sync of a big tree ends waits this long and one sync, not
/// twice that sync first.
const MAX_GAP: Duration = Duration::from_secs(30);
/// A wait shorter than this is not worth writing the status for.
const SHOW_WAIT: Duration = Duration::from_secs(2);
/// With directories out of the watcher's reach, the safety net does more of the work.
const DEGRADED_RECONCILE: Duration = Duration::from_secs(300);
/// Idle this long past the pause after a sync, the daemon gives back the memory that sync freed. Not at
/// once: a change that comes in the meantime finds it still there, which in a big tree is seconds.
const IDLE_BEFORE_GIVING_BACK: Duration = Duration::from_secs(30);
/// Below any reverse proxy's usual read timeout (nginx: 60 seconds).
const FEED_WAIT: Duration = Duration::from_secs(25);
const LOCK_ATTEMPTS: u32 = 5;
const FEED_MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A shorter wait is tried again at full length this long after the last cut: what cut it may have been
/// a restart, and a real proxy costs one cut per hour.
const FEED_WAIT_RECOVERY: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    /// Between full syncs when nothing asks for one.
    pub reconcile: Duration,
    /// The longest wait before trying again after a failure, or while paused.
    pub retry: Duration,
    /// Watch the filesystem; without it, only the reconcile finds local changes.
    pub watch: bool,
    /// Long-poll the server; without it, only the reconcile finds remote changes.
    pub feed: bool,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            reconcile: Duration::from_secs(3600),
            retry: Duration::from_secs(60),
            watch: true,
            feed: true,
        }
    }
}

enum Wake {
    /// Something changed under the root, or a person asked; when that was known.
    Local(Instant),
    /// The head the feed was waiting on, and the one it found.
    Remote { after: u64, head: u64 },
    /// The server can be reached again, after it could not.
    ServerBack,
    /// The feed cannot reach the server, which an idle daemon would otherwise not find out.
    FeedDown,
    /// Something for the main loop from the local API: a pause, a resume, a line for the log.
    Control,
}

/// What the daemon is up to, for `cairn status` and the tray: `.cairn/daemon.json`, and the local
/// API's status. Defined in the protocol crate, so every reader has the same fields.
pub use cairn_protocol::local::{DaemonStatus as Status, NotApplied, Waiting, MAX_NOT_APPLIED};

/// Where a pause is kept: in the root's state, so that it outlives a kill and a restart.
const PAUSED_FILE: &str = "paused";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(target_os = "linux")]
fn lock_address(root_id: &str) -> std::io::Result<SocketAddr> {
    SocketAddr::from_abstract_name(format!("cairn-daemon-{root_id}"))
}

/// Held for the life of the daemon. A name in the abstract socket namespace rather than a file, which
/// would be an open descriptor on the disk and keep it from being unmounted. It is the local API's
/// socket as well. Public for tests that play a daemon.
#[cfg(target_os = "linux")]
pub fn take_lock(root_id: &str) -> std::io::Result<UnixListener> {
    UnixListener::bind_addr(&lock_address(root_id)?)
}

/// macOS has no abstract sockets. A lock on a file in the user's library, not in the root, for the same
/// reason; the kernel drops it when the process dies.
#[cfg(target_os = "macos")]
pub fn take_lock(root_id: &str) -> std::io::Result<std::fs::File> {
    let dir = cairn_protocol::local::user_dir()
        .ok_or_else(|| std::io::Error::other("HOME is not set"))?;
    std::fs::create_dir_all(&dir)?;
    // The local API's socket is here too.
    crate::fsops::make_private_dir(&dir)?;
    let file = std::fs::File::create(dir.join(format!("daemon-{root_id}.lock")))?;
    match crate::fsops::try_lock(&file)? {
        true => Ok(file),
        false => Err(std::io::ErrorKind::AddrInUse.into()),
    }
}

/// As on macOS: a lock on a file in the user's local application data, not in the root, which the
/// kernel drops when the process dies.
#[cfg(windows)]
pub fn take_lock(root_id: &str) -> std::io::Result<std::fs::File> {
    let dir = cairn_protocol::local::user_dir()
        .ok_or_else(|| std::io::Error::other("LOCALAPPDATA is not set"))?;
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("daemon-{root_id}.lock")))?;
    match crate::fsops::try_lock(&file)? {
        true => Ok(file),
        false => Err(std::io::ErrorKind::AddrInUse.into()),
    }
}

/// What the daemon of this root last wrote, if one is running: for a daemon that does not answer on the
/// local API, as one from before it does not. [`crate::control::status_of`] asks first.
pub fn running_status(path: &Path) -> Option<Status> {
    let root_id = read_root_id(path).ok()?;
    if take_lock(&root_id).is_ok() {
        return None;
    }
    let text = std::fs::read(state_file(path)).unwrap_or_default();
    Some(serde_json::from_slice(&text).unwrap_or_default())
}

fn state_file(path: &Path) -> std::path::PathBuf {
    path.join(crate::fsops::name_path(cairn_protocol::STATE_DIR))
        .join(STATE_FILE_DAEMON)
}

/// Best effort: it is a report. Through descriptors that refuse symlinks, as all else in a root is, and
/// a missing `.cairn` must never be made here by writing one.
fn write_status(path: &Path, status: &Status) {
    let write = || -> Result<()> {
        let bytes = serde_json::to_vec_pretty(status)?;
        let state = Dir::open_root(path)?.open_dir(cairn_protocol::STATE_DIR)?;
        let tmp = format!("{STATE_FILE_DAEMON}.new");
        let _ = state.unlink(tmp.as_bytes());
        let mut file = std::fs::File::from(state.create_new(tmp.as_bytes(), 0o600)?);
        file.write_all(&bytes)?;
        state.rename_over(tmp.as_bytes(), &state, STATE_FILE_DAEMON.as_bytes())?;
        Ok(())
    };
    let _ = write();
}

/// Written and shown: the file for whoever reads it, the local API for the rest.
fn show(path: &Path, status: &Status, hub: &Hub) {
    write_status(path, status);
    hub.publish(status);
}

/// What a daemon said before it was restarted: the last changes it made are still the last ones.
fn previous_recent(path: &Path) -> Vec<cairn_protocol::local::Activity> {
    std::fs::read(state_file(path))
        .ok()
        .and_then(|b| serde_json::from_slice::<Status>(&b).ok())
        .map(|s| s.recent)
        .unwrap_or_default()
}

const PAUSED_DETAIL: &str =
    "paused by request: no sync starts until it is resumed, and changes here and on the server wait";

/// A pause, as the local API sets it and the main loop obeys it.
#[derive(Default)]
struct Pause {
    since: Mutex<Option<i64>>,
}

impl Pause {
    fn get(&self) -> Option<i64> {
        *self.since.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Kept in the root before it counts: a pause that a kill could undo would be one in name only.
    fn set(&self, root: &Path, on: bool) -> Result<()> {
        let mut since = self.since.lock().unwrap_or_else(|e| e.into_inner());
        if on == since.is_some() {
            return Ok(());
        }
        let state = Dir::open_root(root)?.open_dir(cairn_protocol::STATE_DIR)?;
        if on {
            let now = now_secs();
            let tmp = format!("{PAUSED_FILE}.new");
            let _ = state.unlink(tmp.as_bytes());
            let mut file = std::fs::File::from(state.create_new(tmp.as_bytes(), 0o600)?);
            file.write_all(now.to_string().as_bytes())?;
            file.sync_all()?;
            drop(file);
            state.rename_over(tmp.as_bytes(), &state, PAUSED_FILE.as_bytes())?;
            state.sync()?;
            *since = Some(now);
        } else {
            match state.unlink(PAUSED_FILE.as_bytes()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            state.sync()?;
            *since = None;
        }
        Ok(())
    }

    /// As kept in the root. A file that does not read as a time is still a pause.
    fn load(&self, root: &Path) -> Result<()> {
        let state = Dir::open_root(root)?.open_dir(cairn_protocol::STATE_DIR)?;
        let since = match state.open_read(PAUSED_FILE.as_bytes()) {
            Ok(fd) => {
                let mut text = String::new();
                std::fs::File::from(fd).read_to_string(&mut text)?;
                Some(text.trim().parse::<i64>().unwrap_or_else(|_| now_secs()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        *self.since.lock().unwrap_or_else(|e| e.into_inner()) = since;
        Ok(())
    }
}

/// The open conflicts, with where their copies are on this device: a path only if something is there.
fn open_conflicts(root: &Path) -> Result<Vec<OpenConflict>> {
    let config = read_config(root)?;
    let here = |names: Option<&[cairn_protocol::Name]>| {
        let names = names?;
        let mut at = root.to_path_buf();
        for n in names {
            at.push(crate::fsops::name_path(n.as_bytes()));
        }
        std::fs::symlink_metadata(&at).is_ok().then_some(at)
    };
    Ok(crate::restore::conflicts(&config)?
        .into_iter()
        .filter(|c| c.open)
        .map(|c| OpenConflict {
            seq: c.entry.seq,
            time: c.entry.time,
            device_name: c.entry.device_name.clone(),
            copy: here(c.copy_now.as_deref()),
            original: here(c.object_now.as_deref()),
            copy_display: c
                .copy_now
                .as_deref()
                .or(c.made_at.as_deref())
                .map_or_else(|| format!("copy {}", c.entry.copy), display_path),
            original_display: c.object_now.as_deref().map(display_path),
        })
        .collect())
}

/// What the local API may ask of this daemon beyond its status.
fn handler(root: &Path, pause: &Arc<Pause>, wake: &Sender<Wake>) -> Arc<Handler> {
    let (root, pause, wake) = (root.to_path_buf(), pause.clone(), wake.clone());
    Arc::new(move |request: Request| match request {
        Request::SyncNow => {
            if pause.get().is_some() {
                return Reply::Error {
                    text: "this daemon is paused: resume it to sync".into(),
                };
            }
            let _ = wake.send(Wake::Local(Instant::now()));
            Reply::Done
        }
        r @ (Request::Pause | Request::Resume) => {
            let on = r == Request::Pause;
            match pause.set(&root, on) {
                Ok(()) => {
                    let _ = wake.send(Wake::Control);
                    Reply::Done
                }
                Err(e) => Reply::Error {
                    text: format!("{}: {e:#}", if on { "not paused" } else { "still paused" }),
                },
            }
        }
        Request::Conflicts => match open_conflicts(&root) {
            Ok(open) => Reply::Conflicts { open },
            Err(e) => Reply::Error {
                text: format!("{e:#}"),
            },
        },
        Request::Status | Request::Subscribe => Reply::Error {
            text: "not a request for the handler".into(),
        },
    })
}

/// Waits for the head to move, and wakes the daemon when it does.
fn feed(
    path: std::path::PathBuf,
    synced_head: Arc<AtomicU64>,
    wake: Sender<Wake>,
    down: Arc<Mutex<Option<String>>>,
) {
    let set_down = |why: Option<String>| {
        let mut slot = down.lock().unwrap_or_else(|e| e.into_inner());
        let news = slot.is_none() && why.is_some();
        *slot = why;
        if news {
            let _ = wake.send(Wake::FeedDown);
        }
    };
    let mut wait = FEED_WAIT;
    let mut backoff = Duration::from_secs(1);
    let mut seen = 0u64;
    let mut failing = false;
    let mut client: Option<Client> = None;
    let mut last_cut = Instant::now();
    loop {
        hit("daemon.feed");
        if last_cut.elapsed() >= FEED_WAIT_RECOVERY {
            wait = FEED_WAIT;
        }
        if client.is_none() {
            client = read_config(&path)
                .and_then(|config| Client::new(&config))
                .map_err(|e| set_down(Some(format!("{e:#}"))))
                .ok();
            if client.is_none() {
                failing = true;
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(FEED_MAX_BACKOFF);
                continue;
            }
        }
        let after = seen.max(synced_head.load(Ordering::SeqCst));
        let started = Instant::now();
        let answer = client
            .as_ref()
            .expect("made above")
            .wait_for_head(after, wait);
        let took = started.elapsed();
        match answer {
            Ok(head) => {
                set_down(None);
                if failing {
                    failing = false;
                    if wake.send(Wake::ServerBack).is_err() {
                        return;
                    }
                }
                backoff = Duration::from_secs(1);
                seen = head;
                if head != after {
                    if wake.send(Wake::Remote { after, head }).is_err() {
                        return;
                    }
                } else if took < wait / 2 {
                    // No news, and no waiting either: an old server, or something in between that
                    // answers for it. Asking again at once would be a busy loop.
                    std::thread::sleep(backoff.max(wait / 4));
                }
            }
            Err(e) => {
                failing = true;
                set_down(Some(format!("{e:#}")));
                // Cut off partway: something in between has a shorter read timeout than this wait.
                if took >= Duration::from_secs(2) && took < wait {
                    wait = Duration::from_secs((took.as_secs() * 3 / 4).max(1));
                    last_cut = Instant::now();
                }
                client = None;
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(FEED_MAX_BACKOFF);
            }
        }
    }
}

/// When it is time, given what is pending.
struct Schedule {
    first_event: Option<Instant>,
    last_event: Option<Instant>,
    remote: bool,
    reconcile_at: Instant,
    /// When to try again. After a failure (`holding`), nothing but the server coming back goes before
    /// it; after a sync that found only files still being written, anything else may.
    retry_at: Option<Instant>,
    holding: bool,
    /// When the gap after the last sync ends.
    gap_until: Instant,
    backoff: Duration,
    /// When the scan began that the last sync's agreement rests on: a change before it, that sync saw,
    /// its own writes included, so an event from then wakes nothing.
    covered: Option<Instant>,
}

impl Schedule {
    fn due(&self) -> Instant {
        let mut due = self.reconcile_at;
        if let Some(retry) = self.retry_at {
            due = due.min(retry);
        }
        let quiet = match (self.first_event, self.last_event) {
            (Some(first), Some(last)) => Some((last + QUIET).min(first + MOST_QUIET)),
            _ => None,
        };
        if quiet.is_some() || self.remote {
            let mut work = self.gap_until;
            if let Some(q) = quiet {
                work = work.max(q);
            }
            if let Some(retry) = self.retry_at.filter(|_| self.holding) {
                work = work.max(retry);
            }
            due = due.min(work);
        }
        due
    }

    /// When work that waits will be done, and why it waits, if anything does.
    fn waiting(&self, now: Instant) -> Option<(Instant, &'static str)> {
        if self.first_event.is_none() && !self.remote {
            return None;
        }
        let due = self.due();
        if due <= now {
            return None;
        }
        let why = if self.holding && self.retry_at == Some(due) {
            "the wait after a failure, before trying again"
        } else if self.gap_until == due {
            "the pause after the last sync, twice the part of it every sync repeats, at most 30 s"
        } else {
            "for the files to be quiet"
        };
        Some((due, why))
    }

    fn note(&mut self, wake: Wake, synced_head: u64) {
        match wake {
            Wake::Local(at) if self.covered.is_some_and(|c| at < c) => {}
            Wake::Local(_) => {
                let now = Instant::now();
                self.first_event.get_or_insert(now);
                self.last_event = Some(now);
            }
            // Ahead of what the last sync saw, or behind where the feed was: news. In between, it is an
            // answer to a question asked before this device's own last commits, which it has seen.
            Wake::Remote { after, head } => {
                self.remote |= head > synced_head || head < after;
            }
            Wake::ServerBack => {
                self.retry_at = None;
                self.remote = true;
            }
            Wake::FeedDown | Wake::Control => {}
        }
    }

    fn failed(&mut self, retry: Duration) {
        self.backoff = (self.backoff * 2).clamp(Duration::from_secs(1), retry);
        self.retry_at = Some(Instant::now() + self.backoff);
        self.holding = true;
    }

    /// Only what is still being written stood in the way: come back for it, holding nothing back.
    fn settling(&mut self, retry: Duration) {
        self.failed(retry);
        self.holding = false;
    }
}

/// Say so if the feed has gone down or come back. Returns whether it had.
fn say_feed(
    down: &Mutex<Option<String>>,
    said: &mut Option<String>,
    log: &mut dyn FnMut(&str),
) -> bool {
    let now = down.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if now == *said {
        return false;
    }
    match &now {
        Some(why) => log(&format!(
            "the change feed is down: {why}; changes on the server wait for it or for a full sync"
        )),
        None => log("the change feed is back"),
    }
    *said = now;
    true
}

/// The lines of a report, for saying each thing once rather than on every retry.
/// Keep what this filesystem refused until it is put right or gone; see [`Status::not_applied`].
fn note_not_applied(
    root: &Path,
    status: &mut Status,
    kept: &mut Vec<NotApplied>,
    report: &SyncReport,
) {
    let applied: BTreeSet<&str> = report.meta_applied.iter().map(String::as_str).collect();
    // A name shown with escapes cannot be looked up by its display, so it stays.
    let gone = |p: &str| !p.contains('\\') && std::fs::symlink_metadata(root.join(p)).is_err();
    kept.retain(|n| !applied.contains(n.path.as_str()) && !gone(&n.path));
    for w in &report.warnings {
        let text = w.to_string();
        if !kept.iter().any(|n| n.text == text) {
            kept.push(NotApplied {
                path: w.path.clone(),
                text,
                since: now_secs(),
            });
        }
    }
    status.not_applied_count = kept.len();
    status.not_applied = kept.iter().take(MAX_NOT_APPLIED).cloned().collect();
}

fn report_lines(report: &SyncReport) -> BTreeSet<String> {
    let mut lines = BTreeSet::new();
    for e in &report.events {
        lines.insert(format!("event: {e}"));
    }
    for s in &report.skips {
        lines.insert(format!("skipped: {s}"));
    }
    for w in &report.warnings {
        lines.insert(format!("not applied here: {w}"));
    }
    if let SyncOutcome::Paused(why) = &report.outcome {
        lines.insert(format!("paused: {why}"));
    }
    lines
}

/// The watcher holds a folder open for each level it walks down, and the scan the same: the soft limit
/// on open files a session starts a daemon with, often 1,024, is no limit of the tree's, and the hard
/// one is this process's to take.
fn raise_open_files(log: &mut dyn FnMut(&str)) {
    #[cfg(target_os = "linux")]
    {
        use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
        let limit = getrlimit(Resource::Nofile);
        if limit.current.is_some() && limit.current != limit.maximum {
            let raised = Rlimit {
                current: limit.maximum,
                maximum: limit.maximum,
            };
            if let Err(e) = setrlimit(Resource::Nofile, raised) {
                log(&format!(
                    "cannot raise the limit on open files from {:?} to {:?}: {e}",
                    limit.current, limit.maximum
                ));
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = log;
}

/// Hand what the allocator holds free back to the system: after a sync of a big tree that is gigabytes,
/// which would otherwise stay this process's while it waits.
fn give_back_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // SAFETY: glibc's own; it only returns free pages to the system.
        unsafe {
            malloc_trim(0);
        }
    }
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: libSystem's own; a null zone means every zone, and a goal of 0 as much as it can.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
}

/// Run until the process ends. Comes back only for what no retry would cure.
pub fn run(path: &Path, opts: &DaemonOptions, log: &mut dyn FnMut(&str)) -> Result<()> {
    let (wake_tx, wake_rx) = channel();
    let synced_head = Arc::new(AtomicU64::new(0));
    let feed_down = Arc::new(Mutex::new(None::<String>));
    let pause = Arc::new(Pause::default());
    let hub = {
        let tx = wake_tx.clone();
        Arc::new(Hub::new(&Status::default(), move || {
            let _ = tx.send(Wake::Control);
        }))
    };

    raise_open_files(log);
    let watcher = if opts.watch {
        let tx = wake_tx.clone();
        match Watcher::start(path, move || {
            let _ = tx.send(Wake::Local(Instant::now()));
        }) {
            Ok(w) => Some(w),
            Err(e) => {
                log(&format!(
                    "cannot watch this filesystem ({e}): changes here are found only by the full sync every {} seconds",
                    opts.reconcile.as_secs()
                ));
                None
            }
        }
    } else {
        None
    };
    if opts.feed {
        let (root, head, tx, down) = (
            path.to_path_buf(),
            synced_head.clone(),
            wake_tx.clone(),
            feed_down.clone(),
        );
        std::thread::spawn(move || feed(root, head, tx, down));
    }

    let mut lock = None;
    let now = Instant::now();
    let mut schedule = Schedule {
        first_event: None,
        last_event: None,
        remote: false,
        reconcile_at: now,
        retry_at: None,
        holding: false,
        gap_until: now,
        backoff: Duration::ZERO,
        covered: None,
    };
    let mut status = Status {
        pid: std::process::id(),
        root: std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
        version: env!("CARGO_PKG_VERSION").into(),
        recent: previous_recent(path),
        ..Status::default()
    };
    let mut was_paused: Option<i64> = None;
    let mut said: BTreeSet<String> = BTreeSet::new();
    let mut said_counts = WatchCounts::default();
    let mut said_feed: Option<String> = None;
    let mut not_applied: Vec<NotApplied> = Vec::new();
    let mut last_error: Option<String> = None;
    let mut walk_first = true;
    // When to give back what the last sync freed, if nothing has started another by then.
    let mut give_back_at: Option<Instant> = None;

    loop {
        for note in hub.take_notes() {
            log(&note);
        }
        let paused = pause.get();
        if paused != was_paused {
            match paused {
                Some(_) => {
                    log("paused by request: no sync starts until it is resumed");
                    status.state = "paused".into();
                    status.detail = Some(PAUSED_DETAIL.into());
                    status.waiting = None;
                }
                None => {
                    log("resumed: syncing what changed while it was paused");
                    // Not in agreement until the sync that follows at once says so.
                    status.state = "partial".into();
                    status.detail =
                        Some("resumed: syncing what changed while it was paused".into());
                    // All of it, watched or not: the sync is state-based, so nothing has been lost.
                    schedule.reconcile_at = Instant::now();
                }
            }
            status.paused_since = paused;
            show(path, &status, &hub);
            was_paused = paused;
        }
        // Paused, only the local API can end the wait.
        let wait = match paused {
            Some(_) => Duration::from_secs(3600),
            None => schedule.due().saturating_duration_since(Instant::now()),
        };
        let wait = give_back_at.map_or(wait, |at| {
            wait.min(at.saturating_duration_since(Instant::now()))
        });
        match wake_rx.recv_timeout(wait) {
            Ok(wake) => {
                schedule.note(wake, synced_head.load(Ordering::SeqCst));
                while let Ok(more) = wake_rx.try_recv() {
                    schedule.note(more, synced_head.load(Ordering::SeqCst));
                }
                // Paused, or just paused or resumed, which the top of the loop says first.
                if pause.get().is_some() || pause.get() != was_paused {
                    continue;
                }
                let now = Instant::now();
                if now < schedule.due() {
                    let mut changed = say_feed(&feed_down, &mut said_feed, log);
                    if changed {
                        status.feed_down.clone_from(&said_feed);
                    }
                    if status.waiting.is_none() {
                        if let Some((due, why)) = schedule
                            .waiting(now)
                            .filter(|(due, _)| *due - now > SHOW_WAIT)
                        {
                            let left = (due - now).as_secs_f64().ceil() as i64;
                            status.waiting = Some(Waiting {
                                until: now_secs() + left,
                                why: why.into(),
                            });
                            changed = true;
                        }
                    }
                    if changed {
                        show(path, &status, &hub);
                    }
                    continue;
                }
            }
            Err(RecvTimeoutError::Timeout)
                if give_back_at.is_some_and(|at| Instant::now() >= at) =>
            {
                give_back_memory();
                give_back_at = None;
                continue;
            }
            Err(RecvTimeoutError::Timeout)
                if pause.get().is_some() || pause.get() != was_paused =>
            {
                continue
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("the daemon's own channel closed"),
        }

        // Held from the first time the marker can be read: a second daemon is refused, not interleaved.
        if lock.is_none() {
            if let Ok(id) = read_root_id(path) {
                // `cairn status` holds this for an instant to see whether a daemon is running.
                let mut taken = take_lock(&id);
                for _ in 0..LOCK_ATTEMPTS {
                    if taken.is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    taken = take_lock(&id);
                }
                match taken {
                    Ok(l) => {
                        status.root_id.clone_from(&id);
                        if let Ok(config) = read_config(path) {
                            status.server = config.server;
                        }
                        if let Err(e) = crate::registry::register(path, &id, &status.server) {
                            log(&format!(
                                "this root could not be added to the list of roots, so `cairn status --all` and the tray do not show it: {e:#}"
                            ));
                        }
                        if let Err(e) = pause.load(path) {
                            log(&format!(
                                "cannot read whether this daemon was paused, so it is: {e:#}"
                            ));
                            *pause.since.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(now_secs());
                        }
                        match crate::control::listen(&l, &id) {
                            Ok(listener) => {
                                hub.publish(&status);
                                crate::control::serve(
                                    listener,
                                    hub.clone(),
                                    handler(path, &pause, &wake_tx),
                                );
                            }
                            Err(e) => log(&format!(
                                "cannot serve the local API, so `cairn status` reads this daemon's file and the tray does not see it: {e}"
                            )),
                        }
                        lock = Some(l);
                        if pause.get().is_some() {
                            continue;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                        bail!(
                            "another cairn daemon is already running for {}",
                            path.display()
                        )
                    }
                    Err(e) => bail!("cannot take the daemon's lock: {e}"),
                }
            }
        }

        let started = Instant::now();
        let reconcile = started >= schedule.reconcile_at;
        // A change here the watcher saw: the sync is to go the whole way, without first looking for
        // nothing to do, which in a big tree is a scan of its own.
        let changed_here = schedule.first_event.is_some();
        schedule.first_event = None;
        schedule.last_event = None;
        schedule.remote = false;
        schedule.retry_at = None;
        // Watches first, so that anything the sync does not see is an event of its own.
        if let Some(w) = &watcher {
            if reconcile || walk_first {
                w.walk_all();
            }
        }
        status.waiting = None;
        // To the local API only: writing the file twice a sync would be for nobody.
        status.syncing = true;
        hub.publish(&status);
        status.syncing = false;
        hit("daemon.sync");
        let result = sync(
            path,
            &SyncOptions {
                changed_here,
                ..SyncOptions::default()
            },
        );
        hit("daemon.synced");
        // What a sync with nothing to do takes: loading the state, the scan, the merge. The rest of a
        // long one was moving data, which the next does not do again. Twice that keeps a tree that never
        // rests from keeping the daemon busy more than a third of the time, and a change after a big
        // download waits about a sync with nothing to do, not twice the download. Up to 30 s: past that,
        // a change waits on the pause longer than on the sync that sends it.
        let repeated = match &result {
            Ok(report) => report.repeated,
            Err(_) => started.elapsed(),
        };
        schedule.gap_until = Instant::now() + (repeated * 2).clamp(MIN_GAP, MAX_GAP);
        schedule.covered = result.as_ref().ok().and_then(|r| r.agreed_since);
        // What the sync held is free again, and goes back to the system once the daemon is idle,
        // rather than staying this process's until the next sync.
        give_back_at = Some(schedule.gap_until + IDLE_BEFORE_GIVING_BACK);

        status.syncs += 1;
        status.last_sync = now_secs();
        let mut lines = BTreeSet::new();
        match result {
            Ok(report) => {
                last_error = None;
                walk_first = false;
                if !report.activity.is_empty() {
                    let mut recent: Vec<_> = report.activity.iter().rev().cloned().collect();
                    recent.append(&mut status.recent);
                    recent.truncate(MAX_RECENT);
                    status.recent = recent;
                }
                status.head = report.head;
                synced_head.store(report.head, Ordering::SeqCst);
                lines = report_lines(&report);
                status.skips = report.skips.iter().map(ToString::to_string).collect();
                note_not_applied(path, &mut status, &mut not_applied, &report);
                if let Some(w) = &watcher {
                    w.set_in_motion(
                        report
                            .skips
                            .iter()
                            .filter(|s| s.kind == SkipKind::KeepsChanging)
                            .map(|s| s.path.clone())
                            .collect(),
                    );
                }
                match &report.outcome {
                    SyncOutcome::Synced => {
                        schedule.backoff = Duration::ZERO;
                        status.state = "synced".into();
                        status.detail = None;
                        status.last_synced = status.last_sync;
                    }
                    SyncOutcome::Partial => {
                        let stuck = report
                            .skips
                            .iter()
                            .any(|s| s.kind.retryable() && !s.kind.in_motion());
                        if stuck {
                            schedule.failed(opts.retry);
                        } else {
                            schedule.settling(opts.retry);
                        }
                        status.state = "partial".into();
                        status.detail = None;
                    }
                    SyncOutcome::Paused(why) => {
                        // Nothing announces a disk coming back, and the watches went with it.
                        schedule.retry_at = Some(Instant::now() + opts.retry);
                        walk_first = true;
                        status.state = "paused".into();
                        status.detail = Some(why.clone());
                    }
                }
                if report.commits > 0 || report.steps_applied > 0 {
                    log(&format!(
                        "synced to sequence {}: {} steps applied here, {} commits pushed, {} bytes uploaded, {} conflict(s)",
                        report.head, report.steps_applied, report.commits, report.bytes_uploaded, report.conflicts
                    ));
                }
            }
            Err(e) if e.downcast_ref::<Busy>().is_some() => {
                // A person's own `cairn sync` or `cairn restore`. It finishes soon, and says how it went.
                schedule.retry_at = Some(Instant::now() + MIN_GAP);
                lines = said.clone();
            }
            Err(e) => {
                schedule.failed(opts.retry);
                let text = format!("{e:#}");
                if last_error.as_ref() != Some(&text) {
                    log(&format!("error: {text}; trying again"));
                }
                status.state = "error".into();
                status.detail = Some(text.clone());
                last_error = Some(text);
                walk_first = true;
            }
        }
        for line in lines.difference(&said) {
            log(line);
        }
        said = lines;

        let counts = watcher.as_ref().map(Watcher::counts).unwrap_or_default();
        if counts.limit_reached && !said_counts.limit_reached {
            log(&format!(
                "the kernel's limit on inotify watches was reached, so {} folders are not watched and changes in them wait for a full sync: raise fs.inotify.max_user_watches",
                counts.unwatched
            ));
        } else if counts.stopped && !said_counts.stopped {
            log("the watcher has stopped: changes here are found only by the full sync");
        } else if counts.unwatched > 0 && said_counts.unwatched == 0 {
            let missed = watcher.as_ref().map(Watcher::missed).unwrap_or_default();
            let more = counts.unwatched.saturating_sub(missed.len());
            log(&format!(
                "{} folders could not be watched, so changes in them wait for a full sync: {}{}",
                counts.unwatched,
                missed.join(", "),
                if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                }
            ));
        }
        said_counts = counts;
        status.watched = counts.watched;
        status.unwatched = counts.unwatched;
        status.watch_limit_reached = counts.limit_reached;

        say_feed(&feed_down, &mut said_feed, log);
        status.feed_down.clone_from(&said_feed);
        show(path, &status, &hub);

        if reconcile {
            let degraded =
                opts.watch && (watcher.is_none() || counts.unwatched > 0 || counts.stopped);
            schedule.reconcile_at = Instant::now()
                + if degraded {
                    opts.reconcile.min(DEGRADED_RECONCILE)
                } else {
                    opts.reconcile
                };
        }
    }
}
