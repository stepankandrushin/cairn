//! A sync root on disk: `.cairn/` at its top, holding the ID marker, configuration and state.
//!
//! The marker is what proves that a folder is the root it claims to be (invariant 5): when a mount
//! has gone away the marker has gone with it, and sync stops instead of reading an empty folder as
//! a mass delete.

use crate::fsops::Dir;
use crate::metadata::ApplyPolicy;
use crate::state::{State, KEY_ROOT_ID};
use anyhow::{bail, Context, Result};
use cairn_protocol::{DeviceId, FilenamePolicy, STATE_DIR};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MARKER: &str = "root-id";
const CONFIG: &str = "config.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub server: String,
    pub namespace: String,
    pub token: String,
    pub device: DeviceId,
    pub device_name: String,
    #[serde(default)]
    pub policy: ApplyPolicy,
    #[serde(default)]
    pub filenames: FilenamePolicy,
    /// Allow plain HTTP beyond loopback, which sends the token in the clear.
    #[serde(default)]
    pub allow_http: bool,
}

pub struct InitOptions {
    pub server: String,
    pub namespace: String,
    pub token: String,
    pub device_name: Option<String>,
    pub policy: ApplyPolicy,
    pub allow_http: bool,
}

/// Sync stopped itself and is waiting for a person.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Paused(pub String);

pub struct Root {
    pub path: PathBuf,
    pub dir: Dir,
    pub state_dir: Dir,
    pub config: Config,
    pub state: State,
    /// An exclusive lock for this process's life: one sync per root at a time.
    _lock: File,
}

fn hostname() -> String {
    let name = rustix::system::uname();
    let name = name.nodename().to_string_lossy().into_owned();
    if name.is_empty() {
        "unknown".into()
    } else {
        name
    }
}

fn write_private(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    let tmp = dir.join(format!("{name}.new"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(name))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Make a directory a sync root. It may hold files; the first sync merges them with the server.
pub fn init_root(path: &Path, opts: InitOptions) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("{}", path.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let state = path.join(std::ffi::OsStr::from_bytes(STATE_DIR));
    if state.exists() {
        bail!("{} is already a sync root", path.display());
    }
    std::fs::create_dir(&state)?;
    std::fs::set_permissions(&state, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;

    let config = Config {
        server: opts.server.trim_end_matches('/').to_string(),
        namespace: opts.namespace,
        token: opts.token,
        device: DeviceId::random(),
        device_name: opts.device_name.unwrap_or_else(hostname),
        policy: opts.policy,
        filenames: FilenamePolicy::Faithful,
        allow_http: opts.allow_http,
    };
    let root_id = hex::encode(rand::random::<[u8; 16]>());
    write_private(&state, CONFIG, &serde_json::to_vec_pretty(&config)?)?;
    let db = State::open(&state.join("state.sqlite"))?;
    db.set(KEY_ROOT_ID, &root_id)?;
    write_private(&state, MARKER, root_id.as_bytes())?;
    File::open(path)?.sync_all()?;
    Ok(())
}

impl Root {
    pub fn open(path: &Path) -> Result<Self> {
        let dir =
            Dir::open_root(path).with_context(|| format!("open sync root {}", path.display()))?;
        let state_path = path.join(std::ffi::OsStr::from_bytes(STATE_DIR));
        let marker = match std::fs::read_to_string(state_path.join(MARKER)) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Paused(format!(
                    "{} has no {} marker: either it was never set up with `cairn init`, or the filesystem \
                     that holds it is not mounted. Sync is paused and nothing was changed.",
                    path.display(),
                    String::from_utf8_lossy(STATE_DIR),
                ))
                .into());
            }
            Err(e) => return Err(e).context("read the root marker"),
        };
        let state_dir = dir
            .open_dir(STATE_DIR)
            .context("open the state directory")?;
        let lock = File::create(state_path.join("lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |_| anyhow::anyhow!("another cairn is already working on {}", path.display()),
        )?;
        let state = State::open(&state_path.join("state.sqlite"))?;
        if state.get(KEY_ROOT_ID)?.as_deref() != Some(marker.trim()) {
            return Err(Paused(format!(
                "the root marker in {} does not match its state: this is not the folder that state belongs to. \
                 Sync is paused and nothing was changed.",
                path.display()
            ))
            .into());
        }
        let config: Config = serde_json::from_slice(&std::fs::read(state_path.join(CONFIG))?)
            .context("read the configuration")?;
        state_dir.ensure_dir(b"stage", 0o700)?;
        state_dir.ensure_dir(b"chunks", 0o700)?;
        Ok(Self {
            path: path.to_path_buf(),
            dir,
            state_dir,
            config,
            state,
            _lock: lock,
        })
    }

    /// Temporary names are this device's own, so a scan can tell its leftovers from a user's file.
    pub fn temp_prefix(&self) -> String {
        format!(".cairn-tmp-{}-", self.config.device)
    }

    pub fn temp_name(&self) -> Vec<u8> {
        format!(
            "{}{}",
            self.temp_prefix(),
            hex::encode(rand::random::<[u8; 8]>())
        )
        .into_bytes()
    }
}
