//! Immutable chunks on local disk: `chunks/ab/abcdef...`. A chunk is at its final path only after
//! its bytes are durable, which is what lets a commit rely on it (invariant 1).

use anyhow::{bail, Context, Result};
use cairn_failpoint::{check, hit};
use cairn_protocol::Hash;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub trait ChunkStore: Send + Sync {
    /// Durable on return.
    fn put(&self, hash: &Hash, data: &[u8]) -> Result<()>;
    /// The stored length, if the chunk exists.
    fn len_of(&self, hash: &Hash) -> Result<Option<u64>>;
    /// Whether the chunk exists and its bytes match its hash.
    fn is_intact(&self, hash: &Hash) -> Result<bool>;
    /// Reads and verifies against the hash.
    fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>>;
    /// Every stored hash, for `fsck`.
    fn list(&self) -> Result<Vec<Hash>>;
}

pub struct FsChunkStore {
    root: PathBuf,
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("fsync directory {}", path.display()))
}

impl FsChunkStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let store = Self { root: root.into() };
        fs::create_dir_all(store.root.join("chunks"))?;
        fs::create_dir_all(store.root.join("incoming"))?;
        // A killed server may have acknowledged nothing yet left a rename not fsynced. From here on
        // every chunk found at its final path may be relied on.
        for fan in fs::read_dir(store.root.join("chunks"))? {
            sync_dir(&fan?.path())?;
        }
        // Partial uploads from a previous life never reached a final path, so they are garbage.
        for e in fs::read_dir(store.root.join("incoming"))? {
            fs::remove_file(e?.path())?;
        }
        sync_dir(&store.root)?;
        Ok(store)
    }

    fn path(&self, hash: &Hash) -> PathBuf {
        let h = hash.to_string();
        self.root.join("chunks").join(&h[..2]).join(h)
    }
}

impl ChunkStore for FsChunkStore {
    fn put(&self, hash: &Hash, data: &[u8]) -> Result<()> {
        if Hash::of(data) != *hash {
            bail!("chunk {hash} does not match its hash");
        }
        let dest = self.path(hash);
        let fan = dest.parent().expect("a chunk path has a parent");
        // Already here: a concurrent upload may not have fsynced its entry yet, and a damaged copy is
        // replaced rather than trusted.
        if fs::read(&dest).is_ok_and(|existing| existing == data) {
            sync_dir(fan)?;
            return Ok(());
        }
        if !fan.exists() {
            fs::create_dir_all(fan)?;
            sync_dir(fan.parent().expect("a fan directory has a parent"))?;
        }
        let tmp = self
            .root
            .join("incoming")
            .join(format!("{hash}.{}", hex::encode(rand::random::<[u8; 8]>())));
        let written = (|| -> Result<()> {
            check("server.chunk.before_write")?;
            let mut f = File::create(&tmp)?;
            f.write_all(data)?;
            hit("server.chunk.written");
            f.sync_all()?;
            hit("server.chunk.synced");
            fs::rename(&tmp, &dest)?;
            hit("server.chunk.renamed");
            sync_dir(fan)?;
            hit("server.chunk.durable");
            Ok(())
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written
    }

    fn len_of(&self, hash: &Hash) -> Result<Option<u64>> {
        match fs::metadata(self.path(hash)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn is_intact(&self, hash: &Hash) -> Result<bool> {
        match fs::read(self.path(hash)) {
            Ok(data) => Ok(Hash::of(&data) == *hash),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(hash)) {
            Ok(data) => {
                if Hash::of(&data) != *hash {
                    bail!("chunk {hash} is damaged on disk: its content does not match its hash");
                }
                Ok(Some(data))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn list(&self) -> Result<Vec<Hash>> {
        let mut out = Vec::new();
        for fan in fs::read_dir(self.root.join("chunks"))? {
            for e in fs::read_dir(fan?.path())? {
                let name = e?.file_name();
                let name = name.to_string_lossy();
                out.push(
                    name.parse()
                        .with_context(|| format!("stray file in chunk store: {name}"))?,
                );
            }
        }
        Ok(out)
    }
}
