//! The `cairn` command-line client.

use anyhow::{anyhow, bail, Context, Result};
use cairn_engine::client::Client;
use cairn_engine::restore;
use cairn_engine::root::Paused;
use cairn_engine::scan::display_path;
use cairn_engine::{
    init_root, sync, ApplyPolicy, InitOptions, OwnerPolicy, Root, SyncOptions, SyncOutcome,
};
use cairn_protocol::{Name, RevisionId};
use clap::{Parser, Subcommand, ValueEnum};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

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
        #[arg(default_value = ".")]
        dir: PathBuf,
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
    /// The conflict inbox.
    Conflicts,
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

fn open_root(explicit: Option<PathBuf>) -> Result<Root> {
    Root::open(&find_root(explicit)?)
}

fn print_sync(report: &cairn_engine::SyncReport) -> u8 {
    for line in &report.planned {
        println!("{line}");
    }
    for e in &report.events {
        println!("event: {e}");
    }
    for s in &report.skips {
        println!("skipped: {s}");
    }
    for w in &report.warnings {
        println!("not applied here: {w}");
    }
    match &report.outcome {
        SyncOutcome::Synced => {
            println!(
                "synced at sequence {}: {} steps applied here, {} commits pushed, {} chunks ({} bytes) uploaded, {} conflict(s)",
                report.head, report.steps_applied, report.commits, report.chunks_uploaded, report.bytes_uploaded, report.conflicts
            );
            EXIT_OK
        }
        SyncOutcome::Partial => {
            println!("not fully synced yet: nothing was lost; see above, and sync again");
            EXIT_PARTIAL
        }
        SyncOutcome::Paused(why) => {
            println!("paused: {why}");
            EXIT_PAUSED
        }
    }
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
            println!(
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
        Command::Status => {
            let report = sync(
                &find_root(cli.root)?,
                &SyncOptions {
                    allow_mass_delete: true,
                    dry_run: true,
                },
            )?;
            if report.planned.is_empty() {
                println!("nothing to do at sequence {}", report.head);
            }
            print_sync(&report);
            Ok(status_code(&report))
        }
        Command::Log { limit } => {
            let root = open_root(cli.root)?;
            let client = Client::new(&root.config)?;
            let changes = restore::journal(&client)?;
            let skip = limit.map_or(0, |n| changes.len().saturating_sub(n));
            for c in &changes[skip..] {
                println!(
                    "{:>8}  {}  {:<16} {} op(s)",
                    c.seq,
                    date_time(c.time),
                    c.device_name,
                    c.ops.len()
                );
                for op in &c.ops {
                    if let cairn_protocol::Op::Note { text, .. } = op {
                        println!("          note: {text}");
                    }
                }
            }
            Ok(EXIT_OK)
        }
        Command::History { path, at } => {
            let root = open_root(cli.root)?;
            for h in restore::history(&root, &to_names(&path)?, at)? {
                println!(
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
                Some(seq) => println!(
                    "restored {} as sequence {seq} ({} op(s))",
                    display_path(&names),
                    outcome.ops
                ),
                None => println!("{} is already in that state", display_path(&names)),
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
            let root = open_root(cli.root)?;
            let names = path
                .as_deref()
                .map(to_names)
                .transpose()?
                .unwrap_or_default();
            let policy = if exact {
                ApplyPolicy::exact()
            } else {
                root.config.policy.clone()
            };
            let (seq, warnings) = cairn_engine::export::export(&root, at, &names, &dest, &policy)?;
            for w in &warnings {
                println!("not applied: {w}");
            }
            println!("exported sequence {seq} to {}", dest.display());
            Ok(if warnings.is_empty() {
                EXIT_OK
            } else {
                EXIT_PARTIAL
            })
        }
        Command::Conflicts => {
            let root = open_root(cli.root)?;
            let client = Client::new(&root.config)?;
            for c in client.conflicts()? {
                println!(
                    "{:>8}  {}  {:<16} copy {} of {}, against revision {}",
                    c.seq,
                    date_time(c.time),
                    c.device_name,
                    c.copy,
                    c.object,
                    c.competing_revision
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
                println!("paused: {p}");
                return ExitCode::from(EXIT_PAUSED);
            }
            eprintln!("cairn: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}
