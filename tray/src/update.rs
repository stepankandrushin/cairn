//! Updates as the tray shows them. The updater is the daemons' (update.rs in the engine): it puts a new
//! tray in place beside `cairn`, the bundle Cairn.app beside Cairn Sync.app on macOS. The tray reads
//! what the updater wrote, writes the policy a person chooses, asks a daemon to update now, and starts
//! itself again once its program has been replaced.

use cairn_protocol::update::{settings_path, state_path, Policy, Status};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Set in the environment of a tray started by one that is restarting, which may still hold the
/// tray's lock for a moment.
pub const RESTARTED: &str = "CAIRN_TRAY_RESTARTED";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    id: u64,
    len: u64,
    modified: u128,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    let nanos = |t: std::io::Result<SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos())
    };
    #[cfg(unix)]
    let id = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let id = nanos(m.created()) as u64;
    Some(Stamp {
        id,
        len: m.len(),
        modified: nanos(m.modified()),
    })
}

static START: OnceLock<Option<(PathBuf, Stamp)>> = OnceLock::new();

/// Where this program was started from and what was there: first thing at start, as on Linux a
/// replaced program's own path reads "(deleted)".
pub fn remember_start() {
    START.get_or_init(|| {
        let p = std::env::current_exe().ok()?;
        #[cfg(unix)]
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        let s = stamp(&p)?;
        Some((p, s))
    });
}

/// Another program is at this one's path: an update, or a person, put it there.
pub fn replaced() -> bool {
    match START.get() {
        Some(Some((path, then))) => stamp(path).is_some_and(|now| now != *then),
        _ => false,
    }
}

/// The exit status by which a tray under its LaunchAgent asks launchd to start it again: the agent
/// keeps it running after any exit but a successful one.
#[cfg(target_os = "macos")]
const RESTART: i32 = 75;

/// Start the program now at this one's path, with the same arguments, after which this one exits. On
/// Windows and macOS as a new process: macOS's menu bar knows a tray by the process it began as, and
/// gives a program exec'd in that process no icon for as long as it runs. On macOS under its
/// LaunchAgent by exiting with `RESTART`, for launchd to start it; on Linux in this process, by
/// exec. Returns only if it could not.
pub fn restart() -> String {
    let Some(Some((path, _))) = START.get() else {
        return "cannot tell where the tray is, to start it again".into();
    };
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        if std::env::var_os("XPC_SERVICE_NAME").is_some_and(|v| v == crate::actions::login::LABEL) {
            std::process::exit(RESTART);
        }
        // Its own process group, which a launchd job's end does not take with it.
        match std::process::Command::new(path)
            .args(&args)
            .env(RESTARTED, "1")
            .process_group(0)
            .spawn()
        {
            Ok(_) => std::process::exit(0),
            Err(e) => format!("cannot start {} again: {e}", path.display()),
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(path).args(&args).exec();
        format!("cannot start {} again: {err}", path.display())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        match std::process::Command::new(path)
            .args(&args)
            .env(RESTARTED, "1")
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
        {
            Ok(_) => std::process::exit(0),
            Err(e) => format!("cannot start {} again: {e}", path.display()),
        }
    }
}

/// Whether this program was put in place after its process began: an older tray exec'd it there, as
/// trays before 0.1.9 did on macOS when an update replaced them, which leaves it with no icon. An
/// exec is the only way a process runs a file newer than itself, as a file's change time cannot be
/// set back.
#[cfg(target_os = "macos")]
pub fn in_an_older_process() -> bool {
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;
    let Some(Some((path, _))) = START.get() else {
        return false;
    };
    let Ok(m) = std::fs::metadata(path) else {
        return false;
    };
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a proc_bsdinfo, and `size` its size.
    let n = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if n != size {
        return false;
    }
    let began = Duration::new(info.pbi_start_tvsec, info.pbi_start_tvusec as u32 * 1000);
    let put = Duration::new(
        m.ctime().max(0) as u64,
        m.ctime_nsec().clamp(0, 999_999_999) as u32,
    );
    put > began
}

/// What the updater last wrote: `None` if it never has.
pub fn status() -> Result<Option<Status>, String> {
    let p = state_path().ok_or("no directory for the updater's state")?;
    match std::fs::read(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", p.display())),
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("cannot read {}: {e}", p.display())),
    }
}

fn settings_file() -> Result<PathBuf, String> {
    settings_path().ok_or_else(|| "no directory for the update settings".into())
}

fn read_settings(p: &Path) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    match std::fs::read(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(format!("cannot read {}: {e}", p.display())),
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "cannot read {}: {e}; move it away to use the defaults",
                p.display()
            )
        }),
    }
}

/// The policy as set, `auto` when nothing is.
pub fn policy() -> Result<Policy, String> {
    policy_in(&settings_file()?)
}

fn policy_in(p: &Path) -> Result<Policy, String> {
    match read_settings(p)?.get("policy") {
        None => Ok(Policy::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("the update policy: {e}")),
    }
}

/// Writes the policy and keeps everything else in the file, a feed and keys `cairn` reads included.
/// A file that does not read is not written over.
pub fn set_policy(policy: Policy) -> Result<(), String> {
    set_policy_in(&settings_file()?, policy)
}

fn set_policy_in(p: &Path, policy: Policy) -> Result<(), String> {
    let mut s = read_settings(p)?;
    s.insert(
        "policy".into(),
        serde_json::to_value(policy).map_err(|e| e.to_string())?,
    );
    let dir = p.parent().expect("a file in a folder");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(&s).map_err(|e| e.to_string())?;
    crate::registry::write_new(p, &bytes).map_err(|e| format!("cannot write {}: {e}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_is_written_and_the_rest_of_the_settings_kept() {
        let dir = std::env::temp_dir().join(format!("cairn-tray-upd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("update-settings.json");
        assert_eq!(policy_in(&file).unwrap(), Policy::Auto);
        std::fs::write(
            &file,
            br#"{"policy":"auto","feed":"https://example.org/u.json","keys":["k"]}"#,
        )
        .unwrap();
        set_policy_in(&file, Policy::Notify).unwrap();
        assert_eq!(policy_in(&file).unwrap(), Policy::Notify);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(v["feed"], "https://example.org/u.json");
        assert_eq!(v["keys"][0], "k");
        std::fs::write(&file, b"{not json").unwrap();
        assert!(set_policy_in(&file, Policy::Off).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"{not json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
