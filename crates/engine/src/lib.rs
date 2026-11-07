//! The sync engine: everything a client does to a real filesystem and to the server.

pub mod apply;
pub mod chunker;
pub mod client;
pub mod control;
pub mod daemon;
pub mod deleted;
pub mod export;
#[cfg_attr(windows, path = "fsops_windows.rs")]
pub mod fsops;
pub mod identify;
pub mod incr;
pub mod merge;
pub mod metadata;
pub mod progress;
pub mod rebuild;
pub mod record;
pub mod refusal;
pub mod registry;
pub mod restore;
pub mod root;
pub mod scan;
pub mod schedule;
pub mod scope;
pub mod selection;
pub mod state;
pub mod steady;
pub mod sync;
pub mod twins;
pub mod update;
#[cfg_attr(target_os = "macos", path = "watch_fsevents.rs")]
#[cfg_attr(windows, path = "watch_windows.rs")]
pub mod watch;

pub use metadata::{ApplyPolicy, OwnerPolicy};
pub use root::{init_root, InitOptions, Root};
pub use sync::{sync, SyncOptions, SyncOutcome, SyncReport};

/// A full disk or quota, anywhere in an error chain, the state database's own included.
pub fn is_disk_full(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        if let Some(rusqlite::Error::SqliteFailure(f, _)) = e.downcast_ref::<rusqlite::Error>() {
            return f.code == rusqlite::ErrorCode::DiskFull;
        }
        e.downcast_ref::<std::io::Error>()
            .is_some_and(fsops::is_disk_full)
    })
}

/// A unit test that cannot run here, for `why`: it says so and passes, and the caller returns. Where
/// `CAIRN_NO_SKIP` is set, as in GitHub's CI, it fails instead, as the harness's `skip` does.
#[cfg(test)]
#[track_caller]
pub(crate) fn skip(why: &str) {
    if std::env::var_os("CAIRN_NO_SKIP").is_some_and(|v| !v.is_empty() && v != "0") {
        panic!("{why}, and CAIRN_NO_SKIP says every test must run here");
    }
    eprintln!("{why}: skipped");
}
