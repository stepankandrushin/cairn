//! What the sync running now is doing, for whoever shows it while it runs: the daemon's status and log.
//!
//! The sync's thread counts as it goes, through the [`Progress`] [`install`]ed on it; any other
//! thread reads a [`snapshot`](Progress::snapshot). Counting is an atomic add, and a phase or a file
//! being read a short lock, so it costs nothing a scan would notice. A thread the sync starts for a
//! part of its work counts nothing unless it installs the same one.

use cairn_protocol::local::{Phase, Progress as View, Secs};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn now_secs() -> Secs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as Secs)
        .unwrap_or(0)
}

#[derive(Debug)]
struct At {
    phase: Phase,
    since: Secs,
    reading: Option<(String, u64)>,
}

/// One sync's counts. Shared between its thread and whoever shows it.
#[derive(Debug)]
pub struct Progress {
    started: Secs,
    at: Mutex<At>,
    round: AtomicU32,
    entries: AtomicU64,
    entries_known: AtomicU64,
    files_read: AtomicU64,
    bytes_read: AtomicU64,
    done: AtomicU64,
    total: AtomicU64,
    downloaded: AtomicU64,
    to_download: AtomicU64,
    uploaded: AtomicU64,
    to_upload: AtomicU64,
    /// Goes up with every change above, so a reader can tell a sync that moves from one that does not.
    ticks: AtomicU64,
}

impl Default for Progress {
    fn default() -> Self {
        let now = now_secs();
        Self {
            started: now,
            at: Mutex::new(At {
                phase: Phase::Loading,
                since: now,
                reading: None,
            }),
            round: AtomicU32::new(0),
            entries: AtomicU64::new(0),
            entries_known: AtomicU64::new(0),
            files_read: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            downloaded: AtomicU64::new(0),
            to_download: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            to_upload: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
        }
    }
}

impl Progress {
    fn lock(&self) -> std::sync::MutexGuard<'_, At> {
        self.at.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tick(&self) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
    }

    /// A count that only goes up with movement: two equal readings mean nothing moved between them.
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    pub fn files_read(&self) -> (u64, u64) {
        (
            self.files_read.load(Ordering::Relaxed),
            self.bytes_read.load(Ordering::Relaxed),
        )
    }

    /// The counts now, with `moved` as the reader last saw them move.
    pub fn snapshot(&self, moved: Secs) -> View {
        let at = self.lock();
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        View {
            started: self.started,
            round: self.round.load(Ordering::Relaxed),
            phase: at.phase,
            phase_since: at.since,
            entries: get(&self.entries),
            entries_known: get(&self.entries_known),
            files_read: get(&self.files_read),
            bytes_read: get(&self.bytes_read),
            reading: at.reading.as_ref().map(|(p, _)| p.clone()),
            reading_size: at.reading.as_ref().map_or(0, |(_, s)| *s),
            done: get(&self.done),
            total: get(&self.total),
            downloaded: get(&self.downloaded),
            to_download: get(&self.to_download),
            uploaded: get(&self.uploaded),
            to_upload: get(&self.to_upload),
            moved,
        }
    }

    fn set_phase(&self, phase: Phase) {
        let mut at = self.lock();
        if at.phase != phase {
            at.phase = phase;
            at.since = now_secs();
            at.reading = None;
            self.done.store(0, Ordering::Relaxed);
            self.total.store(0, Ordering::Relaxed);
        }
        drop(at);
        self.tick();
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Progress>>> = const { RefCell::new(None) };
}

/// Counts on this thread go to `p` until the guard is dropped, which puts back what was there.
pub fn install(p: Arc<Progress>) -> Installed {
    Installed(CURRENT.with(|c| c.borrow_mut().replace(p)))
}

pub struct Installed(Option<Arc<Progress>>);

impl Drop for Installed {
    fn drop(&mut self) {
        let before = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = before);
    }
}

fn with(f: impl FnOnce(&Progress)) {
    CURRENT.with(|c| {
        if let Some(p) = c.borrow().as_ref() {
            f(p);
        }
    });
}

pub fn round(n: u32) {
    with(|p| {
        p.round.store(n, Ordering::Relaxed);
        p.tick();
    });
}

pub fn phase(phase: Phase) {
    with(|p| p.set_phase(phase));
}

/// A scan begins, of a tree that held about `known` entries at the last sync: 0 when only part of it
/// is listed.
pub fn scanning(known: u64) {
    with(|p| {
        p.set_phase(Phase::Scanning);
        p.entries.store(0, Ordering::Relaxed);
        p.entries_known.store(known, Ordering::Relaxed);
    });
}

/// The scan has looked at one more entry.
pub fn entry() {
    with(|p| {
        p.entries.fetch_add(1, Ordering::Relaxed);
        p.tick();
    });
}

/// A file's content is to be read, to hash it.
pub fn reading(path: &str, size: u64) {
    with(|p| {
        p.lock().reading = Some((path.to_string(), size));
        p.files_read.fetch_add(1, Ordering::Relaxed);
        p.tick();
    });
}

/// That file's read is over, however it went.
pub fn read_done() {
    with(|p| p.lock().reading = None);
}

/// Bytes read from a file, to hash them.
pub fn read(n: u64) {
    with(|p| {
        p.bytes_read.fetch_add(n, Ordering::Relaxed);
        p.tick();
    });
}

/// How many of the phase's own things there are to do.
pub fn total(n: u64) {
    with(|p| {
        p.total.store(n, Ordering::Relaxed);
        p.tick();
    });
}

/// One more of them done.
pub fn done(n: u64) {
    with(|p| {
        p.done.fetch_add(n, Ordering::Relaxed);
        p.tick();
    });
}

/// So many of them done, as when the phase comes back to the next of them.
pub fn set_done(n: u64) {
    with(|p| {
        p.done.store(n, Ordering::Relaxed);
        p.tick();
    });
}

pub fn to_download(n: u64) {
    with(|p| {
        p.to_download.fetch_add(n, Ordering::Relaxed);
        p.tick();
    });
}

pub fn downloaded(n: u64) {
    with(|p| {
        p.downloaded.fetch_add(n, Ordering::Relaxed);
        p.tick();
    });
}

pub fn to_upload(n: u64) {
    with(|p| {
        p.to_upload.fetch_add(n, Ordering::Relaxed);
        p.tick();
    });
}

pub fn uploaded(n: u64) {
    with(|p| {
        p.uploaded.fetch_add(n, Ordering::Relaxed);
        p.tick();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_go_to_the_installed_one_and_stop_with_its_guard() {
        let p = Arc::new(Progress::default());
        entry();
        {
            let _on = install(p.clone());
            scanning(10);
            entry();
            entry();
            reading("a/b", 7);
            read(7);
            read_done();
            phase(Phase::Applying);
            total(3);
            done(1);
        }
        entry();
        let v = p.snapshot(0);
        assert_eq!((v.entries, v.entries_known), (2, 10));
        assert_eq!((v.files_read, v.bytes_read, v.reading), (1, 7, None));
        assert_eq!((v.phase, v.done, v.total), (Phase::Applying, 1, 3));
        let before = p.ticks();
        assert_eq!(p.ticks(), before);
    }
}
