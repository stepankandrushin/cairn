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
    /// `get` of each of `hashes`, handed to `each` in their order: a download's. Up to `READERS` are
    /// read at once, as a disk that seeks serves a queue of reads many times faster than one read at
    /// a time. It ends as reading one after the other would: `Ok(Some(hash))` for the first not
    /// here, or the first error, whichever comes first in that order, and nothing after it handed on.
    fn get_in_order(
        &self,
        hashes: &[Hash],
        each: &mut dyn FnMut(&Hash, Vec<u8>),
    ) -> Result<Option<Hash>> {
        let missing = read_in_order(hashes, Ahead::DOWNLOAD, |hash| self.get(hash), each)?;
        Ok(missing.map(|i| hashes[i]))
    }
}

/// How many chunks one download reads at once. Cold chunks of under 64 KiB, on a mirror of two hard
/// disks with a million and a half of them, came at 70 to 95 a second one at a time, 280 at 8 at once,
/// 350 to 370 at 16, 32 and 64, both disks then at work where one alone had been.
const READERS: usize = 32;

/// How far reads may run ahead of what is handed on.
#[derive(Clone, Copy)]
struct Ahead {
    readers: usize,
    /// Read and not yet handed on, in chunks.
    chunks: usize,
    /// The same in bytes. The next to be handed on is read whatever this holds, so one read always
    /// goes on.
    bytes: usize,
}

impl Ahead {
    /// A download's answer is held whole until sent, up to `MAX_CHUNK_BATCH_BYTES`: what is read
    /// ahead adds at most a quarter of that, and what the readers have in hand.
    const DOWNLOAD: Ahead = Ahead {
        readers: READERS,
        chunks: 2 * READERS,
        bytes: 16 << 20,
    };
}

/// What `read_in_order`'s readers share with the one handing on.
#[derive(Default)]
struct Window {
    /// Read and not yet handed on, by index.
    read: std::collections::BTreeMap<usize, Result<Option<Vec<u8>>>>,
    bytes: usize,
    /// The index handed on next.
    next: usize,
    /// Nothing more is handed on: readers take no more.
    done: bool,
}

fn held(got: &Result<Option<Vec<u8>>>) -> usize {
    got.as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map_or(0, Vec::len)
}

/// `read` of every item, handed to `each` in the items' order, up to `ahead.readers` at once and no
/// further ahead of `each` than `ahead` allows. The first `Ok(None)` (`Ok(Some(index))` here) or
/// error, in the items' order, ends it, and what was read past it is dropped.
fn read_in_order<T: Sync>(
    items: &[T],
    ahead: Ahead,
    read: impl Fn(&T) -> Result<Option<Vec<u8>>> + Sync,
    each: &mut dyn FnMut(&T, Vec<u8>),
) -> Result<Option<usize>> {
    let one_by_one = |each: &mut dyn FnMut(&T, Vec<u8>)| {
        for (i, item) in items.iter().enumerate() {
            match read(item)? {
                Some(data) => each(item, data),
                None => return Ok(Some(i)),
            }
        }
        Ok(None)
    };
    if items.len() <= 1 || ahead.readers <= 1 {
        return one_by_one(each);
    }
    let claimed = AtomicUsize::new(0);
    let window = Mutex::new(Window::default());
    let lock = || window.lock().unwrap_or_else(|e| e.into_inner());
    // A read done, for the one handing on; and room ahead, or the end, for the readers.
    let (ready, room) = (std::sync::Condvar::new(), std::sync::Condvar::new());
    let reader = || loop {
        let i = claimed.fetch_add(1, Ordering::SeqCst);
        let Some(item) = items.get(i) else {
            return;
        };
        let mut w = lock();
        while !w.done && i != w.next && (i >= w.next + ahead.chunks || w.bytes >= ahead.bytes) {
            w = room.wait(w).unwrap_or_else(|e| e.into_inner());
        }
        if w.done {
            return;
        }
        drop(w);
        // A reader that panicked would leave its item unread, and the answer waiting for ever.
        let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read(item)))
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "reading a chunk failed: its reader panicked"
                ))
            });
        let mut w = lock();
        w.bytes += held(&got);
        w.read.insert(i, got);
        drop(w);
        ready.notify_one();
    };
    std::thread::scope(|scope| {
        let spawned = (0..items.len().min(ahead.readers))
            .filter(|_| {
                std::thread::Builder::new()
                    .spawn_scoped(scope, reader)
                    .is_ok()
            })
            .count();
        // No thread to be had: one at a time, here.
        if spawned == 0 {
            return one_by_one(each);
        }
        let mut handed = || {
            for (i, item) in items.iter().enumerate() {
                let got = {
                    let mut w = lock();
                    loop {
                        if let Some(got) = w.read.remove(&i) {
                            w.bytes -= held(&got);
                            w.next = i + 1;
                            break got;
                        }
                        w = ready.wait(w).unwrap_or_else(|e| e.into_inner());
                    }
                };
                room.notify_all();
                match got? {
                    Some(data) => each(item, data),
                    None => return Ok(Some(i)),
                }
            }
            Ok(None)
        };
        let outcome = handed();
        lock().done = true;
        room.notify_all();
        outcome
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cairn-chunks-{}",
            hex::encode(rand::random::<[u8; 8]>())
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A read that takes a while, longer for some items than others, so that they finish out of order.
    fn slow(i: usize) {
        std::thread::sleep(Duration::from_micros((i * 7919 % 13) as u64 * 200));
    }

    #[test]
    fn a_download_is_handed_on_in_the_order_asked() {
        let dir = tempdir();
        let store = FsChunkStore::open(&dir).unwrap();
        let data: Vec<Vec<u8>> = (0..400u32)
            .map(|i| i.to_le_bytes().repeat(i as usize % 50 + 1))
            .collect();
        let chunks: Vec<(Hash, &[u8])> = data.iter().map(|d| (Hash::of(d), d.as_slice())).collect();
        store.put_all(&chunks).unwrap();
        // Another order than they were stored in, and one of them twice.
        let mut asked: Vec<Hash> = chunks.iter().rev().map(|(h, _)| *h).collect();
        asked.insert(100, asked[3]);
        let mut got = Vec::new();
        let missing = store
            .get_in_order(&asked, &mut |h, d| got.push((*h, d)))
            .unwrap();
        assert_eq!(missing, None);
        assert_eq!(got.len(), asked.len());
        for ((h, d), want) in got.iter().zip(&asked) {
            assert_eq!(h, want);
            assert_eq!(Hash::of(d), *h);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_download_ends_at_its_first_chunk_missing_or_damaged_in_the_order_asked() {
        let dir = tempdir();
        let store = FsChunkStore::open(&dir).unwrap();
        let data: Vec<Vec<u8>> = (0..60u32)
            .map(|i| format!("chunk {i}").into_bytes())
            .collect();
        let chunks: Vec<(Hash, &[u8])> = data.iter().map(|d| (Hash::of(d), d.as_slice())).collect();
        store.put_all(&chunks).unwrap();
        let asked: Vec<Hash> = chunks.iter().map(|(h, _)| *h).collect();
        fs::remove_file(store.path(&asked[10])).unwrap();
        fs::write(store.path(&asked[30]), b"not chunk 30").unwrap();

        let mut got = Vec::new();
        let missing = store
            .get_in_order(&asked, &mut |h, _| got.push(*h))
            .unwrap();
        assert_eq!(missing, Some(asked[10]));
        assert_eq!(got, asked[..10]);

        let mut got = Vec::new();
        let err = store
            .get_in_order(&asked[11..], &mut |h, _| got.push(*h))
            .unwrap_err();
        assert!(err.to_string().contains("is damaged on disk"), "{err:#}");
        assert_eq!(got, asked[11..30]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn what_ends_it_is_the_first_in_order_not_the_first_read() {
        let items: Vec<usize> = (0..40).collect();
        let ahead = Ahead {
            readers: 8,
            chunks: 16,
            bytes: 1 << 20,
        };
        // 5 is not there, and 9 fails long before 5's read is done.
        let read = |&i: &usize| -> Result<Option<Vec<u8>>> {
            match i {
                5 => {
                    std::thread::sleep(Duration::from_millis(50));
                    Ok(None)
                }
                9 => bail!("item 9 is damaged"),
                _ => Ok(Some(vec![i as u8])),
            }
        };
        let mut handed = Vec::new();
        let end = read_in_order(&items, ahead, read, &mut |&i, _| handed.push(i)).unwrap();
        assert_eq!(end, Some(5));
        assert_eq!(handed, (0..5).collect::<Vec<_>>());

        // The other way round: the error is first, though read last.
        let read = |&i: &usize| -> Result<Option<Vec<u8>>> {
            match i {
                5 => {
                    std::thread::sleep(Duration::from_millis(50));
                    bail!("item 5 is damaged")
                }
                9 => Ok(None),
                _ => Ok(Some(vec![i as u8])),
            }
        };
        let mut handed = Vec::new();
        let err = read_in_order(&items, ahead, read, &mut |&i, _| handed.push(i)).unwrap_err();
        assert_eq!(err.to_string(), "item 5 is damaged");
        assert_eq!(handed, (0..5).collect::<Vec<_>>());
    }

    /// Reads overlap, never more than `readers` at once, and none starts further ahead of what is
    /// handed on than `ahead` allows, in chunks or, once that much is held, in bytes.
    #[test]
    fn reads_overlap_within_what_may_be_read_ahead() {
        let items: Vec<usize> = (0..300).collect();
        for (ahead, most) in [
            // In chunks: a read starts only below `next + chunks`, and `handed` lags `next` by one.
            (
                Ahead {
                    readers: 8,
                    chunks: 5,
                    bytes: 1 << 20,
                },
                5,
            ),
            // In bytes, 100 an item: at most two held when a read starts, seven other readers, and
            // the lag.
            (
                Ahead {
                    readers: 8,
                    chunks: 1000,
                    bytes: 300,
                },
                10,
            ),
        ] {
            let reading = AtomicUsize::new(0);
            let most_reading = AtomicUsize::new(0);
            let handed = AtomicUsize::new(0);
            let most_ahead = AtomicUsize::new(0);
            let read = |&i: &usize| -> Result<Option<Vec<u8>>> {
                let now = reading.fetch_add(1, Ordering::SeqCst) + 1;
                most_reading.fetch_max(now, Ordering::SeqCst);
                most_ahead.fetch_max(i - handed.load(Ordering::SeqCst), Ordering::SeqCst);
                slow(i);
                reading.fetch_sub(1, Ordering::SeqCst);
                Ok(Some(vec![i as u8; 100]))
            };
            let mut order = Vec::new();
            let end = read_in_order(&items, ahead, read, &mut |&i, d| {
                assert_eq!(d, vec![i as u8; 100]);
                order.push(i);
                handed.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
            assert_eq!(end, None);
            assert_eq!(order, items);
            let (reading, ahead_of) = (
                most_reading.load(Ordering::SeqCst),
                most_ahead.load(Ordering::SeqCst),
            );
            assert!((2..=8).contains(&reading), "{reading} read at once");
            assert!(ahead_of <= most, "a read began {ahead_of} ahead");
        }
    }

    #[test]
    fn a_reader_that_panics_fails_the_download_rather_than_hanging_it() {
        let items: Vec<usize> = (0..20).collect();
        let ahead = Ahead {
            readers: 4,
            chunks: 8,
            bytes: 1 << 20,
        };
        let read = |&i: &usize| -> Result<Option<Vec<u8>>> {
            assert_ne!(i, 7, "a reader's bug");
            Ok(Some(vec![1]))
        };
        let mut handed = 0;
        let err = read_in_order(&items, ahead, read, &mut |_, _| handed += 1).unwrap_err();
        assert!(err.to_string().contains("panicked"), "{err:#}");
        assert_eq!(handed, 7);
    }
}
