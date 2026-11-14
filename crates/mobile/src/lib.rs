//! The core of Cairn's phone apps, called from Swift (and later Kotlin) through uniffi.
//!
//! A phone keeps no replica of a folder: a namespace can hold hundreds of thousands of objects, and an
//! app extension has little memory. It lists a folder when it is opened, downloads a file when it is
//! opened, and follows the change feed from the head for what the Files app has seen. Its writes are
//! new files and folders alone: a photo sent in one request (`PUT /file`), which the server stores and
//! commits as this device, and a folder made by a commit of its own.
//!
//! Every call blocks; Swift calls them off the main thread.

uniffi::setup_scaffolding!();

pub mod folder;
pub mod fp;
pub mod naming;
pub mod session;
pub mod uploads;

pub use folder::Folder;
pub use session::Session;
pub use uploads::Store;

use cairn_protocol::{ObjectId, RevisionId};

/// Why a call did not do what it was asked, in words a person acts on.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CairnError {
    /// The server answered and refused, with its reason.
    #[error("{reason}")]
    Refused { status: u16, reason: String },
    /// No such object, or none this token reaches.
    #[error("{reason}")]
    NotFound { reason: String },
    /// The server could not be reached, or the connection was lost: trying again is safe.
    #[error("{reason}")]
    Unreachable { reason: String },
    /// Asked to stop, and stopped.
    #[error("stopped")]
    Cancelled,
    /// Anything else, here on the phone: its disk, its database, a bad argument.
    #[error("{reason}")]
    Failed { reason: String },
}

impl From<anyhow::Error> for CairnError {
    fn from(e: anyhow::Error) -> Self {
        let reason = format!("{e:#}");
        if cairn_engine::client::is_not_found(&e) {
            return CairnError::NotFound { reason };
        }
        if cairn_engine::client::is_forbidden(&e) {
            return CairnError::Refused {
                status: 403,
                reason,
            };
        }
        if reason.contains("cannot reach the server") {
            return CairnError::Unreachable { reason };
        }
        CairnError::Failed { reason }
    }
}

impl From<rusqlite::Error> for CairnError {
    fn from(e: rusqlite::Error) -> Self {
        CairnError::Failed {
            reason: format!("the phone's database: {e}"),
        }
    }
}

impl From<std::io::Error> for CairnError {
    fn from(e: std::io::Error) -> Self {
        CairnError::Failed {
            reason: e.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, CairnError>;

pub(crate) fn bad(reason: impl Into<String>) -> CairnError {
    CairnError::Failed {
        reason: reason.into(),
    }
}

/// An ID from Swift's text: the hex the server writes.
pub(crate) fn object_of(s: &str) -> Result<ObjectId> {
    if s == "root" {
        return Ok(ObjectId::ROOT);
    }
    s.parse()
        .map_err(|_| bad(format!("{s:?} is not an object ID")))
}

pub(crate) fn revision_of(s: &str) -> Result<RevisionId> {
    s.parse()
        .map_err(|_| bad(format!("{s:?} is not a revision ID")))
}

/// A request for something else to send: the Photos system, a player streaming a video.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<Header>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// Told how far a transfer is; answering false stops it.
#[uniffi::export(with_foreign)]
pub trait TransferProgress: Send + Sync {
    fn progress(&self, done: u64, total: u64) -> bool;
}

/// A new device's ID, made once when the app is first signed in and kept.
#[uniffi::export]
pub fn new_device_id() -> String {
    cairn_protocol::DeviceId::random().to_string()
}

/// The version of this library, the workspace's.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
