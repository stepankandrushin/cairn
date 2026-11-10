//! A zip written as it goes: entries stored, not compressed, each with its size and CRC-32 in its local
//! header, so that any unzip reads it, streaming ones too. Zip64 only where a size or offset needs it.

use std::io::{self, Write};

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// CRC-32 (IEEE), as zip has it, carried across calls.
#[derive(Clone, Copy)]
pub struct Crc(u32);

impl Crc {
    pub fn new() -> Self {
        Crc(0xFFFF_FFFF)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn value(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

const MAX32: u64 = 0xFFFF_FFFF;

/// What the central directory says of an entry.
struct Entry {
    name: Vec<u8>,
    dir: bool,
    mode: u32,
    mtime: Option<u32>,
    dos: (u16, u16),
    crc: u32,
    size: u64,
    offset: u64,
}

pub struct ZipWriter<W: Write> {
    out: W,
    at: u64,
    entries: Vec<Entry>,
}

/// MS-DOS date and time, as zip needs them, from seconds since the epoch in UTC; 1980 at the earliest.
fn dos_time(sec: i64) -> (u16, u16) {
    let sec = sec.max(315_532_800);
    let days = sec.div_euclid(86_400);
    let rem = sec.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = (yoe + era * 400 + i64::from(m <= 2)).min(2107);
    let time = ((rem / 3600) << 11) | (((rem % 3600) / 60) << 5) | ((rem % 60) / 2);
    let date = ((y - 1980) << 9) | (m << 5) | d;
    (time as u16, date as u16)
}

impl<W: Write> ZipWriter<W> {
    pub fn new(out: W) -> Self {
        ZipWriter {
            out,
            at: 0,
            entries: Vec::new(),
        }
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.at += bytes.len() as u64;
        Ok(())
    }

    /// The local header of an entry whose size and CRC are known; its data follows, `size` bytes.
    fn header(
        &mut self,
        name: &[u8],
        dir: bool,
        mode: u32,
        sec: i64,
        crc: u32,
        size: u64,
    ) -> io::Result<()> {
        let mtime = u32::try_from(sec).ok();
        let zip64 = size >= MAX32;
        let dos = dos_time(sec);
        let mut extra = Vec::new();
        if zip64 {
            extra.extend_from_slice(&1u16.to_le_bytes());
            extra.extend_from_slice(&16u16.to_le_bytes());
            extra.extend_from_slice(&size.to_le_bytes());
            extra.extend_from_slice(&size.to_le_bytes());
        }
        if let Some(t) = mtime {
            extra.extend_from_slice(&0x5455u16.to_le_bytes());
            extra.extend_from_slice(&5u16.to_le_bytes());
            extra.push(1);
            extra.extend_from_slice(&t.to_le_bytes());
        }
        self.entries.push(Entry {
            name: name.to_vec(),
            dir,
            mode,
            mtime,
            dos,
            crc,
            size,
            offset: self.at,
        });
        let small = if zip64 { MAX32 as u32 } else { size as u32 };
        let mut h = Vec::with_capacity(30 + name.len() + extra.len());
        h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        h.extend_from_slice(&(if zip64 { 45u16 } else { 20u16 }).to_le_bytes());
        // UTF-8 names.
        h.extend_from_slice(&0x0800u16.to_le_bytes());
        h.extend_from_slice(&0u16.to_le_bytes());
        h.extend_from_slice(&dos.0.to_le_bytes());
        h.extend_from_slice(&dos.1.to_le_bytes());
        h.extend_from_slice(&crc.to_le_bytes());
        h.extend_from_slice(&small.to_le_bytes());
        h.extend_from_slice(&small.to_le_bytes());
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        h.extend_from_slice(name);
        h.extend_from_slice(&extra);
        self.put(&h)
    }

    /// A folder: `name` ends in `/`.
    pub fn dir(&mut self, name: &[u8], mode: u32, sec: i64) -> io::Result<()> {
        self.header(name, true, mode, sec, 0, 0)
    }

    /// A file whose CRC is known: its bytes come next, by `data`, exactly `size` of them.
    pub fn file_header(
        &mut self,
        name: &[u8],
        mode: u32,
        sec: i64,
        crc: u32,
        size: u64,
    ) -> io::Result<()> {
        self.header(name, false, mode, sec, crc, size)
    }

    pub fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.put(bytes)
    }

    /// The central directory and its end, then the writer back.
    pub fn finish(mut self) -> io::Result<W> {
        let start = self.at;
        let entries = std::mem::take(&mut self.entries);
        for e in &entries {
            let mut extra = Vec::new();
            let mut big = Vec::new();
            if e.size >= MAX32 {
                big.extend_from_slice(&e.size.to_le_bytes());
                big.extend_from_slice(&e.size.to_le_bytes());
            }
            if e.offset >= MAX32 {
                big.extend_from_slice(&e.offset.to_le_bytes());
            }
            if !big.is_empty() {
                extra.extend_from_slice(&1u16.to_le_bytes());
                extra.extend_from_slice(&(big.len() as u16).to_le_bytes());
                extra.extend_from_slice(&big);
            }
            if let Some(t) = e.mtime {
                extra.extend_from_slice(&0x5455u16.to_le_bytes());
                extra.extend_from_slice(&5u16.to_le_bytes());
                extra.push(1);
                extra.extend_from_slice(&t.to_le_bytes());
            }
            let zip64 = !big.is_empty();
            let small = if e.size >= MAX32 {
                MAX32 as u32
            } else {
                e.size as u32
            };
            let offset = if e.offset >= MAX32 {
                MAX32 as u32
            } else {
                e.offset as u32
            };
            let kind = if e.dir { 0o040000 } else { 0o100000 };
            let external = ((kind | (e.mode & 0o777)) << 16) | if e.dir { 0x10 } else { 0 };
            let mut h = Vec::with_capacity(46 + e.name.len() + extra.len());
            h.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            // Made on Unix, so the modes are read.
            h.extend_from_slice(&((3u16 << 8) | 45).to_le_bytes());
            h.extend_from_slice(&(if zip64 { 45u16 } else { 20u16 }).to_le_bytes());
            h.extend_from_slice(&0x0800u16.to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            h.extend_from_slice(&e.dos.0.to_le_bytes());
            h.extend_from_slice(&e.dos.1.to_le_bytes());
            h.extend_from_slice(&e.crc.to_le_bytes());
            h.extend_from_slice(&small.to_le_bytes());
            h.extend_from_slice(&small.to_le_bytes());
            h.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // comment
            h.extend_from_slice(&0u16.to_le_bytes()); // disk
            h.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            h.extend_from_slice(&external.to_le_bytes());
            h.extend_from_slice(&offset.to_le_bytes());
            h.extend_from_slice(&e.name);
            h.extend_from_slice(&extra);
            self.put(&h)?;
        }
        let size = self.at - start;
        let count = entries.len() as u64;
        if count >= 0xFFFF || size >= MAX32 || start >= MAX32 {
            let record = self.at;
            let mut z = Vec::with_capacity(56 + 20);
            z.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
            z.extend_from_slice(&44u64.to_le_bytes());
            z.extend_from_slice(&((3u16 << 8) | 45).to_le_bytes());
            z.extend_from_slice(&45u16.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&count.to_le_bytes());
            z.extend_from_slice(&count.to_le_bytes());
            z.extend_from_slice(&size.to_le_bytes());
            z.extend_from_slice(&start.to_le_bytes());
            z.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&record.to_le_bytes());
            z.extend_from_slice(&1u32.to_le_bytes());
            self.put(&z)?;
        }
        let mut end = Vec::with_capacity(22);
        end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        let c16 = count.min(0xFFFF) as u16;
        end.extend_from_slice(&c16.to_le_bytes());
        end.extend_from_slice(&c16.to_le_bytes());
        end.extend_from_slice(&(size.min(MAX32) as u32).to_le_bytes());
        end.extend_from_slice(&(start.min(MAX32) as u32).to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        self.put(&end)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_is_zips() {
        let mut c = Crc::new();
        c.update(b"123456789");
        assert_eq!(c.value(), 0xCBF4_3926);
        let mut split = Crc::new();
        split.update(b"1234");
        split.update(b"56789");
        assert_eq!(split.value(), 0xCBF4_3926);
        assert_eq!(Crc::new().value(), 0);
    }

    #[test]
    fn dos_times() {
        // 2026-11-10 13:14:16 UTC.
        let (t, d) = dos_time(1_794_316_456);
        assert_eq!((d >> 9) + 1980, 2026);
        assert_eq!((d >> 5) & 0xF, 11);
        assert_eq!(d & 0x1F, 10);
        assert_eq!((t >> 11, (t >> 5) & 0x3F, (t & 0x1F) * 2), (13, 14, 16));
        // Before 1980, as 1980.
        assert_eq!(dos_time(0).1 >> 9, 0);
    }
}

#[cfg(test)]
mod big {
    use super::*;

    /// Counts what is written, keeping only what a reader needs to check: everything but the zeros of
    /// the big entry's data.
    struct Sink {
        kept: Vec<u8>,
        skipping: bool,
        len: u64,
    }

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.len += b.len() as u64;
            if !self.skipping {
                self.kept.extend_from_slice(b);
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Past 65,535 entries and 4 GiB, the zip64 records say what the classic ones cannot.
    #[test]
    fn zip64_where_sizes_and_counts_need_it() {
        let mut z = ZipWriter::new(Sink {
            kept: Vec::new(),
            skipping: false,
            len: 0,
        });
        z.dir(b"top/", 0o755, 1_700_000_000).unwrap();
        let big: u64 = (1 << 32) + 5;
        let zeros = vec![0u8; 1 << 20];
        // The CRC of that many zeros, which the writer takes as given.
        z.file_header(b"top/big", 0o644, 1_700_000_000, 0xB1C2_A1A3, big)
            .unwrap();
        let split = z.out.kept.len();
        z.out.skipping = true;
        let mut left = big;
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            z.data(&zeros[..n]).unwrap();
            left -= n as u64;
        }
        z.out.skipping = false;
        for i in 0..70_000u32 {
            z.file_header(
                format!("top/{i}").as_bytes(),
                0o600,
                0,
                Crc::new().value(),
                0,
            )
            .unwrap();
        }
        let sink = z.finish().unwrap();
        let k = &sink.kept;
        let u32le = |at: usize| u32::from_le_bytes(k[at..at + 4].try_into().unwrap());
        let u64le = |at: usize| u64::from_le_bytes(k[at..at + 8].try_into().unwrap());
        // The end record, then before it the zip64 locator and record.
        let end = k.len() - 22;
        assert_eq!(u32le(end), 0x0605_4b50);
        assert_eq!(u32le(end - 20), 0x0706_4b50);
        let record_at = u64le(end - 20 + 8);
        let record = k.len() - 22 - 20 - 56;
        assert_eq!(u32le(record), 0x0606_4b50);
        assert_eq!(record_at, sink.len - 22 - 20 - 56);
        assert_eq!(u64le(record + 32), 70_002);
        let cd_size = u64le(record + 40);
        let cd_start = u64le(record + 48);
        assert_eq!(cd_start + cd_size, record_at);
        assert!(cd_start > big);
        // For an unzip to read: the zeros as a hole, so the file costs what the rest does.
        if let Ok(path) = std::env::var("CAIRN_ZIP64_OUT") {
            use std::io::{Seek, SeekFrom};
            let mut f = std::fs::File::create(path).unwrap();
            f.write_all(&k[..split]).unwrap();
            f.seek(SeekFrom::Current(big as i64)).unwrap();
            f.write_all(&k[split..]).unwrap();
        }
    }
}
