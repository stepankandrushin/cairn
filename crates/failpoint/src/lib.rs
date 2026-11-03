//! Named points where the failure harness can kill, starve or pause a real process.
//!
//! Built without the `enabled` feature every function here is an empty inline, so release binaries
//! carry no trace of it. With the feature, the environment decides what a hit does:
//!
//! - `CAIRN_FP_LOG=<file>`: append every hit's name, to enumerate a run's steps.
//! - `CAIRN_FP_CRASH=<n>`: SIGKILL this process on the n-th hit (1-based), or `<name>` on that name.
//! - `CAIRN_FP_ENOSPC=<n>`: `check` returns ENOSPC from the n-th hit on, or `<name>` on that name.
//! - `CAIRN_FP_PAUSE=<name>:<dir>`: on the hit, create `<dir>/reached` and wait for `<dir>/go`.
//! - `CAIRN_FP_REFUSE_NAME=<name>`: `refuse_name` refuses that name as a filesystem refuses bytes it
//!   cannot hold (EILSEQ), as APFS does a name that is not UTF-8.

use std::io;

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
    fn sigkill_self() -> io::Result<()> {
        let pid = std::process::id().to_string();
        std::process::Command::new("kill")
            .args(["-9", &pid])
            .status()?;
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
                return Err(io::Error::from_raw_os_error(28));
            }
        }
        Ok(())
    }

    pub fn refuse_name(name: &[u8]) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        const EILSEQ: i32 = 92;
        #[cfg(not(target_os = "macos"))]
        const EILSEQ: i32 = 84;
        match std::env::var_os("CAIRN_FP_REFUSE_NAME") {
            Some(refused)
                if std::os::unix::ffi::OsStrExt::as_bytes(refused.as_os_str()) == name =>
            {
                Err(io::Error::from_raw_os_error(EILSEQ))
            }
            _ => Ok(()),
        }
    }
}
