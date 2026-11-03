//! What a client did not sync, why, and what to do about it (invariant 8).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipKind {
    Fifo,
    Socket,
    CharDevice,
    BlockDevice,
    MountPoint,
    PermissionDenied,
    KeepsChanging,
    VanishedDuringScan,
    CannotApply,
    /// A name on the server this filesystem cannot hold. Not retried until something changes the names.
    CannotHoldName,
    /// The server has something at a path where this device has something it excludes.
    ExcludedInTheWay,
    ReadError,
}

impl SkipKind {
    /// Whether another run could succeed without the user changing the tree's shape.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            SkipKind::PermissionDenied
                | SkipKind::KeepsChanging
                | SkipKind::VanishedDuringScan
                | SkipKind::CannotApply
                | SkipKind::ExcludedInTheWay
                | SkipKind::ReadError
        )
    }

    /// Something still being written or moved, which its own next change or close announces: no
    /// reason to hold back anything else.
    pub fn in_motion(self) -> bool {
        matches!(self, SkipKind::KeepsChanging | SkipKind::VanishedDuringScan)
    }

    pub fn reason(self) -> &'static str {
        match self {
            SkipKind::Fifo => "named pipes are not synced",
            SkipKind::Socket => "sockets are not synced",
            SkipKind::CharDevice => "character devices are not synced",
            SkipKind::BlockDevice => "block devices are not synced",
            SkipKind::MountPoint => {
                "another filesystem is mounted here; sync does not cross mounts"
            }
            SkipKind::PermissionDenied => "waiting for permission",
            SkipKind::KeepsChanging => "file keeps changing",
            SkipKind::VanishedDuringScan => "vanished while being scanned",
            SkipKind::CannotApply => "destination cannot represent this",
            SkipKind::CannotHoldName => "this filesystem cannot hold this name",
            SkipKind::ExcludedInTheWay => "something this device excludes is at that name",
            SkipKind::ReadError => "could not be read",
        }
    }

    pub fn action(self) -> &'static str {
        match self {
            SkipKind::Fifo | SkipKind::Socket | SkipKind::CharDevice | SkipKind::BlockDevice => {
                "move it out of the sync root to stop this report"
            }
            SkipKind::MountPoint => "sync that filesystem as its own root",
            SkipKind::PermissionDenied => {
                "grant read access, or run with access to it, and sync again"
            }
            SkipKind::KeepsChanging => "sync again once it has stopped changing",
            SkipKind::VanishedDuringScan => "sync again",
            SkipKind::CannotApply => "the server keeps the data; read it with `cairn export`",
            SkipKind::CannotHoldName => {
                "the server and other devices keep it, and it comes here once renamed there or the name in its way here is gone; read it with `cairn export`"
            }
            SkipKind::ExcludedInTheWay => {
                "move what is here, or change the rule, and the server's will come"
            }
            SkipKind::ReadError => "check the disk, then sync again",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Skip {
    /// Printable path relative to the root.
    pub path: String,
    pub kind: SkipKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.kind.reason())?;
        if let Some(detail) = &self.detail {
            write!(f, " ({detail})")?;
        }
        write!(f, "; {}", self.kind.action())
    }
}
