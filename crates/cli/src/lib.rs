//! The `cairn` command-line client.

use anyhow::{anyhow, bail, Context, Result};
use cairn_engine::client::Client;
use cairn_engine::daemon::DaemonOptions;
use cairn_engine::root::{open_config, Busy, Paused};
use cairn_engine::scan::display_path;
use cairn_engine::{
    init_root, sync, ApplyPolicy, InitOptions, OwnerPolicy, Root, SyncOptions, SyncOutcome,
};
use cairn_engine::{restore, selection};
use cairn_protocol::{Name, RevisionId};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
        #[arg(long, default_value = "default")]
        namespace: String,
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
        /// Seconds between full syncs when nothing asks for one: the safety net for missed events.
        #[arg(long, default_value_t = 3600)]
        reconcile_secs: u64,
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
    /// What a sync would do now.
    Status,
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
            Component::Normal(n) => out.push(Name::new(n.as_bytes().to_vec())?),
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
    for e in &report.events {
        outln!("event: {e}");
    }
    for s in &report.skips {
        outln!("skipped: {s}");
    }
    for w in &report.warnings {
        outln!("not applied here: {w}");
    }
    if !report.excluded_here.is_empty() || !report.not_synced_here.is_empty() {
        outln!(
            "by this device's choice: {} entries here not synced, {} on the server not here; `cairn status` lists them",
            report.excluded_here.len(),
            report.not_synced_here.len()
        );
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

fn print_daemon(s: &cairn_engine::daemon::Status) {
    outln!(
        "daemon: running as process {}, {}{}; last sync {}, last fully synced {}; {} folders watched",
        s.pid,
        if s.state.is_empty() { "starting" } else { &s.state },
        s.detail.as_ref().map_or(String::new(), |d| format!(" ({d})")),
        date_time(s.last_sync),
        if s.last_synced == 0 {
            "never".to_string()
        } else {
            date_time(s.last_synced)
        },
        s.watched,
    );
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
    if let Some(w) = s.waiting.as_ref().filter(|w| w.until > now()) {
        outln!(
            "daemon: changes wait until {}, {} s from now: {}",
            date_time(w.until),
            w.until - now(),
            w.why
        );
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn run_sync(root: &Path, opts: SyncOptions) -> Result<u8> {
    Ok(print_sync(&sync(root, &opts)?))
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
            init_root(
                &dir,
                InitOptions {
                    server,
                    namespace,
                    token: token.trim().to_string(),
                    device_name,
                    policy,
                    allow_http,
                },
            )?;
            let root = Root::open(&dir)?;
            let head = Client::new(&root.config)
                .context("checking the server")?
                .info()?
                .head;
            outln!(
                "{} is a sync root; the server is at sequence {head}",
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
            },
        ),
        Command::Daemon {
            reconcile_secs,
            retry_secs,
            no_watch,
            no_feed,
        } => {
            let root = find_root(cli.root)?;
            let opts = DaemonOptions {
                reconcile: Duration::from_secs(reconcile_secs.max(1)),
                retry: Duration::from_secs(retry_secs.max(1)),
                watch: !no_watch,
                feed: !no_feed,
            };
            // Whoever reads this going away is no reason to stop syncing.
            let say = |line: &str| {
                let mut out = std::io::stdout();
                let _ = writeln!(out, "{} {line}", date_time(now()));
                let _ = out.flush();
            };
            say(&format!("starting for {}", root.display()));
            cairn_engine::daemon::run(&root, &opts, &mut |line| say(line))?;
            Ok(EXIT_ERROR)
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
        Command::Status => {
            let root_path = find_root(cli.root)?;
            match cairn_engine::daemon::running_status(&root_path) {
                Some(s) => print_daemon(&s),
                None => outln!("daemon: not running"),
            }
            let report = match sync(
                &root_path,
                &SyncOptions {
                    allow_mass_delete: true,
                    dry_run: true,
                },
            ) {
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
        Command::Log { limit } => {
            let client = Client::new(&open_config(&find_root(cli.root)?)?)?;
            let changes = restore::journal(&client)?;
            let skip = limit.map_or(0, |n| changes.len().saturating_sub(n));
            for c in &changes[skip..] {
                outln!(
                    "{:>8}  {}  {:<16} {} op(s)",
                    c.seq,
                    date_time(c.time),
                    c.device_name,
                    c.ops.len()
                );
                for op in &c.ops {
                    if let cairn_protocol::Op::Note { text, .. } = op {
                        outln!("          note: {text}");
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
        Command::Conflicts { all } => {
            let list = restore::conflicts(&open_config(&find_root(cli.root)?)?)?;
            let path = |p: &Option<Vec<Name>>| p.as_deref().map(display_path);
            for c in list.iter().filter(|c| all || c.open) {
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
            let open = list.iter().filter(|c| c.open).count();
            if open == 0 {
                outln!("no conflict waits for a person");
            }
            if !all && open < list.len() {
                outln!(
                    "{} more dealt with, the copy deleted, renamed or moved: --all lists them",
                    list.len() - open
                );
            }
            Ok(EXIT_OK)
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
