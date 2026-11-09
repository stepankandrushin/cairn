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
//! It also serves the local API (control.rs) to `cairn status` and the tray: its status, and the
//! requests that change no tree. Look now is an event like the watcher's; a pause stops it starting a
//! sync until a resume, and outlives a restart, being kept in `.cairn`; update now asks the updater.
//!
//! And it keeps its programs up to date (update.rs): a thread asks the feed every hour or so, and when
//! this program has been replaced, by that or by a person, [`run`] returns between syncs for the daemon
//! to start again on the new one.

use crate::client::{Client, Retries};
use crate::control::{Handler, Hub};
use crate::fsops::Dir;
use crate::root::{read_config, read_root_id, Busy};
use crate::scan::{display_path, STATE_FILE_DAEMON};
use crate::state::Compaction;
use crate::sync::{sync, SyncOptions, SyncOutcome, SyncReport};
use crate::watch::{WatchCounts, Watcher};
use anyhow::{bail, Result};
use cairn_failpoint::hit;
use cairn_protocol::local::{OpenConflict, Reply, Request, DEFAULT_DELETED_DAYS, MAX_RECENT};
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
/// Below any reverse proxy's usual read timeout (nginx: 60 seconds).
const FEED_WAIT: Duration = Duration::from_secs(25);
const LOCK_ATTEMPTS: u32 = 5;
const FEED_MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A shorter wait is tried again at full length this long after the last cut: what cut it may have been
/// a restart, and a real proxy costs one cut per hour.
const FEED_WAIT_RECOVERY: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    /// Between syncs when nothing asks for one, each with a scan of the whole tree.
    pub reconcile: Duration,
    /// Between checks of the ledger the syncs that take what changed alone rest on, against what the
    /// state says: one is due this long after the last whose every decision was the full sync's.
    pub full_sync: Duration,
    /// The longest wait before trying again after a failure, or while paused.
    pub retry: Duration,
    /// Watch the filesystem; without it, only the reconcile finds local changes.
    pub watch: bool,
    /// Long-poll the server; without it, only the reconcile finds remote changes.
    pub feed: bool,
    /// Where a sync that runs long says, from another thread, how far it has got, and one in which
    /// nothing moves says so: the daemon's log, as `log` is. Without it they are only in the status.
    pub say: Option<fn(&str)>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            reconcile: Duration::from_secs(3600),
            full_sync: Duration::from_secs(86_400),
            retry: Duration::from_secs(60),
            watch: true,
            feed: true,
            say: None,
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

/// While a sync runs, how often what it is doing goes to the local API, for `cairn status` and the tray.
const PROGRESS_SHOWN: Duration = Duration::from_secs(2);
/// A sync still running this long after it began says in the log what it is doing, and again every
/// [`PROGRESS_SAID`] after: a scan that reads a folder just taken in can run for an hour, and a log
/// silent for that long reads as a daemon that has hung.
const PROGRESS_FIRST: Duration = Duration::from_secs(60);
const PROGRESS_SAID: Duration = Duration::from_secs(300);
/// A sync in which nothing has moved for this long says so in the log, once, and again when it moves.
const STALLED: Duration = Duration::from_secs(600);

fn tuned(name: &str, default: Duration) -> Duration {
    Duration::from_millis(cairn_failpoint::tuned(name, default.as_millis() as u64))
}

/// The sync running now, as the reporter shows it.
struct Running {
    progress: Arc<crate::progress::Progress>,
    status: Status,
    ticks: u64,
    moved: Instant,
    moved_secs: i64,
    next_line: Instant,
    stalled: bool,
}

/// What the main loop and the reporter share: the sync running now, if one is.
#[derive(Default)]
struct Live {
    running: Mutex<Option<Running>>,
    wake: std::sync::Condvar,
}

impl Live {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A sync begins: shown at once, with what it is doing from then on.
    fn begin(&self, progress: Arc<crate::progress::Progress>, status: &Status, hub: &Hub) {
        let now = Instant::now();
        let running = Running {
            ticks: progress.ticks(),
            progress,
            status: status.clone(),
            moved: now,
            moved_secs: now_secs(),
            next_line: now + tuned("daemon.progress_first_ms", PROGRESS_FIRST),
            stalled: false,
        };
        let mut live = self.lock();
        hub.publish(&running.shown());
        *live = Some(running);
        self.wake.notify_all();
    }

    /// It is over: the reporter shows nothing more of it, and what the main loop shows next stands.
    fn end(&self) {
        *self.lock() = None;
    }
}

impl Running {
    fn shown(&self) -> Status {
        let mut status = self.status.clone();
        status.syncing = true;
        status.progress = Some(self.progress.snapshot(self.moved_secs));
        status
    }
}

/// Shows the sync running now every [`PROGRESS_SHOWN`], and says in the log how a long one goes. The
/// lock is held while it shows, so a sync that has ended is never shown again after the main loop's
/// own status.
fn report_progress(live: Arc<Live>, hub: Arc<Hub>, say: Option<fn(&str)>) {
    let shown_every = tuned("daemon.progress_shown_ms", PROGRESS_SHOWN);
    let said_every = tuned("daemon.progress_said_ms", PROGRESS_SAID);
    let stalled_after = tuned("daemon.stalled_ms", STALLED);
    let mut guard = live.lock();
    loop {
        // Between syncs it sleeps until the next begins: an idle daemon stays idle.
        guard = match guard.is_some() {
            true => live
                .wake
                .wait_timeout(guard, shown_every)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0),
            false => live.wake.wait(guard).unwrap_or_else(|e| e.into_inner()),
        };
        let Some(r) = guard.as_mut() else {
            continue;
        };
        let now = Instant::now();
        let ticks = r.progress.ticks();
        let still = now.duration_since(r.moved);
        if ticks != r.ticks {
            r.ticks = ticks;
            if r.stalled {
                r.stalled = false;
                if let Some(say) = say {
                    say(&format!(
                        "the sync moves again, after {} in which nothing did",
                        cairn_protocol::local::span(still.as_secs() as i64)
                    ));
                }
            }
            r.moved = now;
            r.moved_secs = now_secs();
        }
        let status = r.shown();
        hub.publish(&status);
        let Some(say) = say else {
            continue;
        };
        let Some(progress) = &status.progress else {
            continue;
        };
        if now >= r.next_line {
            say(&format!("still syncing, {}", progress.describe(now_secs())));
            r.next_line = now + said_every;
        }
        if !r.stalled && now.duration_since(r.moved) >= stalled_after {
            r.stalled = true;
            say(&format!(
                "nothing in this sync has moved for {}, so it may be stuck; it has run {}",
                cairn_protocol::local::span(now.duration_since(r.moved).as_secs() as i64),
                progress.describe(now_secs()),
            ));
        }
    }
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
fn handler(
    root: &Path,
    pause: &Arc<Pause>,
    wake: &Sender<Wake>,
    updater: &Arc<crate::update::Worker>,
    hub: &Arc<Hub>,
) -> Arc<Handler> {
    let (root, pause, wake, updater, hub) = (
        root.to_path_buf(),
        pause.clone(),
        wake.clone(),
        updater.clone(),
        Arc::downgrade(hub),
    );
    let deleted = crate::deleted::Cache::new();
    Arc::new(move |request: Request| match request {
        Request::Deleted { days } => {
            let since = crate::deleted::days_ago(days.unwrap_or(DEFAULT_DELETED_DAYS));
            let listed = read_config(&root).and_then(|config| {
                deleted.with(&config, |index, _| Ok((index.head, index.list(since))))
            });
            match listed {
                Ok((head, list)) => Reply::Deleted {
                    head,
                    items: list.iter().map(|d| d.item()).collect(),
                },
                Err(e) => Reply::Error {
                    text: format!("cannot list what was deleted: {e:#}"),
                },
            }
        }
        Request::Undelete { ids } => {
            let done = crate::deleted::parse_ids(&ids).and_then(|ids| {
                let config = read_config(&root)?;
                deleted.with(&config, |index, config| {
                    crate::deleted::undelete(config, index, &ids)
                })
            });
            match done {
                Ok(outcome) => {
                    if !outcome.seqs.is_empty() {
                        // Paused, it waits like any change on the server.
                        if pause.get().is_none() {
                            let _ = wake.send(Wake::Local(Instant::now()));
                        }
                        if let Some(hub) = hub.upgrade() {
                            hub.note(format!(
                                "brought back {} deleted item(s) as sequence(s) {:?}, as asked",
                                outcome.items.len(),
                                outcome.seqs
                            ));
                        }
                    }
                    Reply::Undeleted {
                        seqs: outcome.seqs,
                        items: outcome.items.iter().map(|u| u.item()).collect(),
                        already: outcome
                            .already
                            .iter()
                            .map(|(_, p)| display_path(p))
                            .collect(),
                    }
                }
                Err(e) => Reply::Error {
                    text: format!("nothing was brought back: {e:#}"),
                },
            }
        }
        Request::UpdateNow => {
            updater.nudge();
            Reply::Done
        }
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
                .and_then(|config| Client::new(&config, Retries::Never))
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

/// A sync that listed only some folders saw nothing of what was skipped elsewhere: what the last one
/// said of those stands, but what may have settled since, which the sync after it looks at again.
fn carry_skips(report: &mut SyncReport, last: &[cairn_protocol::Skip]) {
    let Some(looked) = &report.looked_in else {
        return;
    };
    let looked: BTreeSet<&str> = looked.iter().map(String::as_str).collect();
    for s in last {
        let folder = s.path.rsplit_once('/').map_or("", |(f, _)| f);
        if !looked.contains(folder) && !s.kind.retryable() && !report.skips.contains(s) {
            report.skips.push(s.clone());
        }
    }
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
    if let Some(why) = &report.whole_because {
        lines.insert(format!("looked at the whole tree: {why}"));
    } else if let Some(why) = &report.rebuilt_because {
        lines.insert(format!("rebuilt the ledger from the state: {why}"));
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
/// which would otherwise stay this process's while it waits. On macOS 26 and 27 the call below was
/// measured to give back nothing: what does it there is the daemon running with the allocator's cache of
/// large blocks off (`without_large_cache` in the CLI), so that a freed block goes back at once. On
/// Windows, once a big sync has grown the heap, what each later one frees stays committed, a few hundred
/// megabytes on a big tree, until the calls below hand it back.
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
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Memory::{
            GetProcessHeap, HeapCompact, HeapOptimizeResources, HeapSetInformation,
        };
        use windows_sys::Win32::System::SystemServices::{
            HEAP_OPTIMIZE_RESOURCES_CURRENT_VERSION, HEAP_OPTIMIZE_RESOURCES_INFORMATION,
        };
        // Free blocks merged, then the low-fragmentation heap's caches emptied and what is free decommitted,
        // in every heap of the process. The second gives nearly all of it back; merging first lets it give
        // more (on 200k files, 13 MB left against 21). Either failing leaves the memory this process's
        // until the next sync tries again, which is not worth a line a sync.
        let info = HEAP_OPTIMIZE_RESOURCES_INFORMATION {
            Version: HEAP_OPTIMIZE_RESOURCES_CURRENT_VERSION,
            Flags: 0,
        };
        // SAFETY: the process's own heap, which lives as long as it does; a null heap means every heap,
        // and the information is the documented structure, of its own size.
        unsafe {
            HeapCompact(GetProcessHeap(), 0);
            HeapSetInformation(
                std::ptr::null_mut(),
                HeapOptimizeResources,
                (&info as *const HEAP_OPTIMIZE_RESOURCES_INFORMATION).cast(),
                std::mem::size_of_val(&info),
            );
        }
    }
}

/// Run until the process ends. Comes back only for what no retry would cure.
/// Keep this root in sync until something fails that a restart is needed for, which is an error, or
/// this program has been replaced, when it returns `Ok` between syncs, for the caller to start the new
/// one (`update::restart`).
pub fn run(path: &Path, opts: &DaemonOptions, log: &mut dyn FnMut(&str)) -> Result<()> {
    // A version just installed that keeps dying here before a sync is taken back, and this restarts
    // into the one before.
    if let Some(line) = crate::update::daemon_starting(path, log) {
        log(&line);
        return Ok(());
    }
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

    let live = Arc::new(Live::default());
    {
        let (live, hub, say) = (live.clone(), hub.clone(), opts.say);
        std::thread::spawn(move || report_progress(live, hub, say));
    }

    let updater = {
        let tx = wake_tx.clone();
        Arc::new(crate::update::Worker::start(move || {
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
        version: crate::update::running_version(),
        recent: previous_recent(path),
        update: updater.status(),
        ..Status::default()
    };
    let mut was_paused: Option<i64> = None;
    let mut said: BTreeSet<String> = BTreeSet::new();
    let mut said_counts = WatchCounts::default();
    let mut said_feed: Option<String> = None;
    // Why the state was last not compacted, so that a disk short of room for it is said once.
    let mut said_compact: Option<(String, String)> = None;
    let mut not_applied: Vec<NotApplied> = Vec::new();
    let mut last_error: Option<String> = None;
    let mut walk_first = true;
    // When the scan began that the last agreement in this process rests on: the watcher's marks since
    // then are where anything can have changed, if it can be trusted (scope.rs).
    let mut agreed_at: Option<Instant> = None;
    let mut last_skips: Vec<cairn_protocol::Skip> = Vec::new();

    loop {
        for note in hub.take_notes() {
            log(&note);
        }
        for note in updater.take_notes() {
            log(&note);
        }
        // Between syncs, as always here.
        if updater.replaced() {
            log("this program has been replaced, by an update or by hand: restarting into the new one");
            return Ok(());
        }
        updater.waited();
        let update = updater.status();
        if update != status.update {
            status.update = update;
            show(path, &status, &hub);
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
                                    handler(path, &pause, &wake_tx, &updater, &hub),
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
        // The folders marked since the last agreement, where nothing can have been missed: a watcher
        // still reading, every folder watched, not the reconcile, and not the first sync, which places
        // the watches. Otherwise the whole tree.
        let scope = match (&watcher, agreed_at) {
            (Some(w), Some(since)) if !reconcile && !walk_first => {
                let counts = w.counts();
                (!counts.stopped && counts.unwatched == 0 && !counts.limit_reached).then(|| {
                    crate::scope::Source {
                        marks: w.marks(),
                        since,
                    }
                })
            }
            _ => None,
        };
        status.waiting = None;
        // To the local API only, while it runs: writing the file twice a sync would be for nobody.
        let progress = Arc::new(crate::progress::Progress::default());
        live.begin(progress.clone(), &status, &hub);
        hit("daemon.sync");
        let result = sync(
            path,
            &SyncOptions {
                changed_here,
                full_every: Some(opts.full_sync),
                scope,
                progress: Some(progress),
                ..SyncOptions::default()
            },
        );
        live.end();
        hit("daemon.synced");
        // Got through a sync, whatever it found: a version just installed works.
        updater.synced();
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

        status.syncs += 1;
        status.last_sync = now_secs();
        let compact_due = result.as_ref().is_ok_and(|r| r.compact_due);
        let mut lines = BTreeSet::new();
        match result {
            Ok(mut report) => {
                last_error = None;
                walk_first = false;
                // In the log alone, as a compaction is: nothing a person sees changes.
                if let Some(m) = &report.migrated {
                    log(m);
                }
                if let Some(t) = report.agreed_since {
                    agreed_at = Some(t);
                    if let Some(w) = &watcher {
                        crate::scope::lock(&w.marks()).prune(t);
                    }
                }
                carry_skips(&mut report, &last_skips);
                last_skips.clone_from(&report.skips);
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
                        // A file the system refuses, or that fails to read, is that file's alone: it is
                        // tried again on the backoff, and everything else goes on syncing meanwhile.
                        let stuck = report.skips.iter().any(|s| {
                            s.kind.retryable()
                                && !s.kind.in_motion()
                                && !matches!(
                                    s.kind,
                                    SkipKind::PermissionDenied | SkipKind::ReadError
                                )
                        });
                        // In a folder it may only read, what only a commit would settle waits for a
                        // change here or on the server: another try now would find the same.
                        let waits =
                            report.read_only && report.skips.iter().all(|s| !s.kind.retryable());
                        if stuck {
                            schedule.failed(opts.retry);
                        } else if waits {
                            schedule.backoff = Duration::ZERO;
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
                if report.retries > 0 {
                    log(&format!(
                        "tried requests again after the network failed them: {} time(s)",
                        report.retries
                    ));
                }
                if report.checked == Some(true) {
                    log(&format!(
                        "checked the ledger against the state, as every {} seconds: it agrees",
                        opts.full_sync.as_secs()
                    ));
                }
                let long = started.elapsed() >= tuned("daemon.progress_first_ms", PROGRESS_FIRST);
                if report.commits > 0 || report.steps_applied > 0 || long {
                    let took = match (&report.whole_because, report.part) {
                        (None, Some((_, took, of))) => {
                            format!(", looked at {took} of {of} entries")
                        }
                        _ => String::new(),
                    };
                    let looked = match &report.looked_in {
                        Some(folders) => format!(", listed {} folder(s)", folders.len()),
                        None => String::new(),
                    };
                    let read = match report.files_read {
                        0 => String::new(),
                        n => format!(
                            ", read {n} file(s), {}, to hash them",
                            cairn_protocol::local::bytes(report.bytes_read)
                        ),
                    };
                    log(&format!(
                        "synced to sequence {}: {} steps applied here, {} commits pushed, {} bytes uploaded, {} conflict(s){took}{looked}, in {} round(s), scans {:.1} s{read}, took {}",
                        report.head, report.steps_applied, report.commits, report.bytes_uploaded, report.conflicts,
                        report.rounds, report.scan_time.as_secs_f64(),
                        cairn_protocol::local::span(started.elapsed().as_secs() as i64)
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
        // What the sync held, its report with it, is free again: back to the system now rather than this
        // process's while it waits. A change that comes soon after costs the next sync a few page faults.
        give_back_memory();
        hit("daemon.gave_back");
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
            log("the watcher has stopped: changes here are found only by the full sync, and every sync looks at the whole tree");
        } else if counts.unwatched > 0 && said_counts.unwatched == 0 {
            let missed = watcher.as_ref().map(Watcher::missed).unwrap_or_default();
            let more = counts.unwatched.saturating_sub(missed.len());
            log(&format!(
                "{} folders could not be watched, so changes in them wait for a full sync, and every sync looks at the whole tree: {}{}",
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

        // Once the sync is shown, and before the next: in the log alone, as it changes nothing a
        // person sees. One that cannot be done leaves the state as it was, and the next sync tries again.
        if compact_due {
            // By what it is about, as the room a disk has changes from one sync to the next.
            let why = match crate::sync::compact(path) {
                Ok(done @ Compaction::Done { .. }) => {
                    log(&done.to_string());
                    give_back_memory();
                    None
                }
                Ok(Compaction::NotDue(_)) => None,
                Ok(ref short @ Compaction::NoRoom { ref place, .. }) => {
                    Some((place.display().to_string(), short.to_string()))
                }
                Err(e) if e.downcast_ref::<Busy>().is_some() => said_compact.clone(),
                Err(e) => {
                    let line = format!("the state was not compacted, and stays as it was: {e:#}");
                    Some((line.clone(), line))
                }
            };
            if let Some((about, line)) = &why {
                if said_compact.as_ref().map(|(a, _)| a) != Some(about) {
                    log(line);
                }
            }
            said_compact = why;
            hit("daemon.compacted");
        }

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
