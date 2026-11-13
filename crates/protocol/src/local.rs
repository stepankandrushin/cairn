//! The local API: what a device's daemons tell the programs that show them (`cairn status --all`, the
//! tray), and the few things those may ask. One definition on every side, so none of them drops a field
//! it does not know. It never leaves the device; docs/protocol.md has the wire to the server.
//!
//! Each daemon serves it on its own local socket, [`control_address`]: one JSON object per line each
//! way, a [`Request`] in and [`Reply`] lines out. Only the user the daemon runs as may connect. A program
//! that shows a daemon holds no sync logic: what changes a tree goes through the daemon or the CLI, or is
//! an ordinary file operation a person could make, such as removing a conflict copy.
//!
//! The roots of this user on this device are listed in the registry, [`registry_path`], which
//! `cairn init` and a starting daemon add to. A root listed there that has gone is shown as gone, never
//! dropped quietly.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Seconds since the epoch, as everywhere in this module.
pub type Secs = i64;

/// The registry: every root of this user on this device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub roots: Vec<RootEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootEntry {
    pub path: PathBuf,
    pub root_id: String,
    /// The server's base URL, for "open in the browser".
    pub server: String,
    pub added: Secs,
}

/// One root as `cairn status --all --json` shows it: its entry in the registry, whether it is still
/// there, and what its daemon says, if one runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootView {
    pub root: RootEntry,
    /// The folder still holds this root: its `.cairn/root-id` reads as the entry's ID.
    pub present: bool,
    pub daemon: Option<DaemonStatus>,
}

/// What a daemon is up to: `.cairn/daemon.json`, and what it sends for [`Request::Status`] and
/// [`Request::Subscribe`]. Fields are only ever added, with a default, so an older reader keeps working.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    /// `synced`, `partial`, `paused` or `error`; empty while it starts.
    pub state: String,
    pub detail: Option<String>,
    pub last_sync: Secs,
    pub last_synced: Secs,
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
    /// Since when a person has paused this daemon. A pause outlives a restart, and lasts until
    /// [`Request::Resume`]; while it lasts `state` is `paused`.
    #[serde(default)]
    pub paused_since: Option<Secs>,
    /// A sync is running now. Said only on the local API: the file is written after each sync, so
    /// there it is always false.
    #[serde(default)]
    pub syncing: bool,
    /// While `syncing`, what that sync is doing, how far it has got, and when it last moved: a sync
    /// that takes an hour tells itself from a stuck one by this, not by how long it has run.
    #[serde(default)]
    pub progress: Option<Progress>,
    /// The last changes this daemon sent or applied, newest first, at most [`MAX_RECENT`].
    #[serde(default)]
    pub recent: Vec<Activity>,
    /// The root, its ID and its server, so a reader needs nothing else to show it.
    #[serde(default)]
    pub root: PathBuf,
    #[serde(default)]
    pub root_id: String,
    #[serde(default)]
    pub server: String,
    /// The daemon's version, `CARGO_PKG_VERSION`.
    #[serde(default)]
    pub version: String,
    /// What this device's updater knows: the same for every daemon of this user here, as there is one
    /// updater. `None` from a daemon from before updates.
    #[serde(default)]
    pub update: Option<crate::update::Status>,
}

pub const MAX_NOT_APPLIED: usize = 200;
pub const MAX_RECENT: usize = 100;

/// What part of a sync is running. A reader that meets a part it does not know shows it as
/// [`Phase::Other`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Reading what this device knows: its state, the ledger, or rebuilding the ledger.
    #[default]
    Loading,
    /// Taking what the server's journal has since the last sync.
    Journal,
    /// Looking at every entry in the tree, or at what changed, and reading the files that are new
    /// or changed here, to hash them.
    Scanning,
    /// Matching what it found with the server's tree and working out what to do.
    Deciding,
    /// Downloading and placing what the server has, and removing and moving what it says.
    Applying,
    /// Sending the server the content it does not have yet.
    Uploading,
    /// Sending the changes made here as commits.
    Committing,
    /// Recording what was done, for the next sync.
    Recording,
    #[serde(other)]
    Other,
}

/// What the sync running now is doing: [`DaemonStatus::progress`]. Counts start at nought with each
/// sync; `entries` with each scan, the others go on across its rounds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// When the sync began.
    pub started: Secs,
    /// Its round, from 1: a round that changed something is followed by one that checks it.
    pub round: u32,
    pub phase: Phase,
    /// When it came to that phase.
    pub phase_since: Secs,
    /// Entries the scan running now, or the last one, has looked at, and how many the tree had at
    /// the last sync, as a guide to how many it will; 0 where the scan looks at part of the tree.
    pub entries: u64,
    pub entries_known: u64,
    /// The files this sync has read to hash them, and their bytes: what makes a scan long, as when a
    /// folder is taken in that the last sync left out.
    pub files_read: u64,
    pub bytes_read: u64,
    /// The file being read now, relative to the root, and its size.
    #[serde(default)]
    pub reading: Option<String>,
    #[serde(default)]
    pub reading_size: u64,
    /// The phase's own count of what it has to do, where it knows it: steps applied, chunks uploaded
    /// or commits sent. `total` is 0 where it is not known.
    pub done: u64,
    pub total: u64,
    /// Bytes downloaded and uploaded so far in this sync, and how many it has found it has to.
    pub downloaded: u64,
    pub to_download: u64,
    pub uploaded: u64,
    pub to_upload: u64,
    /// When any count above last went up, or the phase or the file read changed.
    pub moved: Secs,
}

impl Phase {
    fn doing(self) -> &'static str {
        match self {
            Phase::Loading => "loading what this device knows",
            Phase::Journal => "taking the server's journal",
            Phase::Scanning => "looking at the tree",
            Phase::Deciding => "working out what to do",
            Phase::Applying => "applying the server's changes here",
            Phase::Uploading => "uploading",
            Phase::Committing => "sending commits",
            Phase::Recording => "recording what it did",
            Phase::Other => "busy",
        }
    }
}

impl Progress {
    /// The phase and how far it has got in it, as "looking at the tree, 120,345 of about 556,010
    /// entries".
    pub fn doing(&self) -> String {
        let mut line = self.phase.doing().to_string();
        let count = |n: u64| group(n);
        match self.phase {
            Phase::Scanning if self.entries_known >= self.entries.max(1) => {
                line += &format!(
                    ", {} of about {} entries",
                    count(self.entries),
                    count(self.entries_known)
                )
            }
            Phase::Scanning if self.entries == 1 => line += ", 1 entry",
            Phase::Scanning => line += &format!(", {} entries", count(self.entries)),
            Phase::Journal if self.done > 0 => {
                line += &format!(", {} operations", count(self.done))
            }
            Phase::Applying if self.total > 0 => {
                line += &format!(", {} of {} steps", count(self.done), count(self.total))
            }
            Phase::Uploading if self.total > 0 => {
                line += &format!(", {} of {} chunks", count(self.done), count(self.total))
            }
            Phase::Committing if self.total > 0 => {
                line += &format!(", {} of {}", count(self.done), count(self.total))
            }
            _ => {}
        }
        line
    }

    /// Short enough for a tray's line: what it is doing and for how long.
    pub fn brief(&self, now: Secs) -> String {
        format!("{}, for {}", self.doing(), span(now - self.started))
    }

    /// What it is doing, in one line, as `cairn status`, the tray and the daemon's log say it: how long
    /// the sync has run, what it is at and how far it has got there, what it has read and moved, and
    /// how long since anything moved.
    pub fn describe(&self, now: Secs) -> String {
        let mut line = format!("for {}", span(now - self.started));
        if self.round > 1 {
            line += &format!(", round {}", self.round);
        }
        line += ": ";
        line += &self.doing();
        line += &format!(", at it for {}", span(now - self.phase_since));
        if self.files_read > 0 || self.reading.is_some() {
            line += &format!(
                "; read {} file(s), {}, to hash them",
                group(self.files_read),
                bytes(self.bytes_read)
            );
        }
        if let Some(path) = &self.reading {
            line += &format!(", now {path} ({})", bytes(self.reading_size));
        }
        if self.to_download > 0 || self.downloaded > 0 {
            line += &format!(
                "; downloaded {} of {}",
                bytes(self.downloaded),
                bytes(self.to_download.max(self.downloaded))
            );
        }
        if self.to_upload > 0 || self.uploaded > 0 {
            line += &format!(
                "; uploaded {} of {}",
                bytes(self.uploaded),
                bytes(self.to_upload.max(self.uploaded))
            );
        }
        line += &format!("; last moved {} ago", span(now - self.moved));
        line
    }
}

/// A count with its thousands apart, as 556,010.
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A size as a person reads it.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} bytes")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// A length of time as a person reads it: 45 s, 3 min 12 s, 1 h 4 min.
pub fn span(secs: Secs) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotApplied {
    pub path: String,
    pub text: String,
    pub since: Secs,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    pub until: Secs,
    pub why: String,
}

/// One change a sync made: sent from here (`up`) or applied here (`down`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub time: Secs,
    pub direction: Direction,
    pub change: Change,
    /// Relative to the root, as `cairn status` prints it.
    pub path: String,
    /// Where it went, for [`Change::Moved`].
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Added,
    Changed,
    /// Only its metadata.
    Metadata,
    Moved,
    Removed,
    /// A conflict copy was made for it.
    Conflict,
}

/// What a program may ask a daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// One [`Reply::Status`], then the daemon closes.
    Status,
    /// A [`Reply::Status`] now and again on every change, until the program closes.
    Subscribe,
    /// Look now: the same as an event from the watcher, so it only brings the next sync forward.
    SyncNow,
    Pause,
    Resume,
    /// The open conflicts, from the server's inbox, with where their copies are on this device.
    Conflicts,
    /// Ask the feed now and install what it offers, whatever the policy: `done` at once, and the
    /// status's `update` says how it went. Every program updated restarts into the new version, this
    /// daemon too, between syncs.
    UpdateNow,
    /// What was deleted in the last `days` days, [`DEFAULT_DELETED_DAYS`] if not said, and is still
    /// deleted: [`Reply::Deleted`]. The daemon reads the server's journal for it, all of it the first
    /// time and then what is new, so a first answer can take a while.
    Deleted {
        #[serde(default)]
        days: Option<u32>,
    },
    /// Bring back what [`Reply::Deleted`] listed, by its `id`, as new commits: [`Reply::Undeleted`],
    /// and then a sync, which brings it here.
    Undelete {
        ids: Vec<String>,
    },
    /// An address that opens this root's folder on the server's web page, signed in as this root's
    /// token without typing it: [`Reply::SignInLink`]. It signs a browser in once, within two minutes.
    SignInLink,
}

pub const DEFAULT_DELETED_DAYS: u32 = 30;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Status(Box<DaemonStatus>),
    /// The request is taken; the status that follows shows its effect.
    Done,
    Conflicts {
        open: Vec<OpenConflict>,
    },
    /// Newest first: a folder once, with what was deleted with it.
    Deleted {
        /// The journal's sequence the list is as of.
        head: u64,
        items: Vec<DeletedItem>,
    },
    Undeleted {
        /// The commits made, in order: one unless it was too large for one.
        seqs: Vec<u64>,
        items: Vec<UndeletedItem>,
        /// Asked for but not deleted now, each where it is, relative to the root.
        already: Vec<String>,
    },
    SignInLink {
        url: String,
    },
    Error {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenConflict {
    pub seq: u64,
    pub time: Secs,
    pub device_name: String,
    /// The copy on this device, if it is here (excluded or not held, it is not).
    pub copy: Option<PathBuf>,
    /// The version it competes with, on this device, if that is here and has not been deleted.
    pub original: Option<PathBuf>,
    /// Both relative to the root, as `cairn conflicts` prints them, for when a path above is `None`.
    pub copy_display: String,
    pub original_display: Option<String>,
}

/// Something deleted and still deleted, from the server's history: a file, or a folder with what was
/// deleted with it. `cairn deleted --json` prints the same.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedItem {
    /// What [`Request::Undelete`] and `cairn undelete` take: the object's ID.
    pub id: String,
    /// The commit that deleted it, and when, by which device.
    pub seq: u64,
    pub time: Secs,
    pub device_name: String,
    /// Relative to the root: in its folder as that is now, or as it was if that is deleted too.
    pub path: String,
    pub kind: crate::ObjectKind,
    /// A file's size; for a folder, the bytes of the files deleted with it.
    pub bytes: u64,
    /// For a folder, how many files were deleted with it.
    pub files: u64,
    /// The folder it was in is deleted too, and comes back with it, empty of anything else.
    pub folder_gone: bool,
}

/// One thing brought back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndeletedItem {
    pub id: String,
    /// Where it is now, relative to the root.
    pub path: String,
    /// Where it was when deleted, which is where it is unless `renamed`.
    pub was: String,
    /// Its name had been taken since, so it is back beside what has it now, under a restored name.
    pub renamed: bool,
    /// What came back with it, itself and any folder above it included.
    pub objects: u64,
}

/// Where a daemon listens, for the root with this ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalAddress {
    /// Linux: a name in the abstract socket namespace, the daemon's lock as well, so a socket that
    /// answers is a daemon that is running, and nothing is left on a disk.
    Abstract(String),
    /// macOS: a socket file in the user's library, beside the daemon's lock.
    Path(PathBuf),
    /// Windows: a named pipe, `\\.\pipe\...`, which only the daemon's user may open.
    Pipe(String),
}

pub fn control_address(root_id: &str) -> Option<LocalAddress> {
    #[cfg(target_os = "linux")]
    {
        Some(LocalAddress::Abstract(format!("cairn-daemon-{root_id}")))
    }
    #[cfg(target_os = "macos")]
    {
        // A socket path is at most 104 bytes on macOS, so the name is the ID's first 64 bits: about
        // 65 bytes in all for a short user name.
        let short = root_id.get(..16).unwrap_or(root_id);
        user_dir().map(|d| LocalAddress::Path(d.join(format!("{short}.sock"))))
    }
    #[cfg(windows)]
    {
        Some(LocalAddress::Pipe(format!(
            r"\\.\pipe\cairn-daemon-{root_id}"
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = root_id;
        None
    }
}

/// `roots.json` in the user's state directory: `$XDG_STATE_HOME/cairn` or `~/.local/state/cairn` on
/// Linux, `~/Library/Application Support/Cairn` on macOS, `%LOCALAPPDATA%\Cairn` on Windows.
pub fn registry_path() -> Option<PathBuf> {
    user_dir().map(|d| d.join("roots.json"))
}

/// The per-user directory of the registry, and on macOS and Windows of the daemons' locks and sockets.
/// `CAIRN_USER_DIR` puts it elsewhere, for tests: every program that is to find the same daemons needs
/// the same one.
pub fn user_dir() -> Option<PathBuf> {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(d) = var("CAIRN_USER_DIR") {
        return Some(d);
    }
    #[cfg(target_os = "macos")]
    {
        var("HOME").map(|h| h.join("Library/Application Support/Cairn"))
    }
    #[cfg(windows)]
    {
        var("LOCALAPPDATA").map(|d| d.join("Cairn"))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        var("XDG_STATE_HOME")
            .map(|d| d.join("cairn"))
            .or_else(|| var("HOME").map(|h| h.join(".local/state/cairn")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_are_one_line_of_json_each() {
        let r: Request = serde_json::from_str(r#"{"op":"sync_now"}"#).unwrap();
        assert_eq!(r, Request::SyncNow);
        let s = serde_json::to_string(&Reply::Status(Box::default())).unwrap();
        assert!(s.starts_with(r#"{"reply":"status","pid":0"#) && !s.contains('\n'));
    }

    #[test]
    fn a_status_from_before_these_fields_still_reads() {
        let old = r#"{"pid":7,"state":"synced","detail":null,"last_sync":1,"last_synced":1,"head":3,
            "syncs":1,"watched":2,"unwatched":0,"watch_limit_reached":false,"feed_down":null,
            "skips":[],"waiting":null}"#;
        let s: DaemonStatus = serde_json::from_str(old).unwrap();
        assert_eq!((s.pid, s.paused_since, s.recent.len()), (7, None, 0));
        assert_eq!(s.progress, None);
    }

    #[test]
    fn a_long_scan_says_how_far_it_has_got_and_when_it_last_moved() {
        let p = Progress {
            started: 1000,
            round: 1,
            phase: Phase::Scanning,
            phase_since: 1010,
            entries: 120_345,
            entries_known: 556_010,
            files_read: 13_201,
            bytes_read: 129_277_427_712,
            reading: Some("photo/a.mov".into()),
            reading_size: 1_288_490_189,
            moved: 4240,
            ..Progress::default()
        };
        assert_eq!(
            p.describe(4241),
            "for 54 min 1 s: looking at the tree, 120,345 of about 556,010 entries, at it for 53 min 51 s; \
             read 13,201 file(s), 120.4 GiB, to hash them, now photo/a.mov (1.2 GiB); last moved 1 s ago"
        );
        let up = Progress {
            started: 0,
            round: 2,
            phase: Phase::Uploading,
            phase_since: 3600,
            done: 3,
            total: 10,
            uploaded: 3 << 20,
            to_upload: 10 << 20,
            moved: 3700,
            ..Progress::default()
        };
        assert_eq!(
            up.describe(4300),
            "for 1 h 11 min, round 2: uploading, 3 of 10 chunks, at it for 11 min 40 s; uploaded 3.0 MiB \
             of 10.0 MiB; last moved 10 min 0 s ago"
        );
    }

    #[test]
    fn a_phase_from_a_newer_daemon_still_reads() {
        let p: Phase = serde_json::from_str(r#""pondering""#).unwrap();
        assert_eq!(p, Phase::Other);
        let s: Phase = serde_json::from_str(r#""scanning""#).unwrap();
        assert_eq!(s, Phase::Scanning);
    }
}
