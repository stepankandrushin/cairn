//! A server's data directory: chunks plus metadata, and the rule tying them together. A commit is
//! accepted only if every chunk it names is already durable (invariant 1).

use crate::chunks::{sync_dir, ChunkStore, FsChunkStore};
use crate::meta::{CommitOutcome, MetaStore};
use crate::sqlite::SqliteStore;
use anyhow::{Context, Result};
use cairn_protocol::manifest::ManifestError;
use cairn_protocol::wire::{Change, CommitRequest, CommitResponse, ConflictEntry, RejectReason};
use cairn_protocol::{Content, Hash, Manifest};
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One namespace's chunks live apart from every other's: dedup never crosses a namespace, so
/// "which chunks are missing" reveals nothing about what another namespace stores.
pub struct Repo {
    root: PathBuf,
    meta: Mutex<Box<dyn MetaStore>>,
    chunks: Mutex<BTreeMap<String, std::sync::Arc<FsChunkStore>>>,
    /// Holds an exclusive lock on the data directory for the life of the process.
    _lock: File,
}

/// The data directory's lock, held exclusively until the process ends however it ends.
#[cfg(unix)]
fn lock(root: &Path) -> Result<File> {
    let lock = File::create(root.join("lock")).context("create the data directory's lock file")?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| anyhow::anyhow!("another cairn-server holds {}", root.display()))?;
    Ok(lock)
}

/// On Windows the lock is the file open and shared with no one.
#[cfg(windows)]
fn lock(root: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(root.join("lock"))
        .or_else(|e| {
            if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
                anyhow::bail!("another cairn-server holds {}", root.display());
            }
            Err(e).context("create the data directory's lock file")
        })
}

impl Repo {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("ns"))?;
        let lock = lock(&root)?;
        let meta = SqliteStore::open(&root.join("meta.sqlite"))?;
        sync_dir(&root)?;
        Ok(Self {
            root,
            meta: Mutex::new(Box::new(meta)),
            chunks: Mutex::new(BTreeMap::new()),
            _lock: lock,
        })
    }

    fn meta(&self) -> std::sync::MutexGuard<'_, Box<dyn MetaStore>> {
        self.meta.lock().expect("a store panic aborts the process")
    }

    pub fn namespace_exists(&self, ns: &str) -> Result<bool> {
        self.meta().namespace_exists(ns)
    }

    pub fn namespaces(&self) -> Result<Vec<String>> {
        self.meta().namespaces()
    }

    pub fn chunks(&self, ns: &str) -> Result<std::sync::Arc<FsChunkStore>> {
        let mut map = self
            .chunks
            .lock()
            .expect("a store panic aborts the process");
        if let Some(store) = map.get(ns) {
            return Ok(store.clone());
        }
        // Namespace names reach the disk hex-encoded, so no name can address anything else.
        let store = std::sync::Arc::new(FsChunkStore::open(
            self.root.join("ns").join(hex::encode(ns)),
        )?);
        sync_dir(&self.root.join("ns"))?;
        map.insert(ns.to_string(), store.clone());
        Ok(store)
    }

    pub fn head(&self, ns: &str) -> Result<u64> {
        self.meta().head(ns)
    }

    pub fn missing(&self, ns: &str, hashes: &[Hash]) -> Result<Vec<Hash>> {
        let chunks = self.chunks(ns)?;
        let mut out = Vec::new();
        for h in hashes {
            // A damaged chunk counts as missing, so that the client's good copy replaces it.
            if !chunks.is_intact(h)? {
                out.push(*h);
            }
        }
        Ok(out)
    }

    pub fn commit(&self, ns: &str, req: &CommitRequest, time: i64) -> Result<CommitResponse> {
        if let Some(reject) = self.check_content(ns, req)? {
            return Ok(CommitResponse::Rejected { reject });
        }
        let outcome = self.meta().commit(ns, req, time)?;
        Ok(match outcome {
            CommitOutcome::Committed { seq, .. } => CommitResponse::Committed { seq },
            CommitOutcome::Rejected { op_index, reason } => CommitResponse::Rejected {
                reject: RejectReason::Op { op_index, reason },
            },
        })
    }

    /// Everything the commit's revisions point at must already be here and durable.
    fn check_content(&self, ns: &str, req: &CommitRequest) -> Result<Option<RejectReason>> {
        let chunks = self.chunks(ns)?;
        let mut seen: HashSet<Hash> = HashSet::new();
        for op in &req.ops {
            let Some(body) = op.body() else { continue };
            let Content::File { manifest, size } = &body.content else {
                continue;
            };
            let mut streams: Vec<(Hash, Option<u64>)> = vec![(*manifest, Some(*size))];
            streams.extend(body.metadata.streams.iter().map(|s| (s.manifest, None)));
            for (hash, want_size) in streams {
                if !seen.insert(hash) {
                    continue;
                }
                if let Some(reject) = check_manifest(&*chunks, &hash, want_size)? {
                    return Ok(Some(reject));
                }
            }
        }
        Ok(None)
    }

    pub fn changes(&self, ns: &str, after: u64, limit: u32) -> Result<(Vec<Change>, u64)> {
        let mut meta = self.meta();
        Ok((meta.changes(ns, after, limit)?, meta.head(ns)?))
    }

    pub fn conflicts(&self, ns: &str) -> Result<Vec<ConflictEntry>> {
        self.meta().conflicts(ns)
    }

    pub(crate) fn with_meta<T>(
        &self,
        f: impl FnOnce(&mut dyn MetaStore) -> Result<T>,
    ) -> Result<T> {
        f(self.meta().as_mut())
    }
}

fn check_manifest(
    chunks: &dyn ChunkStore,
    hash: &Hash,
    want_size: Option<u64>,
) -> Result<Option<RejectReason>> {
    let Some(bytes) = chunks.get(hash)? else {
        return Ok(Some(RejectReason::MissingChunk { hash: *hash }));
    };
    let bad = |message: String| {
        Ok(Some(RejectReason::BadManifest {
            hash: *hash,
            message,
        }))
    };
    let manifest = match Manifest::decode(&bytes) {
        Ok(m) => m,
        Err(
            e @ (ManifestError::Decode(_) | ManifestError::Version(_) | ManifestError::Layout(_)),
        ) => {
            return bad(e.to_string());
        }
    };
    if want_size.is_some_and(|s| s != manifest.size) {
        return bad(format!(
            "revision says {} bytes, manifest says {}",
            want_size.unwrap_or(0),
            manifest.size
        ));
    }
    for c in manifest.chunks() {
        match chunks.len_of(&c.hash)? {
            None => return Ok(Some(RejectReason::MissingChunk { hash: c.hash })),
            Some(len) if len != u64::from(c.len) => {
                return bad(format!(
                    "chunk {} is {len} bytes, manifest says {}",
                    c.hash, c.len
                ));
            }
            Some(_) => {}
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::{
        ChunkRef, DeviceId, Link, Metadata, Name, ObjectId, Op, OpId, RevisionBody, RevisionId,
        Segment,
    };

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cairn-repo-{}",
            hex::encode(rand::random::<[u8; 8]>())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_file(manifest: Hash, size: u64) -> CommitRequest {
        CommitRequest {
            op_id: OpId::random(),
            device: DeviceId::random(),
            device_name: "d".into(),
            ops: vec![Op::Create {
                object: ObjectId::random(),
                revision: RevisionId::random(),
                body: RevisionBody {
                    content: Content::File { manifest, size },
                    metadata: Metadata::default(),
                },
                link: Link {
                    parent: ObjectId::ROOT,
                    name: Name::new(*b"f").unwrap(),
                },
                conflict_of: None,
            }],
        }
    }

    #[test]
    fn a_commit_is_refused_until_its_chunks_are_here() {
        let dir = tempdir();
        let repo = Repo::open(&dir).unwrap();
        let data = b"hello";
        let chunk = Hash::of(data);
        let manifest = Manifest::new(
            5,
            vec![Segment {
                offset: 0,
                chunks: vec![ChunkRef {
                    hash: chunk,
                    len: 5,
                }],
            }],
        );
        let mbytes = manifest.encode();
        let mhash = Hash::of(&mbytes);
        let store = repo.chunks("default").unwrap();

        let req = create_file(mhash, 5);
        assert_eq!(
            repo.commit("default", &req, 0).unwrap(),
            CommitResponse::Rejected {
                reject: RejectReason::MissingChunk { hash: mhash }
            }
        );
        store.put(&mhash, &mbytes).unwrap();
        assert_eq!(
            repo.commit("default", &req, 0).unwrap(),
            CommitResponse::Rejected {
                reject: RejectReason::MissingChunk { hash: chunk }
            }
        );
        store.put(&chunk, data).unwrap();

        let wrong_size = create_file(mhash, 6);
        assert!(matches!(
            repo.commit("default", &wrong_size, 0).unwrap(),
            CommitResponse::Rejected {
                reject: RejectReason::BadManifest { .. }
            }
        ));

        assert_eq!(
            repo.commit("default", &req, 0).unwrap(),
            CommitResponse::Committed { seq: 1 }
        );
        drop(repo);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dedup_never_crosses_a_namespace() {
        let dir = tempdir();
        let repo = Repo::open(&dir).unwrap();
        repo.with_meta(|m| m.create_namespace("other")).unwrap();
        let h = Hash::of(b"secret");
        repo.chunks("default").unwrap().put(&h, b"secret").unwrap();
        assert!(repo.missing("default", &[h]).unwrap().is_empty());
        assert_eq!(repo.missing("other", &[h]).unwrap(), [h]);
        drop(repo);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_second_server_on_one_data_directory_is_refused() {
        let dir = tempdir();
        let repo = Repo::open(&dir).unwrap();
        assert!(Repo::open(&dir).is_err());
        drop(repo);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_chunk_is_an_error_not_data() {
        let dir = tempdir();
        let store = FsChunkStore::open(&dir).unwrap();
        let h = Hash::of(b"x");
        store.put(&h, b"x").unwrap();
        let s = h.to_string();
        std::fs::write(dir.join("chunks").join(&s[..2]).join(&s), b"y").unwrap();
        assert!(store.get(&h).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
