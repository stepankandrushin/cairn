//! Requests and responses of the HTTP API. See docs/protocol.md.

use crate::ids::{DeviceId, Hash, ObjectId, OpId, RevisionId};
use crate::ops::{Op, OpReject};
use serde::{Deserialize, Serialize};

pub const MAX_CHUNK_BATCH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Info {
    pub protocol: u32,
    pub server_version: String,
    pub head: u64,
    #[serde(default)]
    pub max_batch_bytes: u64,
    /// What the token asking may do in this namespace. Absent from a server before accounts, whose one
    /// token may do everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
    /// That a note may name more objects than one (`Op::Note`'s `also`). Absent from a server
    /// before them, which would keep only the first: a client sends such a server a note to each.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub note_groups: bool,
}

/// What a user may do in a namespace, which a person knows as a folder: each level holds the one
/// before it. No level at all is no access, and the namespace answers as one that does not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Read the tree, its history and its conflicts; change nothing.
    Read,
    /// Also upload and commit.
    Write,
    /// Also say who else has access, and at which level.
    Manage,
}

impl Access {
    pub const ALL: [Access; 3] = [Access::Read, Access::Write, Access::Manage];

    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Manage => "manage",
        }
    }

    pub fn can_write(self) -> bool {
        self >= Access::Write
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Access {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Access::ALL
            .into_iter()
            .find(|a| a.as_str() == s)
            .ok_or_else(|| format!("{s:?} is not an access level: read, write or manage"))
    }
}

/// `GET /v1/me`: who the token is, and every namespace it reaches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    pub user: String,
    pub folders: Vec<FolderAccess>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderAccess {
    pub folder: String,
    pub access: Access,
}

/// One user's access to a namespace, as `GET /v1/ns/{ns}/access` lists them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub user: String,
    pub access: Access,
}

/// The body of `PUT /v1/ns/{ns}/access/{user}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub access: Access,
}

/// A user or namespace name a server takes: 1 to 64 of ASCII letters, digits, `.`, `_` and `-`, not
/// starting with `.` or `-`, so that it is the same in a URL, a file name and a shell.
pub fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HashList {
    pub hashes: Vec<Hash>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitRequest {
    pub op_id: OpId,
    pub device: DeviceId,
    pub device_name: String,
    pub ops: Vec<Op>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CommitResponse {
    /// Durable; `seq` is the journal position. A retried `op_id` gets the first answer.
    Committed { seq: u64 },
    /// Nothing was applied.
    Rejected { reject: RejectReason },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RejectReason {
    Op {
        op_index: usize,
        reason: OpReject,
    },
    /// A manifest or chunk the commit names is not on the server.
    MissingChunk {
        hash: Hash,
    },
    BadManifest {
        hash: Hash,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub seq: u64,
    pub op_id: OpId,
    pub device: DeviceId,
    pub device_name: String,
    /// Seconds since the epoch, server clock.
    pub time: i64,
    pub ops: Vec<Op>,
    /// The user whose token made the commit, as named then. Absent before accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangesResponse {
    pub changes: Vec<Change>,
    pub head: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictEntry {
    pub seq: u64,
    pub copy: ObjectId,
    pub object: ObjectId,
    pub competing_revision: RevisionId,
    pub device_name: String,
    pub time: i64,
}

/// One chunk in a batch body: 32-byte hash, big-endian u32 length, the bytes.
pub fn write_frame(out: &mut Vec<u8>, hash: &Hash, data: &[u8]) {
    out.extend_from_slice(&hash.0);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("batch body is truncated")]
    Truncated,
    #[error("chunk {0} does not match its hash")]
    HashMismatch(Hash),
}

/// Split a batch body into verified chunks.
pub fn read_frames(mut body: &[u8]) -> Result<Vec<(Hash, &[u8])>, FrameError> {
    let mut out = Vec::new();
    while !body.is_empty() {
        if body.len() < 36 {
            return Err(FrameError::Truncated);
        }
        let hash = Hash(body[..32].try_into().expect("slice is 32 bytes"));
        let len = u32::from_be_bytes(body[32..36].try_into().expect("slice is 4 bytes")) as usize;
        body = &body[36..];
        if body.len() < len {
            return Err(FrameError::Truncated);
        }
        let (data, rest) = body.split_at(len);
        if Hash::of(data) != hash {
            return Err(FrameError::HashMismatch(hash));
        }
        out.push((hash, data));
        body = rest;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_hold_the_ones_below_and_parse_as_written() {
        assert!(Access::Manage > Access::Write && Access::Write > Access::Read);
        assert!(!Access::Read.can_write());
        assert!(Access::Write.can_write() && Access::Manage.can_write());
        for a in Access::ALL {
            assert_eq!(a.as_str().parse::<Access>(), Ok(a));
            assert_eq!(serde_json::to_string(&a).unwrap(), format!("\"{a}\""));
        }
        assert!("admin".parse::<Access>().is_err());
    }

    #[test]
    fn an_info_or_change_from_before_accounts_still_reads() {
        let info: Info =
            serde_json::from_str(r#"{"protocol":1,"server_version":"0.1.11","head":3}"#).unwrap();
        assert_eq!(info.access, None);
        let change: Change = serde_json::from_str(
            r#"{"seq":1,"op_id":"00000000000000000000000000000001","device":"00000000000000000000000000000002","device_name":"a","time":0,"ops":[]}"#,
        )
        .unwrap();
        assert_eq!(change.user, None);
        // And neither field is sent where there is none, so an older peer sees what it always did.
        assert!(!serde_json::to_string(&change).unwrap().contains("user"));
    }

    #[test]
    fn account_names_are_plain() {
        for good in ["default", "alice", "team-2026", "a.b_c", "A"] {
            assert!(valid_account_name(good), "{good}");
        }
        let long = "x".repeat(65);
        for bad in ["", ".hidden", "-x", "a b", "a/b", "é", "a:b", long.as_str()] {
            assert!(!valid_account_name(bad), "{bad}");
        }
    }

    #[test]
    fn frames_round_trip_and_catch_damage() {
        let mut body = Vec::new();
        for data in [&b"one"[..], b"", b"three"] {
            write_frame(&mut body, &Hash::of(data), data);
        }
        let frames = read_frames(&body).unwrap();
        assert_eq!(
            frames.iter().map(|f| f.1).collect::<Vec<_>>(),
            [&b"one"[..], b"", b"three"]
        );

        assert_eq!(
            read_frames(&body[..body.len() - 1]),
            Err(FrameError::Truncated)
        );
        let mut bad = body.clone();
        bad[36] ^= 1;
        assert!(matches!(
            read_frames(&bad),
            Err(FrameError::HashMismatch(_))
        ));
    }
}
