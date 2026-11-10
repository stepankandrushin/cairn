//! Server storage: immutable content-addressed chunks, and a journal behind a narrow trait so
//! PostgreSQL can later run the same suite as SQLite.

pub mod chunks;
pub mod codec;
pub mod fsck;
pub mod meta;
pub mod migrate;
pub mod repo;
pub mod sqlite;

pub use chunks::{ChunkStore, FsChunkStore};
pub use meta::{Child, CommitOutcome, Head, MetaStore, Revisioned, ShareRow, TokenInfo, UserInfo};
pub use repo::Repo;
pub use sqlite::{token_hash, token_id, SqliteStore, FIRST_USER, SCHEMA};

/// ENOSPC and EDQUOT, whose numbers differ between Unixes.
#[cfg(target_os = "macos")]
const DISK_FULL: &[i32] = &[28, 69];
#[cfg(all(unix, not(target_os = "macos")))]
const DISK_FULL: &[i32] = &[28, 122];
/// ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL and ERROR_DISK_QUOTA_EXCEEDED.
#[cfg(windows)]
const DISK_FULL: &[i32] = &[112, 39, 1295];

/// True when an error chain ends in a full disk or quota, so callers can report it as such.
pub fn is_disk_full(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return io.raw_os_error().is_some_and(|c| DISK_FULL.contains(&c));
        }
        if let Some(rusqlite::Error::SqliteFailure(f, _)) = e.downcast_ref::<rusqlite::Error>() {
            return f.code == rusqlite::ErrorCode::DiskFull;
        }
        false
    })
}
