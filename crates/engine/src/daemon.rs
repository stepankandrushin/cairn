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

use crate::client::Client;
use crate::fsops::Dir;
use crate::root::{read_config, read_root_id, Busy};
use crate::scan::STATE_FILE_DAEMON;
use crate::sync::{sync, SyncOptions, SyncOutcome, SyncReport};
use crate::watch::{WatchCounts, Watcher};
use anyhow::{bail, Result};
use cairn_failpoint::hit;
use cairn_protocol::SkipKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
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
/// A wait shorter than this is not worth writing the status for.
const SHOW_WAIT: Duration = Duration::from_secs(2);
/// With directories out of the watcher's reach, the safety net does more of the work.
const DEGRADED_RECONCILE: Duration = Duration::from_secs(300);
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
    Local,
    Remote(u64),
    /// The server can be reached again, after it could not.
    ServerBack,
    /// The feed cannot reach the server, which an idle daemon would otherwise not find out.
    FeedDown,
}

/// What the daemon is up to, for `cairn status`: `.cairn/daemon.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    /// `synced`, `partial`, `paused` or `error`.
    pub state: String,
    pub detail: Option<String>,
    pub last_sync: i64,
    pub last_synced: i64,
    pub head: u64,
    pub syncs: u64,
    pub watched: usize,
    pub unwatched: usize,
    pub watch_limit_reached: bool,
    /// Why the change feed is down, if it is.
    pub feed_down: Option<String>,
    pub skips: Vec<String>,
    /// Changes here or on the server that wait, until when and why.
    pub waiting: Option<Waiting>,
    /// Metadata the filesystem here refused, which the server keeps: each until a later sync applies
    /// that path's in full, the path is gone, or the daemon restarts. A sync says so only once, since
    /// after it nothing differs, so this is where it stays to be seen.
    #[serde(default)]
    pub not_applied: Vec<NotApplied>,
    /// How many there are, of which `not_applied` holds the first [`MAX_NOT_APPLIED`].
    #[serde(default)]
    pub not_applied_count: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotApplied {
    pub path: String,
    pub text: String,
    /// Seconds since the epoch.
    pub since: i64,
}

pub const MAX_NOT_APPLIED: usize = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    /// Seconds since the epoch.
    pub until: i64,
    pub why: String,
}

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
/// would be an open descriptor on the disk and keep it from being unmounted.
#[cfg(target_os = "linux")]
fn take_lock(root_id: &str) -> std::io::Result<UnixListener> {
    UnixListener::bind_addr(&lock_address(root_id)?)
}

/// macOS has no abstract sockets. A lock on a file in the user's library, not in the root, for the same
/// reason; the kernel drops it when the process dies.
#[cfg(target_os = "macos")]
fn take_lock(root_id: &str) -> std::io::Result<std::fs::File> {
    let home = std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("HOME is not set"))?;
    let dir = Path::new(&home).join("Library/Application Support/Cairn");
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::File::create(dir.join(format!("daemon-{root_id}.lock")))?;
    match crate::fsops::try_lock(&file)? {
        true => Ok(file),
        false => Err(std::io::ErrorKind::AddrInUse.into()),
    }
}

/// As on macOS: a lock on a file in the user's local application data, not in the root, which the
/// kernel drops when the process dies.
#[cfg(windows)]
fn take_lock(root_id: &str) -> std::io::Result<std::fs::File> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| std::io::Error::other("LOCALAPPDATA is not set"))?;
    let dir = Path::new(&base).join("Cairn");
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

/// What the daemon of this root last said, if one is running.
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
                    if wake.send(Wake::Remote(head)).is_err() {
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
            "the pause after the last sync, twice as long as its scan"
        } else {
            "for the files to be quiet"
        };
        Some((due, why))
    }

    fn note(&mut self, wake: Wake, synced_head: u64) {
        match wake {
            Wake::Local => {
                let now = Instant::now();
                self.first_event.get_or_insert(now);
                self.last_event = Some(now);
            }
            // An answer to a question asked before this device's own last commit.
            Wake::Remote(head) => self.remote |= head != synced_head,
            Wake::ServerBack => {
                self.retry_at = None;
                self.remote = true;
            }
            Wake::FeedDown => {}
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

/// Run until the process ends. Comes back only for what no retry would cure.
pub fn run(path: &Path, opts: &DaemonOptions, log: &mut dyn FnMut(&str)) -> Result<()> {
    let (wake_tx, wake_rx) = channel();
    let synced_head = Arc::new(AtomicU64::new(0));
    let feed_down = Arc::new(Mutex::new(None::<String>));

    let watcher = if opts.watch {
        let tx = wake_tx.clone();
        match Watcher::start(path, move || {
            let _ = tx.send(Wake::Local);
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
    };
    let mut status = Status {
        pid: std::process::id(),
        ..Status::default()
    };
    let mut said: BTreeSet<String> = BTreeSet::new();
    let mut said_counts = WatchCounts::default();
    let mut said_feed: Option<String> = None;
    let mut not_applied: Vec<NotApplied> = Vec::new();
    let mut last_error: Option<String> = None;
    let mut walk_first = true;

    loop {
        let wait = schedule.due().saturating_duration_since(Instant::now());
        match wake_rx.recv_timeout(wait) {
            Ok(wake) => {
                schedule.note(wake, synced_head.load(Ordering::SeqCst));
                while let Ok(more) = wake_rx.try_recv() {
                    schedule.note(more, synced_head.load(Ordering::SeqCst));
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
                        write_status(path, &status);
                    }
                    continue;
                }
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
                    Ok(l) => lock = Some(l),
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
        hit("daemon.sync");
        let result = sync(path, &SyncOptions::default());
        hit("daemon.synced");
        // Every sync repeats its scan; the rest of a long one was moving data, which the next does not
        // do again. Twice the scan keeps a tree that never rests from keeping the daemon scanning more
        // than a third of the time, and a change after a big download waits about a scan, not twice
        // the download.
        let repeated = match &result {
            Ok(report) => report.scan_time,
            Err(_) => started.elapsed(),
        };
        schedule.gap_until = Instant::now() + MIN_GAP.max(repeated * 2);

        status.syncs += 1;
        status.last_sync = now_secs();
        let mut lines = BTreeSet::new();
        match result {
            Ok(report) => {
                last_error = None;
                walk_first = false;
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
            log(&format!(
                "{} folders could not be watched, so changes in them wait for a full sync",
                counts.unwatched
            ));
        }
        said_counts = counts;
        status.watched = counts.watched;
        status.unwatched = counts.unwatched;
        status.watch_limit_reached = counts.limit_reached;

        say_feed(&feed_down, &mut said_feed, log);
        status.feed_down.clone_from(&said_feed);
        write_status(path, &status);

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
