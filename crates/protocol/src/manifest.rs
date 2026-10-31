//! A file's content: ordered chunk hashes laid out in data segments, with holes between them.

use crate::ids::Hash;
use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u32 = 1;

/// FastCDC bounds. Part of the format: clients must agree for chunks to dedupe.
pub const CHUNK_MIN: u32 = 512 * 1024;
pub const CHUNK_AVG: u32 = 2 * 1024 * 1024;
pub const CHUNK_MAX: u32 = 8 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: Hash,
    pub len: u32,
}

/// A run of data at `offset`. Bytes between segments, and after the last, are a hole.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Segment {
    pub offset: u64,
    pub chunks: Vec<ChunkRef>,
}

impl Segment {
    pub fn len(&self) -> u64 {
        self.chunks.iter().map(|c| u64::from(c.len)).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub v: u32,
    pub size: u64,
    pub segments: Vec<Segment>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest is not valid: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("unsupported manifest version {0}")]
    Version(u32),
    #[error("manifest layout is inconsistent: {0}")]
    Layout(&'static str),
}

impl Manifest {
    pub fn new(size: u64, segments: Vec<Segment>) -> Self {
        Self {
            v: FORMAT_VERSION,
            size,
            segments,
        }
    }

    /// The canonical encoding, the input to the manifest's hash: two clients that read the same
    /// content produce the same bytes.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a manifest always encodes")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        let m: Manifest = serde_json::from_slice(bytes)?;
        if m.v != FORMAT_VERSION {
            return Err(ManifestError::Version(m.v));
        }
        m.validate()?;
        Ok(m)
    }

    pub fn hash(&self) -> Hash {
        Hash::of(&self.encode())
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut end = 0u64;
        for s in &self.segments {
            if s.offset < end {
                return Err(ManifestError::Layout(
                    "segments overlap or are out of order",
                ));
            }
            if s.chunks.is_empty() {
                return Err(ManifestError::Layout("empty segment"));
            }
            end = s
                .offset
                .checked_add(s.len())
                .ok_or(ManifestError::Layout("segment overflows"))?;
        }
        if end > self.size {
            return Err(ManifestError::Layout(
                "segments extend past the file's size",
            ));
        }
        Ok(())
    }

    pub fn chunks(&self) -> impl Iterator<Item = &ChunkRef> {
        self.segments.iter().flat_map(|s| s.chunks.iter())
    }

    /// Every chunk with the byte offset it lives at.
    pub fn placed_chunks(&self) -> impl Iterator<Item = (u64, &ChunkRef)> {
        self.segments.iter().flat_map(|s| {
            let mut at = s.offset;
            s.chunks.iter().map(move |c| {
                let here = at;
                at += u64::from(c.len);
                (here, c)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(len: u32) -> ChunkRef {
        ChunkRef {
            hash: Hash::of(&len.to_be_bytes()),
            len,
        }
    }

    #[test]
    fn holes_are_gaps_between_segments() {
        let m = Manifest::new(
            100,
            vec![
                Segment {
                    offset: 10,
                    chunks: vec![chunk(5), chunk(5)],
                },
                Segment {
                    offset: 50,
                    chunks: vec![chunk(10)],
                },
            ],
        );
        let back = Manifest::decode(&m.encode()).unwrap();
        assert_eq!(back, m);
        let at: Vec<u64> = back.placed_chunks().map(|(o, _)| o).collect();
        assert_eq!(at, [10, 15, 50]);
    }

    #[test]
    fn inconsistent_layouts_are_rejected() {
        let overlap = Manifest::new(
            100,
            vec![
                Segment {
                    offset: 0,
                    chunks: vec![chunk(50)],
                },
                Segment {
                    offset: 10,
                    chunks: vec![chunk(5)],
                },
            ],
        );
        assert!(overlap.validate().is_err());
        let past_end = Manifest::new(
            1,
            vec![Segment {
                offset: 0,
                chunks: vec![chunk(5)],
            }],
        );
        assert!(past_end.validate().is_err());
    }
}
