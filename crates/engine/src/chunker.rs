//! Reading a file into a manifest: data segments found with `SEEK_DATA`/`SEEK_HOLE`, cut by FastCDC,
//! hashed with BLAKE3. A hole is never read as zeros, so a sparse file costs what it occupies.

use crate::fsops::{read_at, read_exact_at, AsFd, BorrowedFd};
use cairn_protocol::manifest::{CHUNK_AVG, CHUNK_MAX, CHUNK_MIN};
use cairn_protocol::{ChunkRef, Hash, Manifest, Segment};
use fastcdc::v2020::StreamCDC;
use std::io::{self, Read};

pub use crate::fsops::data_runs;

/// Whether a manifest leaves any of its file as a hole.
pub fn has_holes(m: &Manifest) -> bool {
    m.segments.iter().map(Segment::len).sum::<u64>() < m.size
}

/// Give a file written from a manifest the holes that manifest has. On Linux the `ftruncate` before the
/// writes already left them. APFS fills such a gap when the file is flushed unless it is told otherwise,
/// and the next scan would then send this file back without its holes, to every device. Where this
/// is refused the file is whole, only bigger than it should be.
#[cfg(target_os = "macos")]
pub fn restore_holes(fd: BorrowedFd<'_>, m: &Manifest) {
    const BLOCK: u64 = 4096;
    let punch = |from: u64, to: u64| {
        let (start, end) = (from.next_multiple_of(BLOCK), to - to % BLOCK);
        if start < end {
            let _ = crate::fsops::punch_hole(fd, start, end - start);
        }
    };
    let mut at = 0;
    for segment in &m.segments {
        punch(at, segment.offset);
        at = segment.offset + segment.len();
    }
    punch(at, m.size);
}

#[cfg(not(target_os = "macos"))]
pub fn restore_holes(_: BorrowedFd<'_>, _: &Manifest) {}

/// Whether an open file holds the bytes of `m`, wherever its own holes are: each chunk of `m`, read
/// where `m` has it, hashes the same, and what `m` has as a hole reads as zeros.
pub fn same_bytes(fd: BorrowedFd<'_>, m: &Manifest) -> io::Result<bool> {
    let size = crate::fsops::fstat(fd)?.size;
    if size != m.size {
        return Ok(false);
    }
    let runs = data_runs(fd, size)?;
    let mut buf = Vec::new();
    let mut at = 0;
    for segment in &m.segments {
        if !zeros(fd, &runs, at, segment.offset, &mut buf)? {
            return Ok(false);
        }
        let mut offset = segment.offset;
        for c in &segment.chunks {
            buf.resize(c.len as usize, 0);
            read_exact_at(fd, &mut buf, offset)?;
            if Hash::of(&buf) != c.hash {
                return Ok(false);
            }
            offset += u64::from(c.len);
        }
        at = offset;
    }
    zeros(fd, &runs, at, size, &mut buf)
}

/// Whether `[from, to)` of a file reads as zeros. Only its data runs are read, since a hole is zeros.
fn zeros(
    fd: BorrowedFd<'_>,
    runs: &[(u64, u64)],
    from: u64,
    to: u64,
    buf: &mut Vec<u8>,
) -> io::Result<bool> {
    for &(offset, len) in runs {
        let (mut at, end) = (offset.max(from), (offset + len).min(to));
        while at < end {
            let n = ((end - at).min(1 << 20)) as usize;
            buf.resize(n, 0);
            read_exact_at(fd, buf, at)?;
            if buf.iter().any(|&b| b != 0) {
                return Ok(false);
            }
            at += n as u64;
        }
    }
    Ok(true)
}

struct RunReader<'a> {
    fd: BorrowedFd<'a>,
    at: u64,
    left: u64,
}

impl Read for RunReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        if want == 0 {
            return Ok(0);
        }
        let n = read_at(self.fd, &mut buf[..want], self.at)?;
        self.at += n as u64;
        self.left -= n as u64;
        crate::progress::read(n as u64);
        Ok(n)
    }
}

/// Chunk an open file. `size` is the size from the same `stat` the caller will compare afterwards.
pub fn manifest_of(fd: impl AsFd, size: u64) -> io::Result<Manifest> {
    let fd = fd.as_fd();
    let mut segments = Vec::new();
    for (offset, len) in data_runs(fd, size)? {
        let mut chunks = Vec::new();
        if len <= u64::from(CHUNK_MIN) {
            // Small data needs no cut search, and this path skips the chunker's large buffer: the
            // result is the same single chunk.
            let mut buf = vec![0u8; len as usize];
            read_exact_at(fd, &mut buf, offset)?;
            crate::progress::read(len);
            chunks.push(ChunkRef {
                hash: Hash::of(&buf),
                len: len as u32,
            });
        } else {
            let reader = RunReader {
                fd,
                at: offset,
                left: len,
            };
            for c in StreamCDC::new(reader, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX) {
                let c = c.map_err(|e| match e {
                    fastcdc::v2020::Error::IoError(io) => io,
                    other => io::Error::other(other.to_string()),
                })?;
                chunks.push(ChunkRef {
                    hash: Hash::of(&c.data),
                    len: c.length as u32,
                });
            }
        }
        segments.push(Segment { offset, chunks });
    }
    Ok(Manifest::new(size, segments))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    /// Positional writes as the engine makes them, on every platform.
    trait WriteAt {
        fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    }

    impl WriteAt for File {
        fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
            crate::fsops::write_all_at(self.as_fd(), buf, offset)
        }
    }

    fn temp(name: &str) -> (std::path::PathBuf, File) {
        let dir = std::env::temp_dir().join(format!(
            "cairn-chunk-{}",
            hex::encode(rand::random::<[u8; 8]>())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let f = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        (dir, f)
    }

    /// Deterministic noise, so that the chunker finds cut points.
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            out.extend_from_slice(&seed.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn inserting_bytes_changes_only_nearby_chunks() {
        let (dir, f) = temp("big");
        let mut data = noise(32 * 1024 * 1024, 42);
        f.write_all_at(&data, 0).unwrap();
        let before = manifest_of(&f, data.len() as u64).unwrap();
        assert!(before.chunks().count() > 3, "expected several chunks");

        data.splice(
            5_000_000..5_000_000,
            b"a few inserted bytes".iter().copied(),
        );
        f.write_all_at(&data, 0).unwrap();
        let after = manifest_of(&f, data.len() as u64).unwrap();

        let old: std::collections::HashSet<_> = before.chunks().map(|c| c.hash).collect();
        let changed = after.chunks().filter(|c| !old.contains(&c.hash)).count();
        let total = after.chunks().count();
        assert!(changed <= 3, "{changed} of {total} chunks changed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_and_small_files_are_exact() {
        let (dir, f) = temp("small");
        assert_eq!(manifest_of(&f, 0).unwrap().segments.len(), 0);
        f.write_all_at(b"hello", 0).unwrap();
        let m = manifest_of(&f, 5).unwrap();
        let c: Vec<_> = m.chunks().collect();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].hash, Hash::of(b"hello"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_same_bytes_with_other_holes_are_the_same_bytes() {
        const SIZE: u64 = 3 << 20;
        let (dir, f) = temp("sparse");
        // Sparse as the engine makes a file: NTFS has to be told before the file grows.
        crate::fsops::set_len(f.as_fd(), SIZE, true).unwrap();
        f.write_all_at(b"head", 0).unwrap();
        f.write_all_at(b"tail", SIZE - 4).unwrap();
        let sparse = manifest_of(&f, SIZE).unwrap();
        // The same bytes, with the middle written as zeros rather than left a hole.
        let (dir2, g) = temp("dense");
        g.write_all_at(&std::fs::read(dir.join("sparse")).unwrap(), 0)
            .unwrap();
        let dense = manifest_of(&g, SIZE).unwrap();
        assert_ne!(sparse, dense, "this filesystem has no holes to test with");
        assert!(same_bytes(g.as_fd(), &sparse).unwrap());
        assert!(same_bytes(f.as_fd(), &dense).unwrap());
        // One byte other than zero where one of them has a hole is not.
        g.write_all_at(b"x", 1 << 20).unwrap();
        assert!(!same_bytes(g.as_fd(), &sparse).unwrap());
        assert!(!same_bytes(g.as_fd(), &dense).unwrap());
        g.write_all_at(b"\0", 1 << 20).unwrap();
        assert!(same_bytes(g.as_fd(), &sparse).unwrap());
        g.set_len(SIZE + 1).unwrap();
        assert!(!same_bytes(g.as_fd(), &sparse).unwrap());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn holes_are_not_chunks() {
        let (dir, f) = temp("sparse");
        // A terabyte file with two blocks of data; reading it as zeros would take hours.
        let size: u64 = 1 << 40;
        crate::fsops::set_len(f.as_fd(), size, true).unwrap();
        f.write_all_at(&[1u8; 4096], 1 << 20).unwrap();
        f.write_all_at(&[2u8; 4096], 1 << 39).unwrap();
        let m = manifest_of(&f, size).unwrap();
        assert_eq!(m.size, size);
        assert_eq!(m.segments.len(), 2, "{m:?}");
        assert!(m.segments[0].offset <= 1 << 20);
        assert!(m.segments[1].offset <= 1 << 39 && m.segments[1].offset > 1 << 38);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
