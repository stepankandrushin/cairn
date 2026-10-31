//! Server storage: immutable content-addressed chunks, and a journal behind a narrow trait so
//! PostgreSQL can later run the same suite as SQLite.

pub mod chunks;
pub mod fsck;
pub mod meta;
pub mod repo;
pub mod sqlite;

pub use chunks::{ChunkStore, FsChunkStore};
pub use meta::{CommitOutcome, MetaStore};
pub use repo::Repo;
pub use sqlite::SqliteStore;

/// True when an error chain ends in a full disk or quota, so callers can report it as such.
pub fn is_disk_full(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return matches!(io.raw_os_error(), Some(28) | Some(122));
        }
        if let Some(rusqlite::Error::SqliteFailure(f, _)) = e.downcast_ref::<rusqlite::Error>() {
            return f.code == rusqlite::ErrorCode::DiskFull;
        }
        false
    })
}
