//! Named points where the failure harness can kill, starve or pause a real process.
//!
//! Built without the `enabled` feature every function here is an empty inline, so release binaries
//! carry no trace of it. With the feature, the environment decides what a hit does:
//!
//! - `CAIRN_FP_LOG=<file>`: append every hit's name, to enumerate a run's steps.
//! - `CAIRN_FP_CRASH=<n>`: SIGKILL this process on the n-th hit (1-based), or `<name>` on that name;
//!   on Windows `TerminateProcess`, exiting with `KILLED_EXIT_CODE`.
//! - `CAIRN_FP_ENOSPC=<n>`: `check` returns ENOSPC from the n-th hit on, or `<name>` on that name.
//! - `CAIRN_FP_PAUSE=<name>:<dir>`: on the hit, create `<dir>/reached` and wait for `<dir>/go`.
//! - `CAIRN_FP_REFUSE_NAME=<name>`: `refuse_name` refuses that name as a filesystem refuses bytes it
//!   cannot hold (EILSEQ), as APFS does a name that is not UTF-8.
//! - `CAIRN_FP_EPERM=<name>`: `deny` refuses that point with EPERM, as a filesystem refuses a change
//!   to a file this user does not own. Not a step: numbered points elsewhere stay where they were.
//! - `CAIRN_FP_UNSUPPORTED=<name>`: `unsupported` says yes at that point, as a filesystem says it
//!   cannot do what is asked, the way NTFS cannot swap two names. Not a step either.

use std::io;

/// What a process a failpoint killed exits with on Windows, where nothing tells a kill from an exit:
/// 128 + 9, as a Unix shell reports a SIGKILL.
pub const KILLED_EXIT_CODE: i32 = 137;

/// A step boundary at which the process may be killed or paused.
#[inline]
pub fn hit(name: &str) {
    #[cfg(feature = "enabled")]
    imp::hit(name);
    #[cfg(not(feature = "enabled"))]
    let _ = name;
}

/// As `hit`, for a step that performs I/O: may also report a full disk.
#[inline]
pub fn check(name: &str) -> io::Result<()> {
    #[cfg(feature = "enabled")]
    return imp::check(name);
    #[cfg(not(feature = "enabled"))]
    {
        let _ = name;
        Ok(())
    }
}

/// Before an entry is made under this name: may refuse it, as some filesystems refuse some names.
#[inline]
pub fn refuse_name(name: &[u8]) -> io::Result<()> {
    #[cfg(feature = "enabled")]
    return imp::refuse_name(name);
    #[cfg(not(feature = "enabled"))]
    {
        let _ = name;
        Ok(())
    }
}

/// Before a change to an entry's metadata: may refuse it, as the filesystem refuses one to a file this
/// user does not own.
#[inline]
pub fn deny(name: &str) -> io::Result<()> {
    #[cfg(feature = "enabled")]
    return imp::deny(name);
    #[cfg(not(feature = "enabled"))]
    {
        let _ = name;
        Ok(())
    }
}

/// Whether the filesystem is to be taken as unable to do what this point does, as one without
/// `RENAME_EXCHANGE` cannot swap two names.
#[inline]
pub fn unsupported(name: &str) -> bool {
    #[cfg(feature = "enabled")]
    return imp::unsupported(name);
    #[cfg(not(feature = "enabled"))]
    {
        let _ = name;
        false
    }
}

#[cfg(feature = "enabled")]
mod imp {
    use std::io::{self, Write};
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static HITS: AtomicU64 = AtomicU64::new(0);

    fn matches(spec: &str, name: &str, seq: u64) -> bool {
        match spec.parse::<u64>() {
            Ok(n) => seq == n,
            Err(_) => spec == name,
        }
    }

    fn step(name: &str) -> u64 {
        let seq = HITS.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(path) = std::env::var("CAIRN_FP_LOG") {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
            {
                let _ = writeln!(f, "{name}");
            }
        }
        if let Ok(spec) = std::env::var("CAIRN_FP_CRASH") {
            if matches(&spec, name, seq) {
                let _ = sigkill_self();
            }
        }
        if let Ok(spec) = std::env::var("CAIRN_FP_PAUSE") {
            if let Some((point, dir)) = spec.split_once(':') {
                if point == name {
                    let dir = Path::new(dir);
                    let _ = std::fs::write(dir.join("reached"), b"");
                    while !dir.join("go").exists() {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        }
        seq
    }

    /// SIGKILL through a child `kill`, so no destructor or buffered write runs.
    #[cfg(unix)]
    fn sigkill_self() -> io::Result<()> {
        let pid = std::process::id().to_string();
        std::process::Command::new("kill")
            .args(["-9", &pid])
            .status()?;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    /// `TerminateProcess` on itself, Windows's SIGKILL: no destructor or buffered write runs. Not an
    /// abort, which would hand the process to Windows Error Reporting first.
    #[cfg(windows)]
    fn sigkill_self() -> io::Result<()> {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn TerminateProcess(process: *mut std::ffi::c_void, code: u32) -> i32;
        }
        // SAFETY: the pseudo-handle of this process is always valid, and ending it is the point.
        unsafe { TerminateProcess(GetCurrentProcess(), super::KILLED_EXIT_CODE as u32) };
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    pub fn hit(name: &str) {
        step(name);
    }

    pub fn check(name: &str) -> io::Result<()> {
        let seq = step(name);
        if let Ok(spec) = std::env::var("CAIRN_FP_ENOSPC") {
            let full = match spec.parse::<u64>() {
                Ok(n) => seq >= n,
                Err(_) => spec == name,
            };
            if full {
                // ENOSPC, or Windows's ERROR_DISK_FULL.
                return Err(io::Error::from_raw_os_error(if cfg!(windows) {
                    112
                } else {
                    28
                }));
            }
        }
        Ok(())
    }

    pub fn unsupported(name: &str) -> bool {
        std::env::var("CAIRN_FP_UNSUPPORTED").is_ok_and(|point| point == name)
    }

    pub fn deny(name: &str) -> io::Result<()> {
        // ERROR_ACCESS_DENIED on Windows.
        const EPERM: i32 = if cfg!(windows) { 5 } else { 1 };
        match std::env::var("CAIRN_FP_EPERM") {
            Ok(point) if point == name => Err(io::Error::from_raw_os_error(EPERM)),
            _ => Ok(()),
        }
    }

    pub fn refuse_name(name: &[u8]) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        const EILSEQ: i32 = 92;
        /// ERROR_INVALID_NAME.
        #[cfg(windows)]
        const EILSEQ: i32 = 123;
        #[cfg(not(any(target_os = "macos", windows)))]
        const EILSEQ: i32 = 84;
        #[cfg(unix)]
        let refused = std::env::var_os("CAIRN_FP_REFUSE_NAME")
            .map(|r| std::os::unix::ffi::OsStrExt::as_bytes(r.as_os_str()).to_vec());
        #[cfg(windows)]
        let refused = std::env::var("CAIRN_FP_REFUSE_NAME")
            .ok()
            .map(String::into_bytes);
        match refused {
            Some(refused) if refused == name => Err(io::Error::from_raw_os_error(EILSEQ)),
            _ => Ok(()),
        }
    }
}
