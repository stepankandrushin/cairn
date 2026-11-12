//! The failure harness lives in `tests/`: it drives real binaries, built here with failpoints on.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Where the harness's `cairn` keeps its lists of roots, and on macOS and Windows its daemons' locks and
/// sockets: its own, never the user's. One per user, since /tmp is shared, and the first run as root,
/// as ci.sh's round trip under sudo is, would otherwise make it root's and refuse everyone else.
pub fn user_dir() -> PathBuf {
    #[cfg(unix)]
    let name = format!("cairn-h-{}", rustix::process::geteuid().as_raw());
    #[cfg(not(unix))]
    let name = String::from("cairn-h");
    std::env::temp_dir().join(name)
}

/// The harness's directory for the roots made in `folder`, a test's sandbox, within `user_dir()`. One
/// for the whole run was a list of every root the run had made, which each `cairn init` and starting
/// daemon read and wrote again whole, fsynced and renamed under its lock, so that the tests running at
/// once took turns at it and waited longer the further the run got. Its name is short, as a socket path
/// on macOS is 104 bytes at most.
pub fn user_dir_of(folder: &Path) -> PathBuf {
    // FNV-1a, which is the same for a path in every run and on every platform.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in folder.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    user_dir().join(format!("{:012x}", hash >> 16))
}

/// The root a run of `cairn` with these arguments is of, as the harness passes it: `--root <path>`
/// first, or the last argument of `init`.
pub fn root_in(args: &[OsString]) -> Option<PathBuf> {
    match args.get(1).and_then(|a| a.to_str()) {
        Some("--root") => args.get(2).map(PathBuf::from),
        Some("init") => args.last().map(PathBuf::from),
        _ => None,
    }
}
