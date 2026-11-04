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
}

pub const MAX_NOT_APPLIED: usize = 200;
pub const MAX_RECENT: usize = 100;

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Status(Box<DaemonStatus>),
    /// The request is taken; the status that follows shows its effect.
    Done,
    Conflicts {
        open: Vec<OpenConflict>,
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
    }
}
