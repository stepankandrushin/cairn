//! This user's roots on this device, for `cairn status --all` and the tray: the registry of
//! `cairn_protocol::local`. `cairn init` and a starting daemon add to it. Nothing here removes an entry
//! because its folder has gone: it reads as gone, so nobody is left thinking a root still syncs.

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

/// The registry as it is; empty if nothing was ever registered.
pub fn load() -> Result<Registry> {
    read(&file()?)
}

/// Each root with whether it is still there and what its daemon says.
pub fn views() -> Result<Vec<RootView>> {
    Ok(load()?.roots.into_iter().map(view).collect())
}

pub fn view(root: RootEntry) -> RootView {
    let present = crate::root::read_root_id(&root.path).is_ok_and(|id| id == root.root_id);
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

/// Writers take turns, so that two adding at once keep both.
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
