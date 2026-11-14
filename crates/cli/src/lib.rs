//! The `cairn` command-line client.

use anyhow::{anyhow, bail, Context, Result};
use cairn_engine::client::{Client, Retries};
use cairn_engine::daemon::DaemonOptions;
use cairn_engine::root::{open_config, Busy, Paused};
use cairn_engine::scan::display_path;
use cairn_engine::state::Compaction;
use cairn_engine::{control, deleted, registry, restore, selection};
use cairn_engine::{
    init_root, sync, ApplyPolicy, InitOptions, OwnerPolicy, Root, SyncOptions, SyncOutcome,
};
use cairn_protocol::local::{self, Reply, Request, RootEntry, RootView};
use cairn_protocol::{Name, ObjectKind, RevisionId};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(windows)]
mod explorer;
#[cfg(target_os = "macos")]
mod macos;

macro_rules! errln {
    ($($t:tt)*) => {{
        let _ = writeln!(std::io::stderr(), $($t)*);
    }};
}

/// Set once the output cannot be written. Whoever reads it going away, as `cairn status | head`
/// does, is no reason to panic or to stop what the command is doing: the rest goes unprinted.
static STDOUT_GONE: AtomicBool = AtomicBool::new(false);

fn out(args: std::fmt::Arguments) {
    if STDOUT_GONE.load(Ordering::Relaxed) {
        return;
    }
    let mut o = std::io::stdout().lock();
    if let Err(e) = o.write_fmt(args).and_then(|()| o.write_all(b"\n")) {
        STDOUT_GONE.store(true, Ordering::Relaxed);
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            errln!("cairn: the output cannot be written, so the rest is not shown: {e}");
        }
    }
}

macro_rules! outln {
    ($($t:tt)*) => { out(format_args!($($t)*)) };
}

/// The tree synced, and anything excluded was listed.
const EXIT_OK: u8 = 0;
const EXIT_ERROR: u8 = 1;
/// Sync paused itself and is waiting for a person; nothing was changed.
const EXIT_PAUSED: u8 = 2;
/// Not fully in agreement yet; another run may finish. Nothing was lost.
const EXIT_PARTIAL: u8 = 3;

#[derive(Parser)]
#[command(
    name = "cairn",
    version,
    about = "Sync a real filesystem with a Cairn server, losing nothing about it"
)]
struct Cli {
    /// The sync root. Default: the nearest folder upward holding a .cairn directory.
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum OwnersArg {
    None,
    Name,
    Number,
}

#[derive(Subcommand)]
enum Command {
    /// Make a folder a sync root. Files already in it are merged with the server on the first sync.
    Init {
        /// The folder to make a root. Default: --root, or else the current folder.
        dir: Option<PathBuf>,
        #[arg(long)]
        server: String,
        /// A file holding the access token. Or set CAIRN_TOKEN.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// The folder on the server to sync here, from those `cairn folders` lists. Default: the one
        /// the token reaches, if it reaches one alone.
        #[arg(long, visible_alias = "folder")]
        namespace: Option<String>,
        /// This device's name in history and in conflict file names. Default: the hostname.
        #[arg(long)]
        device_name: Option<String>,
        /// Apply stored owners to files here. Needs privileges.
        #[arg(long, value_enum, default_value = "none")]
        owners: OwnersArg,
        /// Apply setuid and setgid.
        #[arg(long)]
        setid: bool,
        /// Apply Linux capabilities.
        #[arg(long)]
        capabilities: bool,
        /// Do not apply permission bits here. They stay stored.
        #[arg(long)]
        no_mode: bool,
        /// Allow plain HTTP beyond loopback, which sends the token in the clear.
        #[arg(long)]
        allow_http: bool,
    },
    /// Sync once: scan, merge, apply, push, until both sides agree.
    Sync {
        /// Go ahead even though most of the tree has vanished.
        #[arg(long)]
        allow_mass_delete: bool,
        /// Say what would happen and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Keep this root in sync: watch it and the server, and sync whenever either changes.
    Daemon {
        /// Seconds between syncs when nothing asks for one, each scanning the whole tree: the safety net
        /// for missed events.
        #[arg(long, default_value_t = 3600)]
        reconcile_secs: u64,
        /// Seconds between checks of the ledger that syncs taking what changed alone rest on, against
        /// what the state says: anything it vouched for wrongly is taken up then, and said as a defect.
        #[arg(long, default_value_t = 86_400)]
        full_sync_secs: u64,
        /// The longest wait in seconds before trying again after a failure, or while paused.
        #[arg(long, default_value_t = 60)]
        retry_secs: u64,
        /// Do not watch the filesystem, where its events do not work: only full syncs find changes here.
        #[arg(long)]
        no_watch: bool,
        /// Do not wait on the server: only full syncs find changes there.
        #[arg(long)]
        no_feed: bool,
    },
    /// Stop syncing a folder on the server. It stays excluded however it is renamed.
    Exclude {
        /// Relative to the root.
        path: PathBuf,
        /// Also remove the copy here, except what differs from the server. By default it stays,
        /// no longer looked at.
        #[arg(long)]
        remove_local: bool,
    },
    /// Sync an excluded folder again, or a copy of one left here.
    Include {
        /// Relative to the root.
        path: PathBuf,
        /// A copy left at that path merges as new: what differs goes to the server, and what changed
        /// keeps both versions. Without this, such a copy stops the command.
        #[arg(long)]
        merge: bool,
    },
    /// What a sync would do now, and what this root's daemon says.
    Status {
        /// Every root of this user on this device, as listed when each was made or its daemon
        /// started: what each daemon says, and which roots have gone.
        #[arg(long)]
        all: bool,
        /// As JSON, the local API's types.
        #[arg(long)]
        json: bool,
    },
    /// Take a root that has gone off this user's list of roots, which `cairn status --all` and the
    /// tray show. Nothing in any root changes.
    Forget {
        /// The root's path as `cairn status --all` lists it, or its ID.
        path: PathBuf,
        /// Take it off even though it is still there. It syncs as before, and its daemon lists it
        /// again when it starts.
        #[arg(long)]
        even_if_present: bool,
    },
    /// Pause this root's daemon: it starts no sync until `cairn resume`, through restarts too. Changes
    /// here and on the server wait, and none is lost.
    Pause,
    /// Resume this root's daemon, which then syncs what changed while it was paused.
    Resume,
    /// Send one request of the local API to this root's daemon and print what it answers, one JSON
    /// object per line: what the tray sees.
    Ask {
        /// status, subscribe, sync_now, pause, resume, conflicts, deleted, update_now or sign_in_link;
        /// or a whole request as JSON, as `{"op":"undelete","ids":["..."]}`.
        request: String,
    },
    /// The folders on a server a token reaches, and what it may do in each: read, write, or manage,
    /// which is write and also saying who else has access. Each is synced as a root of its own.
    Folders {
        /// The server. Default: this root's.
        #[arg(long)]
        server: Option<String>,
        /// A file holding the token. Or set CAIRN_TOKEN. Default: this root's.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Allow plain HTTP beyond loopback, which sends the token in the clear.
        #[arg(long)]
        allow_http: bool,
    },
    /// Who has access to this root's folder on the server. A manager of it may also change that.
    Access {
        #[command(subcommand)]
        what: Option<AccessCommand>,
    },
    /// Open this root's folder on the server's web page in a browser, signed in as this root's token,
    /// so nobody types one. The address it opens signs in once, within two minutes.
    Web {
        /// Print the address instead of opening it.
        #[arg(long)]
        print: bool,
    },
    /// The change journal, newest last.
    Log {
        /// Only the last N entries.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// The revisions of a file or directory.
    History {
        /// Relative to the root.
        path: PathBuf,
        /// Look the path up as of this sequence number, to find something deleted or renamed.
        #[arg(long)]
        at: Option<u64>,
    },
    /// Bring a path back to an earlier state, as a new revision. Then syncs, unless --no-sync.
    Restore {
        /// Relative to the root; `.` for all of it.
        path: PathBuf,
        /// The sequence number to go back to, for a file or a whole directory, deleted ones too.
        #[arg(long, conflicts_with = "revision")]
        at: Option<u64>,
        /// One revision of one object, from `cairn history`.
        #[arg(long)]
        revision: Option<String>,
        #[arg(long)]
        no_sync: bool,
    },
    /// Write a directory as of a sequence number into a new folder. Changes nothing else.
    Export {
        dest: PathBuf,
        /// Default: the latest.
        #[arg(long)]
        at: Option<u64>,
        /// The directory to export, relative to the root. Default: all of it.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Apply everything: owners by number, setuid and setgid, capabilities, all xattrs but
        /// security.selinux. Needs privileges; what cannot be applied is listed.
        #[arg(long)]
        exact: bool,
    },
    /// The conflict inbox: competing versions kept as copies, until the copy is deleted, renamed or
    /// moved, which is how a person deals with one.
    Conflicts {
        /// Also those dealt with.
        #[arg(long)]
        all: bool,
    },
    /// What was deleted and is still deleted, newest first, from the server's history: a folder once,
    /// with what was deleted with it. Each line ends with the ID `cairn undelete` takes.
    Deleted {
        /// Only what was deleted in the last N days.
        #[arg(long, default_value_t = cairn_protocol::local::DEFAULT_DELETED_DAYS)]
        days: u32,
        /// One JSON object per line, as the local API gives them to the tray.
        #[arg(long)]
        json: bool,
    },
    /// Bring back what `cairn deleted` lists, by its ID, as it was when deleted: a folder with what was
    /// deleted with it. Nothing is replaced: where a name has been taken since, it comes back beside it
    /// under a restored name. Then syncs, unless --no-sync.
    Undelete {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long)]
        no_sync: bool,
    },
    /// This device's Cairn and its updates: by default each daemon asks the feed every hour or so and
    /// installs a new release, `cairn` and the tray beside it, restarting into it between syncs.
    /// Badges on each entry's icon in Windows Explorer, as the daemon of its root says: synced,
    /// syncing, something not synced inside (a warning), or not synced (an error).
    #[cfg(windows)]
    Explorer {
        #[command(subcommand)]
        what: ExplorerCommand,
    },
    Update {
        #[command(subcommand)]
        what: Option<UpdateCommand>,
    },
}

#[derive(Subcommand)]
enum AccessCommand {
    /// Who has access, at which level: the default.
    List,
    /// Give a user of the server access to this folder at a level, replacing the one it had.
    Grant {
        user: String,
        /// read, write, or manage.
        access: cairn_protocol::wire::Access,
    },
    /// Take all of a user's access to this folder away. Its devices keep what they hold.
    Revoke { user: String },
}

#[derive(Subcommand)]
enum UpdateCommand {
    /// What is installed here, how updates are set, and what the feed offered when last asked. The
    /// default.
    Status {
        /// As JSON, the protocol crate's type.
        #[arg(long)]
        json: bool,
    },
    /// Ask the feed now and say what it offers. Installs nothing.
    Check {
        #[command(flatten)]
        feed: FeedArgs,
    },
    /// Ask the feed and install what it offers now, whatever the setting, a version taken back too.
    /// Then wait for each running daemon here to restart on it; the tray restarts by itself.
    Now {
        #[command(flatten)]
        feed: FeedArgs,
        /// Install a release on another protocol even though not every server here is known to speak
        /// it, which their clients then refuse until they do.
        #[arg(long)]
        any_protocol: bool,
        /// Do not wait for the daemons.
        #[arg(long)]
        no_wait: bool,
    },
    /// Install each new release as it comes, and restart into it: the default.
    Auto,
    /// Only say that a new release is there, here and in the tray; `cairn update now` installs it.
    Notify,
    /// Never ask the feed. `cairn update now` still installs.
    Off,
    /// Put back the version from before the last update, which is then not installed again unless
    /// asked for by `cairn update now`.
    Rollback,
}

#[cfg(windows)]
#[derive(Subcommand)]
enum ExplorerCommand {
    /// Put the handler and its icons in place and register them, which takes an administrator once.
    /// Explorer shows the badges once it starts again.
    On {
        /// A DLL to use instead of the one this build carries, as `cargo build --profile explorer -p
        /// cairn-explorer` makes it.
        #[arg(long)]
        dll: Option<PathBuf>,
    },
    /// Take them away again.
    Off,
    /// Whether they are on, from where, and where they come among Explorer's overlay handlers.
    Status,
    /// What `on` and `off` run as administrator: Explorer's list of overlay handlers alone.
    #[command(hide = true)]
    Handlers {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// What Windows' overlay manager gives each path, as Explorer would show it; with --dll, what that
    /// DLL's handlers say, loaded here without the registry.
    #[command(hide = true)]
    Probe {
        #[arg(long)]
        dll: Option<PathBuf>,
        /// With --dll, ask again this many seconds later, in the same process, as the handler learns
        /// of changes from the daemon.
        #[arg(long)]
        again: Option<u64>,
        paths: Vec<PathBuf>,
    },
}

#[derive(clap::Args)]
struct FeedArgs {
    /// Another feed, for this run only: the URL or path of an update.json, to try a release first.
    /// Several, comma-separated, are asked in order until one answers.
    #[arg(long)]
    feed: Option<String>,
    /// A key that feed is signed with, base64, in place of the build's; may be given more than once.
    #[arg(long = "key")]
    keys: Vec<String>,
}

/// A web address in this user's browser. Only where a desktop is: without one, the address is printed,
/// rather than handed to a browser that would take over the terminal.
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("/usr/bin/open");
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("there is no desktop here to open a browser on".into());
        }
        std::process::Command::new("xdg-open")
    };
    let status = cmd
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("cannot open a browser: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the browser did not open ({status})"))
    }
}

fn find_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let cwd = std::env::current_dir()?;
    let mut at: &Path = &cwd;
    loop {
        if at.join(".cairn").join("root-id").exists() {
            return Ok(at.to_path_buf());
        }
        match at.parent() {
            Some(up) => at = up,
            // Nothing found: Root::open explains it for this folder.
            None => return Ok(cwd),
        }
    }
}

fn to_names(path: &Path) -> Result<Vec<Name>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => {
                let bytes = cairn_engine::fsops::os_name_bytes(n)
                    .ok_or_else(|| anyhow!("{} is not valid Unicode", path.display()))?;
                out.push(Name::new(bytes)?)
            }
            Component::CurDir => {}
            _ => bail!(
                "{} must be a plain path relative to the root",
                path.display()
            ),
        }
    }
    Ok(out)
}

fn print_sync(report: &cairn_engine::SyncReport) -> u8 {
    for line in &report.planned {
        outln!("{line}");
    }
    if let Some(f) = &report.followed {
        outln!("{f}");
    }
    for e in &report.events {
        outln!("event: {e}");
    }
    for s in &report.skips {
        outln!("skipped: {s}");
    }
    for w in &report.warnings {
        outln!("not applied here: {w}");
    }
    if let Some(why) = &report.whole_because {
        outln!("looked at the whole tree: {why}");
    } else if let (Some((_, took, of)), Some(why)) = (report.part, &report.rebuilt_because) {
        outln!("looked at {took} of {of} entries, the ledger rebuilt from the state: {why}");
    } else if let Some((_, took, of)) = report.part {
        outln!("looked at {took} of {of} entries: the rest are as the last agreement left them");
    }
    if !report.excluded_here.is_empty() || !report.not_synced_here.is_empty() {
        outln!(
            "by this device's choice: {} entries here not synced, {} on the server not here; `cairn status` lists them",
            report.excluded_here.len(),
            report.not_synced_here.len()
        );
    }
    if report.retries > 0 {
        outln!(
            "tried requests again after the network failed them: {} time(s)",
            report.retries
        );
    }
    if report.read_only {
        let held = report
            .skips
            .iter()
            .filter(|s| s.kind == cairn_protocol::SkipKind::ReadOnly)
            .count();
        if held > 0 {
            outln!(
                "this folder is read-only for you: {held} change(s) here stay here, not uploaded"
            );
        }
    }
    match &report.outcome {
        SyncOutcome::Synced => {
            outln!(
                "synced at sequence {}: {} steps applied here, {} commits pushed, {} chunks ({} bytes) uploaded, {} conflict(s)",
                report.head, report.steps_applied, report.commits, report.chunks_uploaded, report.bytes_uploaded, report.conflicts
            );
            EXIT_OK
        }
        SyncOutcome::Partial => {
            outln!("not fully synced yet: nothing was lost; see above, and sync again");
            EXIT_PARTIAL
        }
        SyncOutcome::Paused(why) => {
            outln!("paused: {why}");
            EXIT_PAUSED
        }
    }
}

/// This root as the local API shows it, whether or not it is in the list of roots.
fn root_view(root_path: &Path) -> Result<RootView> {
    let root_id = cairn_engine::root::read_root_id(root_path)
        .with_context(|| format!("{} is not a sync root", root_path.display()))?;
    let path = std::path::absolute(root_path)?;
    let listed = registry::load()
        .ok()
        .and_then(|r| r.roots.into_iter().find(|e| e.root_id == root_id));
    let entry = match listed {
        Some(e) => e,
        None => RootEntry {
            path,
            server: open_config(root_path).map(|c| c.server).unwrap_or_default(),
            root_id,
            added: 0,
        },
    };
    let mut view = registry::view(entry);
    // Listed somewhere else, but this is the folder asked about.
    if !view.present {
        view.root.path = root_path.to_path_buf();
        view.present = true;
        view.daemon = control::status_of(root_path, &view.root.root_id);
    }
    Ok(view)
}

fn print_view(v: &RootView) {
    if !v.present {
        outln!(
            "{}: gone: no root with ID {} is there now{}",
            v.root.path.display(),
            v.root.root_id,
            if v.root.added > 0 {
                format!("; listed since {}", date_time(v.root.added))
            } else {
                String::new()
            }
        );
        return;
    }
    outln!("{} (server {})", v.root.path.display(), v.root.server);
    match &v.daemon {
        Some(s) => print_daemon(s),
        None => outln!("daemon: not running"),
    }
}

/// How many of what the daemon's last sync skipped `cairn status` lists.
const MAX_SKIPS_SHOWN: usize = 20;

fn print_daemon(s: &cairn_engine::daemon::Status) {
    let state = format!(
        "{}{}",
        if s.state.is_empty() {
            "starting"
        } else {
            &s.state
        },
        s.detail
            .as_ref()
            .map_or(String::new(), |d| format!(" ({d})"))
    );
    outln!(
        "daemon: running as process {}, {}; last sync {}, last fully synced {}; {} folders watched",
        s.pid,
        match (s.syncing, s.state.is_empty()) {
            (false, _) => state,
            (true, true) => "syncing now, its first sync".to_string(),
            (true, false) => format!("syncing now, after a sync that ended {state}"),
        },
        if s.last_sync == 0 {
            "none yet".to_string()
        } else {
            date_time(s.last_sync)
        },
        if s.last_synced == 0 {
            "never".to_string()
        } else {
            date_time(s.last_synced)
        },
        s.watched,
    );
    match &s.progress {
        Some(p) if s.syncing => outln!("daemon: syncing {}", p.describe(now())),
        _ if s.syncing => outln!("daemon: a sync is running now"),
        _ => {}
    }
    if s.unwatched > 0 {
        outln!(
            "daemon: {} folders are not watched, so changes in them wait for a full sync{}",
            s.unwatched,
            if s.watch_limit_reached {
                ": raise fs.inotify.max_user_watches"
            } else {
                ""
            }
        );
    }
    if let Some(why) = &s.feed_down {
        outln!("daemon: the change feed is down: {why}");
    }
    for n in &s.not_applied {
        outln!(
            "daemon: not applied here since {}: {}",
            date_time(n.since),
            n.text
        );
    }
    if s.not_applied_count > s.not_applied.len() {
        outln!(
            "daemon: and {} more not applied here",
            s.not_applied_count - s.not_applied.len()
        );
    }
    // What the last sync left out, each in the system's words: a file refused here stays listed until
    // it can be read, or is gone.
    for skip in s.skips.iter().take(MAX_SKIPS_SHOWN) {
        outln!("daemon: skipped: {skip}");
    }
    if s.skips.len() > MAX_SKIPS_SHOWN {
        outln!(
            "daemon: and {} more skipped: the daemon's log has them all",
            s.skips.len() - MAX_SKIPS_SHOWN
        );
    }
    if let Some(w) = s.waiting.as_ref().filter(|w| w.until > now()) {
        outln!(
            "daemon: changes wait until {}, {} s from now: {}",
            date_time(w.until),
            w.until - now(),
            w.why
        );
    }
    if let Some(since) = s.paused_since {
        outln!(
            "daemon: paused by request since {}: `cairn resume` resumes it",
            date_time(since)
        );
    }
}

fn json_line(value: &impl serde::Serialize) -> Result<()> {
    outln!("{}", serde_json::to_string(value)?);
    Ok(())
}

/// One request to this root's daemon, its one answer.
fn ask_daemon(root_path: &Path, request: &Request) -> Result<Reply> {
    let root_id = cairn_engine::root::read_root_id(root_path)
        .with_context(|| format!("{} is not a sync root", root_path.display()))?;
    control::ask(&root_id, request, control::IO_WAIT).map_err(|e| not_asked(root_path, &e))
}

/// Why the daemon of this root could not be asked: nothing answers, or what answers is refused.
fn not_asked(root_path: &Path, e: &std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow!("the daemon of {} cannot be asked: {e}", root_path.display())
    } else {
        anyhow!(
            "no daemon answers for {}: {e}. A daemon from before the local API runs without answering",
            root_path.display()
        )
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn run_sync(root: &Path, opts: SyncOptions) -> Result<u8> {
    let report = sync(root, &opts)?;
    let code = print_sync(&report);
    if report.compact_due {
        compact(root);
    }
    Ok(code)
}

/// After a sync whose report is out, as the daemon does: said only when it cannot be done, which leaves
/// the state as it was and the sync's result as printed.
fn compact(root: &Path) {
    match cairn_engine::sync::compact(root) {
        Ok(Compaction::Done { .. } | Compaction::NotDue(_)) => {}
        Ok(short) => errln!("cairn: {short}"),
        Err(e) if e.downcast_ref::<Busy>().is_some() => {}
        Err(e) => errln!("cairn: the state was not compacted, and stays as it was: {e:#}"),
    }
}

fn date_time(epoch: i64) -> String {
    let secs = epoch.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02}:{:02}Z",
        cairn_engine::sync::utc_date(epoch),
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// The folder a token reaches, when it reaches one alone: what `cairn init` syncs unless told which.
/// A server before accounts had one token for every folder, and `default` first.
fn only_folder(server: &str, token: &str, allow_http: bool) -> Result<String> {
    let me = match cairn_engine::client::Client::of_server(server, token, allow_http)?.me() {
        Ok(me) => me,
        Err(e) if format!("{e:#}").contains("has no accounts") => return Ok("default".into()),
        Err(e) => {
            return Err(
                e.context("asking the server which folder to sync, as --folder was not given")
            )
        }
    };
    match me.folders.as_slice() {
        [one] => Ok(one.folder.clone()),
        [] => bail!(
            "user {:?} has access to no folder on this server: a manager of one gives it, or its administrator by `cairn-server grant`",
            me.user
        ),
        many => bail!(
            "user {:?} has access to {} folders, so say which with --folder: {}",
            me.user,
            many.len(),
            many.iter()
                .map(|f| match &f.name {
                    Some(n) if *n != f.folder => format!("{} ({n})", f.folder),
                    _ => f.folder.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn run_inner() -> Result<u8> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init {
            dir,
            server,
            token_file,
            namespace,
            device_name,
            owners,
            setid,
            capabilities,
            no_mode,
            allow_http,
        } => {
            let dir = match (cli.root, dir) {
                (Some(r), Some(d)) if r != d => bail!(
                    "two folders given, {} and --root {}: say which once",
                    d.display(),
                    r.display()
                ),
                (Some(r), _) => r,
                (None, Some(d)) => d,
                (None, None) => PathBuf::from("."),
            };
            let token = match token_file {
                Some(f) => {
                    std::fs::read_to_string(&f).with_context(|| format!("read {}", f.display()))?
                }
                None => std::env::var("CAIRN_TOKEN")
                    .map_err(|_| anyhow!("give --token-file, or set CAIRN_TOKEN"))?,
            };
            if let Some(n) = &device_name {
                cairn_engine::sync::ensure_clean_name(n)?;
            }
            let policy = ApplyPolicy {
                owners: match owners {
                    OwnersArg::None => OwnerPolicy::None,
                    OwnersArg::Name => OwnerPolicy::ByName,
                    OwnersArg::Number => OwnerPolicy::ByNumber,
                },
                setid,
                capabilities,
                mode: !no_mode,
                ..ApplyPolicy::default()
            };
            let namespace = match namespace {
                Some(n) => n,
                None => only_folder(&server, token.trim(), allow_http).with_context(|| {
                    format!(
                        "{} was not made a sync root, and nothing in it changed",
                        dir.display()
                    )
                })?,
            };
            let head = init_root(
                &dir,
                InitOptions {
                    server,
                    namespace: namespace.clone(),
                    token: token.trim().to_string(),
                    device_name,
                    policy,
                    allow_http,
                },
            )?;
            let root = Root::open(&dir)?;
            let listed = cairn_engine::root::read_root_id(&dir)
                .map_err(anyhow::Error::from)
                .and_then(|id| registry::register(&dir, &id, &root.config.server));
            if let Err(e) = listed {
                errln!("cairn: this root could not be added to the list of roots, so `cairn status --all` does not show it until its daemon starts: {e:#}");
            }
            // Its user's own name for the folder, if they have none: the web shows it them.
            if let Some(line) = cairn_engine::root::offer_root_name(&dir) {
                errln!("cairn: {line}");
            }
            outln!(
                "{} is a sync root of the folder {namespace}; the server is at sequence {head}",
                dir.display()
            );
            Ok(EXIT_OK)
        }
        Command::Sync {
            allow_mass_delete,
            dry_run,
        } => run_sync(
            &find_root(cli.root)?,
            SyncOptions {
                allow_mass_delete,
                dry_run,
                ..SyncOptions::default()
            },
        ),
        Command::Daemon {
            reconcile_secs,
            full_sync_secs,
            retry_secs,
            no_watch,
            no_feed,
        } => {
            let root = find_root(cli.root)?;
            // Whoever reads this going away is no reason to stop syncing.
            let say: fn(&str) = |line: &str| {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{} {line}", date_time(now()));
                let _ = out.flush();
            };
            // Before the daemon does anything, as this starts the program again: so that what a sync frees
            // goes back to the system.
            #[cfg(target_os = "macos")]
            if let Some(e) = macos::without_large_cache() {
                say(&format!("the allocator keeps the large blocks a sync frees, so this daemon holds what its biggest sync used while it waits; it syncs as before: {e}"));
            }
            let opts = DaemonOptions {
                reconcile: Duration::from_secs(reconcile_secs.max(1)),
                full_sync: Duration::from_secs(full_sync_secs.max(1)),
                retry: Duration::from_secs(retry_secs.max(1)),
                watch: !no_watch,
                feed: !no_feed,
                say: Some(say),
            };
            // Tries go in this log: a request's first new try and its outcome, not a line a try.
            cairn_engine::client::log_retries_with(say);
            say(&format!(
                "starting for {}, version {}",
                root.display(),
                cairn_engine::update::running_version()
            ));
            // A new version's handler for Explorer's badges, if they are on.
            #[cfg(windows)]
            if let Some(line) = explorer::follow_version() {
                say(&line);
            }
            // The daemon alone: no other command has macOS know it as an app.
            #[cfg(target_os = "macos")]
            if let Err(e) = macos::check_in() {
                say(&format!("macOS does not know this daemon as its app, so Activity Monitor shows it as cairn, without its icon; it syncs as before: {e}"));
            }
            cairn_engine::daemon::run(&root, &opts, &mut |line| say(line))?;
            // This program was replaced: the new one, in this process where the system allows.
            Err(cairn_engine::update::restart())
        }
        Command::Exclude { path, remove_local } => {
            let root_path = find_root(cli.root)?;
            let names = to_names(&path)?;
            selection::check_excludable(&Root::open(&root_path)?, &names)?;
            // Only when the copy here is to go: first, so that what is only here is on the server.
            // A copy that stays is left as it is, and what is new in it was never meant to go up.
            if remove_local && Root::open(&root_path)?.state.cursor()? > 0 {
                let first = sync(&root_path, &SyncOptions::default())?;
                print_sync(&first);
                if first.outcome != SyncOutcome::Synced {
                    bail!("this root is not fully synced, so nothing was excluded: fix what is above and try again");
                }
            }
            {
                let mut root = Root::open(&root_path)?;
                selection::exclude(&mut root, &names, remove_local)?;
            }
            outln!("{} is no longer synced here", display_path(&names));
            run_sync(&root_path, SyncOptions::default())
        }
        Command::Include { path, merge } => {
            let root_path = find_root(cli.root)?;
            let names = to_names(&path)?;
            {
                let mut root = Root::open(&root_path)?;
                selection::include(&mut root, &names, merge)?;
            }
            outln!("{} is synced here again", display_path(&names));
            run_sync(&root_path, SyncOptions::default())
        }
        Command::Status { all: true, json } => {
            let views = registry::views()?;
            if json {
                json_line(&views)?;
                return Ok(EXIT_OK);
            }
            if views.is_empty() {
                outln!(
                    "no roots listed for this user: `cairn init` and a starting daemon list theirs"
                );
            }
            for (i, v) in views.iter().enumerate() {
                if i > 0 {
                    outln!("");
                }
                print_view(v);
            }
            Ok(EXIT_OK)
        }
        Command::Forget {
            path,
            even_if_present,
        } => {
            let gone = registry::forget(&path, even_if_present)?;
            let (path, id) = (gone.entry.path.display(), &gone.entry.root_id);
            if gone.present {
                outln!("{path} (root {id}) is no longer listed, though it is still a root and syncs as before; its daemon lists it again when it starts");
            } else {
                outln!("{path} (root {id}) had gone, and is no longer listed");
            }
            Ok(EXIT_OK)
        }
        Command::Status { all: false, json } => {
            let root_path = find_root(cli.root)?;
            let view = root_view(&root_path)?;
            if !json {
                match &view.daemon {
                    Some(s) => print_daemon(s),
                    None => outln!("daemon: not running"),
                }
            }
            let dry_run = sync(
                &root_path,
                &SyncOptions {
                    allow_mass_delete: true,
                    dry_run: true,
                    ..SyncOptions::default()
                },
            );
            if json {
                let mut out = serde_json::to_value(&view)?;
                let busy = matches!(&dry_run, Err(e) if e.downcast_ref::<Busy>().is_some());
                let report = if busy { None } else { Some(dry_run?) };
                out["busy"] = busy.into();
                out["planned"] = serde_json::to_value(report.as_ref().map(|r| &r.planned))?;
                out["excluded_here"] =
                    serde_json::to_value(report.as_ref().map(|r| &r.excluded_here))?;
                out["not_synced_here"] =
                    serde_json::to_value(report.as_ref().map(|r| &r.not_synced_here))?;
                json_line(&out)?;
                return Ok(report.as_ref().map_or(EXIT_OK, status_code));
            }
            let report = match dry_run {
                Err(e) if e.downcast_ref::<Busy>().is_some() => {
                    outln!("a sync is running now, so what the next one would do is not known yet");
                    return Ok(EXIT_OK);
                }
                r => r?,
            };
            if report.planned.is_empty() {
                outln!("nothing to do at sequence {}", report.head);
            }
            for path in &report.excluded_here {
                outln!("not synced, by .cairn/ignore or `cairn exclude`: {path}");
            }
            for path in &report.not_synced_here {
                outln!("on the server, not synced here: {path}");
            }
            if let Ok(root) = Root::open(&root_path) {
                let excluded = selection::excluded_paths(&root).unwrap_or_default();
                for path in excluded.on_server {
                    outln!("excluded with `cairn exclude`: {}", display_path(&path));
                }
                for (path, there) in excluded.copies {
                    if there {
                        outln!(
                            "left here when it was excluded: {0}; delete it, or `cairn include --merge {0}`",
                            display_path(&path)
                        );
                    } else {
                        outln!(
                            "anything new at {0} stays out too, until `cairn include {0}`",
                            display_path(&path)
                        );
                    }
                }
            }
            print_sync(&report);
            Ok(status_code(&report))
        }
        Command::Folders {
            server,
            token_file,
            allow_http,
        } => {
            let here = || open_config(&find_root(cli.root.clone())?);
            let token = match &token_file {
                Some(f) => {
                    std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?
                }
                None => match std::env::var("CAIRN_TOKEN") {
                    Ok(t) => t,
                    Err(_) => {
                        here()
                            .map_err(|e| {
                                anyhow!(
                                "give --token-file, set CAIRN_TOKEN, or run this in a root: {e:#}"
                            )
                            })?
                            .token
                    }
                },
            };
            let (server, allow_http) = match server {
                Some(s) => (s, allow_http),
                None => {
                    let c = here()
                        .map_err(|e| anyhow!("give --server, or run this in a root: {e:#}"))?;
                    (c.server, allow_http || c.allow_http)
                }
            };
            let me = Client::of_server(&server, &token, allow_http)?.me()?;
            if me.folders.is_empty() {
                outln!(
                    "user {} has access to no folder on {server} yet: one of its managers, or the server's administrator, gives it",
                    me.user
                );
            } else {
                outln!("user {} on {server}:", me.user);
                for f in &me.folders {
                    match &f.name {
                        Some(n) if *n != f.folder => {
                            outln!("{:<24} {:<6}  your name for it: {n}", f.folder, f.access)
                        }
                        _ => outln!("{:<24} {}", f.folder, f.access),
                    }
                }
            }
            Ok(EXIT_OK)
        }
        Command::Access { what } => {
            let config = open_config(&find_root(cli.root)?)?;
            let client = Client::new(&config, Retries::Short)?;
            match what.unwrap_or(AccessCommand::List) {
                AccessCommand::List => {
                    for m in client.members()? {
                        outln!("{:<24} {}", m.user, m.access);
                    }
                }
                AccessCommand::Grant { user, access } => {
                    client.set_member(&user, Some(access))?;
                    outln!(
                        "user {user} may now {} folder {}",
                        match access {
                            cairn_protocol::wire::Access::Read => "read",
                            cairn_protocol::wire::Access::Write => "read and write",
                            cairn_protocol::wire::Access::Manage => "read, write and manage",
                        },
                        config.namespace
                    );
                }
                AccessCommand::Revoke { user } => {
                    client.set_member(&user, None)?;
                    outln!(
                        "user {user} has no access to folder {} any more",
                        config.namespace
                    );
                }
            }
            Ok(EXIT_OK)
        }
        Command::Log { limit } => {
            let client = Client::new(&open_config(&find_root(cli.root)?)?, Retries::Short)?;
            let changes = restore::journal(&client)?;
            let skip = limit.map_or(0, |n| changes.len().saturating_sub(n));
            for c in &changes[skip..] {
                let who = match &c.user {
                    Some(u) => format!("{} ({u})", c.device_name),
                    None => c.device_name.clone(),
                };
                outln!(
                    "{:>8}  {}  {:<16} {} op(s)",
                    c.seq,
                    date_time(c.time),
                    who,
                    c.ops.len()
                );
                for op in &c.ops {
                    if let cairn_protocol::Op::Note { text, also, .. } = op {
                        outln!(
                            "          {}",
                            cairn_engine::sync::note_line(text, also.len())
                        );
                    }
                }
            }
            Ok(EXIT_OK)
        }
        Command::History { path, at } => {
            let config = open_config(&find_root(cli.root)?)?;
            for h in restore::history(&config, &to_names(&path)?, at)? {
                outln!(
                    "{:>8}  {}  {:<16} {}  {:o}  {}{}",
                    h.seq,
                    date_time(h.time),
                    h.device,
                    h.revision,
                    h.mode,
                    h.size.map_or("-".to_string(), |s| format!("{s} bytes")),
                    h.restored_from
                        .map_or(String::new(), |r| format!("  restored from {r}")),
                );
            }
            Ok(EXIT_OK)
        }
        Command::Restore {
            path,
            at,
            revision,
            no_sync,
        } => {
            let root_path = find_root(cli.root)?;
            let names = to_names(&path)?;
            let outcome = {
                let root = Root::open(&root_path)?;
                match (at, revision) {
                    (Some(at), None) => restore::restore_at(&root, &names, at)?,
                    (None, Some(r)) => {
                        let revision: RevisionId =
                            r.parse().map_err(|_| anyhow!("{r} is not a revision ID"))?;
                        restore::restore_revision(&root, &names, revision)?
                    }
                    _ => bail!("say which state: --at SEQ or --revision ID"),
                }
            };
            match outcome.seq {
                Some(seq) => outln!(
                    "restored {} as sequence {seq} ({} op(s))",
                    display_path(&names),
                    outcome.ops
                ),
                None => outln!("{} is already in that state", display_path(&names)),
            }
            if no_sync {
                return Ok(EXIT_OK);
            }
            run_sync(&root_path, SyncOptions::default())
        }
        Command::Export {
            dest,
            at,
            path,
            exact,
        } => {
            let config = open_config(&find_root(cli.root)?)?;
            let names = path
                .as_deref()
                .map(to_names)
                .transpose()?
                .unwrap_or_default();
            let policy = if exact {
                ApplyPolicy::exact()
            } else {
                config.policy.clone()
            };
            let (seq, warnings) =
                cairn_engine::export::export(&config, at, &names, &dest, &policy)?;
            for w in &warnings {
                outln!("not applied: {w}");
            }
            outln!("exported sequence {seq} to {}", dest.display());
            Ok(if warnings.is_empty() {
                EXIT_OK
            } else {
                EXIT_PARTIAL
            })
        }
        Command::Pause | Command::Resume => {
            let pause = matches!(cli.command, Command::Pause);
            let root_path = find_root(cli.root)?;
            let request = if pause {
                Request::Pause
            } else {
                Request::Resume
            };
            match ask_daemon(&root_path, &request)? {
                Reply::Done if pause => outln!(
                    "paused: the daemon starts no sync until `cairn resume`; changes wait, and none is lost"
                ),
                Reply::Done => outln!("resumed: the daemon syncs what changed meanwhile"),
                Reply::Error { text } => bail!("{text}"),
                other => bail!("the daemon answered with {other:?}"),
            }
            Ok(EXIT_OK)
        }
        Command::Ask { request } => {
            let root_path = find_root(cli.root)?;
            // A whole request as JSON, for one that takes more than its name.
            let request: Request = if request.trim_start().starts_with('{') {
                serde_json::from_str(&request)
                    .map_err(|e| anyhow!("{request} is not a request: {e}"))?
            } else {
                serde_json::from_value(serde_json::json!({ "op": request.replace('-', "_") }))
                    .map_err(|_| {
                        anyhow!("{request} is not a request: status, subscribe, sync_now, pause, resume, conflicts, deleted, update_now, sign_in_link or watch_badges, or one as JSON")
                    })?
            };
            let root_id = cairn_engine::root::read_root_id(&root_path)
                .with_context(|| format!("{} is not a sync root", root_path.display()))?;
            let mut c =
                control::Client::connect(&root_id).map_err(|e| not_asked(&root_path, &e))?;
            // A subscription lasts until the daemon or this program ends; what has the daemon read the
            // server's journal takes as long as that does.
            let wait = match request {
                Request::Subscribe | Request::WatchBadges => None,
                Request::Conflicts | Request::Deleted { .. } | Request::Undelete { .. } => {
                    Some(Duration::from_secs(600))
                }
                Request::SignInLink => Some(Duration::from_secs(60)),
                _ => Some(control::IO_WAIT),
            };
            c.set_wait(wait)?;
            c.send(&request)?;
            let mut code = EXIT_OK;
            while let Some(reply) = c.next(wait)? {
                if matches!(reply, Reply::Error { .. }) {
                    code = EXIT_ERROR;
                }
                json_line(&reply)?;
                if STDOUT_GONE.load(Ordering::Relaxed) {
                    break;
                }
            }
            Ok(code)
        }
        #[cfg(windows)]
        Command::Explorer { what } => {
            match what {
                ExplorerCommand::On { dll } => explorer::on(dll.as_deref())?,
                ExplorerCommand::Off => explorer::off()?,
                ExplorerCommand::Status => explorer::status()?,
                ExplorerCommand::Handlers { state } => explorer::set_handlers(state == "on")
                    .context("cannot change Explorer's list of overlay handlers")?,
                ExplorerCommand::Probe {
                    dll: Some(dll),
                    again,
                    paths,
                } => explorer::probe_dll(&dll, &paths, again)?,
                ExplorerCommand::Probe {
                    dll: None, paths, ..
                } => explorer::probe(&paths)?,
            }
            Ok(EXIT_OK)
        }
        Command::Update { what } => {
            update_command(what.unwrap_or(UpdateCommand::Status { json: false }))
        }
        Command::Deleted { days, json } => {
            let index = deleted::index(&open_config(&find_root(cli.root)?)?)?;
            let list = index.list(deleted::days_ago(days));
            for d in &list {
                if json {
                    json_line(&d.item())?;
                    continue;
                }
                let what = match d.kind {
                    ObjectKind::Dir => format!(
                        "{} file{}, {}",
                        d.files,
                        if d.files == 1 { "" } else { "s" },
                        local::bytes(d.bytes)
                    ),
                    ObjectKind::Symlink => "symlink".into(),
                    ObjectKind::File => local::bytes(d.bytes),
                };
                let slash = if d.kind == ObjectKind::Dir { "/" } else { "" };
                let folder = if d.folder_gone {
                    "  (its folder too)"
                } else {
                    ""
                };
                outln!(
                    "{:>8}  {}  {:<16} {what:>22}  {}{slash}{folder}  {}",
                    d.seq,
                    date_time(d.time),
                    d.device_name,
                    display_path(&d.path),
                    d.object
                );
            }
            if !json && list.is_empty() {
                outln!("nothing was deleted in the last {days} days that is still deleted");
            }
            Ok(EXIT_OK)
        }
        Command::Undelete { ids, no_sync } => {
            let root_path = find_root(cli.root)?;
            let ids = deleted::parse_ids(&ids)?;
            let config = open_config(&root_path)?;
            let mut index = deleted::index(&config)?;
            let outcome = deleted::undelete(&config, &mut index, &ids)?;
            for (_, p) in &outcome.already {
                outln!("{} is not deleted", display_path(p));
            }
            for u in &outcome.items {
                if u.renamed {
                    outln!(
                        "brought back {} as {}, as its name is taken now ({} object(s))",
                        display_path(&u.was),
                        display_path(&u.path),
                        u.objects
                    );
                } else {
                    outln!(
                        "brought back {} ({} object(s))",
                        display_path(&u.path),
                        u.objects
                    );
                }
            }
            if let Some(last) = outcome.seqs.last() {
                outln!("as sequence {last}");
            }
            if no_sync || outcome.seqs.is_empty() {
                return Ok(EXIT_OK);
            }
            run_sync(&root_path, SyncOptions::default())
        }
        Command::Web { print } => {
            let config = open_config(&find_root(cli.root)?)?;
            let url = Client::of_server(&config.server, &config.token, config.allow_http)?
                .sign_in_url(Some(&config.namespace))?;
            if print {
                outln!("{url}");
                return Ok(EXIT_OK);
            }
            match open_browser(&url) {
                Ok(()) => outln!(
                    "opened folder {} of {} in the browser, signed in",
                    config.namespace,
                    config.server
                ),
                Err(e) => {
                    eprintln!(
                        "cairn: {e}: open this address within two minutes, which signs in once:"
                    );
                    outln!("{url}");
                }
            }
            Ok(EXIT_OK)
        }
        Command::Conflicts { all } => {
            let root = find_root(cli.root)?;
            let config = open_config(&root)?;
            // Those dealt with say where they were made, which only the whole journal knows.
            let (list, closed) = if all {
                let list = restore::conflicts(&config)?;
                let closed = list.iter().filter(|c| !c.open).count();
                (list, closed)
            } else {
                let inbox = restore::open_conflicts(&config, &root, &mut Default::default())?;
                (inbox.open, inbox.closed)
            };
            let path = |p: &Option<Vec<Name>>| p.as_deref().map(display_path);
            for c in &list {
                let e = &c.entry;
                let made = path(&c.made_at).unwrap_or_else(|| format!("copy {}", e.copy));
                let against = path(&c.object_now).unwrap_or_else(|| "what is since deleted".into());
                let what = if c.open {
                    format!("{made}: the competing version of {against}")
                } else {
                    let now = match path(&c.copy_now) {
                        Some(p) => format!("now {p}"),
                        None => "deleted".into(),
                    };
                    format!("dealt with: {made}, {now}")
                };
                outln!(
                    "{:>8}  {}  {:<16} {what}",
                    e.seq,
                    date_time(e.time),
                    e.device_name
                );
            }
            if list.iter().all(|c| !c.open) {
                outln!("no conflict waits for a person");
            }
            if !all && closed > 0 {
                outln!(
                    "{closed} more dealt with, the copy deleted, renamed or moved: --all lists them"
                );
            }
            Ok(EXIT_OK)
        }
    }
}

fn update_command(what: UpdateCommand) -> Result<u8> {
    use cairn_engine::update::{self, Options, Outcome};
    use cairn_protocol::update::Policy;
    let mut say = |line: &str| outln!("{line}");
    match what {
        UpdateCommand::Status { json } => {
            let status = update::status()?;
            if json {
                json_line(&status)?;
                return Ok(EXIT_OK);
            }
            for p in update::programs()? {
                let v = update::installed_version(&p).unwrap_or_else(|e| format!("({e:#})"));
                outln!("{} {v}: {}", p.name, p.path.display());
            }
            let settings = update::settings()?;
            let feeds = update::feeds(&settings);
            if feeds.is_empty() {
                outln!(
                    "updates: {}, but this build names no feed and the settings none, so nothing updates it",
                    status.policy.name()
                );
            } else {
                outln!(
                    "updates: {}, from {}",
                    status.policy.name(),
                    feeds.join(", or if it does not answer ")
                );
            }
            if status.checked > 0 {
                outln!(
                    "last asked: {}, latest {}{}",
                    date_time(status.checked),
                    status.latest.as_deref().unwrap_or("?"),
                    match &status.answered {
                        Some(f) if feeds.len() > 1 => format!(", from {f}"),
                        _ => String::new(),
                    }
                );
                if let Some(why) = &status.unanswered {
                    outln!("not answered then: {why}");
                }
            } else {
                outln!("last asked: never");
            }
            if let Some(v) = &status.available {
                outln!(
                    "available: {v}, not installed: {}",
                    status
                        .waits
                        .as_deref()
                        .unwrap_or("`cairn update now` installs it")
                );
            } else if let Some(w) = &status.waits {
                outln!("{w}");
            }
            if let Some(v) = &status.installed {
                outln!(
                    "installed: {v} at {}; each program restarts into it, a daemon between syncs",
                    date_time(status.installed_at)
                );
            }
            if let Some(e) = &status.error {
                outln!("error since {}: {e}", date_time(status.error_since));
            }
            if !status.rejected.is_empty() {
                outln!(
                    "taken back here, not installed again unless asked: {}",
                    status.rejected.join(", ")
                );
            }
            for v in registry::views()? {
                if let Some(d) = &v.daemon {
                    let version = if d.version.is_empty() {
                        "a version from before 0.1.1"
                    } else {
                        &d.version
                    };
                    outln!("daemon for {}: {version}", v.root.path.display());
                }
            }
            Ok(EXIT_OK)
        }
        UpdateCommand::Check { feed } => {
            let (outcome, status) = update::run(
                &Options {
                    asked: true,
                    install: false,
                    feed: feed.feed,
                    keys: feed.keys,
                    ..Options::default()
                },
                &mut say,
            )?;
            report_update(&outcome, &status)
        }
        UpdateCommand::Now {
            feed,
            any_protocol,
            no_wait,
        } => {
            // Before: a daemon restarting into the new version answers nobody for a moment.
            let running: Vec<RootEntry> = registry::views()
                .map(|views| {
                    views
                        .into_iter()
                        .filter(|v| v.daemon.is_some())
                        .map(|v| v.root)
                        .collect()
                })
                .unwrap_or_default();
            let (outcome, status) = update::run(
                &Options {
                    asked: true,
                    install: true,
                    any_protocol,
                    feed: feed.feed,
                    keys: feed.keys,
                },
                &mut say,
            )?;
            let code = report_update(&outcome, &status)?;
            if let Outcome::Installed(v) = &outcome {
                if !no_wait {
                    wait_for_daemons(&running, v);
                }
            }
            Ok(code)
        }
        UpdateCommand::Auto | UpdateCommand::Notify | UpdateCommand::Off => {
            let policy = match what {
                UpdateCommand::Auto => Policy::Auto,
                UpdateCommand::Notify => Policy::Notify,
                _ => Policy::Off,
            };
            update::set_policy(policy)?;
            outln!(
                "updates: {}",
                match policy {
                    Policy::Auto => "auto: each new release is installed as it comes",
                    Policy::Notify =>
                        "notify: a new release is shown, and installed by `cairn update now`",
                    Policy::Off => "off: the feed is not asked; `cairn update now` still installs",
                }
            );
            Ok(EXIT_OK)
        }
        UpdateCommand::Rollback => {
            let to = update::rollback("taken back by a person", &mut say)?;
            outln!("back to {to}: each program restarts into it, a daemon between syncs");
            Ok(EXIT_OK)
        }
    }
}

fn report_update(
    outcome: &cairn_engine::update::Outcome,
    status: &cairn_protocol::update::Status,
) -> Result<u8> {
    use cairn_engine::update::Outcome;
    if let (Some(why), Some(by)) = (&status.unanswered, &status.answered) {
        if !matches!(outcome, Outcome::NoFeed | Outcome::Busy) {
            outln!("{why}; {by} answered instead");
        }
    }
    match outcome {
        Outcome::NoFeed => bail!(
            "this build names no feed to update from, and the settings none: --feed names one"
        ),
        Outcome::Busy => bail!("another update is still running here"),
        Outcome::UpToDate(v) => {
            outln!("up to date: the latest release is {v}");
            Ok(EXIT_OK)
        }
        Outcome::Waits(v) => {
            outln!(
                "{v} is not installed: {}",
                status.waits.as_deref().unwrap_or("it waits")
            );
            Ok(EXIT_PARTIAL)
        }
        Outcome::Installed(v) => {
            outln!("installed {v}");
            Ok(EXIT_OK)
        }
    }
}

/// Until each daemon running here says it runs `version`, or a while has passed: a daemon restarts
/// within seconds when idle, and after the sync it is in otherwise.
fn wait_for_daemons(running: &[RootEntry], version: &str) {
    const WAIT: Duration = Duration::from_secs(90);
    let start = std::time::Instant::now();
    for root in running {
        let path = root.path.display().to_string();
        let mut last = String::new();
        loop {
            match control::status_of(&root.path, &root.root_id) {
                Some(s) if s.version == version => {
                    outln!("daemon for {path}: restarted on {version}");
                    break;
                }
                Some(s) => last = s.version,
                None => {}
            }
            if start.elapsed() > WAIT {
                outln!(
                    "daemon for {path}: still {} after {} s; it restarts after the sync it is in",
                    if last.is_empty() {
                        "not answering"
                    } else {
                        &last
                    },
                    WAIT.as_secs()
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

fn status_code(report: &cairn_engine::SyncReport) -> u8 {
    match report.outcome {
        SyncOutcome::Paused(_) => EXIT_PAUSED,
        _ => EXIT_OK,
    }
}

/// The whole client program; `main` of the release binary and of the harness's build.
pub fn run() -> ExitCode {
    // What this program's file is now, to tell later that an update has replaced it.
    cairn_engine::update::remember_start();
    match run_inner() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            if let Some(p) = e.downcast_ref::<Paused>() {
                outln!("paused: {p}");
                return ExitCode::from(EXIT_PAUSED);
            }
            errln!("cairn: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}
