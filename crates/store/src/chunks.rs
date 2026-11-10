//! Immutable chunks on local disk: `chunks/ab/abcdef...`. A chunk is at its final path only after
//! its bytes are durable, which is what lets a commit rely on it (invariant 1).

use anyhow::{bail, Context, Result};
use cairn_failpoint::{check, hit};
use cairn_protocol::Hash;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub trait ChunkStore: Send + Sync {
    /// Durable on return.
    fn put(&self, hash: &Hash, data: &[u8]) -> Result<()> {
        self.put_all(&[(*hash, data)])
    }
    /// Every one durable on return, made so together: an upload request's chunks.
    fn put_all(&self, chunks: &[(Hash, &[u8])]) -> Result<()>;
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

/// Make a directory's entries durable.
pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    open_dir(path)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("fsync directory {}", path.display()))
}

#[cfg(unix)]
fn open_dir(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

/// Windows opens a directory only with backup semantics, and flushes it only through a handle that may
/// add to it.
#[cfg(windows)]
fn open_dir(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_ADD_FILE: u32 = 0x2;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    fs::OpenOptions::new()
        .access_mode(FILE_ADD_FILE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// How many chunks of one request are written and synced at once. On a SATA disk 64 at once make a
/// small durable chunk about 40 times cheaper than one after the other.
const WRITERS: usize = 64;

/// `f` on every item, on up to `WRITERS` threads; the first error stops the others taking more.
fn in_parallel<T: Sync>(items: &[T], f: impl Fn(&T) -> Result<()> + Sync) -> Result<()> {
    if items.len() <= 1 {
        return items.iter().try_for_each(f);
    }
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let work = || loop {
        if failed.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            return;
        }
        let Some(item) = items.get(next.fetch_add(1, Ordering::SeqCst)) else {
            return;
        };
        if let Err(e) = f(item) {
            failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert(e);
            return;
        }
    };
    std::thread::scope(|scope| {
        let spawned = (0..items.len().min(WRITERS))
            .filter(|_| {
                std::thread::Builder::new()
                    .spawn_scoped(scope, work)
                    .is_ok()
            })
            .count();
        // No thread to be had: the work is done here, one at a time.
        if spawned == 0 {
            work();
        }
    });
    match failed.into_inner().unwrap_or_else(|e| e.into_inner()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
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

    /// As `get`, for a pass over every chunk: the bytes are checked and not kept, and on Linux their
    /// pages are let go of the page cache, so that the pass does not push out what the server reads
    /// often, its database above all. `Ok(false)`: not here.
    pub fn check_passing(&self, hash: &Hash) -> Result<bool> {
        let mut file = match File::open(self.path(hash)) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut data)?;
        #[cfg(target_os = "linux")]
        let _ = rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed);
        if Hash::of(&data) != *hash {
            bail!("chunk {hash} is damaged on disk: its content does not match its hash");
        }
        Ok(true)
    }
}

impl ChunkStore for FsChunkStore {
    fn put_all(&self, chunks: &[(Hash, &[u8])]) -> Result<()> {
        // All checked before any is written: a damaged batch leaves nothing behind.
        in_parallel(chunks, |(hash, data)| {
            if Hash::of(data) != *hash {
                bail!("chunk {hash} does not match its hash");
            }
            Ok(())
        })?;
        // The directories whose entries must be durable before the answer.
        let mut fans: BTreeSet<PathBuf> = BTreeSet::new();
        let mut new: Vec<(&[u8], PathBuf, PathBuf)> = Vec::new();
        let mut seen = HashSet::new();
        for (hash, data) in chunks {
            if !seen.insert(*hash) {
                continue;
            }
            let dest = self.path(hash);
            let fan = dest.parent().expect("a chunk path has a parent");
            // Already here: a concurrent upload may not have fsynced its entry yet, and a damaged copy
            // is replaced rather than trusted.
            if fs::read(&dest).is_ok_and(|existing| existing == *data) {
                fans.insert(fan.to_path_buf());
                continue;
            }
            let tmp = self
                .root
                .join("incoming")
                .join(format!("{hash}.{}", hex::encode(rand::random::<[u8; 8]>())));
            new.push((data, tmp, dest));
        }
        let mut made = false;
        for (.., dest) in &new {
            let fan = dest.parent().expect("a chunk path has a parent");
            if !fan.exists() {
                fs::create_dir_all(fan)?;
                made = true;
            }
        }
        if made {
            sync_dir(&self.root.join("chunks"))?;
        }
        // Written and synced in `incoming/`, many at once: their fsyncs overlap, so the filesystem
        // commits them in a few flushes rather than one each. None is at its final path yet.
        let written = in_parallel(&new, |(data, tmp, _)| {
            check("server.chunk.before_write")?;
            let mut f = File::create(tmp)?;
            f.write_all(data)?;
            hit("server.chunk.written");
            f.sync_all()?;
            hit("server.chunk.synced");
            Ok(())
        });
        if let Err(e) = written {
            for (_, tmp, _) in &new {
                let _ = fs::remove_file(tmp);
            }
            return Err(e);
        }
        // Only whole, durable chunks reach their final path.
        let mut renamed = 0;
        let moved = new.iter().try_for_each(|(_, tmp, dest)| -> Result<()> {
            fs::rename(tmp, dest)?;
            hit("server.chunk.renamed");
            fans.insert(
                dest.parent()
                    .expect("a chunk path has a parent")
                    .to_path_buf(),
            );
            renamed += 1;
            Ok(())
        });
        if moved.is_err() {
            for (_, tmp, _) in &new[renamed..] {
                let _ = fs::remove_file(tmp);
            }
        }
        // Then each directory once, for every entry made in it, whatever stopped the renames: the
        // request is answered only once all of it is durable.
        let fans: Vec<PathBuf> = fans.into_iter().collect();
        let synced = in_parallel(&fans, |fan| {
            sync_dir(fan)?;
            hit("server.chunk.durable");
            Ok(())
        });
        moved?;
        synced
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
