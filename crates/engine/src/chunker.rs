//! Reading a file into a manifest: data segments found with `SEEK_DATA`/`SEEK_HOLE`, cut by FastCDC,
//! hashed with BLAKE3. A hole is never read as zeros, so a sparse file costs what it occupies.

use crate::fsops::read_exact_at;
use cairn_protocol::manifest::{CHUNK_AVG, CHUNK_MAX, CHUNK_MIN};
use cairn_protocol::{ChunkRef, Hash, Manifest, Segment};
use fastcdc::v2020::StreamCDC;
use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{seek, SeekFrom};
use rustix::io::Errno;
use std::io::{self, Read};

/// The data runs of a file, as (offset, length).
pub fn data_runs(fd: BorrowedFd<'_>, size: u64) -> io::Result<Vec<(u64, u64)>> {
    let mut runs = Vec::new();
    let mut at = 0;
    while at < size {
        let start = match seek(fd, SeekFrom::Data(at)) {
            Ok(o) => o,
            // No data at or after `at`: the rest is a hole.
            Err(Errno::NXIO) => break,
            // Not supported here: treat all of it as data.
            Err(Errno::INVAL | Errno::OPNOTSUPP) => return Ok(vec![(0, size)]),
            Err(e) => return Err(e.into()),
        };
        if start >= size {
            break;
        }
        let end = match seek(fd, SeekFrom::Hole(start)) {
            Ok(o) => o.min(size),
            Err(Errno::NXIO) => size,
            Err(e) => return Err(e.into()),
        };
        runs.push((start, end - start));
        at = end;
    }
    Ok(runs)
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
        let n = loop {
            match rustix::io::pread(self.fd, &mut buf[..want], self.at) {
                Ok(n) => break n,
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        };
        self.at += n as u64;
        self.left -= n as u64;
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
    use std::os::unix::fs::FileExt;

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
    fn holes_are_not_chunks() {
        let (dir, f) = temp("sparse");
        // A terabyte file with two blocks of data; reading it as zeros would take hours.
        let size: u64 = 1 << 40;
        f.set_len(size).unwrap();
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
