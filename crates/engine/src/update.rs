//! Keeping this user's Cairn programs on this device up to date, from a signed feed
//! (`cairn_protocol::update`).
//!
//! One updater runs at a time: whichever daemon or `cairn update now` holds `update.lock` in the user
//! dir. It asks the feed for `update.json`, checks the release's signature against the keys this build
//! trusts, and takes each program installed here, `cairn` and the tray beside it, to that release:
//!
//! 1. each file is downloaded beside what it replaces, into `.cairn-update`, and its size and SHA-256
//!    checked against the signed release, so a cut, a full disk or a changed file installs nothing;
//! 2. on macOS the new bundle's own signature is checked, and must be by the team of the one it replaces,
//!    as the permissions macOS keeps go by that team;
//! 3. the new program runs once, `--version`, and must say the release's version;
//! 4. it is put in place by renaming, never by writing over the old, which on macOS kills a signed
//!    program and on Windows cannot be done to a running one: on Unix one rename, a bundle swapped whole,
//!    on Windows two, the running one renamed aside first;
//! 5. the one before is kept in `.cairn-update`, for `cairn update rollback`, and for a daemon that
//!    keeps dying on the new version before it gets through a sync, which takes it back by itself.
//!
//! Each program then restarts into the new version: a daemon sees its program replaced and restarts
//! between syncs, and the tray does the same. A kill at any step leaves the old program or the new one in
//! place, whole, and the next run finishes the job.

use crate::fsops;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use cairn_failpoint::{check, hit, tuned};
use cairn_protocol::local::{user_dir, Secs};
use cairn_protocol::update::{
    platform, serves, settings_path, Form, Policy, Release, ReleaseFile, Settings, Signed, Status,
    Version, SIGNED_PREFIX,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The feeds a release build asks, in order, and the public keys its releases are signed with, each
/// list comma-separated: given by whoever builds it. scripts/release.sh insists on both, or on
/// `CAIRN_UPDATE=off`. A build without them updates only from a feed its settings name.
const BUILT_FEED: Option<&str> = option_env!("CAIRN_UPDATE_FEED");
const BUILT_KEYS: Option<&str> = option_env!("CAIRN_UPDATE_KEYS");

/// Between a daemon's checks of the feed, give or take a tenth, so devices do not all ask at once.
const EVERY_SECS: u64 = 3600;
/// The first check after a daemon starts: soon, but not at once, so a fleet started together spreads.
const FIRST_SECS: u64 = 120;
/// How often a daemon looks whether its program has been replaced.
const TICK_MS: u64 = 10_000;
const MAX_FEED: u64 = 1 << 20;
const MAX_FILE: u64 = 1 << 31;
/// A daemon on a new version that starts this many times without getting through a sync, for one root,
/// takes the version back: something in it kills the daemon.
const MAX_STARTS: u32 = 3;
/// Or that has run this long: what kills it is then not the version, or not soon.
const PROVEN_AFTER: Duration = Duration::from_secs(3600);
/// Beside what is replaced, so that every rename stays on one filesystem.
pub const STAGING: &str = ".cairn-update";
/// The state file is held for one read and one rename.
const STATE_WAIT: Duration = Duration::from_secs(10);
/// A person's `cairn update now` waits this long for another updater to finish.
const ASKED_WAIT: Duration = Duration::from_secs(900);
const SELF_CHECK_WAIT: Duration = Duration::from_secs(60);
/// What a daemon exits with on Windows to be started again on the new program, which a process there
/// cannot replace itself with.
pub const EXIT_RESTART: u8 = 75;

fn now_secs() -> Secs {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The version this program is: its build's.
pub fn running_version() -> String {
    cairn_failpoint::version(env!("CARGO_PKG_VERSION"))
}

static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Where this program was started from, as it was then: once it has been replaced, Linux names the
/// running one "(deleted)". `main` asks first thing, so that this is the path it was started by.
pub fn exe() -> Option<&'static Path> {
    EXE.get_or_init(|| {
        let p = std::env::current_exe().ok()?;
        // Through a link to the bundle on macOS, `~/.local/bin/cairn`, to the program in it.
        #[cfg(unix)]
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        Some(p)
    })
    .as_deref()
}

/// Enough of a file's identity to tell it has been replaced: a rename puts another file at the path,
/// and writing over it changes its size or time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    id: u64,
    len: u64,
    modified: i128,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    let nanos = |t: std::io::Result<SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i128)
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

static START_STAMP: OnceLock<Option<Stamp>> = OnceLock::new();

fn start_stamp() -> Option<Stamp> {
    *START_STAMP.get_or_init(|| exe().and_then(stamp))
}

/// Call first thing in `main`, before anything can replace the program.
pub fn remember_start() {
    let _ = start_stamp();
}

/// This program's file is no longer the one it was started from: an update, or a person, put another
/// in its place.
pub fn replaced() -> bool {
    match (start_stamp(), exe()) {
        (Some(then), Some(path)) => stamp(path).is_some_and(|now| now != then),
        _ => false,
    }
}

/// Start this program again from what is at its path now, with the same arguments. On Unix in this
/// very process, so a supervisor sees nothing end and a daemon run by hand goes on; on Windows, where
/// a process cannot do that, by exiting with [`EXIT_RESTART`] for the scheduled task's loop to start it
/// again.
pub fn restart() -> anyhow::Error {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let Some(path) = exe() else {
            return anyhow!("cannot tell where this program is, to start it again");
        };
        let mut cmd = Command::new(path);
        cmd.args(std::env::args_os().skip(1));
        for var in cairn_failpoint::not_passed_on() {
            cmd.env_remove(var);
        }
        let err = cmd.exec();
        anyhow!("cannot start {} again: {err}", path.display())
    }
    #[cfg(not(unix))]
    {
        let _ = std::io::stdout().flush();
        std::process::exit(EXIT_RESTART.into())
    }
}

// --- What is installed here ---

/// One program installed here, as an update replaces it.
#[derive(Clone, Debug)]
pub struct Program {
    /// `cairn` or `cairn-tray`.
    pub name: &'static str,
    /// What a release replaces: the executable, or on macOS the bundle around it.
    pub path: PathBuf,
    pub form: Form,
    /// The executable, in the bundle if there is one.
    pub exe: PathBuf,
}

fn exe_name(program: &str) -> String {
    format!("{program}{}", std::env::consts::EXE_SUFFIX)
}

/// The bundle a macOS program is in: `<Name>.app/Contents/MacOS/<program>`.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let named = |p: &Path, n: &str| p.file_name().is_some_and(|f| f == n);
    (named(macos, "MacOS")
        && named(contents, "Contents")
        && app.extension().is_some_and(|e| e == "app"))
    .then(|| app.to_path_buf())
}

fn program(name: &'static str, exe: &Path) -> Program {
    match bundle_of(exe) {
        Some(app) => Program {
            name,
            path: app,
            form: Form::App,
            exe: exe.to_path_buf(),
        },
        None => Program {
            name,
            path: exe.to_path_buf(),
            form: Form::Binary,
            exe: exe.to_path_buf(),
        },
    }
}

/// `cairn`, this program, and the tray where it is installed beside it: in the same folder, or on
/// macOS as `Cairn.app` beside this program's bundle.
pub fn programs() -> Result<Vec<Program>> {
    let exe = exe().ok_or_else(|| anyhow!("cannot tell where this program is"))?;
    let mut out = vec![program("cairn", exe)];
    let tray = match bundle_of(exe) {
        Some(app) => app
            .with_file_name("Cairn.app")
            .join("Contents/MacOS/cairn-tray"),
        None => exe.with_file_name(exe_name("cairn-tray")),
    };
    if tray.is_file() {
        out.push(program("cairn-tray", &tray));
    }
    Ok(out)
}

/// Run a program and give its output, killing it if it takes longer than `wait`.
fn output_within(cmd: &mut Command, wait: Duration) -> Result<std::process::Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if start.elapsed() > wait {
            let _ = child.kill();
            let _ = child.wait();
            bail!("did not finish within {} s", wait.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What `<exe> --version` says after the program's name.
fn version_of(exe: &Path, name: &str) -> Result<String> {
    let mut cmd = Command::new(exe);
    cmd.arg("--version");
    for var in cairn_failpoint::not_passed_on() {
        cmd.env_remove(var);
    }
    let out = output_within(&mut cmd, SELF_CHECK_WAIT)
        .with_context(|| format!("run {} --version", exe.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next().unwrap_or("").trim();
    match line.strip_prefix(name).and_then(|v| v.strip_prefix(' ')) {
        Some(v) if out.status.success() && Version::parse(v).is_some() => Ok(v.to_string()),
        _ => bail!(
            "{} --version said {line:?} and {}, not `{name} <version>`",
            exe.display(),
            out.status
        ),
    }
}

/// The version of a program as it is installed now: this program's own while its file is still the one
/// it started from.
pub fn installed_version(p: &Program) -> Result<String> {
    if p.name == "cairn" && Some(p.exe.as_path()) == exe() && !replaced() {
        return Ok(running_version());
    }
    version_of(&p.exe, p.name)
}

// --- Settings and state ---

fn dir() -> Result<PathBuf> {
    user_dir().ok_or_else(|| {
        anyhow!(
            "no directory for this user's Cairn state: HOME (LOCALAPPDATA on Windows) is not set"
        )
    })
}

/// The settings as they are: the defaults if none were ever written.
pub fn settings() -> Result<Settings> {
    let path = settings_path().ok_or_else(|| anyhow!("no directory for the update settings"))?;
    match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| {
            format!(
                "{} does not read as update settings; move it away to use the defaults",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

pub fn set_policy(policy: Policy) -> Result<Settings> {
    let mut s = settings()?;
    s.policy = policy;
    let path = settings_path().ok_or_else(|| anyhow!("no directory for the update settings"))?;
    write_json(&path, &s)?;
    Ok(s)
}

/// A version put in place and not yet shown to work: the daemons' starts on it, by root.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Probation {
    version: String,
    since: Secs,
    #[serde(default)]
    starts: BTreeMap<String, u32>,
}

/// `update-state.json`: what is shown, and what only the updater reads.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(flatten)]
    status: Status,
    #[serde(default)]
    probation: Option<Probation>,
}

fn state_path() -> Result<PathBuf> {
    cairn_protocol::update::state_path()
        .ok_or_else(|| anyhow!("no directory for the updater's state"))
}

/// A file that does not read is an error: written over, it would forget which versions were taken
/// back.
fn load_state() -> Result<State> {
    let path = state_path()?;
    match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| {
            format!(
                "{} does not read as the updater's state; move it away to start a new one",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// A new file renamed over the old, so a reader sees one or the other whole.
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let dir = path.parent().expect("a file in a directory");
    std::fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    let _ = fsops::make_private_dir(dir);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!("{name}.{}.new", std::process::id()));
    let result = (|| -> Result<()> {
        check("update.state")?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        let mut f = fsops::private_file(&mut opts).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value)?)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        fsops::sync_dir_path(dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("write {}", path.display()))
}

fn open_lock(name: &str) -> Result<std::fs::File> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("make {}", dir.display()))?;
    let _ = fsops::make_private_dir(&dir);
    let path = dir.join(name);
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))
}

/// `None` if another holds it for longer than `wait`.
fn take_lock(name: &str, wait: Duration) -> Result<Option<std::fs::File>> {
    let file = open_lock(name)?;
    let start = Instant::now();
    loop {
        if fsops::try_lock(&file)? {
            return Ok(Some(file));
        }
        if start.elapsed() >= wait {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read, change and write the state under its own short lock, which the updater's long one does not
/// hold up.
fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> Result<T> {
    let _lock = take_lock("update-state.lock", STATE_WAIT)?.ok_or_else(|| {
        anyhow!(
            "another program has held update-state.lock for {} seconds",
            STATE_WAIT.as_secs()
        )
    })?;
    let mut state = load_state()?;
    let out = f(&mut state);
    write_json(&state_path()?, &state)?;
    Ok(out)
}

/// What the updater knows now, with the policy as set.
pub fn status() -> Result<Status> {
    let mut s = load_state()?.status;
    s.policy = settings()?.policy;
    Ok(s)
}

// --- The feed ---

struct Source {
    /// In the order they are asked: the first that answers with a release a trusted key signed is the
    /// one its files are downloaded beside, and the rest are a fallback for when it does not.
    feeds: Vec<String>,
    keys: Vec<String>,
}

/// The feeds a setting names: one URL or path, or several comma-separated, asked in that order.
fn feeds_of(setting: &str) -> Vec<String> {
    setting
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(String::from)
        .collect()
}

fn source(settings: &Settings, opts: &Options) -> Result<Option<Source>> {
    let feeds = opts
        .feed
        .clone()
        .or_else(|| settings.feed.clone())
        .or_else(|| BUILT_FEED.map(String::from))
        .map(|f| feeds_of(&f))
        .unwrap_or_default();
    if feeds.is_empty() {
        return Ok(None);
    }
    let built = || -> Vec<String> {
        BUILT_KEYS
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(String::from)
            .collect()
    };
    let keys = if !opts.keys.is_empty() {
        opts.keys.clone()
    } else if !settings.keys.is_empty() {
        settings.keys.clone()
    } else {
        built()
    };
    if keys.is_empty() {
        bail!(
            "no key to check {}'s signature with: this build trusts none, and the settings name none",
            feeds.join(", ")
        );
    }
    Ok(Some(Source { feeds, keys }))
}

/// The feeds this user's programs here are updated from, in the order they are asked: the settings', or
/// else the build's.
pub fn feeds(settings: &Settings) -> Vec<String> {
    settings
        .feed
        .clone()
        .or_else(|| BUILT_FEED.map(String::from))
        .map(|f| feeds_of(&f))
        .unwrap_or_default()
}

/// The release of the first of the feeds that answers with one a trusted key signed, and that feed. What
/// each one before it said is kept in the status, which `cairn update` shows and a daemon says once, so a
/// fallback is never what hides a feed that is down; when none answers, the error says what each did.
fn ask(source: &Source, status: &mut Status) -> Result<(String, Release)> {
    let mut failed = Vec::new();
    for feed in &source.feeds {
        let mut bytes = Vec::new();
        let got = fetch(feed, MAX_FEED, &mut bytes)
            .and_then(|_| verify(&bytes, &source.keys).with_context(|| format!("{feed} answered")));
        match got {
            Ok(release) => {
                status.answered = Some(feed.clone());
                status.unanswered = (!failed.is_empty()).then(|| failed.join("; "));
                return Ok((feed.clone(), release));
            }
            Err(e) => failed.push(format!("{e:#}")),
        }
    }
    bail!("{}", failed.join("; "))
}

/// Where `url` is, relative to the feed's own URL or path when it is not absolute.
fn resolve(feed: &str, url: &str) -> String {
    if url.contains("://") || Path::new(url).is_absolute() {
        return url.to_string();
    }
    match feed.rfind(['/', '\\']) {
        Some(i) => format!("{}{url}", &feed[..=i]),
        None => url.to_string(),
    }
}

fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        // The whole of a program, over a slow line: 15 MB at 10 KB/s.
        .timeout_recv_body(Some(Duration::from_secs(1800)))
        .user_agent(format!("cairn/{}", running_version()))
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

/// Copy what is at `url` to `out`, at most `limit` bytes: an `http://` or `https://` URL, or a file's
/// path or `file://` URL, to try a release from a folder before it is published.
fn fetch(url: &str, limit: u64, out: &mut dyn Write) -> Result<u64> {
    let mut reader: Box<dyn Read> = if url.starts_with("http://") || url.starts_with("https://") {
        hit("update.ask");
        let resp = agent()
            .get(url)
            .call()
            .with_context(|| format!("{url}: cannot be reached"))?;
        if resp.status() != 200 {
            bail!("{url} answered {}", resp.status());
        }
        Box::new(
            resp.into_body()
                .into_with_config()
                .limit(limit.saturating_add(1))
                .reader(),
        )
    } else {
        let path = url.strip_prefix("file://").unwrap_or(url);
        Box::new(std::fs::File::open(path).with_context(|| format!("open {path}"))?)
    };
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).with_context(|| format!("read {url}")),
        };
        total += n as u64;
        if total > limit {
            bail!("{url} is longer than the {limit} bytes it may be");
        }
        out.write_all(&buf[..n])?;
    }
}

/// The release in `update.json`, if one of `keys` signed it.
pub fn verify(bytes: &[u8], keys: &[String]) -> Result<Release> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let signed: Signed =
        serde_json::from_slice(bytes).context("the feed's answer is not a signed release")?;
    let mut message = SIGNED_PREFIX.to_vec();
    message.extend_from_slice(signed.release.as_bytes());
    let trusted = signed.signatures.iter().any(|s| {
        keys.contains(&s.key)
            && match (b64.decode(&s.key), b64.decode(&s.signature)) {
                (Ok(key), Ok(sig)) if key.len() == 32 => {
                    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
                        .verify(&message, &sig)
                        .is_ok()
                }
                _ => false,
            }
    });
    if !trusted {
        bail!(
            "the release is not signed by a key this build trusts ({} signature(s), none good): nothing was installed",
            signed.signatures.len()
        );
    }
    let release: Release =
        serde_json::from_str(&signed.release).context("the signed release does not read")?;
    if Version::parse(&release.version).is_none() {
        bail!("the release's version {:?} does not read", release.version);
    }
    Ok(release)
}

// --- A run ---

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// A person asked, by `cairn update now` or the tray: install whatever the policy, a version taken
    /// back included, and wait for another updater rather than leave it to it.
    pub asked: bool,
    /// Install what is found; without it, only say what there is.
    pub install: bool,
    /// Install a release on another protocol even though not every server here is known to speak it.
    pub any_protocol: bool,
    /// Another feed and its keys, for this run alone.
    pub feed: Option<String>,
    pub keys: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// This build names no feed, and the settings none.
    NoFeed,
    /// Another updater is at it.
    Busy,
    /// Everything installed here is at the release's version or newer.
    UpToDate(String),
    /// A newer release, not installed: why is in the status's `waits`.
    Waits(String),
    Installed(String),
}

/// One program to take to the release.
struct Todo {
    program: Program,
    from: String,
    file: ReleaseFile,
}

/// Ask the feed and, as `opts` and the settings say, install what it offers. The status as it is
/// after, written for every reader too.
pub fn run(opts: &Options, log: &mut dyn FnMut(&str)) -> Result<(Outcome, Status)> {
    let wait = if opts.asked {
        ASKED_WAIT
    } else {
        Duration::ZERO
    };
    if opts.asked {
        if let Ok(None) = take_lock("update.lock", Duration::ZERO) {
            log("another update is running here; waiting for it to finish");
        }
    }
    let Some(_lock) = take_lock("update.lock", wait)? else {
        return Ok((Outcome::Busy, status()?));
    };
    let settings = settings()?;
    let Some(source) = source(&settings, opts)? else {
        let mut s = status()?;
        s.waits =
            Some("this build names no feed to update from, and the settings name none".into());
        return Ok((Outcome::NoFeed, s));
    };
    let mut status = load_state()?.status;
    status.policy = settings.policy;
    let mut probation = None;
    let result = attempt(opts, &settings, &source, &mut status, &mut probation, log);
    let now = now_secs();
    match &result {
        Ok(_) => status.error = None,
        Err(e) => {
            let text = format!("{e:#}");
            if status.error.as_ref() != Some(&text) {
                status.error_since = now;
            }
            status.error = Some(text);
        }
    }
    let shown = status.clone();
    with_state(|s| {
        s.status = shown;
        if let Some(p) = probation {
            s.probation = Some(p);
        }
    })?;
    let outcome = result?;
    Ok((outcome, status))
}

fn attempt(
    opts: &Options,
    settings: &Settings,
    source: &Source,
    status: &mut Status,
    probation: &mut Option<Probation>,
    log: &mut dyn FnMut(&str),
) -> Result<Outcome> {
    let (feed, release) = ask(source, status)?;
    status.checked = now_secs();
    status.latest = Some(release.version.clone());
    let new = Version::parse(&release.version).expect("verify parsed it");

    let here = platform();
    let mut todo = Vec::new();
    for p in programs()? {
        let Some(file) = release
            .files
            .iter()
            .find(|f| f.program == p.name && serves(&f.platform, &here))
        else {
            if p.name == "cairn" {
                log(&format!(
                    "{} has no cairn for {here}: nothing here is updated by it",
                    release.version
                ));
            }
            continue;
        };
        if file.form != p.form {
            bail!(
                "{} has {} as {:?}, and here it is installed as {:?}",
                release.version,
                p.name,
                file.form,
                p.form
            );
        }
        let from = match installed_version(&p) {
            Ok(v) => v,
            // The tray, which does not say what it is: replaced by one that does.
            Err(e) if p.name != "cairn" => {
                log(&format!("{e:#}; replacing it"));
                "unknown".into()
            }
            Err(e) => return Err(e),
        };
        if Version::parse(&from).is_some_and(|v| v >= new) {
            continue;
        }
        todo.push(Todo {
            program: p,
            from,
            file: file.clone(),
        });
    }
    if todo.is_empty() {
        status.available = None;
        status.waits = None;
        return Ok(Outcome::UpToDate(release.version));
    }
    status.available = Some(release.version.clone());
    let version = release.version.clone();
    if !opts.asked && status.rejected.contains(&version) {
        status.waits = Some(format!(
            "{version} was taken back on this device, so it is installed only when a person asks, `cairn update now`"
        ));
        return Ok(Outcome::Waits(version));
    }
    if release.protocol != cairn_protocol::PROTOCOL_VERSION && !opts.any_protocol {
        if let Err(why) = servers_speak(release.protocol) {
            status.waits = Some(format!(
                "{version} speaks protocol {}, and this device's servers are not all known to: {why}. It is installed once they are",
                release.protocol
            ));
            return Ok(Outcome::Waits(version));
        }
    }
    if !opts.install {
        status.waits = Some(format!(
            "{version} is not installed yet: `cairn update now` installs it"
        ));
        return Ok(Outcome::Waits(version));
    }
    if !opts.asked && settings.policy != Policy::Auto {
        status.waits = Some(format!(
            "updates here are set to {}: `cairn update now` installs {version}",
            settings.policy.name()
        ));
        return Ok(Outcome::Waits(version));
    }
    for t in &todo {
        log(&format!(
            "{} {} -> {version}: {}",
            t.program.name,
            t.from,
            t.program.path.display()
        ));
        let url = resolve(&feed, &t.file.url);
        install(&t.program, &t.file, &url, &version)
            .with_context(|| format!("{} is still {}", t.program.name, t.from))?;
    }
    status.available = None;
    status.waits = None;
    status.installed = Some(version.clone());
    status.installed_at = now_secs();
    status.rejected.retain(|v| *v != version);
    *probation = Some(Probation {
        version: version.clone(),
        since: now_secs(),
        starts: BTreeMap::new(),
    });
    Ok(Outcome::Installed(version))
}

/// Whether every server of this user's roots here speaks `protocol`; why not if not.
fn servers_speak(protocol: u32) -> std::result::Result<(), String> {
    let reg = crate::registry::load().map_err(|e| format!("{e:#}"))?;
    let mut asked = 0;
    for entry in &reg.roots {
        if crate::root::read_root_id(&entry.path).ok().as_ref() != Some(&entry.root_id) {
            continue;
        }
        let config = crate::root::read_config(&entry.path).map_err(|e| format!("{e:#}"))?;
        match crate::client::Client::server_protocol(&config) {
            Ok(p) if p == protocol => asked += 1,
            Ok(p) => return Err(format!("{} speaks protocol {p}", config.server)),
            Err(e) => return Err(format!("{} cannot be asked: {e:#}", config.server)),
        }
    }
    if asked == 0 {
        return Err("no root of this user is here to ask its server".into());
    }
    Ok(())
}

// --- Installing ---

fn staging_of(target: &Path) -> Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("{} is in no folder", target.display()))?;
    let dir = parent.join(STAGING);
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
            "{} cannot be written by this user, so nothing can be installed there: its owner can, with `cairn update now`",
            parent.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("make {}", dir.display())),
    }
    Ok(dir)
}

/// Where the one before is kept.
fn previous_of(staging: &Path, p: &Program) -> PathBuf {
    match p.form {
        Form::Binary => staging.join(format!(
            "{}-previous{}",
            p.name,
            std::env::consts::EXE_SUFFIX
        )),
        Form::App => staging.join(format!("{}-previous.app", p.name)),
    }
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

struct Sha256Writer<'a> {
    file: &'a mut std::fs::File,
    hash: ring::digest::Context,
}

impl Write for Sha256Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        check("update.write")?;
        let n = self.file.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Download one file of the release next to what it replaces and check it against the release.
fn download(file: &ReleaseFile, url: &str, part: &Path) -> Result<()> {
    if file.size > MAX_FILE {
        bail!(
            "{url} is said to be {} bytes, more than a program may be",
            file.size
        );
    }
    remove_any(part)?;
    let result = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(part)
            .with_context(|| format!("make {}", part.display()))?;
        let mut w = Sha256Writer {
            file: &mut f,
            hash: ring::digest::Context::new(&ring::digest::SHA256),
        };
        let got = fetch(url, file.size, &mut w).map_err(|e| {
            if crate::is_disk_full(&e) {
                e.context(format!(
                    "the disk holding {} is full",
                    part.parent().unwrap_or(part).display()
                ))
            } else {
                e
            }
        })?;
        let hash = hex::encode(w.hash.finish().as_ref());
        if got != file.size {
            bail!("{url} was {got} bytes, and the release says {}", file.size);
        }
        if !hash.eq_ignore_ascii_case(&file.sha256) {
            bail!(
                "{url} has SHA-256 {hash}, and the release says {}",
                file.sha256
            );
        }
        f.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(part);
    }
    result
}

/// Put one program of the release in place of `p`.
fn install(p: &Program, file: &ReleaseFile, url: &str, version: &str) -> Result<()> {
    let staging = staging_of(&p.path)?;
    let part = staging.join(format!("{}-{version}.part", p.name));
    download(file, url, &part)?;
    hit("update.downloaded");
    let (new_path, new_exe) = match p.form {
        Form::Binary => {
            let ready = staging.join(format!(
                "{}-{version}{}",
                p.name,
                std::env::consts::EXE_SUFFIX
            ));
            remove_any(&ready)?;
            std::fs::rename(&part, &ready)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&ready, std::fs::Permissions::from_mode(0o755))?;
            }
            (ready.clone(), ready)
        }
        Form::App => {
            let app = unpack_app(&part, &staging.join(format!("{}-{version}", p.name)))?;
            let _ = std::fs::remove_file(&part);
            let exe = app.join(p.exe.strip_prefix(&p.path).unwrap_or(Path::new("")));
            (app, exe)
        }
    };
    hit("update.unpacked");
    check_signature(&new_path, &p.path)?;
    let said = version_of(&new_exe, p.name)?;
    if Version::parse(&said) != Version::parse(version) {
        bail!(
            "the new {} says it is {said}, and the release says {version}",
            p.name
        );
    }
    hit("update.checked");
    put_in_place(p, &new_path, &staging)?;
    hit("update.placed");
    if p.form == Form::App {
        register_app(&p.path);
        let _ = remove_any(&staging.join(format!("{}-{version}", p.name)));
    }
    Ok(())
}

/// A gzipped tar holding one `<Name>.app`, unpacked into `into`: the bundle's path.
fn unpack_app(archive: &Path, into: &Path) -> Result<PathBuf> {
    remove_any(into)?;
    std::fs::create_dir(into)?;
    let out = output_within(
        Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(into),
        Duration::from_secs(300),
    )
    .context("run tar")?;
    if !out.status.success() {
        bail!(
            "tar could not unpack {}: {}",
            archive.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let entries: Vec<_> = std::fs::read_dir(into)?.collect::<std::io::Result<_>>()?;
    match entries.as_slice() {
        [one]
            if one.path().extension().is_some_and(|e| e == "app") && one.file_type()?.is_dir() =>
        {
            Ok(one.path())
        }
        _ => bail!(
            "{} holds {} entries, not one app bundle",
            archive.display(),
            entries.len()
        ),
    }
}

/// On macOS: the new program's signature holds, and is by the team of the one it replaces, if that one
/// has a team. Elsewhere the release's own signature is the check.
#[cfg(target_os = "macos")]
fn check_signature(new: &Path, current: &Path) -> Result<()> {
    let out = output_within(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict", "--deep"])
            .arg(new),
        Duration::from_secs(300),
    )?;
    if !out.status.success() {
        bail!(
            "the new {} is not validly signed: {}",
            new.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let team = |p: &Path| -> Option<String> {
        let out = output_within(
            Command::new("/usr/bin/codesign")
                .args(["-dv", "--verbose=2"])
                .arg(p),
            Duration::from_secs(60),
        )
        .ok()?;
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|l| l.strip_prefix("TeamIdentifier="))
            .map(str::to_string)
            .filter(|t| t != "not set")
    };
    if let Some(have) = team(current) {
        let got = team(new);
        if got.as_deref() != Some(have.as_str()) {
            bail!(
                "the new {} is signed by team {}, and the one installed by {have}: macOS would take it for another program and ask again for every permission",
                new.display(),
                got.as_deref().unwrap_or("none")
            );
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn check_signature(_: &Path, _: &Path) -> Result<()> {
    Ok(())
}

/// Tell Launch Services about the bundle now at this path, as Finder would on a copy.
fn register_app(path: &Path) {
    if cfg!(target_os = "macos") {
        let _ = output_within(
            Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
                .arg("-f")
                .arg(path),
            Duration::from_secs(60),
        );
    }
}

/// Swap two entries, if this filesystem can.
#[cfg(unix)]
fn exchange(a: &Path, b: &Path) -> Result<bool> {
    if cairn_failpoint::unsupported("update.exchange") {
        return Ok(false);
    }
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    match renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP) => {
            Ok(false)
        }
        Err(e) => Err(std::io::Error::from(e)).context("swap"),
    }
}

#[cfg(not(unix))]
fn exchange(_: &Path, _: &Path) -> Result<bool> {
    Ok(false)
}

fn sync_dir(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = fsops::sync_dir_path(dir);
    }
}

/// The new program, whole and checked at `new`, in place of `p`, and the one it replaces kept as the
/// previous. Every step leaves a whole program at `p.path`, but on Windows between two renames.
fn put_in_place(p: &Program, new: &Path, staging: &Path) -> Result<()> {
    let previous = previous_of(staging, p);
    let target = &p.path;
    // What an earlier update set aside, still running then, on Windows: gone now, or next time.
    let aside = format!("{}-old-", p.name);
    if let Ok(entries) = std::fs::read_dir(staging) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&aside) {
                let _ = remove_any(&e.path());
            }
        }
    }
    if let Err(e) = remove_any(&previous) {
        // A program still running from it, on Windows: out of the way under another name, to go later.
        let aside = staging.join(format!("{}-old-{}", p.name, now_secs()));
        std::fs::rename(&previous, &aside).with_context(|| {
            format!(
                "cannot remove {} ({e}) nor move it aside",
                previous.display()
            )
        })?;
    }
    match p.form {
        Form::Binary if cfg!(unix) => {
            // A second name for the old one, then the new over it: one rename.
            if std::fs::hard_link(target, &previous).is_err() {
                std::fs::copy(target, &previous)
                    .with_context(|| format!("keep {}", target.display()))?;
            }
            hit("update.kept");
            std::fs::rename(new, target)
                .with_context(|| format!("put the new one at {}", target.display()))?;
        }
        _ => {
            if p.form == Form::App && exchange(new, target)? {
                hit("update.kept");
                // The old bundle is where the new one was.
                std::fs::rename(new, &previous)
                    .with_context(|| format!("keep the old one as {}", previous.display()))?;
            } else {
                std::fs::rename(target, &previous)
                    .with_context(|| format!("move {} aside", target.display()))?;
                hit("update.kept");
                if let Err(e) = std::fs::rename(new, target) {
                    // Back where it was, so a program is there.
                    let back = std::fs::rename(&previous, target);
                    return Err(anyhow!(e)).context(match back {
                        Ok(()) => format!("put the new one at {}", target.display()),
                        Err(b) => format!(
                            "put the new one at {}, nor the old one back ({b}): it is {}",
                            target.display(),
                            previous.display()
                        ),
                    });
                }
            }
        }
    }
    sync_dir(target);
    sync_dir(&previous);
    Ok(())
}

/// Put back the version before the last update, and never install the one taken back by itself again.
/// The version now in place.
pub fn rollback(why: &str, log: &mut dyn FnMut(&str)) -> Result<String> {
    let _lock = take_lock("update.lock", ASKED_WAIT)?
        .ok_or_else(|| anyhow!("another update is running here"))?;
    let mut back = Vec::new();
    let mut taken = None;
    for p in programs()? {
        let staging = p
            .path
            .parent()
            .map(|d| d.join(STAGING))
            .ok_or_else(|| anyhow!("{} is in no folder", p.path.display()))?;
        let previous = previous_of(&staging, &p);
        if std::fs::symlink_metadata(&previous).is_err() {
            continue;
        }
        let prev_exe = match p.form {
            Form::Binary => previous.clone(),
            Form::App => previous.join(p.exe.strip_prefix(&p.path).unwrap_or(Path::new(""))),
        };
        let was = installed_version(&p).ok();
        let to = version_of(&prev_exe, p.name)
            .with_context(|| format!("the {} kept from before does not run", p.name))?;
        hit("update.rollback");
        match p.form {
            Form::Binary if cfg!(unix) => std::fs::rename(&previous, &p.path)?,
            _ if p.form == Form::App && exchange(&previous, &p.path)? => {
                let _ = remove_any(&previous);
            }
            _ => {
                let aside = staging.join(format!("{}-old-{}", p.name, now_secs()));
                std::fs::rename(&p.path, &aside)?;
                std::fs::rename(&previous, &p.path)?;
            }
        }
        sync_dir(&p.path);
        if p.form == Form::App {
            register_app(&p.path);
        }
        log(&format!(
            "{} {} -> {to}: {}",
            p.name,
            was.as_deref().unwrap_or("?"),
            p.path.display()
        ));
        if p.name == "cairn" {
            taken = was;
        }
        back.push(to);
    }
    let Some(now) = back.first().cloned() else {
        bail!("nothing kept from before an update here to go back to");
    };
    with_state(|s| {
        if let Some(v) = taken.clone().or_else(|| s.status.installed.clone()) {
            if !s.status.rejected.contains(&v) {
                s.status.rejected.push(v);
            }
        }
        s.status.installed = Some(now.clone());
        s.status.installed_at = now_secs();
        s.status.available = None;
        s.status.waits = Some(format!("taken back to {now}: {why}"));
        s.probation = None;
    })?;
    Ok(now)
}

// --- A daemon's part ---

/// A daemon starting for `root`. On a version just installed that has not yet been seen to work, one
/// more start without a sync: past [`MAX_STARTS`] the version is taken back, and the line for the log
/// says so, for the daemon to restart into the one before.
pub fn daemon_starting(root: &Path, log: &mut dyn FnMut(&str)) -> Option<String> {
    let me = running_version();
    let key = root.to_string_lossy().into_owned();
    let starts = with_state(|s| match &mut s.probation {
        Some(p) if p.version == me => {
            let n = p.starts.entry(key.clone()).or_insert(0);
            *n += 1;
            Some(*n)
        }
        _ => None,
    });
    let starts = match starts {
        Ok(n) => n?,
        Err(e) => {
            log(&format!(
                "cannot count this start on {me}, a new version: {e:#}"
            ));
            return None;
        }
    };
    if starts <= MAX_STARTS {
        return None;
    }
    let why = format!(
        "the daemon for {} started {} times on {me} without getting through a sync",
        root.display(),
        starts - 1
    );
    match rollback(&why, log) {
        Ok(to) => Some(format!("{why}: taken back to {to}, which it restarts into")),
        Err(e) => {
            log(&format!("{why}, and it cannot be taken back: {e:#}"));
            None
        }
    }
}

/// The version this daemon runs has got through a sync, or run long enough: it works.
pub fn proven() {
    let me = running_version();
    let _ = with_state(|s| {
        if s.probation.as_ref().is_some_and(|p| p.version == me) {
            s.probation = None;
        }
    });
}

/// A daemon's updater: a thread that looks every few seconds whether this program has been replaced,
/// and asks the feed every hour or so, or at once when a person asks.
pub struct Worker {
    nudge: Mutex<Sender<()>>,
    shared: Arc<Shared>,
    started: Instant,
    proven: AtomicBool,
}

#[derive(Default)]
struct Shared {
    status: Mutex<Option<Status>>,
    replaced: AtomicBool,
    notes: Mutex<Vec<String>>,
}

impl Worker {
    /// `wake` brings the daemon round to [`Worker::replaced`], [`Worker::status`] and
    /// [`Worker::take_notes`].
    pub fn start(wake: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = channel::<()>();
        let shared = Arc::new(Shared::default());
        *shared.status.lock().unwrap_or_else(|e| e.into_inner()) = status().ok();
        let s = shared.clone();
        std::thread::spawn(move || {
            let tick = Duration::from_millis(tuned("update.tick_ms", TICK_MS));
            let every = tuned("update.every_secs", EVERY_SECS);
            let jittered = |secs: u64| {
                let spread = secs / 10;
                Duration::from_secs(secs - spread + rand::random_range(0..=spread * 2))
            };
            let mut next = Instant::now() + jittered(tuned("update.first_secs", FIRST_SECS));
            let mut said_error: Option<String> = None;
            let mut said_unanswered: Option<String> = None;
            let mut said_found: Option<String> = None;
            let note = |line: String| {
                s.notes.lock().unwrap_or_else(|e| e.into_inner()).push(line);
            };
            let show = |st: Status| {
                let mut cur = s.status.lock().unwrap_or_else(|e| e.into_inner());
                if cur.as_ref() != Some(&st) {
                    *cur = Some(st);
                    true
                } else {
                    false
                }
            };
            loop {
                let asked = match rx.recv_timeout(tick) {
                    Ok(()) => true,
                    Err(RecvTimeoutError::Timeout) => false,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                if s.replaced.load(Ordering::SeqCst) {
                    continue;
                }
                if replaced() {
                    s.replaced.store(true, Ordering::SeqCst);
                    wake();
                    continue;
                }
                let mut changed = false;
                if asked || Instant::now() >= next {
                    next = Instant::now() + jittered(every);
                    let policy = settings().map(|s| s.policy).unwrap_or_default();
                    // Another daemon of this user here asked lately, and there was nothing to do.
                    let lately = status().is_ok_and(|st| {
                        st.error.is_none()
                            && st.available.is_none()
                            && now_secs().saturating_sub(st.checked) < (every * 9 / 10) as i64
                    });
                    if asked || (policy != Policy::Off && !lately) {
                        let mut lines = Vec::new();
                        let result = run(
                            &Options {
                                asked,
                                install: true,
                                ..Options::default()
                            },
                            &mut |l| lines.push(l.to_string()),
                        );
                        for l in lines {
                            note(format!("update: {l}"));
                        }
                        match result {
                            Ok((outcome, st)) => {
                                said_error = None;
                                // A feed passed over for the next is said when it starts and when it
                                // ends, not at every check.
                                if st.unanswered != said_unanswered {
                                    let by = st.answered.as_deref().unwrap_or("another");
                                    note(match &st.unanswered {
                                        Some(why) => {
                                            format!("update: {why}; {by} answered instead")
                                        }
                                        None => format!("update: {by} answers again"),
                                    });
                                    said_unanswered = st.unanswered.clone();
                                }
                                match &outcome {
                                    Outcome::Installed(v) => note(format!(
                                        "update: installed {v}; restarting into it between syncs"
                                    )),
                                    Outcome::Waits(v) if said_found.as_ref() != Some(v) => {
                                        note(format!(
                                            "update: {v} is out; {}",
                                            st.waits.as_deref().unwrap_or("not installed")
                                        ));
                                        said_found = Some(v.clone());
                                    }
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                let text = format!("{e:#}");
                                if said_error.as_ref() != Some(&text) {
                                    note(format!("update: {text}"));
                                    said_error = Some(text);
                                }
                            }
                        }
                        changed = true;
                    }
                }
                // What any updater here wrote, this one's or another's.
                if let Ok(st) = status() {
                    changed |= show(st);
                }
                if changed {
                    wake();
                }
            }
        });
        Self {
            nudge: Mutex::new(tx),
            shared,
            started: Instant::now(),
            proven: AtomicBool::new(false),
        }
    }

    /// A person asked to update now.
    pub fn nudge(&self) {
        let _ = self
            .nudge
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(());
    }

    /// This program has been replaced: the daemon restarts into it between syncs.
    pub fn replaced(&self) -> bool {
        self.shared.replaced.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> Option<Status> {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.shared.notes.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// After each sync, whatever its outcome: the first shows the version works.
    pub fn synced(&self) {
        if !self.proven.swap(true, Ordering::SeqCst) {
            proven();
        }
    }

    /// While waiting: a daemon that has run an hour on the version is not being killed by it.
    pub fn waited(&self) {
        if self.started.elapsed() >= PROVEN_AFTER {
            self.synced();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_of_the_release_is_found_beside_its_feed() {
        let feed = "https://example.org/r/latest/download/update.json";
        assert_eq!(
            resolve(feed, "cairn-linux"),
            "https://example.org/r/latest/download/cairn-linux"
        );
        assert_eq!(resolve(feed, "https://x/y"), "https://x/y");
        assert_eq!(resolve("/srv/dist/update.json", "a"), "/srv/dist/a");
        let abs = if cfg!(windows) { r"C:\abs\a" } else { "/abs/a" };
        assert_eq!(resolve("/srv/dist/update.json", abs), abs);
    }

    #[test]
    fn only_a_key_it_trusts_makes_a_release() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        use ring::signature::KeyPair;
        let b64 = base64::engine::general_purpose::STANDARD;
        let key = b64.encode(pair.public_key().as_ref());
        let release = r#"{"version":"9.9.9","protocol":1,"published":0,"files":[]}"#.to_string();
        let mut msg = SIGNED_PREFIX.to_vec();
        msg.extend_from_slice(release.as_bytes());
        let signed = |release: &str, sig: &[u8]| {
            serde_json::to_vec(&Signed {
                release: release.into(),
                signatures: vec![cairn_protocol::update::Signature {
                    key: key.clone(),
                    signature: b64.encode(sig),
                }],
            })
            .unwrap()
        };
        let good = signed(&release, pair.sign(&msg).as_ref());
        assert_eq!(
            verify(&good, std::slice::from_ref(&key)).unwrap().version,
            "9.9.9"
        );
        // Another key trusted, not this one.
        let other = b64.encode([7u8; 32]);
        assert!(verify(&good, &[other]).is_err());
        // The text changed after signing.
        let changed = signed(
            &release.replace("9.9.9", "9.9.10"),
            pair.sign(&msg).as_ref(),
        );
        assert!(verify(&changed, std::slice::from_ref(&key)).is_err());
        // Signed without the prefix: a signature made for something else.
        let bare = signed(&release, pair.sign(release.as_bytes()).as_ref());
        assert!(verify(&bare, &[key]).is_err());
    }
}
