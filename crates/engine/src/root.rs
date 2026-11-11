//! A sync root on disk: `.cairn/` at its top, holding the ID marker, configuration and state.
//!
//! The marker is what proves that a folder is the root it claims to be (invariant 5): when a mount
//! has gone away the marker has gone with it, and sync stops instead of reading an empty folder as
//! a mass delete.

use crate::fsops::{self, name_path, Dir};
use crate::metadata::ApplyPolicy;
use crate::state::{State, KEY_ROOT_ID};
use anyhow::{bail, Context, Result};
use cairn_failpoint::hit;
use cairn_protocol::{DeviceId, FilenamePolicy, STATE_DIR};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
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

/// Another process is working on this root right now.
#[derive(Debug, thiserror::Error)]
#[error("another cairn is already working on {0}")]
pub struct Busy(pub String);

pub struct Root {
    pub path: PathBuf,
    pub dir: Dir,
    pub state_dir: Dir,
    pub config: Config,
    pub state: State,
    /// An exclusive lock for this process's life: one sync per root at a time.
    _lock: File,
}

/// When a sync last ended with every decision the full sync's, in seconds since the epoch: one whose
/// last round took the whole tree, or the daemon's check, every round of which the full way confirmed.
/// Not in the state file, where a write would cost the next sync with nothing to do its shortcut.
const FULL_CHECKED: &str = "full-checked";

impl Root {
    /// The state's own file.
    pub fn state_file(&self) -> PathBuf {
        self.path.join(name_path(STATE_DIR)).join("state.sqlite")
    }

    pub fn full_checked(&self) -> Option<i64> {
        let path = self.path.join(name_path(STATE_DIR)).join(FULL_CHECKED);
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    pub fn set_full_checked(&self, secs: i64) -> Result<()> {
        write_private(
            &self.path.join(name_path(STATE_DIR)),
            FULL_CHECKED,
            secs.to_string().as_bytes(),
        )
    }
}

fn write_private(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    let tmp = dir.join(format!("{name}.new"));
    let mut f = fsops::private_file(
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true),
    )
    .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(name))?;
    fsops::sync_dir_path(dir)?;
    Ok(())
}

/// Make a directory a sync root, and say where its server is: at what sequence. It may hold files; the
/// first sync merges them with the server. The server is asked first, so one that cannot be reached,
/// refuses the token or speaks another protocol leaves the folder as it was, with nothing to list and
/// nothing in the way of the same init run again.
pub fn init_root(path: &Path, opts: InitOptions) -> Result<u64> {
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("{}", path.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let state = path.join(name_path(STATE_DIR));
    if state.exists() {
        bail!("{} is already a sync root", path.display());
    }
    let config = Config {
        server: opts.server.trim_end_matches('/').to_string(),
        namespace: opts.namespace,
        token: opts.token,
        device: DeviceId::random(),
        device_name: opts.device_name.unwrap_or_else(fsops::hostname),
        policy: opts.policy,
        filenames: FilenamePolicy::Faithful,
        allow_http: opts.allow_http,
    };
    let head = crate::client::Client::new(&config, crate::client::Retries::Short)
        .and_then(|c| c.info())
        .with_context(|| {
            format!(
                "{} was not made a sync root, and nothing in it changed",
                path.display()
            )
        })?
        .head;

    std::fs::create_dir(&state)?;
    fsops::make_private_dir(&state)?;
    let root_id = hex::encode(rand::random::<[u8; 16]>());
    write_private(&state, CONFIG, &serde_json::to_vec_pretty(&config)?)?;
    let db = State::open(&state.join("state.sqlite"))?;
    db.set(KEY_ROOT_ID, &root_id)?;
    write_private(&state, MARKER, root_id.as_bytes())?;
    fsops::sync_dir_path(path)?;
    Ok(head)
}

/// The name of the directory a root is, as its user's own name for the folder: what a device offers
/// the server. None for a name that is not text or would not do as one, or a drive's or the
/// filesystem's top.
pub fn root_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    cairn_protocol::wire::valid_folder_name(name).then(|| name.to_string())
}

/// Offers the server the name of the directory this root is, as its user's own name for the folder,
/// which the server keeps only if they have none yet; what went wrong, if anything. Whatever
/// happens, the sync goes on as before: the name is only what the web shows them.
pub fn offer_root_name(path: &Path) -> Option<String> {
    let name = root_name(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))?;
    let offered = read_config(path).and_then(|config| {
        crate::client::Client::new(&config, crate::client::Retries::Never)?.offer_name(&name)
    });
    // Taken, or theirs already, or a server before such names: nothing to say.
    match offered {
        Ok(_) => None,
        Err(e) => Some(format!(
            "offering this root's name, {name:?}, as its user's name for the folder: {e:#}; offered again at the next start"
        )),
    }
}

/// This root's marker, without taking its lock.
pub fn read_root_id(path: &Path) -> std::io::Result<String> {
    let state = path.join(name_path(STATE_DIR));
    Ok(std::fs::read_to_string(state.join(MARKER))?
        .trim()
        .to_string())
}

/// This root's configuration, without taking its lock.
pub fn read_config(path: &Path) -> Result<Config> {
    let state = path.join(name_path(STATE_DIR));
    serde_json::from_slice(&std::fs::read(state.join(CONFIG))?).context("read the configuration")
}

fn read_marker(path: &Path) -> Result<String> {
    let state_path = path.join(name_path(STATE_DIR));
    match std::fs::read_to_string(state_path.join(MARKER)) {
        Ok(m) => Ok(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Paused(format!(
            "{} has no {} marker: either it was never set up with `cairn init`, or the filesystem \
             that holds it is not mounted. Sync is paused and nothing was changed.",
            path.display(),
            String::from_utf8_lossy(STATE_DIR),
        ))
        .into()),
        Err(e) => Err(e).context("read the root marker"),
    }
}

/// The configuration of a root, for what only asks the server: no lock, so it works while a sync
/// runs, and nothing here is read or changed.
pub fn open_config(path: &Path) -> Result<Config> {
    read_marker(path)?;
    read_config(path)
}

impl Root {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, false)
    }

    /// As `open`, for a sync, which reads most of the state: the state's file is read through first,
    /// in the file's order, as `state::warm` says.
    pub fn open_to_sync(path: &Path) -> Result<Self> {
        Self::open_with(path, true)
    }

    fn open_with(path: &Path, warm: bool) -> Result<Self> {
        let dir =
            Dir::open_root(path).with_context(|| format!("open sync root {}", path.display()))?;
        let state_path = path.join(name_path(STATE_DIR));
        let marker = read_marker(path)?;
        let state_dir = dir
            .open_dir(STATE_DIR)
            .context("open the state directory")?;
        let lock = File::create(state_path.join("lock"))?;
        if !matches!(fsops::try_lock(&lock), Ok(true)) {
            return Err(Busy(path.display().to_string()).into());
        }
        if warm {
            crate::state::warm(&state_path.join("state.sqlite"));
            hit("root.warmed");
        }
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

    /// The namespace was renamed on the server, which calls it `now`: the configuration says so from
    /// here on, written as a whole and durably, as init wrote it.
    pub fn take_up_folder_name(&mut self, now: &str) -> Result<()> {
        let mut config = self.config.clone();
        config.namespace = now.to_string();
        write_private(
            &self.path.join(name_path(STATE_DIR)),
            CONFIG,
            &serde_json::to_vec_pretty(&config)?,
        )?;
        self.config = config;
        Ok(())
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

    /// The name for whatever stands at `original` as it is moved out of the way. Only finished work and
    /// what it displaces ever carry it, so what a killed run leaves under it may be the only copy of
    /// someone's edit, which a half-written temporary cannot be. It says where it came from, as far as
    /// that fits.
    pub fn aside_name(&self, original: &[u8]) -> Vec<u8> {
        let mut name = format!(
            "{}{ASIDE}{}-",
            self.temp_prefix(),
            hex::encode(rand::random::<[u8; 8]>())
        )
        .into_bytes();
        let kept = name_start(original, NAME_MAX.saturating_sub(name.len()));
        name.extend_from_slice(kept);
        name
    }
}

/// As much of `name` as fits in `room` bytes: cut where a character ends, and never just after a dot
/// or a space, as Windows refuses a name that ends in one, and so would refuse the temporary.
fn name_start(name: &[u8], room: usize) -> &[u8] {
    let mut keep = name.len().min(room);
    if let Ok(text) = std::str::from_utf8(name) {
        while !text.is_char_boundary(keep) {
            keep -= 1;
        }
    }
    if keep < name.len() {
        while keep > 0 && matches!(name[keep - 1], b'.' | b' ') {
            keep -= 1;
        }
    }
    &name[..keep]
}

const ASIDE: &str = "aside-";
/// The shortest limit on a name of any filesystem likely to hold a tree, eCryptfs's.
const NAME_MAX: usize = 143;

/// What a temporary's name says about it, given the part after the prefix: `None` if it is not one of
/// `Root::aside_name`'s, and otherwise the name it came from.
pub fn aside_origin(rest: &[u8]) -> Option<&[u8]> {
    let rest = rest.strip_prefix(ASIDE.as_bytes())?;
    Some(rest.get(17..).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_name_ends_where_a_character_does_and_never_in_a_dot_or_a_space() {
        // A device's aside prefix is 67 bytes, leaving 76 of NAME_MAX, which cut a name of 75 bytes and
        // `.md` just after its dot: Windows refused that temporary, and the file never took its new
        // version there.
        let n = [vec![b'n'; 74], b"e.md".to_vec()].concat();
        let n = n.as_slice();
        let prefix = format!(".cairn-tmp-{}-{ASIDE}{}-", "0".repeat(32), "0".repeat(16));
        assert_eq!(prefix.len(), 67);
        let kept = name_start(n, NAME_MAX - prefix.len());
        assert_eq!(kept, &n[..n.len() - 3]);
        assert!(kept.ends_with(b"ne"));
        assert_eq!(name_start(n, 200), n);
        assert_eq!(name_start(b"a. . b", 5), b"a");
        assert_eq!(name_start(b". .x", 3), b"");
        // A name kept whole is kept as it is, which another filesystem may allow.
        assert_eq!(name_start(b"x.", 9), b"x.");
        assert_eq!(name_start("ab\u{e9}.c".as_bytes(), 3), b"ab");
        assert_eq!(
            name_start("ab\u{e9}.c".as_bytes(), 5),
            "ab\u{e9}".as_bytes()
        );
        for room in 0..=n.len() {
            let k = name_start(n, room);
            assert!(k.len() <= room && n.starts_with(k));
            assert!(!k.ends_with(b".") && !k.ends_with(b" "), "{room}");
        }
    }
}
