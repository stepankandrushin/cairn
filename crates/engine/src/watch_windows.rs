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
//! Unlike inotify, this holds the root open while the daemon runs. It shares deleting and renaming, so
//! nothing a person does to the folder is refused for it.

use crate::fsops::{AsFd, Dir};
use crate::scan::display_path;
use crate::selection::{Rules, IGNORE_FILE};
use cairn_failpoint::hit;
use cairn_protocol::{Name, STATE_DIR};
use std::collections::HashSet;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::ReadDirectoryChangesW;
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

const FILE_LIST_DIRECTORY: u32 = 0x1;
const SYNCHRONIZE: u32 = 0x10_0000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x20_0000;

const NOTIFY: u32 = 0x1 // file name
    | 0x2 // directory name
    | 0x4 // attributes
    | 0x8 // size
    | 0x10 // last write
    | 0x40 // creation
    | 0x100; // security

const ACTION_MODIFIED: u32 = 3;
const ERROR_IO_PENDING: i32 = 997;
const ERROR_NOTIFY_ENUM_DIR: i32 = 1022;
/// What one read can hold; also the most a network share answers with.
const BUFFER_WORDS: usize = 8192;

/// How much of the tree events can be expected from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchCounts {
    pub watched: usize,
    /// Directories that could not be watched, whose changes wait for the next full sync.
    pub unwatched: usize,
    /// The kernel's limit on watches was reached. Windows has none.
    pub limit_reached: bool,
    /// Reading events failed, and no more will come.
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
}

trait FromWideLossless {
    fn from_wide_lossless(w: &[u16]) -> Self;
}

impl FromWideLossless for std::ffi::OsString {
    fn from_wide_lossless(w: &[u16]) -> Self {
        std::os::windows::ffi::OsStringExt::from_wide(w)
    }
}

/// One outstanding read: its buffer and OVERLAPPED stay where they are until it completes.
struct Read {
    buf: Box<[u64; BUFFER_WORDS]>,
    at: Box<OVERLAPPED>,
}

// SAFETY: the OVERLAPPED's event handle is null, and the read is only ever touched by the thread that
// owns it once `start` hands it over.
unsafe impl Send for Read {}

fn issue(dir: HANDLE, read: &mut Read) -> std::io::Result<()> {
    *read.at = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer and OVERLAPPED are boxed and outlive the read, which is waited for before
    // either is touched again.
    let ok = unsafe {
        ReadDirectoryChangesW(
            dir,
            read.buf.as_mut_ptr().cast(),
            (BUFFER_WORDS * 8) as u32,
            1,
            NOTIFY,
            std::ptr::null_mut(),
            &mut *read.at,
            None,
        )
    };
    if ok == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_IO_PENDING) {
            return Err(e);
        }
    }
    Ok(())
}

/// Watches a root until the process ends.
pub struct Watcher {
    inner: Arc<Mutex<Inner>>,
    stopped: Arc<AtomicBool>,
}

impl Watcher {
    pub fn start(root: &Path, wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | SYNCHRONIZE)
            .share_mode(0x7)
            .custom_flags(
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED | FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .open(root)?;
        let inner = Arc::new(Mutex::new(Inner {
            rules: load_rules(root),
            root: root.to_path_buf(),
            wake: Box::new(wake),
            in_motion: HashSet::new(),
        }));
        let stopped = Arc::new(AtomicBool::new(false));
        let mut read = Read {
            buf: Box::new([0u64; BUFFER_WORDS]),
            at: Box::new(unsafe { std::mem::zeroed() }),
        };
        let dir = file.as_fd().as_raw_handle() as HANDLE;
        issue(dir, &mut read)?;
        let (shared, done) = (inner.clone(), stopped.clone());
        std::thread::Builder::new()
            .name("cairn-watch".into())
            .spawn(move || {
                // The handle lives as long as this thread, which is as long as the process.
                let file = file;
                let dir = file.as_fd().as_raw_handle() as HANDLE;
                loop {
                    let mut got = 0u32;
                    // SAFETY: the OVERLAPPED is the one the outstanding read was issued with.
                    let ok = unsafe { GetOverlappedResult(dir, &*read.at, &mut got, 1) };
                    let overflow = if ok == 0 {
                        let e = std::io::Error::last_os_error();
                        if e.raw_os_error() != Some(ERROR_NOTIFY_ENUM_DIR) {
                            done.store(true, Ordering::SeqCst);
                            (shared.lock().unwrap_or_else(|e| e.into_inner()).wake)();
                            return;
                        }
                        true
                    } else {
                        got == 0
                    };
                    // Copy out what came before reading again into the same buffer.
                    let bytes: Vec<u8> = {
                        let all = unsafe {
                            std::slice::from_raw_parts(read.buf.as_ptr().cast::<u8>(), got as usize)
                        };
                        all.to_vec()
                    };
                    if issue(dir, &mut read).is_err() {
                        done.store(true, Ordering::SeqCst);
                        (shared.lock().unwrap_or_else(|e| e.into_inner()).wake)();
                        return;
                    }
                    // Before the lock, as on Linux and macOS: a reader paused holding it would also
                    // stop `walk_all`.
                    hit("daemon.watch.read");
                    let mut inner = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let mut wake = false;
                    if overflow {
                        hit("daemon.watch.overflow");
                        inner.rules = load_rules(&inner.root);
                        wake = true;
                    } else {
                        let mut at = 0usize;
                        while at + 12 <= bytes.len() {
                            let word = |o: usize| {
                                u32::from_le_bytes(bytes[at + o..at + o + 4].try_into().unwrap())
                            };
                            let (next, action, len) = (word(0) as usize, word(4), word(8) as usize);
                            let end = (at + 12 + len).min(bytes.len());
                            let name: Vec<u16> = bytes[at + 12..end]
                                .chunks_exact(2)
                                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                .collect();
                            wake |= inner.matters(&name, action);
                            if next == 0 {
                                break;
                            }
                            at += next;
                        }
                    }
                    if wake {
                        (inner.wake)();
                    }
                }
            })?;
        Ok(Self { inner, stopped })
    }

    /// There are no watches to make, only the rules to read again.
    pub fn walk_all(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.rules = load_rules(&inner.root);
    }

    /// The files the last sync found still being written, by their printable paths.
    pub fn set_in_motion(&self, paths: HashSet<String>) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_motion = paths;
    }

    /// Folders that could not be watched: none, as the tree is watched whole.
    pub fn missed(&self) -> Vec<String> {
        Vec::new()
    }

    pub fn counts(&self) -> WatchCounts {
        WatchCounts {
            watched: 1,
            stopped: self.stopped.load(Ordering::SeqCst),
            ..WatchCounts::default()
        }
    }
}
