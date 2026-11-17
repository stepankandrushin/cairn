//! Retention and collection in the server (docs/protocol.md, "Retention"; docs/format.md,
//! "Collection"): each folder's policy, set by its managers or the administrator, a daily run of the
//! policy and a collection of what it gave up, the reads of what it gave up, and `cairn-server gc`.

use crate::{blocking, now, Allowed, ApiResult, App, Caller};
use anyhow::{bail, Result};
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use cairn_protocol::retention::{parse_duration, Policy, DAY, HOUR};
use cairn_protocol::wire::{Access, PrunedAsk, PrunedList, PrunedRevision, Retention};
use cairn_store::gc::{collect, Collected, Hooks};
use cairn_store::meta::RetentionRow;
use cairn_store::Repo;
use std::path::Path as FsPath;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How a server collects: D and G (`serve --gc-delay`, `--upload-grace`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    /// What retention gave up less than this long ago keeps its content, in seconds.
    pub delay: i64,
    /// A chunk written less than this long ago is not taken, in seconds.
    pub grace: u64,
}

pub(crate) const DEFAULT_DELAY: &str = "7d";
pub(crate) const DEFAULT_GRACE: &str = "2d";

impl Settings {
    /// From the flags: D at least a day, far longer than a backup takes, so that a backup's database
    /// checked against the live chunks finds what it names; G at least an hour, longer than any
    /// upload's chunks wait for their commit. A harness build may make either anything.
    pub(crate) fn from_flags(delay: &str, grace: &str) -> Result<Self> {
        let d = parse_duration(delay).map_err(anyhow::Error::msg)?;
        let g = parse_duration(grace).map_err(anyhow::Error::msg)?;
        if d < DAY {
            bail!("--gc-delay {delay} is under a day: what retention gives up keeps its content at least a day, far longer than a backup takes");
        }
        if g < HOUR {
            bail!("--upload-grace {grace} is under an hour: an upload's chunks wait for their commit at least that long");
        }
        Ok(Settings {
            delay: cairn_failpoint::tuned("server.gc.delay", d) as i64,
            grace: cairn_failpoint::tuned("server.gc.grace", g),
        })
    }
}

/// The time the `run`-th collection of this process goes by: the clock's, or for a harness build one
/// pushed on by `server.gc.clock_step` seconds each run, so that a test sees days go by in seconds,
/// from `server.gc.at` if it is given, so that runs a test compares go by one time.
fn clock(run: u64) -> i64 {
    let step = cairn_failpoint::tuned("server.gc.clock_step", 0);
    let from = match cairn_failpoint::tuned("server.gc.at", 0) {
        0 => now(),
        at => at as i64,
    };
    from.saturating_add(run.saturating_mul(step) as i64)
}

/// Collects every folder with a policy, one after another; what each did, or why it did not.
fn run_all(
    repo: &Repo,
    thumbs: Option<&crate::web::thumbs::Thumbs>,
    settings: Settings,
    run: u64,
    dry_run: bool,
    only: Option<&str>,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<(String, Result<Collected>)>> {
    let s = cairn_store::gc::Settings {
        delay: settings.delay,
        grace: Duration::from_secs(settings.grace),
        now: clock(run),
        dry_run,
    };
    let mut out = Vec::new();
    for ns in repo.namespaces()? {
        if only.is_some_and(|o| o != ns) {
            continue;
        }
        if stop() {
            break;
        }
        let dir = repo.with_meta(|m| m.chunk_dir(&ns))?;
        let say = |line: &str| eprintln!("cairn-server: gc: {line}");
        let mut forget = |manifests: &[cairn_protocol::Hash]| {
            if let Some(t) = thumbs {
                if let Err(e) = t.forget(&dir, manifests) {
                    eprintln!("error: gc: folder {ns:?}: the thumbnails of what it removed: {e:#}");
                }
            }
        };
        let mut hooks = Hooks {
            say: &say,
            forget: &mut forget,
            stop,
        };
        let done = collect(repo, &ns, &s, &mut hooks);
        out.push((ns, done));
    }
    Ok(out)
}

/// One line of what a folder's run did.
fn said(ns: &str, c: &Collected, dry_run: bool) -> String {
    let (gave, removed) = if dry_run {
        ("would give up", "would remove")
    } else {
        ("gave up", "removed")
    };
    format!(
        "folder {ns:?}: {gave} {} revision(s) of {} that name content, {} given up await their delay; {removed} {} chunk(s), {} bytes, of {} nothing kept names and old enough",
        c.newly_pruned, c.considered, c.awaiting, if dry_run { c.candidates as u64 } else { c.removed }, c.bytes, c.candidates
    )
}

/// The daily task: a run a while after the server starts, and every day after, each on its own
/// thread, never two at once.
pub(crate) fn spawn(app: Arc<App>) {
    let first = cairn_failpoint::tuned("server.gc.first", HOUR);
    let every = cairn_failpoint::tuned("server.gc.every", DAY).max(1);
    let runs = Arc::new(AtomicU64::new(0));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(first)).await;
        loop {
            let (app2, runs) = (app.clone(), runs.clone());
            let done = tokio::task::spawn_blocking(move || {
                let run = runs.fetch_add(1, Ordering::SeqCst) + 1;
                let closing = app2.closing.subscribe();
                let stop = move || *closing.borrow();
                run_all(
                    &app2.repo,
                    Some(&app2.thumbs),
                    app2.gc,
                    run,
                    false,
                    None,
                    &stop,
                )
            })
            .await;
            match done {
                Ok(Ok(all)) => {
                    for (ns, c) in all {
                        match c {
                            Ok(c) if c.newly_pruned > 0 || c.removed > 0 => {
                                eprintln!("cairn-server: gc: {}", said(&ns, &c, false))
                            }
                            Ok(_) => {}
                            Err(e) => eprintln!("error: gc: folder {ns:?}: {e:#}"),
                        }
                    }
                }
                Ok(Err(e)) => eprintln!("error: gc: {e:#}"),
                Err(e) => eprintln!("error: gc: {e}"),
            }
            if *app.closing.borrow() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(every)).await;
        }
    });
}

/// What the API says of a folder's policy.
fn retention_of(row: Option<&RetentionRow>) -> Result<Retention> {
    Ok(match row {
        None => Retention {
            policy: None,
            trash: None,
            set_by: None,
            set_at: None,
            restored: None,
        },
        Some(r) => {
            let p = r.policy()?;
            Retention {
                policy: Some(p.to_string()),
                trash: Some(p.trash),
                set_by: Some(r.set_by.clone()),
                set_at: Some(r.set_at),
                restored: None,
            }
        }
    })
}

/// A policy set, or with `None` taken away, by `by`: what it is now, and how many revisions given up
/// came back with it.
pub(crate) fn set(repo: &Repo, ns: &str, policy: Option<&Policy>, by: &str) -> Result<Retention> {
    let row = policy.map(|p| RetentionRow {
        keep: p.keep_text(),
        trash: p.trash as i64,
        set_by: by.to_string(),
        set_at: now(),
    });
    let back = repo.with_meta(|m| m.set_retention(ns, row.as_ref()))?;
    let mut out = retention_of(row.as_ref())?;
    out.restored = Some(back);
    Ok(out)
}

/// `GET /v1/ns/{ns}/retention`: anyone with access reads it.
pub(crate) async fn get_retention(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
) -> ApiResult<Response> {
    let row = blocking(move || app.repo.with_meta(|m| m.retention(&ns))).await?;
    Ok(Json(retention_of(row.as_ref())?).into_response())
}

/// `PUT /v1/ns/{ns}/retention`: a manager sets it, or with no policy takes it away.
pub(crate) async fn put_retention(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(allowed): Extension<Allowed>,
    Path(ns): Path<String>,
    Json(body): Json<Retention>,
) -> ApiResult<Response> {
    if allowed.0 < Access::Manage {
        return Ok((
            StatusCode::FORBIDDEN,
            format!(
                "only a manager of folder {ns:?} sets how much of its history it keeps, and user {:?} is not one",
                caller.user
            ),
        )
            .into_response());
    }
    let policy = match body.policy.as_deref().map(str::parse::<Policy>) {
        None => None,
        Some(Ok(p)) => Some(p),
        Some(Err(why)) => return Ok((StatusCode::BAD_REQUEST, why).into_response()),
    };
    let answer = blocking(move || set(&app.repo, &ns, policy.as_ref(), &caller.user)).await?;
    Ok(Json(answer).into_response())
}

/// `POST /v1/ns/{ns}/pruned`: which of these revisions retention gave up, and when.
pub(crate) async fn pruned(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
    Json(ask): Json<PrunedAsk>,
) -> ApiResult<Response> {
    if ask.revisions.len() > 100_000 {
        return Ok((
            StatusCode::PAYLOAD_TOO_LARGE,
            "at most 100,000 revisions in one ask",
        )
            .into_response());
    }
    let found = blocking(move || app.repo.with_meta(|m| m.pruned(&ns, &ask.revisions))).await?;
    Ok(Json(PrunedList {
        pruned: found
            .into_iter()
            .map(|(revision, at)| PrunedRevision { revision, at })
            .collect(),
    })
    .into_response())
}

/// When a revision asked for by a web read was given up: `None` if it was not.
pub(crate) fn pruned_at(
    m: &mut dyn cairn_store::MetaStore,
    ns: &str,
    revision: cairn_protocol::RevisionId,
) -> Result<Option<i64>> {
    Ok(m.pruned(ns, &[revision])?.first().map(|(_, at)| *at))
}

/// The answer to a read of what retention gave up.
pub(crate) fn gone(at: i64) -> Response {
    (
        StatusCode::GONE,
        format!(
            "removed by this folder's retention on {}",
            crate::admin::date(at)
        ),
    )
        .into_response()
}

/// `cairn-server gc`: a collection with the server stopped, or beside it `--dry-run` and `--status`.
pub(crate) fn command(
    data: &FsPath,
    settings: Settings,
    dry_run: bool,
    status: bool,
    folder: Option<&str>,
) -> Result<std::process::ExitCode> {
    if status {
        let repo = Repo::open_for_admin(data)?;
        for ns in repo.namespaces()? {
            if folder.is_some_and(|f| f != ns) {
                continue;
            }
            match repo.with_meta(|m| m.retention(&ns))? {
                None => println!("{ns}: no policy, so everything is kept and nothing is removed"),
                Some(r) => println!(
                    "{ns}: {}, set by {} on {}",
                    r.policy()?,
                    r.set_by,
                    crate::admin::date(r.set_at)
                ),
            }
            let pruned = repo.with_meta(|m| m.all_pruned(&ns))?.len();
            if pruned > 0 {
                println!("  {pruned} revision(s) given up");
            }
            for run in repo.with_meta(|m| m.gc_runs(&ns, 5))? {
                println!(
                    "  run {} on {}: {}, {} chunk(s) and {} bytes removed, what was given up before {} taken{}",
                    run.id,
                    crate::admin::date(run.started),
                    run.state,
                    run.chunks,
                    run.bytes,
                    crate::admin::date(run.cutoff),
                    run.error.map_or(String::new(), |e| format!(": {e}"))
                );
            }
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let repo = if dry_run {
        Repo::open_for_admin(data)?
    } else {
        match Repo::open(data) {
            Ok(r) => r,
            Err(e) if e.to_string().contains("another cairn-server holds") => bail!(
                "a server has {}: it collects by itself each day; beside it, only --dry-run and --status",
                data.display()
            ),
            Err(e) => return Err(e),
        }
    };
    let thumbs = if repo.owns_directory() && !dry_run {
        Some(crate::web::thumbs::Thumbs::open(data)?)
    } else {
        None
    };
    if repo.owns_directory() && !dry_run {
        let left = repo.with_meta(|m| m.gc_unfinished(now()))?;
        if left > 0 {
            println!(
                "{left} collection(s) a process left unfinished marked so; this one starts over"
            );
        }
    }
    let mut failed = false;
    let all = run_all(
        &repo,
        thumbs.as_ref(),
        settings,
        1,
        dry_run,
        folder,
        &|| false,
    )?;
    if all.is_empty() {
        if let Some(f) = folder {
            bail!("no folder {f:?}");
        }
    }
    for (ns, c) in all {
        match c {
            Ok(c) if c.policy.is_none() => {
                println!("folder {ns:?}: no policy, so nothing is given up or removed")
            }
            Ok(c) => println!("{}", said(&ns, &c, dry_run)),
            Err(e) => {
                failed = true;
                println!("error: folder {ns:?}: {e:#}");
            }
        }
    }
    Ok(if failed {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    })
}
