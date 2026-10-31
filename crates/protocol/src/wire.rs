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
