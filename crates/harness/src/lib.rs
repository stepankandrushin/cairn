//! The failure harness lives in `tests/`: it drives real binaries, built here with failpoints on.

/// Where the harness's `cairn` keeps its list of roots, and on macOS and Windows its daemons' locks and
/// sockets: its own, never the user's. One per user, since /tmp is shared, and the first run as root,
/// as ci.sh's round trip under sudo is, would otherwise make it root's and refuse everyone else.
pub fn user_dir() -> std::path::PathBuf {
    #[cfg(unix)]
    let name = format!("cairn-h-{}", rustix::process::geteuid().as_raw());
    #[cfg(not(unix))]
    let name = String::from("cairn-h");
    std::env::temp_dir().join(name)
}
