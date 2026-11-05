//! The sync engine: everything a client does to a real filesystem and to the server.

pub mod apply;
pub mod chunker;
pub mod client;
pub mod control;
pub mod daemon;
pub mod export;
#[cfg_attr(windows, path = "fsops_windows.rs")]
pub mod fsops;
pub mod identify;
pub mod merge;
pub mod metadata;
pub mod registry;
pub mod restore;
pub mod root;
pub mod scan;
pub mod schedule;
pub mod selection;
pub mod state;
pub mod sync;
pub mod twins;
#[cfg_attr(target_os = "macos", path = "watch_fsevents.rs")]
#[cfg_attr(windows, path = "watch_windows.rs")]
pub mod watch;

pub use metadata::{ApplyPolicy, OwnerPolicy};
pub use root::{init_root, InitOptions, Root};
pub use sync::{sync, SyncOptions, SyncOutcome, SyncReport};

/// A full disk or quota, anywhere in an error chain.
pub fn is_disk_full(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(fsops::is_disk_full)
    })
}
