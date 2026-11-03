//! Filesystem access that cannot be redirected through a symlink (invariant 6).
//!
//! Every directory is reached one component at a time from an open root with `O_NOFOLLOW`, and every
//! operation then names a single entry inside that directory's descriptor. No full path is ever
//! handed to the kernel, so swapping a directory for a symlink mid-run cannot move an operation
//! outside the tree.

use cairn_protocol::{Name, Timestamp};
#[cfg(target_os = "linux")]
use rustix::fd::AsRawFd;
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{self, AtFlags, Mode, OFlags, RenameFlags, Timespec, Timestamps};
#[cfg(target_os = "linux")]
use rustix::fs::{Statx, StatxFlags, CWD};
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

/// What `stat` says about an entry, all of it from one call.
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
    /// Which mount it is on. macOS has no bind mounts, so there the device number says it.
    pub mnt_id: u64,
}

#[cfg(target_os = "linux")]
fn stamp(t: rustix::fs::StatxTimestamp) -> Timestamp {
    Timestamp {
        sec: t.tv_sec,
        nsec: t.tv_nsec,
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
const STATX_WANT: StatxFlags = StatxFlags::BASIC_STATS
    .union(StatxFlags::BTIME)
    .union(StatxFlags::MNT_ID);

#[cfg(target_os = "macos")]
fn from_stat(s: fs::Stat) -> Stat {
    let at = |sec: i64, nsec: i64| Timestamp {
        sec,
        nsec: nsec as u32,
    };
    Stat {
        file_type: fs::FileType::from_raw_mode(s.st_mode as _),
        mode: u32::from(s.st_mode) & 0o7777,
        uid: s.st_uid,
        gid: s.st_gid,
        dev: s.st_dev as u64,
        ino: s.st_ino,
        nlink: u64::from(s.st_nlink),
        size: s.st_size as u64,
        mtime: at(s.st_mtime, s.st_mtime_nsec),
        ctime: at(s.st_ctime, s.st_ctime_nsec),
        btime: Some(at(s.st_birthtime, s.st_birthtime_nsec)),
        mnt_id: s.st_dev as u64,
    }
}

/// `true` when this error says an xattr is not there.
pub fn is_no_xattr(e: &io::Error) -> bool {
    #[cfg(target_os = "linux")]
    let absent = Errno::NODATA;
    #[cfg(target_os = "macos")]
    let absent = Errno::NOATTR;
    e.raw_os_error() == Some(absent.raw_os_error())
}

/// What survives a power cut. On macOS a plain `fsync` stops at the drive's cache, and only
/// `F_FULLFSYNC` goes past it; the filesystems that refuse it get what they do offer.
pub fn sync_fd(fd: BorrowedFd<'_>) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if fs::fcntl_fullfsync(fd).is_ok() {
        return Ok(());
    }
    Ok(fs::fsync(fd)?)
}

pub fn is_not_found(e: &io::Error) -> bool {
    e.raw_os_error() == Some(Errno::NOENT.raw_os_error())
}

pub fn is_permission(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(c) if c == Errno::ACCESS.raw_os_error() || c == Errno::PERM.raw_os_error())
}

pub fn is_exists(e: &io::Error) -> bool {
    e.raw_os_error() == Some(Errno::EXIST.raw_os_error())
}

/// Whether another program has this file open for writing: the kernel grants a read lease only when
/// none has. `None` where that cannot be asked: a file of another user's, a filesystem whose leases
/// mean something else (NFS refuses one for every file it holds no delegation for), or not Linux.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn open_for_writing(fd: BorrowedFd<'_>) -> Option<bool> {
    use std::ffi::c_int;
    extern "C" {
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    }
    const F_SETSIG: c_int = 10;
    const F_SETLEASE: c_int = 1024;
    const F_RDLCK: c_int = 0;
    const F_UNLCK: c_int = 2;
    // What a writer opening the file in the instant the lease is held sends instead of SIGIO, which
    // would end the process: ignored unless handled, and nothing here handles it.
    const SIGURG: c_int = 23;
    // Local filesystems that use the kernel's own leases: ext2 to 4, XFS, Btrfs, tmpfs, F2FS,
    // bcachefs, ZFS.
    const GENERIC_LEASES: [i64; 7] = [
        0xEF53,
        0x5846_5342,
        0x9123_683E,
        0x0102_1994,
        0xF2F5_2010,
        0xCA45_1A4E,
        0x2FC1_2FC1,
    ];
    // The width of `f_type` differs between architectures.
    #[allow(clippy::unnecessary_cast)]
    let kind = fs::fstatfs(fd).ok()?.f_type as i64;
    if !GENERIC_LEASES.contains(&kind) {
        return None;
    }
    let raw = fd.as_raw_fd();
    // SAFETY: the descriptor is open for these calls, and each takes an integer argument.
    unsafe {
        if fcntl(raw, F_SETSIG, SIGURG) == -1 {
            return None;
        }
        if fcntl(raw, F_SETLEASE, F_RDLCK) == -1 {
            let busy =
                io::Error::last_os_error().raw_os_error() == Some(Errno::AGAIN.raw_os_error());
            return busy.then_some(true);
        }
        fcntl(raw, F_SETLEASE, F_UNLCK);
    }
    Some(false)
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub fn open_for_writing(_: BorrowedFd<'_>) -> Option<bool> {
    None
}

/// `F_PUNCHHOLE`, which rustix does not have. Both must be multiples of the filesystem's block.
#[cfg(target_os = "macos")]
pub fn punch_hole(fd: BorrowedFd<'_>, offset: u64, len: u64) -> io::Result<()> {
    use rustix::fd::AsRawFd;
    #[repr(C)]
    struct PunchHole {
        flags: u32,
        reserved: u32,
        offset: i64,
        length: i64,
    }
    extern "C" {
        fn fcntl(fd: std::ffi::c_int, cmd: std::ffi::c_int, ...) -> std::ffi::c_int;
    }
    const F_PUNCHHOLE: std::ffi::c_int = 99;
    let arg = PunchHole {
        flags: 0,
        reserved: 0,
        offset: offset as i64,
        length: len as i64,
    };
    // SAFETY: the descriptor is open for the call, and the argument is the struct this command reads.
    if unsafe { fcntl(fd.as_raw_fd(), F_PUNCHHOLE, &arg as *const PunchHole) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
        match fs::mkdirat(&self.0, os(name), Mode::from_raw_mode(mode as _)) {
            Ok(()) => {}
            Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        self.open_dir(name)
    }

    #[cfg(target_os = "linux")]
    pub fn stat_self(&self) -> io::Result<Stat> {
        Ok(from_statx(fs::statx(
            &self.0,
            "",
            AtFlags::EMPTY_PATH,
            STATX_WANT,
        )?))
    }

    #[cfg(target_os = "macos")]
    pub fn stat_self(&self) -> io::Result<Stat> {
        Ok(from_stat(fs::fstat(&self.0)?))
    }

    #[cfg(target_os = "linux")]
    pub fn stat(&self, name: &[u8]) -> io::Result<Stat> {
        Ok(from_statx(fs::statx(
            &self.0,
            os(name),
            AtFlags::SYMLINK_NOFOLLOW,
            STATX_WANT,
        )?))
    }

    #[cfg(target_os = "macos")]
    pub fn stat(&self, name: &[u8]) -> io::Result<Stat> {
        Ok(from_stat(fs::statat(
            &self.0,
            os(name),
            AtFlags::SYMLINK_NOFOLLOW,
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
            Mode::from_raw_mode(mode as _),
        )?)
    }

    pub fn read_link(&self, name: &[u8]) -> io::Result<Vec<u8>> {
        Ok(fs::readlinkat(&self.0, os(name), Vec::new())?.into_bytes())
    }

    pub fn symlink(&self, target: &[u8], name: &[u8]) -> io::Result<()> {
        Ok(fs::symlinkat(os(target), &self.0, os(name))?)
    }

    pub fn mkdir(&self, name: &[u8], mode: u32) -> io::Result<()> {
        Ok(fs::mkdirat(
            &self.0,
            os(name),
            Mode::from_raw_mode(mode as _),
        )?)
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
        sync_fd(self.fd())
    }

    /// The path that addresses one entry through this descriptor, for the calls that only take a
    /// path. The kernel resolves the descriptor's magic link to this very directory; the entry's
    /// last component is then looked up by the `l*` call without following a symlink.
    #[cfg(target_os = "linux")]
    fn proc_entry(&self, name: &[u8]) -> io::Result<PathBuf> {
        let mut path = PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()));
        path.push(os(name));
        Ok(path)
    }

    /// macOS has no such link and no xattr call relative to a descriptor, so this asks the kernel for
    /// the directory's path as it is now. A parent swapped for a symlink in the instant after that can
    /// redirect this one call: the one place here where invariant 6 is weaker than on Linux.
    #[cfg(target_os = "macos")]
    fn proc_entry(&self, name: &[u8]) -> io::Result<PathBuf> {
        let dir = fs::getpath(&self.0)?;
        let mut path = PathBuf::from(OsStr::from_bytes(dir.as_bytes()));
        path.push(os(name));
        Ok(path)
    }

    pub fn list_xattrs(&self, name: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let path = self.proc_entry(name)?;
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
        let path = self.proc_entry(name)?;
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
            self.proc_entry(name)?,
            os(key),
            value,
            fs::XattrFlags::empty(),
        )?)
    }

    pub fn remove_xattr(&self, name: &[u8], key: &[u8]) -> io::Result<()> {
        Ok(fs::lremovexattr(self.proc_entry(name)?, os(key))?)
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
    #[cfg(target_os = "linux")]
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
            Mode::from_raw_mode(mode as _),
            AtFlags::empty(),
        )?)
    }

    /// macOS can do this directly: the flag never follows a symlink, so one swapped in at the last
    /// moment gets the mode itself and nothing else does. Refuses symlinks otherwise.
    #[cfg(target_os = "macos")]
    pub fn chmod(&self, name: &[u8], mode: u32) -> io::Result<()> {
        if self.stat(name)?.file_type == fs::FileType::Symlink {
            return Err(Errno::LOOP.into());
        }
        Ok(fs::chmodat(
            &self.0,
            os(name),
            Mode::from_raw_mode(mode as _),
            AtFlags::SYMLINK_NOFOLLOW,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_open_for_writing_says_so() {
        let dir = std::env::temp_dir().join(format!("cairn-lease-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("f");
        std::fs::write(&path, b"x").unwrap();
        let read = std::fs::File::open(&path).unwrap();
        let Some(idle) = open_for_writing(read.as_fd()) else {
            eprintln!("this filesystem cannot say: skipped");
            return;
        };
        assert!(!idle);
        let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert_eq!(open_for_writing(read.as_fd()), Some(true));
        drop(writer);
        assert_eq!(open_for_writing(read.as_fd()), Some(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
