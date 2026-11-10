//! Types shared by the Cairn server, CLI and engine: one definition, so no component can quietly
//! strip a field it does not understand. See docs/protocol.md and docs/format.md.

pub mod ids;
pub mod local;
pub mod manifest;
pub mod metadata;
pub mod name;
pub mod ops;
pub mod record;
pub mod skip;
pub mod tree;
pub mod update;
pub mod web;
pub mod wire;

pub use ids::{DeviceId, Hash, ObjectId, OpId, RevisionId};
pub use manifest::{ChunkRef, Manifest, Segment};
pub use metadata::{
    Bytes, Metadata, NamedStream, Owner, RecordKey, Timestamp, MODE_MASK, NS_LINUX_XATTR,
    NS_MACOS_FINDER_INFO, NS_MACOS_RESOURCE_FORK, NS_MACOS_XATTR, NS_WINDOWS_ACL, NS_WINDOWS_ADS,
    NS_WINDOWS_ATTRIBUTES,
};
pub use name::{conflict_name, in_git_dir, is_git_dir, FilenamePolicy, Name, NameError};
pub use ops::{
    check_and_apply, ConflictOf, Content, Link, ObjectKind, Op, OpReject, RevisionBody, TreeStore,
};
pub use skip::{Skip, SkipKind};
pub use tree::{Node, Tree};

/// Bumped when an older peer could not safely interpret the wire format.
pub const PROTOCOL_VERSION: u32 = 1;

/// The state directory at the top of a sync root: the one name a root cannot sync.
pub const STATE_DIR: &[u8] = b".cairn";

/// Prefix of transient names in a batch that swaps or cycles entries; never survives a commit.
pub const STAGE_PREFIX: &str = ".cairn-stage-";
