//! The system refusing this process a file that is there, told apart from the file being gone. A path
//! refused is held as it was: never a delete, never a new revision, reported by name with the system's
//! own words, and tried again until it is let in. Matched by kind, never by which program refused: an
//! antivirus, another program holding the file, an access list or a failing disk all come to these.

use std::io;

/// What kind of refusal an error is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not allowed: an access list or mode, a policy, or a program that guards files saying no.
    Permission,
    /// Another program has it open and shares it with no one, or has locked it, for now.
    Busy,
    /// The device failed to read or write it.
    Io,
}

/// Said beside a refusal on Windows, where it is often another program's: no more than that, since
/// nothing tells which one, and naming none is the only way to be right about all of them.
pub const WINDOWS_HINT: &str = "it may be blocked by another program or security software";

/// Windows' codes, by the names Microsoft gives them (System Error Codes 0-499 and 1000-1299 and on).
mod win {
    pub const ERROR_ACCESS_DENIED: i32 = 5;
    pub const ERROR_WRITE_PROTECT: i32 = 19;
    pub const ERROR_CRC: i32 = 23;
    pub const ERROR_SECTOR_NOT_FOUND: i32 = 27;
    pub const ERROR_WRITE_FAULT: i32 = 29;
    pub const ERROR_READ_FAULT: i32 = 30;
    pub const ERROR_GEN_FAILURE: i32 = 31;
    pub const ERROR_SHARING_VIOLATION: i32 = 32;
    pub const ERROR_LOCK_VIOLATION: i32 = 33;
    pub const ERROR_NETWORK_ACCESS_DENIED: i32 = 65;
    /// "Operation did not complete successfully because the file contains a virus or potentially
    /// unwanted software."
    pub const ERROR_VIRUS_INFECTED: i32 = 225;
    /// The same, and the file has been removed: gone by the next look, which makes that a delete.
    pub const ERROR_VIRUS_DELETED: i32 = 226;
    pub const ERROR_DEVICE_HARDWARE_ERROR: i32 = 483;
    pub const ERROR_IO_DEVICE: i32 = 1117;
    pub const ERROR_USER_MAPPED_FILE: i32 = 1224;
    pub const ERROR_ACCESS_DISABLED_BY_POLICY: i32 = 1260;
    pub const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    pub const ERROR_FILE_CORRUPT: i32 = 1392;
    pub const ERROR_DISK_CORRUPT: i32 = 1393;
    pub const ERROR_CANT_ACCESS_FILE: i32 = 1920;
}

/// What a raw Windows error code is, as a refusal.
pub fn of_windows_code(code: i32) -> Option<Refusal> {
    use win::*;
    match code {
        ERROR_ACCESS_DENIED
        | ERROR_WRITE_PROTECT
        | ERROR_NETWORK_ACCESS_DENIED
        | ERROR_VIRUS_INFECTED
        | ERROR_VIRUS_DELETED
        | ERROR_ACCESS_DISABLED_BY_POLICY
        | ERROR_PRIVILEGE_NOT_HELD
        | ERROR_CANT_ACCESS_FILE => Some(Refusal::Permission),
        ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION | ERROR_USER_MAPPED_FILE => {
            Some(Refusal::Busy)
        }
        ERROR_CRC
        | ERROR_SECTOR_NOT_FOUND
        | ERROR_WRITE_FAULT
        | ERROR_READ_FAULT
        | ERROR_GEN_FAILURE
        | ERROR_DEVICE_HARDWARE_ERROR
        | ERROR_IO_DEVICE
        | ERROR_FILE_CORRUPT
        | ERROR_DISK_CORRUPT => Some(Refusal::Io),
        _ => None,
    }
}

/// What a Unix errno is, as a refusal. ETXTBSY is a program running from the file, which Linux lets
/// nobody write while it runs.
#[cfg(unix)]
pub fn of_errno(code: i32) -> Option<Refusal> {
    use rustix::io::Errno;
    let is = |e: Errno| code == e.raw_os_error();
    if is(Errno::ACCESS) || is(Errno::PERM) || is(Errno::ROFS) {
        Some(Refusal::Permission)
    } else if is(Errno::TXTBSY) {
        Some(Refusal::Busy)
    } else if is(Errno::IO) {
        Some(Refusal::Io)
    } else {
        None
    }
}

/// What this error is, as a refusal, on the system this runs on. A file not there is none: that is
/// the file gone, whatever made it go.
pub fn of(e: &io::Error) -> Option<Refusal> {
    let code = e.raw_os_error()?;
    #[cfg(windows)]
    return of_windows_code(code);
    #[cfg(unix)]
    return of_errno(code);
}

/// The system's own words for it, with the hint where it helps: only Windows has programs that
/// refuse files to others as a matter of course.
pub fn describe(e: &io::Error) -> String {
    match of(e) {
        Some(Refusal::Permission) if cfg!(windows) => format!("{e}; {WINDOWS_HINT}"),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_codes_map_by_kind() {
        // What an antivirus gives, among them Defender's for a file it blocks or has taken.
        for code in [5, 19, 65, 225, 226, 1260, 1314, 1920] {
            assert_eq!(of_windows_code(code), Some(Refusal::Permission), "{code}");
        }
        for code in [32, 33, 1224] {
            assert_eq!(of_windows_code(code), Some(Refusal::Busy), "{code}");
        }
        for code in [23, 27, 29, 30, 31, 483, 1117, 1392, 1393] {
            assert_eq!(of_windows_code(code), Some(Refusal::Io), "{code}");
        }
        // Not found, path not found, and a full disk are no refusal: the first two are the file
        // gone, the last is the whole sync's to stop for.
        for code in [2, 3, 112, 0] {
            assert_eq!(of_windows_code(code), None, "{code}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_errnos_map_by_kind() {
        use rustix::io::Errno;
        let kind = |e: Errno| of(&io::Error::from_raw_os_error(e.raw_os_error()));
        assert_eq!(kind(Errno::ACCESS), Some(Refusal::Permission));
        assert_eq!(kind(Errno::PERM), Some(Refusal::Permission));
        assert_eq!(kind(Errno::ROFS), Some(Refusal::Permission));
        assert_eq!(kind(Errno::TXTBSY), Some(Refusal::Busy));
        assert_eq!(kind(Errno::IO), Some(Refusal::Io));
        assert_eq!(kind(Errno::NOENT), None);
        assert_eq!(kind(Errno::NOSPC), None);
        // An error with no code at all is none either.
        assert_eq!(of(&io::Error::other("x")), None);
    }

    #[test]
    fn the_hint_names_no_program() {
        assert!(!WINDOWS_HINT.to_lowercase().contains("defender"));
        assert!(!WINDOWS_HINT.to_lowercase().contains("antivirus"));
    }

    #[cfg(windows)]
    #[test]
    fn a_refusal_on_windows_carries_the_hint_and_the_systems_words() {
        let e = io::Error::from_raw_os_error(225);
        let text = describe(&e);
        assert!(text.contains("os error 225"), "{text}");
        assert!(text.ends_with(WINDOWS_HINT), "{text}");
        assert_eq!(of(&e), Some(Refusal::Permission));
    }
}
