//! Filesystem access that cannot be redirected through a symlink (invariant 6).
//!
//! Every directory is reached one component at a time from an open root with `O_NOFOLLOW`, and every
//! operation then names a single entry inside that directory's descriptor. No full path is ever
//! handed to the kernel, so swapping a directory for a symlink mid-run cannot move an operation
//! outside the tree.

use cairn_protocol::{Name, Timestamp};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{
    self, AtFlags, Mode, OFlags, RenameFlags, Statx, StatxFlags, Timespec, Timestamps, CWD,
};
use rustix::io::Errno;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

fn os(name: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(name))
}

/// An open directory, reached without following any symlink.
#[derive(Debug)]
pub struct Dir(pub OwnedFd);

/// What `stat` says about an entry, all of it from one `statx` call.
#[derive(Clone, Debug)]
pub struct Stat {
    pub file_type: fs::FileType,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub size: u64,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
    pub btime: Option<Timestamp>,
    pub mnt_id: u64,
}

fn stamp(t: rustix::fs::StatxTimestamp) -> Timestamp {
    Timestamp {
        sec: t.tv_sec,
        nsec: t.tv_nsec,
    }
}

fn from_statx(s: Statx) -> Stat {
    Stat {
        file_type: fs::FileType::from_raw_mode(u32::from(s.stx_mode)),
        mode: u32::from(s.stx_mode) & 0o7777,
        uid: s.stx_uid,
        gid: s.stx_gid,
        dev: (u64::from(s.stx_dev_major) << 32) | u64::from(s.stx_dev_minor),
        ino: s.stx_ino,
        nlink: u64::from(s.stx_nlink),
        size: s.stx_size,
        mtime: stamp(s.stx_mtime),
        ctime: stamp(s.stx_ctime),
        btime: (s.stx_mask & StatxFlags::BTIME.bits() != 0).then(|| stamp(s.stx_btime)),
        mnt_id: s.stx_mnt_id,
    }
}

const STATX_WANT: StatxFlags = StatxFlags::BASIC_STATS
    .union(StatxFlags::BTIME)
    .union(StatxFlags::MNT_ID);

pub fn is_not_found(e: &io::Error) -> bool {
    e.raw_os_error() == Some(Errno::NOENT.raw_os_error())
}

pub fn is_permission(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(c) if c == Errno::ACCESS.raw_os_error() || c == Errno::PERM.raw_os_error())
}

pub fn is_exists(e: &io::Error) -> bool {
    e.raw_os_error() == Some(Errno::EXIST.raw_os_error())
}

impl Dir {
    /// Open a root by its path. The last component must be a real directory; symlinks above it are
    /// the user's own choice of where the root lives.
    pub fn open_root(path: &Path) -> io::Result<Dir> {
        let fd = fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Dir(fd))
    }

    pub fn dup(&self) -> io::Result<Dir> {
        Ok(Dir(rustix::io::dup(&self.0)?))
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }

    /// A child directory. Fails on a symlink, whatever it points at.
    pub fn open_dir(&self, name: &[u8]) -> io::Result<Dir> {
        let fd = fs::openat(
            &self.0,
            os(name),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Dir(fd))
    }

    /// A chain of child directories.
    pub fn open_path<'a>(&self, path: impl IntoIterator<Item = &'a Name>) -> io::Result<Dir> {
        let mut at = self.dup()?;
        for name in path {
            at = at.open_dir(name.as_bytes())?;
        }
        Ok(at)
    }

    /// Create a chain of directories as needed, without following symlinks. For state directories.
    pub fn ensure_dir(&self, name: &[u8], mode: u32) -> io::Result<Dir> {
        match fs::mkdirat(&self.0, os(name), Mode::from_raw_mode(mode)) {
            Ok(()) => {}
            Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        self.open_dir(name)
    }

    pub fn stat_self(&self) -> io::Result<Stat> {
        Ok(from_statx(fs::statx(
            &self.0,
            "",
            AtFlags::EMPTY_PATH,
            STATX_WANT,
        )?))
    }

    pub fn stat(&self, name: &[u8]) -> io::Result<Stat> {
        Ok(from_statx(fs::statx(
            &self.0,
            os(name),
            AtFlags::SYMLINK_NOFOLLOW,
            STATX_WANT,
        )?))
    }

    pub fn try_stat(&self, name: &[u8]) -> io::Result<Option<Stat>> {
        match self.stat(name) {
            Ok(s) => Ok(Some(s)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Entry names, sorted so that a scan is deterministic.
    pub fn list(&self) -> io::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut d = fs::Dir::read_from(&self.0)?;
        // The descriptor may have been listed before.
        d.rewind();
        for entry in d {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                out.push(name.to_vec());
            }
        }
        out.sort();
        Ok(out)
    }

    /// The names of entries that are, or may be, directories. Most filesystems say without a stat.
    pub fn subdirs(&self) -> io::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut d = fs::Dir::read_from(&self.0)?;
        d.rewind();
        for entry in d {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"."
                && name != b".."
                && matches!(
                    entry.file_type(),
                    fs::FileType::Directory | fs::FileType::Unknown
                )
            {
                out.push(name.to_vec());
            }
        }
        Ok(out)
    }

    /// Open a regular file for reading. `O_NONBLOCK` so that a FIFO swapped in cannot hang a scan.
    pub fn open_read(&self, name: &[u8]) -> io::Result<OwnedFd> {
        Ok(fs::openat(
            &self.0,
            os(name),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    }

    /// Create a new file, refusing anything already there.
    pub fn create_new(&self, name: &[u8], mode: u32) -> io::Result<OwnedFd> {
        Ok(fs::openat(
            &self.0,
            os(name),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode),
        )?)
    }

    pub fn read_link(&self, name: &[u8]) -> io::Result<Vec<u8>> {
        Ok(fs::readlinkat(&self.0, os(name), Vec::new())?.into_bytes())
    }

    pub fn symlink(&self, target: &[u8], name: &[u8]) -> io::Result<()> {
        Ok(fs::symlinkat(os(target), &self.0, os(name))?)
    }

    pub fn mkdir(&self, name: &[u8], mode: u32) -> io::Result<()> {
        Ok(fs::mkdirat(&self.0, os(name), Mode::from_raw_mode(mode))?)
    }

    pub fn unlink(&self, name: &[u8]) -> io::Result<()> {
        Ok(fs::unlinkat(&self.0, os(name), AtFlags::empty())?)
    }

    /// Remove an empty directory; a directory that gained an entry stays.
    pub fn rmdir(&self, name: &[u8]) -> io::Result<()> {
        Ok(fs::unlinkat(&self.0, os(name), AtFlags::REMOVEDIR)?)
    }

    /// Atomic rename that fails rather than replace anything at the destination.
    pub fn rename_new(&self, from: &[u8], to_dir: &Dir, to: &[u8]) -> io::Result<()> {
        Ok(fs::renameat_with(
            &self.0,
            os(from),
            &to_dir.0,
            os(to),
            RenameFlags::NOREPLACE,
        )?)
    }

    /// Atomically swap two entries. `Ok(false)` where this filesystem cannot.
    pub fn exchange(&self, a: &[u8], b: &[u8]) -> io::Result<bool> {
        match fs::renameat_with(&self.0, os(a), &self.0, os(b), RenameFlags::EXCHANGE) {
            Ok(()) => Ok(true),
            Err(Errno::INVAL | Errno::NOSYS) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic rename over whatever is at the destination.
    pub fn rename_over(&self, from: &[u8], to_dir: &Dir, to: &[u8]) -> io::Result<()> {
        Ok(fs::renameat(&self.0, os(from), &to_dir.0, os(to))?)
    }

    pub fn hard_link(&self, from: &[u8], to_dir: &Dir, to: &[u8]) -> io::Result<()> {
        Ok(fs::linkat(
            &self.0,
            os(from),
            &to_dir.0,
            os(to),
            AtFlags::empty(),
        )?)
    }

    pub fn sync(&self) -> io::Result<()> {
        Ok(fs::fsync(&self.0)?)
    }

    /// The path that addresses one entry through this descriptor, for the calls that only take a
    /// path. The kernel resolves the descriptor's magic link to this very directory; the entry's
    /// last component is then looked up by the `l*` call without following a symlink.
    fn proc_entry(&self, name: &[u8]) -> PathBuf {
        let mut path = PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()));
        path.push(os(name));
        path
    }

    pub fn list_xattrs(&self, name: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let path = self.proc_entry(name);
        let mut buf = vec![0u8; 4096];
        loop {
            match fs::llistxattr(&path, &mut buf[..]) {
                Ok(n) => {
                    return Ok(buf[..n]
                        .split(|&b| b == 0)
                        .filter(|s| !s.is_empty())
                        .map(<[u8]>::to_vec)
                        .collect());
                }
                Err(Errno::RANGE) => buf.resize(buf.len() * 4, 0),
                Err(Errno::NOTSUP) => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn get_xattr(&self, name: &[u8], key: &[u8]) -> io::Result<Vec<u8>> {
        let path = self.proc_entry(name);
        let mut buf = vec![0u8; 4096];
        loop {
            match fs::lgetxattr(&path, os(key), &mut buf[..]) {
                Ok(n) => {
                    buf.truncate(n);
                    return Ok(buf);
                }
                Err(Errno::RANGE) => buf.resize(buf.len() * 4, 0),
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn set_xattr(&self, name: &[u8], key: &[u8], value: &[u8]) -> io::Result<()> {
        Ok(fs::lsetxattr(
            self.proc_entry(name),
            os(key),
            value,
            fs::XattrFlags::empty(),
        )?)
    }

    pub fn remove_xattr(&self, name: &[u8], key: &[u8]) -> io::Result<()> {
        Ok(fs::lremovexattr(self.proc_entry(name), os(key))?)
    }

    pub fn chown(&self, name: &[u8], uid: u32, gid: u32) -> io::Result<()> {
        let (u, g) = (fs::Uid::from_raw(uid), fs::Gid::from_raw(gid));
        Ok(fs::chownat(
            &self.0,
            os(name),
            Some(u),
            Some(g),
            AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }

    /// `chmod` of the entry itself. A descriptor opened with `O_PATH | O_NOFOLLOW` pins the inode,
    /// and its magic link reaches that inode and nothing else. Refuses symlinks.
    pub fn chmod(&self, name: &[u8], mode: u32) -> io::Result<()> {
        let fd = fs::openat(
            &self.0,
            os(name),
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let st = fs::fstat(&fd)?;
        if fs::FileType::from_raw_mode(st.st_mode) == fs::FileType::Symlink {
            return Err(Errno::LOOP.into());
        }
        Ok(fs::chmodat(
            CWD,
            format!("/proc/self/fd/{}", fd.as_raw_fd()),
            Mode::from_raw_mode(mode),
            AtFlags::empty(),
        )?)
    }

    pub fn set_mtime(&self, name: &[u8], mtime: Timestamp) -> io::Result<()> {
        let times = Timestamps {
            last_access: Timespec {
                tv_sec: 0,
                tv_nsec: fs::UTIME_OMIT,
            },
            last_modification: Timespec {
                tv_sec: mtime.sec,
                tv_nsec: i64::from(mtime.nsec) as _,
            },
        };
        Ok(fs::utimensat(
            &self.0,
            os(name),
            &times,
            AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }

    pub fn fstype_precision_ns(&self) -> u32 {
        // Filesystems whose timestamps are coarser than a nanosecond; others keep what they are given.
        const MSDOS_SUPER_MAGIC: i64 = 0x4d44;
        // The width of `f_type` differs between architectures.
        #[allow(clippy::unnecessary_cast)]
        match fs::fstatfs(&self.0) {
            Ok(s) if s.f_type as i64 == MSDOS_SUPER_MAGIC => 2_000_000_000,
            _ => 1,
        }
    }
}

/// Keep going on interrupts; a short positional read is otherwise a real end of data.
pub fn read_exact_at(fd: BorrowedFd<'_>, buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match rustix::io::pread(fd, &mut buf[filled..], offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                filled += n;
                offset += n as u64;
            }
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn write_all_at(fd: BorrowedFd<'_>, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match rustix::io::pwrite(fd, buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
