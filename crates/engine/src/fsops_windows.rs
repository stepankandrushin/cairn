//! Filesystem access that cannot be redirected through a symlink or junction (invariant 6), on Windows.
//!
//! As on Unix, every directory is reached one component at a time from an open root, and every
//! operation then names a single entry inside that directory's handle: `NtCreateFile` takes a name
//! relative to an open directory, which is Windows's `openat`, and renames and links name their new
//! directory by handle too. Every open passes `FILE_OPEN_REPARSE_POINT`, so a symlink, junction or
//! mount point is opened as itself and never followed, and a directory that turns out to be one is
//! refused. No full path is handed to the system after the root, so swapping a directory for a junction
//! mid-run cannot move an operation outside the tree.
//!
//! What NTFS cannot hold, it is not asked to: names Windows programs cannot open (`:`, `?`, `CON`, a
//! trailing dot or space, bytes that are not Unicode) are refused before any call, so an arriving name
//! is out of view here rather than turned into an alternate data stream or a file nothing can open.
//! There are no modes, owners or xattrs: a scan reports a fixed mode (the read-only attribute as the
//! write bits of a file) and no owner, so what is stored stays as it was (invariant 2). Symlinks are
//! neither made nor read yet: an arriving one is out of view, and one made here is reported.
//!
//! Every open shares reading, writing and deleting, so a scan never stands in another program's way.

use cairn_protocol::{Name, Timestamp};
use std::ffi::OsStr;
use std::io;
use std::mem::{offset_of, size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle};
pub use std::os::windows::io::{BorrowedHandle as BorrowedFd, OwnedHandle as OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    NtCreateFile, NtQueryInformationFile, NtSetInformationFile, FILE_LINK_INFORMATION,
    FILE_RENAME_INFORMATION, FILE_STAT_INFORMATION,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, RtlNtStatusToDosError, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FlushFileBuffers, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetVolumeInformationByHandleW, LockFileEx, ReOpenFile, ReadFile, SetFileInformationByHandle,
    WriteFile, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_END_OF_FILE_INFO, FILE_FULL_DIR_INFO,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK, OVERLAPPED};

/// The handle types, under the names the rest of the engine uses for descriptors.
pub trait AsFd {
    fn as_fd(&self) -> BorrowedFd<'_>;
}

impl<T: AsHandle> AsFd for T {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.as_handle()
    }
}

/// Whether this platform makes symlinks. Not Windows yet: one needs a privilege or developer mode, and
/// a choice between a file and a directory link that a Unix target does not make.
pub const HOLDS_SYMLINKS: bool = false;

/// Why a name refused for its bytes cannot be held, when nothing more precise is known.
pub const BAD_NAME: &str = "Windows programs cannot open this name";

/// What a scan says of an entry whose type it does not know: on Windows, a link of some kind.
pub const UNKNOWN_TYPE: &str =
    "a symlink, junction or mount point: this device does not sync these yet, and never follows one";

/// What `stat` says about an entry, in the terms the engine uses on every platform.
#[derive(Clone, Debug)]
pub struct Stat {
    pub file_type: FileType,
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
    /// Which volume it is on: its serial number, as for `dev`.
    pub mnt_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileType {
    RegularFile,
    Directory,
    Symlink,
    Fifo,
    Socket,
    CharacterDevice,
    BlockDevice,
    Unknown,
}

/// An open directory, reached without following any link.
#[derive(Debug)]
pub struct Dir {
    handle: OwnedFd,
    /// The volume's serial number. Everything under a directory is on its volume, since a mount point
    /// is never entered.
    volume: u64,
    precision: OnceLock<u32>,
}

// Access rights, sharing, attributes and options: values fixed by the Windows ABI.
const FILE_LIST_DIRECTORY: u32 = 0x1;
const FILE_ADD_FILE: u32 = 0x2;
const FILE_TRAVERSE: u32 = 0x20;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const DELETE: u32 = 0x1_0000;
const SYNCHRONIZE: u32 = 0x10_0000;
const GENERIC_READ: u32 = 0x8000_0000;
const FILE_GENERIC_READ: u32 = 0x12_0089;
const FILE_GENERIC_WRITE: u32 = 0x12_0116;

const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
const SHARE_READ_DELETE: u32 = 0x1 | 0x4;

const ATTR_READONLY: u32 = 0x1;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_NORMAL: u32 = 0x80;
const ATTR_SPARSE: u32 = 0x200;
const ATTR_REPARSE_POINT: u32 = 0x400;
/// The attributes `FileBasicInfo` may set.
const ATTR_SETTABLE: u32 = 0x31A7;

const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x20_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x200_0000;

const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const OPT_DIRECTORY: u32 = 0x1;
const OPT_SEQUENTIAL_ONLY: u32 = 0x4;
const OPT_SYNCHRONOUS: u32 = 0x20;
const OPT_NON_DIRECTORY: u32 = 0x40;
const OPT_OPEN_REPARSE_POINT: u32 = 0x20_0000;

// Information classes.
const FILE_RENAME_INFORMATION_CLASS: i32 = 10;
const FILE_LINK_INFORMATION_CLASS: i32 = 11;
const FILE_DISPOSITION_INFORMATION_CLASS: i32 = 13;
const FILE_DISPOSITION_INFORMATION_EX_CLASS: i32 = 64;
const FILE_RENAME_INFORMATION_EX_CLASS: i32 = 65;
const FILE_STAT_INFORMATION_CLASS: i32 = 68;
const FILE_BASIC_INFO_CLASS: i32 = 0;
const FILE_END_OF_FILE_INFO_CLASS: i32 = 6;
const FILE_ATTRIBUTE_TAG_INFO_CLASS: i32 = 9;
const FILE_FULL_DIRECTORY_INFO_CLASS: i32 = 14;
const FILE_FULL_DIRECTORY_RESTART_INFO_CLASS: i32 = 15;

const RENAME_REPLACE_IF_EXISTS: u32 = 0x1;
const RENAME_POSIX_SEMANTICS: u32 = 0x2;
const RENAME_IGNORE_READONLY: u32 = 0x40;
const DISPOSITION_DELETE: u32 = 0x1;
const DISPOSITION_POSIX_SEMANTICS: u32 = 0x2;
const DISPOSITION_IGNORE_READONLY: u32 = 0x10;

const FSCTL_SET_SPARSE: u32 = 0x9_00C4;
const FSCTL_QUERY_ALLOCATED_RANGES: u32 = 0x9_40CF;

const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x1;
const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x2;

// Reparse tags. A name surrogate is a link to another name: a symlink, junction or mount point.
const TAG_NAME_SURROGATE: u32 = 0x2000_0000;
const TAG_AF_UNIX: u32 = 0x8000_0023;
const TAG_LX_FIFO: u32 = 0x8000_0024;
const TAG_LX_CHR: u32 = 0x8000_0025;
const TAG_LX_BLK: u32 = 0x8000_0026;

// NTSTATUS values handled here rather than translated.
const STATUS_INVALID_INFO_CLASS: u32 = 0xC000_0003;
const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;
const STATUS_DELETE_PENDING: u32 = 0xC000_0056;
const STATUS_FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
const STATUS_NOT_SUPPORTED: u32 = 0xC000_00BB;
const STATUS_NOT_IMPLEMENTED: u32 = 0xC000_0002;

// Win32 errors.
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;
const ERROR_HANDLE_EOF: i32 = 38;
const ERROR_HANDLE_DISK_FULL: i32 = 39;
const ERROR_NOT_SUPPORTED: i32 = 50;
const ERROR_FILE_EXISTS: i32 = 80;
const ERROR_INVALID_PARAMETER: i32 = 87;
const ERROR_DISK_FULL: i32 = 112;
const ERROR_INVALID_NAME: i32 = 123;
const ERROR_DIR_NOT_EMPTY: i32 = 145;
const ERROR_BAD_PATHNAME: i32 = 161;
const ERROR_ALREADY_EXISTS: i32 = 183;
const ERROR_FILENAME_EXCED_RANGE: i32 = 206;
const ERROR_MORE_DATA: i32 = 234;
const ERROR_DIRECTORY: i32 = 267;
const ERROR_DIRECTORY_NOT_SUPPORTED: i32 = 336;
const ERROR_STOPPED_ON_SYMLINK: i32 = 681;
const ERROR_NOT_FOUND: i32 = 1168;
const ERROR_DISK_QUOTA_EXCEEDED: i32 = 1295;
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
const ERROR_NOT_A_REPARSE_POINT: i32 = 4390;
const ERROR_NO_MORE_FILES: i32 = 18;

/// 100 ns intervals between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF: i64 = 116_444_736_000_000_000;
/// NTFS keeps times in 100 ns intervals.
const NTFS_PRECISION_NS: u32 = 100;

fn error(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn nt_error(status: i32) -> io::Error {
    let code = match status as u32 {
        // Being deleted: gone, as far as anything here is concerned.
        STATUS_DELETE_PENDING => ERROR_FILE_NOT_FOUND,
        // Not "access denied", which is what the generic translation makes of it.
        STATUS_FILE_IS_A_DIRECTORY => ERROR_DIRECTORY_NOT_SUPPORTED,
        // SAFETY: a pure translation of a status code.
        _ => (unsafe { RtlNtStatusToDosError(status) }) as i32,
    };
    error(code)
}

fn bool_ok(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(last_error())
    } else {
        Ok(())
    }
}

fn raw(h: BorrowedFd<'_>) -> HANDLE {
    h.as_raw_handle() as HANDLE
}

/// Names as bytes: UTF-8, with WTF-8's encoding of a lone surrogate for the names Windows allows that
/// are not Unicode, so every name here has bytes and the bytes give the name back.
fn to_wide(name: &[u8]) -> Option<Vec<u16>> {
    let mut out = Vec::with_capacity(name.len());
    let mut i = 0;
    let mut after_lone_high = false;
    while i < name.len() {
        let b0 = name[i];
        let (len, init) = match b0 {
            0x00..=0x7F => (1, u32::from(b0)),
            0xC2..=0xDF => (2, u32::from(b0 & 0x1F)),
            0xE0..=0xEF => (3, u32::from(b0 & 0x0F)),
            0xF0..=0xF4 => (4, u32::from(b0 & 0x07)),
            _ => return None,
        };
        if i + len > name.len() {
            return None;
        }
        let mut cp = init;
        for &b in &name[i + 1..i + len] {
            if b & 0xC0 != 0x80 {
                return None;
            }
            cp = (cp << 6) | u32::from(b & 0x3F);
        }
        let shortest = match len {
            1 => true,
            2 => cp >= 0x80,
            3 => cp >= 0x800,
            _ => (0x1_0000..=0x10_FFFF).contains(&cp),
        };
        if !shortest {
            return None;
        }
        i += len;
        match cp {
            0xD800..=0xDBFF => {
                out.push(cp as u16);
                after_lone_high = true;
                continue;
            }
            // A pair written as two lone halves would be a second spelling of one character.
            0xDC00..=0xDFFF if after_lone_high => return None,
            0xDC00..=0xDFFF => out.push(cp as u16),
            0x1_0000.. => {
                let c = cp - 0x1_0000;
                out.push(0xD800 | (c >> 10) as u16);
                out.push(0xDC00 | (c & 0x3FF) as u16);
            }
            _ => out.push(cp as u16),
        }
        after_lone_high = false;
    }
    Some(out)
}

fn from_wide(wide: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(wide.len());
    for c in char::decode_utf16(wide.iter().copied()) {
        match c {
            Ok(c) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Err(e) => {
                let u = e.unpaired_surrogate();
                out.extend_from_slice(&[
                    0xE0 | (u >> 12) as u8,
                    0x80 | ((u >> 6) & 0x3F) as u8,
                    0x80 | (u & 0x3F) as u8,
                ]);
            }
        }
    }
    out
}

/// A name to look up in a directory. `.` is the directory itself. Anything that would make the lookup
/// mean more than one entry of this directory is refused: a separator, or the `:` that names a stream.
fn lookup_name(name: &[u8]) -> io::Result<Vec<u16>> {
    if name == b"." {
        return Ok(Vec::new());
    }
    if name.is_empty() || name == b".." {
        return Err(error(ERROR_INVALID_NAME));
    }
    let wide = to_wide(name).ok_or_else(|| error(ERROR_INVALID_NAME))?;
    if wide.len() > 32_000 {
        return Err(error(ERROR_FILENAME_EXCED_RANGE));
    }
    if wide
        .iter()
        .any(|&c| c == 0 || c == u16::from(b'\\') || c == u16::from(b'/') || c == u16::from(b':'))
    {
        return Err(error(ERROR_INVALID_NAME));
    }
    Ok(wide)
}

/// Why Windows programs could not open this name, if they could not: what NTFS itself takes is more
/// than they can.
fn unopenable(wide: &[u16]) -> Option<&'static str> {
    if wide.is_empty() {
        return Some("an empty name");
    }
    if wide
        .iter()
        .any(|&c| c < 0x20 || b"<>:\"/\\|?*".iter().any(|&b| c == u16::from(b)))
    {
        return Some(
            "it has a character Windows refuses in a name: < > : \" / \\ | ? * or a control character",
        );
    }
    if matches!(wide.last(), Some(&c) if c == u16::from(b' ') || c == u16::from(b'.')) {
        return Some("it ends in a dot or a space, which Windows programs drop from a name");
    }
    // A device name, with or without an extension, and with spaces before the dot.
    let stem: Vec<u16> = wide
        .iter()
        .copied()
        .take_while(|&c| c != u16::from(b'.'))
        .collect();
    let stem = String::from_utf16_lossy(&stem);
    let stem = stem.trim_end_matches(' ').to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|n| {
            matches!(
                n,
                "1" | "2"
                    | "3"
                    | "4"
                    | "5"
                    | "6"
                    | "7"
                    | "8"
                    | "9"
                    | "\u{b9}"
                    | "\u{b2}"
                    | "\u{b3}"
            )
        })
    };
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT");
    device.then_some(
        "it is a device to Windows: CON, PRN, AUX, NUL, COM1 to COM9 or LPT1 to LPT9, with any extension",
    )
}

/// Why this name cannot be held here, beyond what NTFS says.
pub fn name_problem(name: &[u8]) -> Option<&'static str> {
    match to_wide(name) {
        None => Some("its bytes are not Unicode, which a Windows name has to be"),
        Some(wide) => unopenable(&wide),
    }
}

/// A name to create: one Windows programs can open, and no longer than NTFS allows.
fn new_name(name: &[u8]) -> io::Result<Vec<u16>> {
    let wide = lookup_name(name)?;
    if wide.len() > 255 {
        return Err(error(ERROR_FILENAME_EXCED_RANGE));
    }
    if unopenable(&wide).is_some() {
        return Err(error(ERROR_INVALID_NAME));
    }
    Ok(wide)
}

fn filetime(t: i64) -> Timestamp {
    let ns = i128::from(t - EPOCH_DIFF) * 100;
    Timestamp {
        sec: ns.div_euclid(1_000_000_000) as i64,
        nsec: ns.rem_euclid(1_000_000_000) as u32,
    }
}

/// A time as NTFS keeps it, rounded down to its 100 ns. Zero would mean "leave it", so none is that.
fn to_filetime(t: Timestamp) -> io::Result<i64> {
    let ticks = i128::from(t.sec) * 10_000_000 + i128::from(t.nsec / 100) + i128::from(EPOCH_DIFF);
    if ticks <= 0 || ticks > i128::from(i64::MAX) {
        return Err(error(ERROR_INVALID_PARAMETER));
    }
    Ok(ticks as i64)
}

fn kind_of(attributes: u32, tag: u32) -> FileType {
    if attributes & ATTR_REPARSE_POINT != 0 {
        match tag {
            TAG_AF_UNIX => return FileType::Socket,
            TAG_LX_FIFO => return FileType::Fifo,
            TAG_LX_CHR => return FileType::CharacterDevice,
            TAG_LX_BLK => return FileType::BlockDevice,
            t if t & TAG_NAME_SURROGATE != 0 => return FileType::Unknown,
            // Anything else is a file or directory that a filter keeps, such as deduplication.
            _ => {}
        }
    }
    if attributes & ATTR_DIRECTORY != 0 {
        FileType::Directory
    } else {
        FileType::RegularFile
    }
}

/// The mode a scan reports: fixed, so that what is stored stays, except that the read-only attribute
/// of a file is its write bits.
fn mode_of(kind: FileType, attributes: u32) -> u32 {
    match kind {
        FileType::Directory => 0o755,
        FileType::RegularFile if attributes & ATTR_READONLY != 0 => 0o444,
        FileType::RegularFile => 0o644,
        _ => 0o777,
    }
}

fn is_link(attributes: u32, tag: u32) -> bool {
    attributes & ATTR_REPARSE_POINT != 0 && tag & TAG_NAME_SURROGATE != 0
}

fn tag_info(h: BorrowedFd<'_>) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    // SAFETY: the buffer is the struct this class fills.
    unsafe {
        let mut info: FILE_ATTRIBUTE_TAG_INFO = zeroed();
        bool_ok(GetFileInformationByHandleEx(
            raw(h),
            FILE_ATTRIBUTE_TAG_INFO_CLASS,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        ))?;
        Ok(info)
    }
}

fn basic_info(h: BorrowedFd<'_>) -> io::Result<FILE_BASIC_INFO> {
    // SAFETY: the buffer is the struct this class fills.
    unsafe {
        let mut info: FILE_BASIC_INFO = zeroed();
        bool_ok(GetFileInformationByHandleEx(
            raw(h),
            FILE_BASIC_INFO_CLASS,
            (&mut info as *mut FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        ))?;
        Ok(info)
    }
}

fn set_basic_info(h: BorrowedFd<'_>, info: &FILE_BASIC_INFO) -> io::Result<()> {
    // SAFETY: the buffer is the struct this class reads.
    bool_ok(unsafe {
        SetFileInformationByHandle(
            raw(h),
            FILE_BASIC_INFO_CLASS,
            (info as *const FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })
}

fn by_handle(h: BorrowedFd<'_>) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: the buffer is the struct this call fills.
    unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = zeroed();
        bool_ok(GetFileInformationByHandle(raw(h), &mut info))?;
        Ok(info)
    }
}

fn stat_handle(h: BorrowedFd<'_>, volume: Option<u64>) -> io::Result<Stat> {
    // SAFETY: the buffer is the struct this class fills.
    let (status, info) = unsafe {
        let mut info: FILE_STAT_INFORMATION = zeroed();
        let mut iosb: IO_STATUS_BLOCK = zeroed();
        let status = NtQueryInformationFile(
            raw(h),
            &mut iosb,
            (&mut info as *mut FILE_STAT_INFORMATION).cast(),
            size_of::<FILE_STAT_INFORMATION>() as u32,
            FILE_STAT_INFORMATION_CLASS,
        );
        (status, info)
    };
    let (attributes, tag, ino, nlink, size, mtime, ctime, btime) = if status >= 0 {
        (
            info.FileAttributes,
            info.ReparseTag,
            info.FileId as u64,
            u64::from(info.NumberOfLinks),
            info.EndOfFile as u64,
            info.LastWriteTime,
            info.ChangeTime,
            info.CreationTime,
        )
    } else if matches!(
        status as u32,
        STATUS_INVALID_INFO_CLASS | STATUS_INVALID_PARAMETER | STATUS_NOT_IMPLEMENTED
    ) {
        // Before Windows 10 1709, or a filesystem that does not answer this class.
        let by = by_handle(h)?;
        let basic = basic_info(h)?;
        let tag = tag_info(h)?;
        (
            basic.FileAttributes,
            tag.ReparseTag,
            (u64::from(by.nFileIndexHigh) << 32) | u64::from(by.nFileIndexLow),
            u64::from(by.nNumberOfLinks),
            (u64::from(by.nFileSizeHigh) << 32) | u64::from(by.nFileSizeLow),
            basic.LastWriteTime,
            basic.ChangeTime,
            basic.CreationTime,
        )
    } else {
        return Err(nt_error(status));
    };
    let volume = match volume {
        Some(v) => v,
        None => u64::from(by_handle(h)?.dwVolumeSerialNumber),
    };
    let file_type = kind_of(attributes, tag);
    Ok(Stat {
        file_type,
        mode: mode_of(file_type, attributes),
        uid: 0,
        gid: 0,
        dev: volume,
        ino,
        nlink,
        size,
        mtime: filetime(mtime),
        ctime: filetime(ctime),
        btime: Some(filetime(btime)),
        mnt_id: volume,
    })
}

/// `NtCreateFile` of one name relative to an open directory, never following a link.
fn nt_open(
    at: BorrowedFd<'_>,
    name: &[u16],
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
    attributes: u32,
) -> io::Result<OwnedFd> {
    let bytes = name.len() * 2;
    if bytes > usize::from(u16::MAX) - 1 {
        return Err(error(ERROR_FILENAME_EXCED_RANGE));
    }
    // SAFETY: every pointer is to a live value of the type the call reads, for the call's length. The
    // handle it returns is new and owned by nothing else.
    unsafe {
        let name = UNICODE_STRING {
            Length: bytes as u16,
            MaximumLength: bytes as u16,
            Buffer: name.as_ptr() as *mut u16,
        };
        let attrs = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: raw(at),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE as _,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut iosb: IO_STATUS_BLOCK = zeroed();
        let mut handle: HANDLE = std::ptr::null_mut();
        let status = NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attrs,
            &mut iosb,
            std::ptr::null(),
            attributes,
            share,
            disposition,
            options | OPT_SYNCHRONOUS | OPT_OPEN_REPARSE_POINT,
            std::ptr::null(),
            0,
        );
        if status < 0 {
            return Err(nt_error(status));
        }
        Ok(OwnedFd::from_raw_handle(handle as _))
    }
}

fn set_information(h: BorrowedFd<'_>, buf: &[u8], class: i32) -> Result<(), i32> {
    // SAFETY: the buffer holds the struct this class reads, for its length.
    let status = unsafe {
        let mut iosb: IO_STATUS_BLOCK = zeroed();
        NtSetInformationFile(
            raw(h),
            &mut iosb,
            buf.as_ptr().cast(),
            buf.len() as u32,
            class,
        )
    };
    if status < 0 {
        Err(status)
    } else {
        Ok(())
    }
}

/// Whether a refused information class means "not here" rather than a real failure.
fn unsupported_class(status: i32) -> bool {
    matches!(
        status as u32,
        STATUS_INVALID_INFO_CLASS | STATUS_INVALID_PARAMETER | STATUS_NOT_SUPPORTED
    )
}

/// The buffer of a rename or link: flags, the new directory and the new name.
fn name_info(flags: u32, to_dir: BorrowedFd<'_>, to: &[u16]) -> Vec<u8> {
    // The two structs share this layout.
    const _: () = assert!(
        offset_of!(FILE_RENAME_INFORMATION, FileName)
            == offset_of!(FILE_LINK_INFORMATION, FileName)
    );
    let name_at = offset_of!(FILE_RENAME_INFORMATION, FileName);
    let len = (name_at + to.len() * 2).max(size_of::<FILE_RENAME_INFORMATION>());
    // Aligned for the handle in it.
    let mut words = vec![0u64; len.div_ceil(8)];
    // SAFETY: the buffer is big enough for the header and the name, and aligned for it.
    unsafe {
        let info = words.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        (*info).Anonymous.Flags = flags;
        (*info).RootDirectory = raw(to_dir);
        (*info).FileNameLength = (to.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            to.as_ptr(),
            words.as_mut_ptr().cast::<u8>().add(name_at).cast::<u16>(),
            to.len(),
        );
        std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), len).to_vec()
    }
}

fn rename_handle(h: BorrowedFd<'_>, to_dir: &Dir, to: &[u16], replace: bool) -> io::Result<()> {
    let flags = if replace {
        RENAME_REPLACE_IF_EXISTS | RENAME_POSIX_SEMANTICS | RENAME_IGNORE_READONLY
    } else {
        0
    };
    let buf = name_info(flags, to_dir.fd(), to);
    match set_information(h, &buf, FILE_RENAME_INFORMATION_EX_CLASS) {
        Ok(()) => Ok(()),
        Err(s) if unsupported_class(s) => {
            // Before Windows 10 1709, or not NTFS: the first field is only "replace if exists".
            let buf = name_info(u32::from(replace), to_dir.fd(), to);
            set_information(h, &buf, FILE_RENAME_INFORMATION_CLASS).map_err(nt_error)
        }
        Err(s) => Err(nt_error(s)),
    }
}

/// Delete what a handle is open on. POSIX semantics: the name goes at once even if another program
/// still has the file open, rather than lingering until it closes and refusing a new file there.
fn dispose(h: BorrowedFd<'_>) -> io::Result<()> {
    let flags = DISPOSITION_DELETE | DISPOSITION_POSIX_SEMANTICS | DISPOSITION_IGNORE_READONLY;
    match set_information(
        h,
        &flags.to_le_bytes(),
        FILE_DISPOSITION_INFORMATION_EX_CLASS,
    ) {
        Ok(()) => Ok(()),
        Err(s) if unsupported_class(s) => {
            set_information(h, &[1u8], FILE_DISPOSITION_INFORMATION_CLASS).map_err(nt_error)
        }
        Err(s) => Err(nt_error(s)),
    }
}

/// `true` when this error says an xattr is not there.
pub fn is_no_xattr(e: &io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_NOT_FOUND)
}

/// What survives a power cut: the file's data and its record.
pub fn sync_fd(fd: BorrowedFd<'_>) -> io::Result<()> {
    bool_ok(unsafe { FlushFileBuffers(raw(fd)) })
}

fn has_code(e: &io::Error, codes: &[i32]) -> bool {
    e.raw_os_error().is_some_and(|c| codes.contains(&c))
}

pub fn is_not_found(e: &io::Error) -> bool {
    has_code(e, &[ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND])
}

pub fn is_permission(e: &io::Error) -> bool {
    has_code(e, &[ERROR_ACCESS_DENIED, ERROR_PRIVILEGE_NOT_HELD])
}

pub fn is_exists(e: &io::Error) -> bool {
    has_code(e, &[ERROR_FILE_EXISTS, ERROR_ALREADY_EXISTS])
}

pub fn is_not_empty(e: &io::Error) -> bool {
    has_code(e, &[ERROR_DIR_NOT_EMPTY])
}

/// A full disk or quota met by an entry in `dir`. Windows has no limit on a field that it reports so.
pub fn is_out_of_room(_dir: &Dir, e: &io::Error) -> bool {
    is_disk_full(e)
}

pub fn is_disk_full(e: &io::Error) -> bool {
    has_code(
        e,
        &[
            ERROR_DISK_FULL,
            ERROR_HANDLE_DISK_FULL,
            ERROR_DISK_QUOTA_EXCEEDED,
        ],
    )
}

pub fn is_bad_name(e: &io::Error) -> bool {
    has_code(e, &[ERROR_INVALID_NAME, ERROR_BAD_PATHNAME])
}

pub fn is_name_too_long(e: &io::Error) -> bool {
    has_code(e, &[ERROR_FILENAME_EXCED_RANGE])
}

pub fn is_invalid(e: &io::Error) -> bool {
    has_code(e, &[ERROR_INVALID_PARAMETER, ERROR_NOT_A_REPARSE_POINT])
}

/// Another program has the file open and shares it with no one, or has locked it.
pub fn is_busy(e: &io::Error) -> bool {
    has_code(e, &[ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION])
}

pub fn bad_name_error() -> io::Error {
    error(ERROR_INVALID_NAME)
}

pub fn exists_error() -> io::Error {
    error(ERROR_ALREADY_EXISTS)
}

/// A birth time, where it tells a reused inode number from the file that had it: never on NTFS. A file
/// ID carries a sequence number, so it is never reused, and a creation time moves between files: one
/// renamed or made where another has just gone takes that one's (tunneling), as a swap of two names does.
pub fn birth_mark(_: Option<Timestamp>) -> Option<Timestamp> {
    None
}

/// No Windows account passes every check the way root does.
pub fn is_superuser() -> bool {
    false
}

pub fn hostname() -> String {
    match std::env::var("COMPUTERNAME") {
        Ok(name) if !name.is_empty() => name,
        _ => "unknown".into(),
    }
}

pub fn try_lock(file: &std::fs::File) -> io::Result<bool> {
    // SAFETY: the handle is open for the call, and the OVERLAPPED gives the offset.
    let ok = unsafe {
        let mut at: OVERLAPPED = zeroed();
        LockFileEx(
            raw(file.as_handle()),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut at,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let e = last_error();
    if has_code(&e, &[ERROR_LOCK_VIOLATION]) {
        Ok(false)
    } else {
        Err(e)
    }
}

/// The files that hold the token live where the user put the root. Making them private to the user
/// is the folder's ACL's work on Windows, not a mode's.
pub fn private_file(opts: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    opts
}

pub fn make_private_dir(_: &Path) -> io::Result<()> {
    Ok(())
}

pub fn sync_dir_path(path: &Path) -> io::Result<()> {
    Dir::open_root(path)?.sync()
}

pub fn push_name(path: &mut PathBuf, name: &[u8]) {
    path.push(String::from_utf8_lossy(name).as_ref());
}

pub fn name_path(name: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(name).into_owned())
}

pub fn os_name_bytes(name: &OsStr) -> Option<Vec<u8>> {
    Some(from_wide(&name.encode_wide().collect::<Vec<u16>>()))
}

/// `stat` of an open handle.
pub fn fstat(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    stat_handle(fd, None)
}

/// Set a file's size. A file that is to have holes is made sparse first, or growing it would allocate
/// all of it, and writing near its end would fill everything before with zeros.
pub fn set_len(fd: BorrowedFd<'_>, size: u64, sparse: bool) -> io::Result<()> {
    if sparse {
        let mut out = 0u32;
        // SAFETY: no buffers: this control code with none sets the flag. Refused, the file is whole.
        unsafe {
            DeviceIoControl(
                raw(fd),
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut out,
                std::ptr::null_mut(),
            );
        }
    }
    let info = FILE_END_OF_FILE_INFO {
        EndOfFile: size as i64,
    };
    // SAFETY: the buffer is the struct this class reads.
    bool_ok(unsafe {
        SetFileInformationByHandle(
            raw(fd),
            FILE_END_OF_FILE_INFO_CLASS,
            (&info as *const FILE_END_OF_FILE_INFO).cast(),
            size_of::<FILE_END_OF_FILE_INFO>() as u32,
        )
    })
}

fn at_offset(offset: u64) -> OVERLAPPED {
    // SAFETY: all zeros is a valid OVERLAPPED; the offset is then set.
    let mut o: OVERLAPPED = unsafe { zeroed() };
    o.Anonymous.Anonymous.Offset = offset as u32;
    o.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    o
}

/// One positional read. The handle is synchronous, so this waits for it.
pub fn read_at(fd: BorrowedFd<'_>, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let want = buf.len().min(u32::MAX as usize) as u32;
    let mut got = 0u32;
    let mut at = at_offset(offset);
    // SAFETY: the buffer holds `want` bytes, and the OVERLAPPED lives for the call.
    let ok = unsafe { ReadFile(raw(fd), buf.as_mut_ptr(), want, &mut got, &mut at) };
    if ok == 0 {
        let e = last_error();
        if has_code(&e, &[ERROR_HANDLE_EOF]) {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(got as usize)
}

fn write_at(fd: BorrowedFd<'_>, buf: &[u8], offset: u64) -> io::Result<usize> {
    let want = buf.len().min(u32::MAX as usize) as u32;
    let mut put = 0u32;
    let mut at = at_offset(offset);
    // SAFETY: the buffer holds `want` bytes, and the OVERLAPPED lives for the call.
    bool_ok(unsafe { WriteFile(raw(fd), buf.as_ptr(), want, &mut put, &mut at) })?;
    Ok(put as usize)
}

/// The data runs of a file, as (offset, length): all of it unless it is sparse, and then what NTFS has
/// allocated, in its own units of 64 KB or so, which is no change while the bytes are the same.
pub fn data_runs(fd: BorrowedFd<'_>, size: u64) -> io::Result<Vec<(u64, u64)>> {
    if size == 0 {
        return Ok(Vec::new());
    }
    if basic_info(fd)?.FileAttributes & ATTR_SPARSE == 0 {
        return Ok(vec![(0, size)]);
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Range {
        offset: i64,
        length: i64,
    }
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let mut from = 0u64;
    loop {
        let ask = Range {
            offset: from as i64,
            length: (size - from) as i64,
        };
        let mut out = vec![Range::default(); 512];
        let mut got = 0u32;
        // SAFETY: the buffers are the structs this control code reads and fills, for their lengths.
        let ok = unsafe {
            DeviceIoControl(
                raw(fd),
                FSCTL_QUERY_ALLOCATED_RANGES,
                (&ask as *const Range).cast(),
                size_of::<Range>() as u32,
                out.as_mut_ptr().cast(),
                (out.len() * size_of::<Range>()) as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        let more = if ok == 0 {
            let e = last_error();
            if !has_code(&e, &[ERROR_MORE_DATA]) {
                return Err(e);
            }
            true
        } else {
            false
        };
        let n = got as usize / size_of::<Range>();
        for r in &out[..n] {
            let start = (r.offset as u64).min(size);
            let end = (r.offset as u64).saturating_add(r.length as u64).min(size);
            if start >= end {
                continue;
            }
            match runs.last_mut() {
                Some(last) if last.0 + last.1 == start => last.1 += end - start,
                _ => runs.push((start, end - start)),
            }
            from = end;
        }
        if !more || n == 0 || from >= size {
            break;
        }
    }
    Ok(runs)
}

/// No watch on what is done to a file is had here yet: every placed file is read again by the next scan.
pub struct WriteWatch;

impl WriteWatch {
    pub fn new() -> Option<Self> {
        None
    }

    pub fn watch(&mut self, _: BorrowedFd<'_>) -> Option<i32> {
        None
    }

    pub fn watch_writes(&mut self, _: BorrowedFd<'_>) -> Option<i32> {
        None
    }

    pub fn and_metadata(&mut self, _: BorrowedFd<'_>, _: i32) -> bool {
        false
    }

    pub fn untouched(&mut self, _: i32) -> bool {
        false
    }

    pub fn forget(&mut self, _: i32) {}
}

/// Not asked here yet: what comes down is still brought in slices, so the cache stays small.
pub fn free_space(_: &Dir) -> io::Result<Option<u64>> {
    Ok(None)
}

/// Whether another program has this file open for writing: a second open that shares reading but not
/// writing is refused while any handle can write. `None` where that cannot be asked.
pub fn open_for_writing(fd: BorrowedFd<'_>) -> Option<bool> {
    // SAFETY: the handle is open for the call, and a handle it returns is closed here.
    unsafe {
        let probe = ReOpenFile(raw(fd), GENERIC_READ, SHARE_READ_DELETE, 0);
        if probe == INVALID_HANDLE_VALUE || probe.is_null() {
            let e = last_error();
            return has_code(&e, &[ERROR_SHARING_VIOLATION]).then_some(true);
        }
        CloseHandle(probe);
    }
    Some(false)
}

impl Dir {
    /// Open a root by its path. The last component must be a real directory; links above it are the
    /// user's own choice of where the root lives.
    pub fn open_root(path: &Path) -> io::Result<Dir> {
        use std::os::windows::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            .share_mode(SHARE_ALL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let handle = OwnedFd::from(file);
        let tag = tag_info(handle.as_handle())?;
        if is_link(tag.FileAttributes, tag.ReparseTag) {
            return Err(error(ERROR_STOPPED_ON_SYMLINK));
        }
        if tag.FileAttributes & ATTR_DIRECTORY == 0 {
            return Err(error(ERROR_DIRECTORY));
        }
        let volume = u64::from(by_handle(handle.as_handle())?.dwVolumeSerialNumber);
        Ok(Dir {
            handle,
            volume,
            precision: OnceLock::new(),
        })
    }

    pub fn dup(&self) -> io::Result<Dir> {
        Ok(Dir {
            handle: self.handle.try_clone()?,
            volume: self.volume,
            precision: self.precision.clone(),
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.handle.as_handle()
    }

    fn open(&self, name: &[u16], access: u32, options: u32) -> io::Result<OwnedFd> {
        nt_open(self.fd(), name, access, SHARE_ALL, FILE_OPEN, options, 0)
    }

    /// A child directory. Fails on a link, whatever it points at.
    pub fn open_dir(&self, name: &[u8]) -> io::Result<Dir> {
        let handle = self.open(
            &lookup_name(name)?,
            FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES,
            OPT_DIRECTORY,
        )?;
        let tag = tag_info(handle.as_handle())?;
        if is_link(tag.FileAttributes, tag.ReparseTag) {
            return Err(error(ERROR_STOPPED_ON_SYMLINK));
        }
        Ok(Dir {
            handle,
            volume: self.volume,
            precision: self.precision.clone(),
        })
    }

    /// A chain of child directories.
    pub fn open_path<'a>(&self, path: impl IntoIterator<Item = &'a Name>) -> io::Result<Dir> {
        let mut at = self.dup()?;
        for name in path {
            at = at.open_dir(name.as_bytes())?;
        }
        Ok(at)
    }

    /// The directory at this path below, as `open_path(path)?.stat_self()` finds it.
    pub fn stat_dir_at(&self, path: &[Name]) -> io::Result<Stat> {
        if path.is_empty() {
            return self.stat_self();
        }
        self.open_path(path.iter())?.stat_self()
    }

    /// Create a directory if it is not there, without following links. For state directories.
    pub fn ensure_dir(&self, name: &[u8], mode: u32) -> io::Result<Dir> {
        match self.mkdir(name, mode) {
            Ok(()) => {}
            Err(e) if is_exists(&e) => {}
            Err(e) => return Err(e),
        }
        self.open_dir(name)
    }

    pub fn stat_self(&self) -> io::Result<Stat> {
        stat_handle(self.fd(), Some(self.volume))
    }

    pub fn stat(&self, name: &[u8]) -> io::Result<Stat> {
        let handle = self.open(&lookup_name(name)?, FILE_READ_ATTRIBUTES, 0)?;
        stat_handle(handle.as_handle(), Some(self.volume))
    }

    pub fn try_stat(&self, name: &[u8]) -> io::Result<Option<Stat>> {
        match self.stat(name) {
            Ok(s) => Ok(Some(s)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Each entry's name and attributes, with its reparse tag where it has one.
    fn entries(&self) -> io::Result<Vec<(Vec<u8>, u32, u32)>> {
        let mut out = Vec::new();
        let mut buf = vec![0u64; 8192];
        let mut class = FILE_FULL_DIRECTORY_RESTART_INFO_CLASS;
        loop {
            // SAFETY: the buffer is aligned and as long as said; the call fills it with entries.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    raw(self.fd()),
                    class,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                )
            };
            if ok == 0 {
                let e = last_error();
                if has_code(&e, &[ERROR_NO_MORE_FILES]) {
                    break;
                }
                return Err(e);
            }
            class = FILE_FULL_DIRECTORY_INFO_CLASS;
            let base = buf.as_ptr().cast::<u8>();
            let mut at = 0usize;
            loop {
                // SAFETY: each entry the call wrote starts at an offset it gave, within the buffer.
                let (next, attributes, ea, name) = unsafe {
                    let entry = base.add(at).cast::<FILE_FULL_DIR_INFO>();
                    let len = (*entry).FileNameLength as usize / 2;
                    let name_at = base.add(at + offset_of!(FILE_FULL_DIR_INFO, FileName));
                    let name = std::slice::from_raw_parts(name_at.cast::<u16>(), len).to_vec();
                    (
                        (*entry).NextEntryOffset as usize,
                        (*entry).FileAttributes,
                        (*entry).EaSize,
                        name,
                    )
                };
                if name != [u16::from(b'.')] && name != [u16::from(b'.'); 2] {
                    // For a reparse point the EA size is its tag.
                    let tag = if attributes & ATTR_REPARSE_POINT != 0 {
                        ea
                    } else {
                        0
                    };
                    out.push((from_wide(&name), attributes, tag));
                }
                if next == 0 {
                    break;
                }
                at += next;
            }
        }
        Ok(out)
    }

    /// Entry names, sorted so that a scan is deterministic.
    pub fn list(&self) -> io::Result<Vec<Vec<u8>>> {
        let mut out: Vec<Vec<u8>> = self.entries()?.into_iter().map(|e| e.0).collect();
        out.sort();
        Ok(out)
    }

    /// The names of entries that are directories, and not links to one.
    pub fn subdirs(&self) -> io::Result<Vec<Vec<u8>>> {
        Ok(self
            .entries()?
            .into_iter()
            .filter(|(_, a, t)| a & ATTR_DIRECTORY != 0 && !is_link(*a, *t))
            .map(|e| e.0)
            .collect())
    }

    /// Open a regular file for reading, sharing everything. A link is refused, not read.
    pub fn open_read(&self, name: &[u8]) -> io::Result<OwnedFd> {
        self.open_read_with(name, OPT_NON_DIRECTORY)
    }

    /// As `open_read`, for a file read through once, as a scan reads one to hash it or an upload one
    /// of its chunks: `FILE_SEQUENTIAL_ONLY`, `FILE_FLAG_SEQUENTIAL_SCAN` as `CreateFileW` has it, after
    /// which the cache manager gives its pages up first. A scan reading a tree bigger than memory would
    /// otherwise push out what is read again and again, the state above all.
    pub fn open_read_once(&self, name: &[u8]) -> io::Result<OwnedFd> {
        self.open_read_with(name, OPT_NON_DIRECTORY | OPT_SEQUENTIAL_ONLY)
    }

    fn open_read_with(&self, name: &[u8], options: u32) -> io::Result<OwnedFd> {
        let handle = self.open(&lookup_name(name)?, FILE_GENERIC_READ, options)?;
        let tag = tag_info(handle.as_handle())?;
        if is_link(tag.FileAttributes, tag.ReparseTag) {
            return Err(error(ERROR_STOPPED_ON_SYMLINK));
        }
        Ok(handle)
    }

    /// Create a new file, refusing anything already there. Its access is the folder's ACL's.
    pub fn create_new(&self, name: &[u8], _mode: u32) -> io::Result<OwnedFd> {
        nt_open(
            self.fd(),
            &new_name(name)?,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            SHARE_ALL,
            FILE_CREATE,
            OPT_NON_DIRECTORY,
            ATTR_NORMAL,
        )
    }

    pub fn read_link(&self, _name: &[u8]) -> io::Result<Vec<u8>> {
        Err(error(ERROR_NOT_SUPPORTED))
    }

    pub fn symlink(&self, _target: &[u8], _name: &[u8]) -> io::Result<()> {
        Err(error(ERROR_NOT_SUPPORTED))
    }

    pub fn mkdir(&self, name: &[u8], _mode: u32) -> io::Result<()> {
        nt_open(
            self.fd(),
            &new_name(name)?,
            FILE_LIST_DIRECTORY,
            SHARE_ALL,
            FILE_CREATE,
            OPT_DIRECTORY,
            ATTR_NORMAL,
        )
        .map(drop)
    }

    /// Open an entry to remove or move it: the link itself if it is one.
    fn open_to_change(&self, name: &[u8]) -> io::Result<(OwnedFd, FILE_ATTRIBUTE_TAG_INFO)> {
        let handle = self.open(&lookup_name(name)?, DELETE | FILE_READ_ATTRIBUTES, 0)?;
        let tag = tag_info(handle.as_handle())?;
        Ok((handle, tag))
    }

    /// Remove a file or a link, never a directory.
    pub fn unlink(&self, name: &[u8]) -> io::Result<()> {
        let (handle, tag) = self.open_to_change(name)?;
        if tag.FileAttributes & ATTR_DIRECTORY != 0 && !is_link(tag.FileAttributes, tag.ReparseTag)
        {
            return Err(error(ERROR_DIRECTORY_NOT_SUPPORTED));
        }
        dispose(handle.as_handle())
    }

    /// Remove an empty directory; a directory that gained an entry stays.
    pub fn rmdir(&self, name: &[u8]) -> io::Result<()> {
        let (handle, tag) = self.open_to_change(name)?;
        if tag.FileAttributes & ATTR_DIRECTORY == 0 || is_link(tag.FileAttributes, tag.ReparseTag) {
            return Err(error(ERROR_DIRECTORY));
        }
        dispose(handle.as_handle())
    }

    /// Atomic rename that fails rather than replace anything at the destination. NTFS takes a name
    /// that is another link of the same file as no obstacle and drops the source name, where Linux
    /// refuses: refused here too, except for a change of case alone, which is that same file's name.
    pub fn rename_new(&self, from: &[u8], to_dir: &Dir, to: &[u8]) -> io::Result<()> {
        let to_wide = new_name(to)?;
        let (handle, _) = self.open_to_change(from)?;
        if let Some(there) = to_dir.try_stat(to)? {
            let source = stat_handle(handle.as_handle(), Some(self.volume))?;
            if (there.dev, there.ino) == (source.dev, source.ino) && there.nlink > 1 {
                let same_dir = self.stat_self()?.ino == to_dir.stat_self()?.ino;
                if !(same_dir && from.eq_ignore_ascii_case(to)) {
                    return Err(exists_error());
                }
            }
        }
        rename_handle(handle.as_handle(), to_dir, &to_wide, false)
    }

    /// NTFS cannot swap two entries.
    pub fn exchange(&self, _a: &[u8], _b: &[u8]) -> io::Result<bool> {
        Ok(false)
    }

    /// Atomic rename over whatever file is at the destination, even one another program has open.
    pub fn rename_over(&self, from: &[u8], to_dir: &Dir, to: &[u8]) -> io::Result<()> {
        let to = new_name(to)?;
        let (handle, _) = self.open_to_change(from)?;
        rename_handle(handle.as_handle(), to_dir, &to, true)
    }

    pub fn hard_link(&self, from: &[u8], to_dir: &Dir, to: &[u8]) -> io::Result<()> {
        let to = new_name(to)?;
        let handle = self.open(
            &lookup_name(from)?,
            FILE_WRITE_ATTRIBUTES,
            OPT_NON_DIRECTORY,
        )?;
        let buf = name_info(0, to_dir.fd(), &to);
        set_information(handle.as_handle(), &buf, FILE_LINK_INFORMATION_CLASS).map_err(nt_error)
    }

    /// Make this directory's entries durable. A flush needs a handle that can write to it.
    pub fn sync(&self) -> io::Result<()> {
        let writable = self.open(&[], FILE_ADD_FILE, OPT_DIRECTORY)?;
        bool_ok(unsafe { FlushFileBuffers(raw(writable.as_handle())) })
    }

    pub fn list_xattrs(&self, _name: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        Ok(Vec::new())
    }

    pub fn get_xattr(&self, _name: &[u8], _key: &[u8]) -> io::Result<Vec<u8>> {
        Err(error(ERROR_NOT_FOUND))
    }

    pub fn set_xattr(&self, _name: &[u8], _key: &[u8], _value: &[u8]) -> io::Result<()> {
        Err(error(ERROR_NOT_SUPPORTED))
    }

    pub fn remove_xattr(&self, _name: &[u8], _key: &[u8]) -> io::Result<()> {
        Err(error(ERROR_NOT_FOUND))
    }

    pub fn chown(&self, _name: &[u8], _uid: u32, _gid: u32) -> io::Result<()> {
        Err(error(ERROR_NOT_SUPPORTED))
    }

    /// A file's write bits become its read-only attribute; a directory's mode is not held.
    pub fn chmod(&self, name: &[u8], mode: u32) -> io::Result<()> {
        let handle = self.open(
            &lookup_name(name)?,
            FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            0,
        )?;
        let tag = tag_info(handle.as_handle())?;
        if is_link(tag.FileAttributes, tag.ReparseTag) {
            return Err(error(ERROR_STOPPED_ON_SYMLINK));
        }
        if tag.FileAttributes & ATTR_DIRECTORY != 0 {
            return Ok(());
        }
        let read_only = mode & 0o222 == 0;
        if (tag.FileAttributes & ATTR_READONLY != 0) == read_only {
            return Ok(());
        }
        let mut attributes = (tag.FileAttributes ^ ATTR_READONLY) & ATTR_SETTABLE;
        if attributes == 0 {
            attributes = ATTR_NORMAL;
        }
        let info = FILE_BASIC_INFO {
            FileAttributes: attributes,
            ..Default::default()
        };
        set_basic_info(handle.as_handle(), &info)
    }

    pub fn set_mtime(&self, name: &[u8], mtime: Timestamp) -> io::Result<()> {
        let at = to_filetime(mtime)?;
        let handle = self.open(&lookup_name(name)?, FILE_WRITE_ATTRIBUTES, 0)?;
        let info = FILE_BASIC_INFO {
            LastWriteTime: at,
            ..Default::default()
        };
        set_basic_info(handle.as_handle(), &info)
    }

    /// How finely this volume keeps times: 100 ns on NTFS and ReFS, 2 s on FAT, 10 ms on exFAT.
    pub fn fstype_precision_ns(&self) -> u32 {
        *self.precision.get_or_init(|| {
            let mut name = [0u16; 64];
            // SAFETY: the buffer holds as many characters as said; the other outputs are not asked.
            let ok = unsafe {
                GetVolumeInformationByHandleW(
                    raw(self.fd()),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    name.as_mut_ptr(),
                    name.len() as u32,
                )
            };
            if ok == 0 {
                return NTFS_PRECISION_NS;
            }
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            match String::from_utf16_lossy(&name[..len]).as_str() {
                "FAT" | "FAT32" => 2_000_000_000,
                "exFAT" => 10_000_000,
                _ => NTFS_PRECISION_NS,
            }
        })
    }
}

/// Done with these bytes of a file opened with `open_read_once`: here its open said so already.
pub fn read_once_done(_: BorrowedFd<'_>, _: u64, _: u64) {}

/// Keep going until the buffer is full; a short positional read is otherwise a real end of data.
pub fn read_exact_at(fd: BorrowedFd<'_>, buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match read_at(fd, &mut buf[filled..], offset)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                filled += n;
                offset += n as u64;
            }
        }
    }
    Ok(())
}

pub fn write_all_at(fd: BorrowedFd<'_>, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match write_at(fd, buf, offset)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> (PathBuf, Dir) {
        let dir = std::env::temp_dir().join(format!("cairn-win-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        let d = Dir::open_root(&dir).unwrap();
        (dir, d)
    }

    #[test]
    fn names_round_trip_as_bytes_and_lone_surrogates_too() {
        for name in ["plain.txt", "Ünïcödé", "日本語", "emoji 😀", "a b"] {
            let w = to_wide(name.as_bytes()).unwrap();
            assert_eq!(w, name.encode_utf16().collect::<Vec<_>>());
            assert_eq!(from_wide(&w), name.as_bytes());
        }
        let lone = [u16::from(b'a'), 0xD800, u16::from(b'b')];
        let bytes = from_wide(&lone);
        assert_eq!(to_wide(&bytes).unwrap(), lone);
        // Not UTF-8 at all, an overlong spelling, and a pair spelled as two halves.
        for bad in [&b"\xff"[..], b"\xc0\xaf", b"\xed\xa0\x80\xed\xb0\x80"] {
            assert!(to_wide(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn names_windows_programs_cannot_open_are_refused() {
        for bad in [
            "a:b",
            "a?b",
            "a*b",
            "a<b",
            "a|b",
            "a\"b",
            "trailing.",
            "trailing ",
            "CON",
            "con.txt",
            "NUL",
            "Com1",
            "lpt9.log",
            "AUX .txt",
            "tab\there",
        ] {
            assert!(new_name(bad.as_bytes()).is_err(), "{bad}");
        }
        for good in ["CONSOLE", "com10", "a.b.c", ".cairn", "..x", "x..y", "LPT"] {
            assert!(new_name(good.as_bytes()).is_ok(), "{good}");
        }
        // Lookups refuse only what would name more than one entry, or a stream.
        assert!(lookup_name(b"a:b").is_err());
        assert!(lookup_name(b"a\\b").is_err());
        assert!(lookup_name(b"trailing.").is_ok());
        assert!(is_bad_name(&new_name(b"a:b").unwrap_err()));
    }

    #[test]
    fn a_stream_name_never_reaches_ntfs() {
        let (path, d) = temp_root();
        drop(d.create_new(b"f", 0o644).unwrap());
        let e = d.create_new(b"f:hidden", 0o644).unwrap_err();
        assert!(is_bad_name(&e), "{e}");
        assert!(d.stat(b"f:hidden").is_err());
        assert_eq!(d.list().unwrap(), vec![b"f".to_vec()]);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn files_are_made_moved_linked_and_removed_through_handles() {
        let (path, d) = temp_root();
        let sub = d.ensure_dir(b"sub", 0o755).unwrap();
        let fd = sub.create_new(b"a", 0o644).unwrap();
        write_all_at(fd.as_fd(), b"hello", 0).unwrap();
        sync_fd(fd.as_fd()).unwrap();
        drop(fd);
        assert!(is_exists(&sub.create_new(b"a", 0o644).unwrap_err()));
        let st = sub.stat(b"a").unwrap();
        assert_eq!(st.file_type, FileType::RegularFile);
        assert_eq!((st.size, st.nlink, st.mode), (5, 1, 0o644));
        assert_eq!(d.stat(b"sub").unwrap().file_type, FileType::Directory);

        sub.hard_link(b"a", &d, b"b").unwrap();
        assert_eq!(sub.stat(b"a").unwrap().nlink, 2);
        assert_eq!(d.stat(b"b").unwrap().ino, st.ino);

        // No replace, then replace, across directories.
        assert!(is_exists(&d.rename_new(b"b", &sub, b"a").unwrap_err()));
        let fd = d.create_new(b"c", 0o644).unwrap();
        write_all_at(fd.as_fd(), b"new", 0).unwrap();
        drop(fd);
        d.rename_over(b"c", &sub, b"a").unwrap();
        let mut buf = [0u8; 3];
        read_exact_at(sub.open_read(b"a").unwrap().as_fd(), &mut buf, 0).unwrap();
        assert_eq!(&buf, b"new");
        assert!(is_not_found(&d.stat(b"c").unwrap_err()));

        assert!(is_not_empty(&d.rmdir(b"sub").unwrap_err()));
        assert!(d.unlink(b"sub").is_err());
        assert!(d.rmdir(b"b").is_err());
        sub.unlink(b"a").unwrap();
        d.unlink(b"b").unwrap();
        d.rmdir(b"sub").unwrap();
        assert!(d.list().unwrap().is_empty());
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn a_case_twin_is_the_same_entry_and_cannot_be_made() {
        let (path, d) = temp_root();
        drop(d.create_new(b"Foo.txt", 0o644).unwrap());
        assert!(is_exists(&d.create_new(b"foo.txt", 0o644).unwrap_err()));
        assert_eq!(
            d.stat(b"foo.txt").unwrap().ino,
            d.stat(b"Foo.txt").unwrap().ino
        );
        // A case-only rename is a rename.
        d.rename_new(b"Foo.txt", &d, b"foo.txt").unwrap();
        assert_eq!(d.list().unwrap(), vec![b"foo.txt".to_vec()]);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn the_mtime_set_is_the_mtime_kept_after_the_writer_closes() {
        let (path, d) = temp_root();
        let fd = d.create_new(b"f", 0o644).unwrap();
        write_all_at(fd.as_fd(), b"data", 0).unwrap();
        sync_fd(fd.as_fd()).unwrap();
        let want = Timestamp {
            sec: 1_600_000_000,
            nsec: 123_456_789,
        };
        d.set_mtime(b"f", want).unwrap();
        sync_fd(fd.as_fd()).unwrap();
        drop(fd);
        let got = d.stat(b"f").unwrap().mtime;
        assert_eq!(
            got,
            Timestamp {
                sec: 1_600_000_000,
                nsec: 123_456_700
            }
        );
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn read_only_is_the_write_bits_of_a_file_and_does_not_block_replacing_it() {
        let (path, d) = temp_root();
        drop(d.create_new(b"f", 0o644).unwrap());
        d.chmod(b"f", 0o444).unwrap();
        assert_eq!(d.stat(b"f").unwrap().mode, 0o444);
        drop(d.create_new(b"g", 0o644).unwrap());
        d.rename_over(b"g", &d, b"f").unwrap();
        assert_eq!(d.stat(b"f").unwrap().mode, 0o644);
        d.chmod(b"f", 0o555).unwrap();
        d.unlink(b"f").unwrap();
        d.mkdir(b"dir", 0o700).unwrap();
        d.chmod(b"dir", 0o500).unwrap();
        assert_eq!(d.stat(b"dir").unwrap().mode, 0o755);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn a_file_open_elsewhere_can_still_be_replaced_and_removed() {
        let (path, d) = temp_root();
        let fd = d.create_new(b"f", 0o644).unwrap();
        write_all_at(fd.as_fd(), b"old", 0).unwrap();
        drop(fd);
        // Another program reading it, sharing deletes as Rust's and most programs' opens do: the
        // name is replaced at once, and the reader still reads what it opened.
        let other = std::fs::File::open(path.join("f")).unwrap();
        let fd = d.create_new(b"g", 0o644).unwrap();
        write_all_at(fd.as_fd(), b"new", 0).unwrap();
        drop(fd);
        d.rename_over(b"g", &d, b"f").unwrap();
        let mut buf = [0u8; 3];
        read_exact_at(other.as_fd(), &mut buf, 0).unwrap();
        assert_eq!(&buf, b"old");
        let mut buf = [0u8; 3];
        read_exact_at(d.open_read(b"f").unwrap().as_fd(), &mut buf, 0).unwrap();
        assert_eq!(&buf, b"new");
        // And removed while it is still open.
        d.unlink(b"f").unwrap();
        assert!(d.try_stat(b"f").unwrap().is_none());
        drop(other);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn a_writer_is_seen_and_an_exclusive_one_is_busy() {
        use std::os::windows::fs::OpenOptionsExt;
        let (path, d) = temp_root();
        drop(d.create_new(b"f", 0o644).unwrap());
        let read = d.open_read(b"f").unwrap();
        assert_eq!(open_for_writing(read.as_fd()), Some(false));
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(path.join("f"))
            .unwrap();
        assert_eq!(open_for_writing(read.as_fd()), Some(true));
        drop(writer);
        assert_eq!(open_for_writing(read.as_fd()), Some(false));
        drop(read);
        let exclusive = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(path.join("f"))
            .unwrap();
        let e = d.open_read(b"f").unwrap_err();
        assert!(is_busy(&e), "{e}");
        // Its attributes can still be read.
        assert!(d.stat(b"f").is_ok());
        drop(exclusive);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn a_sparse_file_has_holes_and_says_where_its_data_is() {
        let (path, d) = temp_root();
        let fd = d.create_new(b"s", 0o644).unwrap();
        let size = 64 << 20;
        set_len(fd.as_fd(), size, true).unwrap();
        write_all_at(fd.as_fd(), &[7u8; 4096], 32 << 20).unwrap();
        sync_fd(fd.as_fd()).unwrap();
        let runs = data_runs(fd.as_fd(), size).unwrap();
        assert_eq!(runs.len(), 1, "{runs:?}");
        let (at, len) = runs[0];
        assert!(
            at <= 32 << 20 && at + len >= (32 << 20) + 4096 && len < 1 << 20,
            "{runs:?}"
        );
        drop(fd);
        let fd = d.create_new(b"dense", 0o644).unwrap();
        write_all_at(fd.as_fd(), b"abc", 0).unwrap();
        assert_eq!(data_runs(fd.as_fd(), 3).unwrap(), vec![(0, 3)]);
        drop(fd);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn a_junction_is_never_entered() {
        let (path, d) = temp_root();
        let outside = path.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"x").unwrap();
        let tree = path.join("tree");
        std::fs::create_dir(&tree).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(tree.join("j"))
            .arg(&outside)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let t = d.open_dir(b"tree").unwrap();
        assert_eq!(t.stat(b"j").unwrap().file_type, FileType::Unknown);
        assert!(t.open_dir(b"j").is_err());
        assert!(t.subdirs().unwrap().is_empty());
        // Removing it removes the junction, not what it points at.
        t.unlink(b"j").unwrap();
        assert!(outside.join("secret").exists());
        assert!(Dir::open_root(&path.join("tree")).is_ok());
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn a_directory_sync_and_the_lock_work() {
        let (path, d) = temp_root();
        d.sync().unwrap();
        sync_dir_path(&path).unwrap();
        let lock = path.join("lock");
        let a = std::fs::File::create(&lock).unwrap();
        assert!(try_lock(&a).unwrap());
        let b = std::fs::File::create(&lock).unwrap();
        assert!(!try_lock(&b).unwrap());
        drop(a);
        assert!(try_lock(&b).unwrap());
        drop(b);
        assert_eq!(d.fstype_precision_ns(), 100);
        std::fs::remove_dir_all(&path).unwrap();
    }
}
