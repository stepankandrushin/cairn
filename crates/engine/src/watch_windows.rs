//! Telling the daemon that something under a root changed, on Windows. As with inotify and FSEvents,
//! which paths does not matter: any event leads to a whole sync, whose scan is what decides what
//! changed, so that a lost event or a wrong path here can only delay a change, never lose one.
//!
//! `ReadDirectoryChangesW` watches a whole tree from one handle, so there is no walk, no limit on
//! watches and no directory that could go unwatched. The first read is issued before `start` returns,
//! and the system queues changes for the handle from then on, so whatever is written before the first
//! sync is found by it and whatever comes after is an event. A queue that overflowed says so with an
//! empty answer, which is a wake like any other.
//!
//! The handle is on the folder it was opened on, wherever that folder goes, and nothing comes when the
//! folder itself is renamed. So a second watch, on the folder above, looks at the root's path whenever
//! a folder there is made, removed or renamed: the root renamed away, or another folder put at its
//! path, is a wake at once, and the root's watch moves to what is at the path now, as a walk does with
//! inotify. So does each walk the daemon asks for. A root that is a drive's own has nothing above it,
//! and a folder further up renamed is found by the reconcile, as on Linux.
//!
//! Unlike inotify, this holds the root and the folder above it open while the daemon runs. Both share
//! deleting and renaming, so nothing a person does to either is refused for it.

use crate::fsops::{fstat, is_not_found, AsFd, Dir, FileType};
use crate::scan::display_path;
use crate::selection::{Rules, IGNORE_FILE};
use cairn_failpoint::hit;
use cairn_protocol::{Name, STATE_DIR};
use std::collections::HashSet;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::ReadDirectoryChangesW;
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const FILE_LIST_DIRECTORY: u32 = 0x1;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const SYNCHRONIZE: u32 = 0x10_0000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x20_0000;
const ERROR_DIRECTORY: i32 = 267;

const NOTIFY: u32 = 0x1 // file name
    | 0x2 // directory name
    | 0x4 // attributes
    | 0x8 // size
    | 0x10 // last write
    | 0x40 // creation
    | 0x100; // security
/// In the folder above: a folder made, removed or renamed, which is all a root's own rename is there.
const NOTIFY_ABOVE: u32 = 0x2;

const ACTION_MODIFIED: u32 = 3;
const ERROR_IO_PENDING: i32 = 997;
const ERROR_NOTIFY_ENUM_DIR: i32 = 1022;
/// What one read can hold; also the most a network share answers with.
const BUFFER_WORDS: usize = 8192;

/// How much of the tree events can be expected from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchCounts {
    pub watched: usize,
    /// Directories that could not be watched, whose changes wait for the next full sync: here only the
    /// folder above the root, whose watch is what notices the root renamed.
    pub unwatched: usize,
    /// The kernel's limit on watches was reached. Windows has none.
    pub limit_reached: bool,
    /// Reading events failed, and no more will come until the root is watched again.
    pub stopped: bool,
}

struct Inner {
    root: PathBuf,
    rules: Rules,
    wake: Box<dyn Fn() + Send>,
    /// Files the last sync found still being written, by their printable paths: their writes wake
    /// nothing, as the daemon comes back for them anyway, though their creation or rename does.
    in_motion: HashSet<String>,
}

fn load_rules(root: &Path) -> Rules {
    // A sync refuses rules it cannot read. Here the worst of no rules is some extra wakes.
    Dir::open_root(root)
        .ok()
        .and_then(|root| root.open_dir(STATE_DIR).ok())
        .and_then(|state| Rules::load(&state).ok())
        .unwrap_or_else(Rules::none)
}

impl Inner {
    /// Whether a change at this path, relative to the root, means a sync. A name that cannot be
    /// understood does: the scan decides.
    fn matters(&mut self, rel: &[u16], action: u32) -> bool {
        let parts: Vec<Vec<u8>> = rel
            .split(|&c| c == u16::from(b'\\'))
            .map(|p| {
                crate::fsops::os_name_bytes(&std::ffi::OsString::from_wide_lossless(p))
                    .unwrap_or_default()
            })
            .collect();
        if parts.first().map(Vec::as_slice) == Some(STATE_DIR) {
            // Everything else in there is the sync's own, and moves all the time.
            if parts.len() == 2 && parts[1] == IGNORE_FILE {
                self.rules = load_rules(&self.root);
                return true;
            }
            return false;
        }
        let names: Result<Vec<Name>, _> = parts.into_iter().map(Name::new).collect();
        let Ok(names) = names else {
            return true;
        };
        if action == ACTION_MODIFIED && self.in_motion.contains(&display_path(&names)) {
            return false;
        }
        // Nothing under an excluded directory can be brought back, as in git, so a busy build folder
        // that is ignored never keeps the daemon scanning. The event does not say whether the entry
        // itself is a directory.
        !(1..=names.len()).any(|n| self.rules.excludes(&names[..n], n < names.len()))
    }

    /// The root's events, as the read brought them.
    fn root_events(&mut self, bytes: &[u8]) -> bool {
        let mut wake = false;
        let mut at = 0usize;
        while at + 12 <= bytes.len() {
            let word = |o: usize| u32::from_le_bytes(bytes[at + o..at + o + 4].try_into().unwrap());
            let (next, action, len) = (word(0) as usize, word(4), word(8) as usize);
            let end = (at + 12 + len).min(bytes.len());
            let name: Vec<u16> = bytes[at + 12..end]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            wake |= self.matters(&name, action);
            if next == 0 {
                break;
            }
            at += next;
        }
        wake
    }
}

trait FromWideLossless {
    fn from_wide_lossless(w: &[u16]) -> Self;
}

impl FromWideLossless for std::ffi::OsString {
    fn from_wide_lossless(w: &[u16]) -> Self {
        std::os::windows::ffi::OsStringExt::from_wide(w)
    }
}

/// What a completed read brought.
enum Got {
    Events(Vec<u8>),
    /// More changed than the queue held.
    Overflow,
}

/// One folder watched: its handle, the read outstanding on it, and which folder it is. The buffer and
/// OVERLAPPED stay where they are until the read completes, which dropping waits for.
struct Watch {
    file: std::fs::File,
    buf: Box<[u64; BUFFER_WORDS]>,
    at: Box<OVERLAPPED>,
    /// Set when the read completes.
    done: OwnedHandle,
    /// The volume and file ID, to tell this folder from another put at its path.
    id: (u64, u64),
    subtree: bool,
    filter: u32,
    pending: bool,
}

// SAFETY: one thread at a time owns a watch, the one that looked or the watcher's after it, and only the
// owner touches the read. Its buffer and OVERLAPPED are boxed: moving the watch moves nothing the system
// writes to.
unsafe impl Send for Watch {}

fn event() -> std::io::Result<OwnedHandle> {
    // SAFETY: a new manual-reset event, owned by what it is returned to.
    let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if h.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

fn raw(h: &impl AsRawHandle) -> HANDLE {
    h.as_raw_handle() as HANDLE
}

fn is_set(h: &OwnedHandle) -> bool {
    // SAFETY: the event is open; a zero timeout only looks.
    unsafe { WaitForSingleObject(raw(h), 0) == WAIT_OBJECT_0 }
}

impl Watch {
    /// The folder at this path, not read from yet. The root is opened as itself, never through a link,
    /// as a sync opens it; the folder above is wherever its path leads, as that is where the root's
    /// name is.
    fn open(path: &Path, subtree: bool, filter: u32, follow: bool) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        let flags = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED;
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            .share_mode(0x7)
            .custom_flags(if follow {
                flags
            } else {
                flags | FILE_FLAG_OPEN_REPARSE_POINT
            })
            .open(path)?;
        let st = fstat(file.as_fd())?;
        if st.file_type != FileType::Directory {
            return Err(std::io::Error::from_raw_os_error(ERROR_DIRECTORY));
        }
        Ok(Watch {
            file,
            buf: Box::new([0u64; BUFFER_WORDS]),
            at: Box::new(unsafe { std::mem::zeroed() }),
            done: event()?,
            id: (st.dev, st.ino),
            subtree,
            filter,
            pending: false,
        })
    }

    fn issue(&mut self) -> std::io::Result<()> {
        *self.at = unsafe { std::mem::zeroed() };
        self.at.hEvent = raw(&self.done);
        // SAFETY: the buffer and OVERLAPPED are boxed and outlive the read, which is waited for before
        // either is touched again.
        let ok = unsafe {
            ResetEvent(raw(&self.done));
            ReadDirectoryChangesW(
                raw(&self.file),
                self.buf.as_mut_ptr().cast(),
                (BUFFER_WORDS * 8) as u32,
                i32::from(self.subtree),
                self.filter,
                std::ptr::null_mut(),
                &mut *self.at,
                None,
            )
        };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_IO_PENDING) {
                return Err(e);
            }
        }
        self.pending = true;
        Ok(())
    }

    /// What the completed read brought, with the next read issued before it is looked at. An error
    /// means no more will come from this handle.
    fn take(&mut self) -> std::io::Result<Got> {
        let mut got = 0u32;
        // SAFETY: the OVERLAPPED is the one the read was issued with, and the read has completed.
        let ok = unsafe { GetOverlappedResult(raw(&self.file), &*self.at, &mut got, 0) };
        self.pending = false;
        let got = if ok == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_NOTIFY_ENUM_DIR) {
                return Err(e);
            }
            Got::Overflow
        } else if got == 0 {
            Got::Overflow
        } else {
            // SAFETY: the read wrote this many bytes into the buffer.
            let all =
                unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), got as usize) };
            Got::Events(all.to_vec())
        };
        self.issue()?;
        Ok(got)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if self.pending {
            let mut got = 0u32;
            // SAFETY: the read is this handle's own; the buffer is freed only once it has ended.
            unsafe {
                CancelIoEx(raw(&self.file), &*self.at);
                GetOverlappedResult(raw(&self.file), &*self.at, &mut got, 1);
            }
        }
    }
}

/// What the latest look at each path found, under one lock. The daemon's walk and the thread's own
/// looks both leave a new watch here, its read already issued, for the thread to take and wait on; the
/// counts are read from here. So a reader stopped at a failpoint, or busy, holds up neither a walk nor
/// the daemon's look at the counts.
struct Places {
    root: PathBuf,
    above: Option<PathBuf>,
    /// The folder the root's watch is on, held by the thread or about to be.
    root_id: Option<(u64, u64)>,
    /// A watch the thread is to take in place of its own; `Some(None)` for none.
    new_root: Option<Option<Watch>>,
    /// Why what is at the root's path could not be watched.
    root_failed: Option<String>,
    above_id: Option<(u64, u64)>,
    new_above: Option<Option<Watch>>,
    above_failed: Option<String>,
    /// The thread has ended: nothing more comes.
    gone: bool,
}

impl Places {
    /// Look at what is at the root's path: whether that is another folder than the one watched, or
    /// none, either of which is news for a sync. What is gone is no failure, as the sync then says the
    /// root has gone; what is there and cannot be watched is.
    fn look_root(&mut self) -> std::io::Result<bool> {
        let current = self.root_id;
        let opened = Watch::open(&self.root, true, NOTIFY, false).and_then(|mut w| {
            if current != Some(w.id) {
                w.issue()?;
            }
            Ok(w)
        });
        match opened {
            Ok(w) => {
                self.root_failed = None;
                if current == Some(w.id) {
                    return Ok(false);
                }
                self.root_id = Some(w.id);
                self.new_root = Some(Some(w));
                Ok(true)
            }
            Err(e) => {
                let had = self.root_id.take().is_some();
                if had {
                    self.new_root = Some(None);
                }
                if is_not_found(&e) {
                    self.root_failed = None;
                    Ok(had)
                } else {
                    self.root_failed = Some(e.to_string());
                    Err(e)
                }
            }
        }
    }

    fn look_above(&mut self) {
        let Some(above) = self.above.clone() else {
            return;
        };
        let current = self.above_id;
        let opened = Watch::open(&above, false, NOTIFY_ABOVE, true).and_then(|mut w| {
            if current != Some(w.id) {
                w.issue()?;
            }
            Ok(w)
        });
        match opened {
            Ok(w) => {
                self.above_failed = None;
                if current != Some(w.id) {
                    self.above_id = Some(w.id);
                    self.new_above = Some(Some(w));
                }
            }
            Err(e) => {
                if self.above_id.take().is_some() {
                    self.new_above = Some(None);
                }
                // Gone with the root, which the sync says.
                self.above_failed = (!is_not_found(&e)).then(|| e.to_string());
            }
        }
    }

    fn counts(&self) -> WatchCounts {
        WatchCounts {
            watched: usize::from(self.root_id.is_some()),
            unwatched: usize::from(self.above_failed.is_some()),
            limit_reached: false,
            stopped: self.gone || (self.root_id.is_none() && self.root_failed.is_some()),
        }
    }
}

struct Shared {
    places: Mutex<Places>,
    /// Set when there is a new watch for the thread to take.
    control: OwnedHandle,
}

/// Watches a root until the process ends.
pub struct Watcher {
    inner: Arc<Mutex<Inner>>,
    shared: Arc<Shared>,
}

impl Watcher {
    pub fn start(root: &Path, wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let root = std::path::absolute(root)?;
        let mut places = Places {
            above: root.parent().map(Path::to_path_buf),
            root: root.clone(),
            root_id: None,
            new_root: None,
            root_failed: None,
            above_id: None,
            new_above: None,
            above_failed: None,
            gone: false,
        };
        // A root that cannot be watched at all is said once, and the daemon goes by the reconcile.
        places.look_root()?;
        places.look_above();
        let inner = Arc::new(Mutex::new(Inner {
            rules: load_rules(&root),
            root,
            wake: Box::new(wake),
            in_motion: HashSet::new(),
        }));
        let shared = Arc::new(Shared {
            places: Mutex::new(places),
            control: event()?,
        });
        let (on_inner, on_shared) = (inner.clone(), shared.clone());
        std::thread::Builder::new()
            .name("cairn-watch".into())
            .spawn(move || {
                watch(&on_shared, &on_inner);
                // Whatever ended the thread, nothing more comes.
                on_shared
                    .places
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .gone = true;
                (on_inner.lock().unwrap_or_else(|e| e.into_inner()).wake)();
            })?;
        Ok(Self { inner, shared })
    }

    /// The rules read again, and the watches placed from the root's path as it is now, their reads
    /// issued before this returns: a watch is placed before the sync it leads to.
    pub fn walk_all(&self) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.rules = load_rules(&inner.root);
        }
        let mut places = self.shared.places.lock().unwrap_or_else(|e| e.into_inner());
        places.look_above();
        let _ = places.look_root();
        if places.new_root.is_some() || places.new_above.is_some() {
            // SAFETY: the event is open as long as the shared state is.
            unsafe { SetEvent(raw(&self.shared.control)) };
        }
    }

    /// The files the last sync found still being written, by their printable paths.
    pub fn set_in_motion(&self, paths: HashSet<String>) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_motion = paths;
    }

    /// Folders that could not be watched: at most the one above the root, as the tree is watched whole.
    pub fn missed(&self) -> Vec<String> {
        let places = self.shared.places.lock().unwrap_or_else(|e| e.into_inner());
        match (&places.above, &places.above_failed) {
            (Some(above), Some(why)) => vec![format!(
                "{}, the folder above the root, so the root renamed is noticed only by the full sync ({why})",
                above.display()
            )],
            _ => Vec::new(),
        }
    }

    pub fn counts(&self) -> WatchCounts {
        self.shared
            .places
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counts()
    }
}

/// The watcher's thread, which owns the watches it waits on: until a wait fails, which nothing here can
/// cure.
fn watch(shared: &Shared, inner: &Mutex<Inner>) {
    let (mut on_root, mut on_above): (Option<Watch>, Option<Watch>) = (None, None);
    loop {
        {
            let mut places = shared.places.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(w) = places.new_root.take() {
                on_root = w;
            }
            if let Some(w) = places.new_above.take() {
                on_above = w;
            }
        }
        let mut handles = vec![raw(&shared.control)];
        handles.extend(on_root.iter().map(|w| raw(&w.done)));
        handles.extend(on_above.iter().map(|w| raw(&w.done)));
        // SAFETY: each handle is open until the wait returns, as only this thread closes them.
        let r =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
        if r >= WAIT_OBJECT_0 + handles.len() as u32 {
            return;
        }
        if is_set(&shared.control) {
            // SAFETY: the event is open as long as the shared state is. What it announced is taken at
            // the top.
            unsafe { ResetEvent(raw(&shared.control)) };
        }
        // Each read that is done, whichever woke the wait: a busy tree must not keep the folder above
        // unread.
        let (mut events, mut overflow, mut root_failed) = (None, false, None);
        if let Some(on) = on_root.as_mut().filter(|w| is_set(&w.done)) {
            match on.take() {
                Ok(Got::Events(bytes)) => events = Some(bytes),
                Ok(Got::Overflow) => overflow = true,
                Err(e) => root_failed = Some(e),
            }
        }
        let (mut above_news, mut above_failed) = (false, None);
        if let Some(on) = on_above.as_mut().filter(|w| is_set(&w.done)) {
            match on.take() {
                Ok(_) => above_news = true,
                Err(e) => above_failed = Some(e),
            }
        }
        let mut wake = false;
        if root_failed.is_some() || above_news || above_failed.is_some() {
            let mut places = shared.places.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(e) = root_failed {
                // Its folder is going, or cannot be read from now: that is for a sync to look at, and
                // until the root is watched again it is the reconcile that finds changes. Unless a newer
                // look has put another watch in its place already.
                let id = on_root.take().map(|w| w.id);
                if id.is_some() && id == places.root_id {
                    places.root_id = None;
                    places.root_failed = Some(e.to_string());
                }
                wake = true;
            }
            if let Some(e) = &above_failed {
                let id = on_above.take().map(|w| w.id);
                if id.is_some() && id == places.above_id {
                    places.above_id = None;
                    places.above_failed = Some(e.to_string());
                }
            }
            if above_news || above_failed.is_some() {
                // Which name the event was about does not matter: what is at the root's path now is
                // what is looked at, so no form of its name, in any case, can be missed.
                wake |= places.look_root().unwrap_or(true);
            }
        }
        if events.is_some() || overflow {
            // Before the lock, as on Linux and macOS: a reader paused here holds nothing the daemon
            // takes.
            hit("daemon.watch.read");
        }
        let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
        if overflow {
            hit("daemon.watch.overflow");
            inner.rules = load_rules(&inner.root);
            wake = true;
        }
        if let Some(bytes) = events {
            wake |= inner.root_events(&bytes);
        }
        if wake {
            (inner.wake)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};
    use std::time::Duration;

    /// Whether a wake came within this long, with any more that came by then taken too.
    fn woken(rx: &Receiver<()>, within: Duration) -> bool {
        let got = rx.recv_timeout(within).is_ok();
        std::thread::sleep(Duration::from_millis(200));
        while rx.try_recv().is_ok() {}
        got
    }

    /// Renamed away, the root is news at once, where its own watch says nothing; a folder made beside it
    /// is none; and another folder put at its path is the one watched from then on, as is one made there
    /// again the moment the last is deleted.
    #[test]
    fn a_root_renamed_or_replaced_is_noticed_and_watched_where_it_is_now() {
        let top = std::env::temp_dir().join(format!("cairn-watch-{:016x}", rand::random::<u64>()));
        let root = top.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let (tx, rx) = channel();
        let w = Watcher::start(&root, move || {
            let _ = tx.send(());
        })
        .unwrap();
        let soon = Duration::from_secs(5);
        let quiet = Duration::from_secs(1);
        std::fs::write(root.join("f"), "x").unwrap();
        assert!(woken(&rx, soon), "a write in the root");

        std::fs::create_dir(top.join("beside")).unwrap();
        std::fs::rename(top.join("beside"), top.join("beside2")).unwrap();
        assert!(!woken(&rx, quiet), "a folder beside the root is no news");

        std::fs::rename(&root, top.join("away")).unwrap();
        assert!(woken(&rx, soon), "the root renamed away");
        assert_eq!(w.counts().watched, 0, "nothing is at its path");
        assert!(!w.counts().stopped);
        std::fs::write(top.join("away").join("g"), "x").unwrap();
        assert!(
            !woken(&rx, quiet),
            "a write in the folder that went is no news"
        );

        std::fs::create_dir(&root).unwrap();
        assert!(woken(&rx, soon), "another folder at the root's path");
        assert_eq!(w.counts().watched, 1);
        std::fs::write(root.join("h"), "x").unwrap();
        assert!(woken(&rx, soon), "a write in the folder now at the path");

        // The watch keeps no name taken: a folder deleted while it is held is gone at once.
        std::fs::remove_file(root.join("h")).unwrap();
        std::fs::remove_dir(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        w.walk_all();
        while rx.try_recv().is_ok() {}
        std::fs::write(root.join("i"), "x").unwrap();
        assert!(woken(&rx, soon), "a write in the folder made again");
        assert!(w.missed().is_empty(), "{:?}", w.missed());
        assert_eq!(w.counts().unwatched, 0);
        let _ = std::fs::remove_dir_all(&top);
    }
}
