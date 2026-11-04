//! Taking a root that has gone off this user's registry of roots, as `cairn forget` does
//! (crates/engine/src/registry.rs, `forget`): under the same `roots.lock`, by a new file renamed over the
//! old, and never a root whose folder still holds it. Written here rather than run through `cairn`,
//! which a tray started by launchd or at logon may not find on its PATH.

use cairn_protocol::local::{Registry, RootEntry};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

/// As the engine's: longer than any writer holds it, each for one read and one rename.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// The ID a folder says it holds, in its `.cairn/root-id`.
pub fn root_id_at(path: &Path) -> std::io::Result<String> {
    Ok(
        std::fs::read_to_string(path.join(".cairn").join("root-id"))?
            .trim()
            .to_string(),
    )
}

/// Takes `entry` off the registry in `file`, as it was when a person was shown it gone; what it did, in
/// a sentence for them.
pub fn forget(file: &Path, entry: &RootEntry) -> Result<String, String> {
    let shown = entry.path.display();
    // Asked before the lock, as the engine does: a folder on a mount that hangs must not hold up every
    // other writer.
    if root_id_at(&entry.path).is_ok_and(|id| id == entry.root_id) {
        return Err(format!(
            "{shown} holds this root again, so it stays on the list: nothing was taken off"
        ));
    }
    let dir = file
        .parent()
        .ok_or_else(|| format!("{} is in no folder", file.display()))?;
    let _lock = lock(dir)?;
    let bytes = std::fs::read(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let mut reg: Registry = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "{} does not read as a list of roots ({e}); nothing was taken off",
            file.display()
        )
    })?;
    if !reg
        .roots
        .iter()
        .any(|e| e.root_id == entry.root_id || e.path == entry.path)
    {
        return Ok(format!(
            "{shown} was already off this device's list of roots."
        ));
    }
    let before = reg.roots.len();
    reg.roots.retain(|e| e != entry);
    if reg.roots.len() == before {
        return Err(format!(
            "{shown} changed in the list since it was shown; nothing was taken off, so look again"
        ));
    }
    write(file, &reg).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(format!(
        "{shown} is off this device's list of roots. Nothing in it or on the server changed, and \
         should the root come back, its daemon lists it again when it starts."
    ))
}

/// The engine's lock: `flock` on Unix, a lock on the first byte on Windows, which this one, on the whole
/// file, overlaps.
fn lock(dir: &Path) -> Result<std::fs::File, String> {
    let path = dir.join("roots.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(format!("cannot lock {}: {e}", path.display()))
            }
        }
        if start.elapsed() > LOCK_WAIT {
            return Err(format!(
                "another program has held {} for {} seconds; nothing was taken off",
                path.display(),
                LOCK_WAIT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A new file, durable, renamed over the old, so a reader sees one or the other whole.
fn write(file: &Path, reg: &Registry) -> std::io::Result<()> {
    let dir = file.parent().expect("a file in a folder");
    let tmp = dir.join(format!("roots.json.{}.new", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(reg)?)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, file)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(windows)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ADD_FILE, FILE_FLAG_BACKUP_SEMANTICS};
    std::fs::OpenOptions::new()
        .access_mode(FILE_ADD_FILE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?
        .sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("cairn-tray-registry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn entry(dir: &Path, name: &str, id: &str) -> RootEntry {
        RootEntry {
            path: dir.join(name),
            root_id: id.into(),
            server: "https://example.invalid".into(),
            added: 1,
        }
    }

    fn put(file: &Path, roots: &[&RootEntry]) {
        let reg = Registry {
            roots: roots.iter().map(|e| (*e).clone()).collect(),
        };
        std::fs::write(file, serde_json::to_vec_pretty(&reg).unwrap()).unwrap();
    }

    fn listed(file: &Path) -> Vec<RootEntry> {
        serde_json::from_slice::<Registry>(&std::fs::read(file).unwrap())
            .unwrap()
            .roots
    }

    /// The engine's own lock, taken as it takes it (crates/engine/src/fsops*.rs, `try_lock`).
    fn hold_as_the_engine(file: &std::fs::File) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: an open descriptor.
            let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            assert_eq!(r, 0, "{}", std::io::Error::last_os_error());
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
            };
            // SAFETY: an open handle, and an OVERLAPPED that gives the offset, 0.
            let ok = unsafe {
                let mut at = std::mem::zeroed();
                LockFileEx(
                    file.as_raw_handle() as _,
                    LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                    0,
                    1,
                    0,
                    &mut at,
                )
            };
            assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
        }
    }

    #[test]
    fn a_gone_root_is_taken_off_and_one_still_there_is_not() {
        let s = scratch("forget");
        let file = s.0.join("roots.json");
        let gone = entry(&s.0, "gone", "id-gone");
        let here = entry(&s.0, "here", "id-here");
        std::fs::create_dir_all(here.path.join(".cairn")).unwrap();
        std::fs::write(here.path.join(".cairn/root-id"), "id-here\n").unwrap();
        put(&file, &[&gone, &here]);

        let e = forget(&file, &here).unwrap_err();
        assert!(e.contains("holds this root again"), "{e}");
        assert_eq!(listed(&file), vec![gone.clone(), here.clone()]);

        // As shown, but changed in the list since: a daemon that listed it again, say.
        let mut stale = gone.clone();
        stale.server = "https://elsewhere.invalid".into();
        let e = forget(&file, &stale).unwrap_err();
        assert!(e.contains("changed in the list"), "{e}");
        assert_eq!(listed(&file).len(), 2);

        let said = forget(&file, &gone).unwrap();
        assert!(said.contains("is off this device's list"), "{said}");
        assert_eq!(listed(&file), vec![here.clone()]);
        let said = forget(&file, &gone).unwrap();
        assert!(said.contains("already off"), "{said}");
        assert_eq!(listed(&file), vec![here]);
        let left: Vec<_> = std::fs::read_dir(&s.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".new"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn it_waits_for_the_engines_lock() {
        let s = scratch("lock");
        let file = s.0.join("roots.json");
        let gone = entry(&s.0, "gone", "id-gone");
        put(&file, &[&gone]);
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(s.0.join("roots.lock"))
            .unwrap();
        hold_as_the_engine(&held);
        let (path, e) = (file.clone(), gone.clone());
        let t = std::thread::spawn(move || forget(&path, &e));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            listed(&file),
            vec![gone],
            "taken off while the engine held the lock"
        );
        drop(held);
        t.join().unwrap().unwrap();
        assert!(listed(&file).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_new_list_is_this_users_only() {
        use std::os::unix::fs::PermissionsExt;
        let s = scratch("mode");
        let file = s.0.join("roots.json");
        let gone = entry(&s.0, "gone", "id-gone");
        let other = entry(&s.0, "other", "id-other");
        put(&file, &[&gone, &other]);
        forget(&file, &gone).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
