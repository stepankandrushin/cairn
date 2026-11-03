//! Telling the daemon that something under a root changed, on macOS. As with inotify, which paths does
//! not matter: any event leads to a whole sync, whose scan is what decides what changed, so that a lost
//! event or a wrong path here can only delay a change, never lose one.
//!
//! FSEvents watches a whole tree from one stream, so there is no walk, no limit on watches and no
//! directory that could go unwatched. It holds no descriptor in the root either. The stream starts
//! before the first sync, so whatever was written before it is found by that sync.

use crate::fsops::Dir;
use crate::scan::display_path;
use crate::selection::{Rules, IGNORE_FILE};
use cairn_failpoint::hit;
use cairn_protocol::{Name, STATE_DIR};
use std::collections::HashSet;
use std::ffi::{c_char, c_void, CStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type CFIndex = isize;
type Stream = *mut c_void;

#[repr(C)]
struct StreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type Callback =
    extern "C" fn(Stream, *mut c_void, usize, *const *const c_char, *const u32, *const u64);

const SINCE_NOW: u64 = u64::MAX;
const UTF8: u32 = 0x0800_0100;

const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_WATCH_ROOT: u32 = 0x04;
/// The daemon's own writes need no wake: nothing else is in them, and the sync that made them saw them.
const CREATE_IGNORE_SELF: u32 = 0x08;
const CREATE_FILE_EVENTS: u32 = 0x10;

const EVENT_MUST_SCAN: u32 = 0x01;
const EVENT_USER_DROPPED: u32 = 0x02;
const EVENT_KERNEL_DROPPED: u32 = 0x04;
const EVENT_ROOT_CHANGED: u32 = 0x20;
const EVENT_MOUNT: u32 = 0x40;
const EVENT_UNMOUNT: u32 = 0x80;
const EVENT_ITEM_CREATED: u32 = 0x100;
const EVENT_ITEM_REMOVED: u32 = 0x200;
const EVENT_ITEM_RENAMED: u32 = 0x800;
const EVENT_IS_DIR: u32 = 0x0002_0000;

/// Gathers a burst into one wake. The first event of a quiet spell is delivered at once (no defer).
const LATENCY_SECONDS: f64 = 0.1;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithBytes(
        alloc: *const c_void,
        bytes: *const u8,
        len: CFIndex,
        encoding: u32,
        external: u8,
    ) -> *const c_void;
    fn CFArrayCreate(
        alloc: *const c_void,
        values: *const *const c_void,
        len: CFIndex,
        callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(what: *const c_void);
    static kCFTypeArrayCallBacks: c_void;
}

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn FSEventStreamCreate(
        alloc: *const c_void,
        callback: Callback,
        context: *const StreamContext,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> Stream;
    fn FSEventStreamSetDispatchQueue(stream: Stream, queue: *mut c_void);
    fn FSEventStreamStart(stream: Stream) -> u8;
}

extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
}

/// How much of the tree events can be expected from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchCounts {
    pub watched: usize,
    /// Directories that could not be watched, whose changes wait for the next full sync.
    pub unwatched: usize,
    /// The kernel's limit on watches was reached. FSEvents has none.
    pub limit_reached: bool,
    /// Reading events failed, and no more will come.
    pub stopped: bool,
}

struct Inner {
    /// Its real path, since that is what events are named by.
    root: PathBuf,
    rules: Rules,
    wake: Box<dyn Fn() + Send>,
    /// Files the last sync found still being written, by their printable paths: their writes wake
    /// nothing, as the daemon comes back for them anyway. FSEvents says nothing of a close.
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
    /// Whether this event means a sync. A name that cannot be understood does: the scan decides.
    fn matters(&mut self, path: &[u8], flags: u32) -> bool {
        let dropped = EVENT_MUST_SCAN
            | EVENT_USER_DROPPED
            | EVENT_KERNEL_DROPPED
            | EVENT_ROOT_CHANGED
            | EVENT_MOUNT
            | EVENT_UNMOUNT;
        if flags & dropped != 0 {
            hit("daemon.watch.overflow");
            self.rules = load_rules(&self.root);
            return true;
        }
        let Ok(rel) = Path::new(std::ffi::OsStr::from_bytes(path)).strip_prefix(&self.root) else {
            return true;
        };
        let parts: Vec<&[u8]> = rel.iter().map(|p| p.as_bytes()).collect();
        if parts.first() == Some(&STATE_DIR) {
            // Everything else in there is the sync's own, and moves all the time.
            if parts.len() == 2 && parts[1] == IGNORE_FILE {
                self.rules = load_rules(&self.root);
                return true;
            }
            return false;
        }
        let names: Result<Vec<Name>, _> = parts.iter().map(|p| Name::new(p.to_vec())).collect();
        let Ok(names) = names else {
            return true;
        };
        let is_dir = flags & EVENT_IS_DIR != 0;
        let only_written =
            flags & (EVENT_ITEM_CREATED | EVENT_ITEM_REMOVED | EVENT_ITEM_RENAMED) == 0;
        if only_written && !is_dir && self.in_motion.contains(&display_path(&names)) {
            return false;
        }
        // A busy log file that is ignored must not keep the daemon scanning.
        names.is_empty() || !self.rules.excludes(&names, is_dir)
    }
}

extern "C" fn on_events(
    _: Stream,
    info: *mut c_void,
    count: usize,
    paths: *const *const c_char,
    flags: *const u32,
    _: *const u64,
) {
    // SAFETY: `info` is the Mutex leaked in `start`, and the arrays hold `count` entries for this call.
    let inner = unsafe { &*(info as *const Mutex<Inner>) };
    let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
    hit("daemon.watch.read");
    let mut wake = false;
    for i in 0..count {
        let (path, flag) = unsafe { (CStr::from_ptr(*paths.add(i)).to_bytes(), *flags.add(i)) };
        wake |= inner.matters(path, flag);
    }
    if wake {
        (inner.wake)();
    }
}

/// Watches a root until the process ends.
pub struct Watcher {
    inner: Arc<Mutex<Inner>>,
}

impl Watcher {
    pub fn start(root: &Path, wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let real = std::fs::canonicalize(root)?;
        let inner = Arc::new(Mutex::new(Inner {
            rules: load_rules(&real),
            root: real.clone(),
            wake: Box::new(wake),
            in_motion: HashSet::new(),
        }));
        let refused = || std::io::Error::other("FSEvents refused the stream");
        let bytes = real.as_os_str().as_bytes();
        // SAFETY: each call gets what its header asks for. The stream and its queue live until the
        // process ends, so the reference given to them is never released.
        unsafe {
            let path = CFStringCreateWithBytes(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len() as CFIndex,
                UTF8,
                0,
            );
            if path.is_null() {
                return Err(refused());
            }
            let values = [path];
            let paths = CFArrayCreate(
                std::ptr::null(),
                values.as_ptr(),
                1,
                std::ptr::addr_of!(kCFTypeArrayCallBacks).cast(),
            );
            CFRelease(path);
            if paths.is_null() {
                return Err(refused());
            }
            let context = StreamContext {
                version: 0,
                info: Arc::into_raw(inner.clone()) as *mut c_void,
                retain: std::ptr::null(),
                release: std::ptr::null(),
                copy_description: std::ptr::null(),
            };
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                on_events,
                &context,
                paths,
                SINCE_NOW,
                LATENCY_SECONDS,
                CREATE_NO_DEFER | CREATE_WATCH_ROOT | CREATE_IGNORE_SELF | CREATE_FILE_EVENTS,
            );
            CFRelease(paths);
            if stream.is_null() {
                return Err(refused());
            }
            let queue = dispatch_queue_create(c"cairn.watch".as_ptr(), std::ptr::null());
            FSEventStreamSetDispatchQueue(stream, queue);
            if FSEventStreamStart(stream) == 0 {
                return Err(refused());
            }
        }
        Ok(Self { inner })
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

    pub fn counts(&self) -> WatchCounts {
        WatchCounts {
            watched: 1,
            ..WatchCounts::default()
        }
    }
}
