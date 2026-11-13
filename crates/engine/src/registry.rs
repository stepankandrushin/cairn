//! This user's roots on this device, for `cairn-sync status --all` and the tray: the registry of
//! `cairn_protocol::local`. `cairn-sync init` and a starting daemon add to it. Nothing removes an entry
//! because its folder has gone: it reads as gone, so nobody is left thinking a root still syncs, until a
//! person takes it off with `cairn-sync forget`.

use anyhow::{anyhow, Context, Result};
use cairn_protocol::local::{registry_path, Registry, RootEntry, RootView};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Longer than any writer holds it: each holds it for one read and one rename.
const LOCK_WAIT: Duration = Duration::from_secs(10);

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn file() -> Result<PathBuf> {
    registry_path().ok_or_else(|| {
        anyhow!(
            "no directory for this user's list of roots: HOME (LOCALAPPDATA on Windows) is not set"
        )
    })
}

/// Add this root, or bring its entry up to date: one entry per root ID, and per path, since a root
/// made again where another was replaces it.
pub fn register(path: &Path, root_id: &str, server: &str) -> Result<()> {
    let file = file()?;
    let dir = file.parent().expect("a file in a directory");
    std::fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    let _ = crate::fsops::make_private_dir(dir);
    let _lock = lock(dir)?;
    let mut reg = read(&file)?;
    let path = std::path::absolute(path)?;
    if let Some(e) = reg.roots.iter().find(|e| e.root_id == root_id) {
        if e.path == path && e.server == server {
            return Ok(());
        }
    }
    let added = reg
        .roots
        .iter()
        .find(|e| e.root_id == root_id)
        .map_or_else(now_secs, |e| e.added);
    reg.roots.retain(|e| e.root_id != root_id && e.path != path);
    reg.roots.push(RootEntry {
        path,
        root_id: root_id.to_string(),
        server: server.to_string(),
        added,
    });
    write(&file, &reg)
}

/// What [`forget`] took off the list.
#[derive(Debug)]
pub struct Forgotten {
    pub entry: RootEntry,
    /// It was still there: its folder held its ID.
    pub present: bool,
}

/// Take a root off the list, by its path as listed or by its ID. One that is still there is taken off
/// only when `even_if_present`: it syncs as before, and its daemon lists it again when it starts.
pub fn forget(path_or_id: &Path, even_if_present: bool) -> Result<Forgotten> {
    let file = file()?;
    let listed = read(&file)?;
    let absolute = std::path::absolute(path_or_id)?;
    let by_path = listed
        .roots
        .iter()
        .find(|e| e.path == absolute || e.path == path_or_id);
    let by_id = || {
        listed
            .roots
            .iter()
            .find(|e| Path::new(&e.root_id) == path_or_id)
    };
    let Some(entry) = by_path.or_else(by_id).cloned() else {
        return Err(anyhow!(
            "no root is listed at {}: `cairn-sync status --all` lists them",
            path_or_id.display()
        ));
    };
    // Asked before the lock: a folder on a mount that hangs must not hold up every other writer.
    let present = is_present(&entry);
    if present && !even_if_present {
        return Err(anyhow!(
            "{} is still there, root {}: it is taken off the list only with --even-if-present, and \
             its daemon lists it again when it starts",
            entry.path.display(),
            entry.root_id
        ));
    }
    let dir = file.parent().expect("a file in a directory");
    let _lock = lock(dir)?;
    let mut reg = read(&file)?;
    let before = reg.roots.len();
    reg.roots.retain(|e| *e != entry);
    if reg.roots.len() == before {
        return Err(anyhow!(
            "{} changed in the list while this looked at it; nothing was taken off, so ask again",
            entry.path.display()
        ));
    }
    write(&file, &reg)?;
    Ok(Forgotten { entry, present })
}

/// The registry as it is; empty if nothing was ever registered.
pub fn load() -> Result<Registry> {
    read(&file()?)
}

/// Each root with whether it is still there and what its daemon says.
pub fn views() -> Result<Vec<RootView>> {
    Ok(load()?.roots.into_iter().map(view).collect())
}

/// The folder still holds this root: its `.cairn/root-id` reads as the entry's ID.
fn is_present(root: &RootEntry) -> bool {
    crate::root::read_root_id(&root.path).is_ok_and(|id| id == root.root_id)
}

pub fn view(root: RootEntry) -> RootView {
    let present = is_present(&root);
    let daemon = if present {
        crate::control::status_of(&root.path, &root.root_id)
    } else {
        None
    };
    RootView {
        root,
        present,
        daemon,
    }
}

/// A file that does not read is an error, not an empty list: writing over it would drop every root
/// in it without a word.
fn read(file: &Path) -> Result<Registry> {
    match std::fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} does not read as a list of roots; move it away to start a new one",
                file.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", file.display())),
    }
}

/// A new file renamed over the old, so a reader sees one or the other whole.
fn write(file: &Path, reg: &Registry) -> Result<()> {
    let dir = file.parent().expect("a file in a directory");
    let tmp = dir.join(format!("roots.json.{}.new", std::process::id()));
    let result = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        let mut f = crate::fsops::private_file(&mut opts).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(reg)?)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, file)?;
        crate::fsops::sync_dir_path(dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("write {}", file.display()))
}

/// Writers take turns, so that two adding at once keep both. The tray's Forget takes this lock too, and
/// writes the file as [`write`] does (tray/src/registry.rs): a change to either is a change to both.
fn lock(dir: &Path) -> Result<std::fs::File> {
    let path = dir.join("roots.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let start = Instant::now();
    loop {
        if crate::fsops::try_lock(&file)? {
            return Ok(file);
        }
        if start.elapsed() > LOCK_WAIT {
            return Err(anyhow!(
                "another program has held {} for {} seconds",
                path.display(),
                LOCK_WAIT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
