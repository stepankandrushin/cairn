//! The Cairn server: Axum over the store. Blocking storage work runs off the async threads.

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use cairn_protocol::wire::{
    self, Access, ChangesResponse, CommitRequest, Grant, HashList, Info, Me, Member,
    MAX_CHUNK_BATCH_BYTES,
};
use cairn_store::{is_disk_full, token_hash, ChunkStore, Repo};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

mod admin;
mod web;

struct App {
    repo: Repo,
    /// The web's uploads in progress.
    uploads: web::upload::Uploads,
    /// What signs a browser's session, and the sign-in tickets waiting.
    sessions: web::session::Sessions,
    /// What is deleted in each namespace, replayed from its journal and kept up with each commit.
    deleted: web::deleted::Indexes,
    /// Where a public link is opened, when that is another address than this server's own:
    /// `serve --link-url`.
    link_url: Option<String>,
    /// Moves after every durable commit, in any namespace: what a waiting `GET /changes` sleeps on.
    commits: tokio::sync::watch::Sender<u64>,
    /// Set when the server is stopping, so that no waiting request holds the shutdown up.
    closing: tokio::sync::watch::Sender<bool>,
}

/// About as much of the journal as one `GET /changes` answers: a new device asks for all of it, and
/// a commit can hold 48 MiB of operations. One entry goes out whatever its size.
const CHANGES_BYTES: usize = 16 * 1024 * 1024;

/// The longest a `GET /changes` may wait. A reverse proxy's read timeout is the client's to stay under.
const MAX_WAIT_SECS: u64 = 120;

/// An error as the API reports it. A full disk has its own status, so a client can say so plainly.
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if is_disk_full(&self.0) {
            return (
                StatusCode::INSUFFICIENT_STORAGE,
                "the server's disk is full",
            )
                .into_response();
        }
        eprintln!("error: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

type ApiResult<T> = Result<T, ApiError>;

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> ApiResult<T> {
    Ok(tokio::task::spawn_blocking(f)
        .await
        .map_err(anyhow::Error::from)??)
}

/// The user a request's token belongs to: every request has one, or it is answered 401.
#[derive(Clone)]
struct Caller {
    user: String,
}

/// What the caller may do in the namespace of the path: no access at all is answered as a namespace
/// that does not exist, so that nobody learns what is there by asking.
#[derive(Clone, Copy)]
struct Allowed(Access);

impl Allowed {
    /// A 403 to answer with, unless the caller may at least `want` here.
    fn refuses(self, want: Access, ns: &str, caller: &Caller) -> Option<Response> {
        if self.0 >= want {
            return None;
        }
        let why = match want {
            Access::Read => unreachable!("any access reads"),
            Access::Write => format!("folder {ns:?} is read-only for user {:?}", caller.user),
            Access::Manage => format!(
                "only a manager of folder {ns:?} says who has access to it, and user {:?} is not one",
                caller.user
            ),
        };
        Some((StatusCode::FORBIDDEN, why).into_response())
    }
}

async fn authorize(State(app): State<Arc<App>>, mut req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").map(str::to_string));
    // Looked up by its hash, which a guess cannot steer: a stored hash is no token, and timing the
    // lookup says nothing about any token there is.
    let hash = match presented {
        Some(Some(token)) => token_hash(&token),
        Some(None) => return (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response(),
        // A browser signed in: its session, which names a token and changes something only with the
        // web's header.
        None => match web::session::cookie_value(&req) {
            Some(cookie) => {
                if let Some(refused) = web::cookie_request_refused(&req) {
                    return refused;
                }
                match app.sessions.verify(&cookie) {
                    Some(hash) => hash,
                    None => {
                        return (StatusCode::UNAUTHORIZED, "signed out: sign in again")
                            .into_response()
                    }
                }
            }
            None => return (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response(),
        },
    };
    let found = blocking(move || app.repo.with_meta(|m| m.token_user(&hash))).await;
    match found {
        Ok(Some(user)) => {
            req.extensions_mut().insert(Caller { user });
            req.extensions_mut().insert(web::session::Presented(hash));
            next.run(req).await
        }
        Ok(None) => (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response(),
        Err(e) => e.into_response(),
    }
}

async fn check_namespace(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Path(params): Path<Vec<(String, String)>>,
    mut req: Request,
    next: Next,
) -> Response {
    let ns = params
        .iter()
        .find(|(k, _)| k == "ns")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let user = caller.user.clone();
    let name = ns.clone();
    match blocking(move || app.repo.with_meta(|m| m.access(&name, &user))).await {
        Ok(Some(access)) => {
            req.extensions_mut().insert(Allowed(access));
            req.extensions_mut()
                .insert(web::View::member(&ns, &caller.user, access));
            next.run(req).await
        }
        // The same answer whether it exists or not.
        Ok(None) => (
            StatusCode::NOT_FOUND,
            format!("no folder {ns:?} for user {:?}", caller.user),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

async fn info(
    State(app): State<Arc<App>>,
    Extension(allowed): Extension<Allowed>,
    Path(ns): Path<String>,
) -> ApiResult<Json<Info>> {
    let head = blocking(move || app.repo.head(&ns)).await?;
    Ok(Json(Info {
        protocol: cairn_protocol::PROTOCOL_VERSION,
        server_version: env!("CARGO_PKG_VERSION").into(),
        head,
        max_batch_bytes: MAX_CHUNK_BATCH_BYTES as u64,
        access: Some(allowed.0),
    }))
}

/// Who the token is, and every namespace it may reach: what a client offers to sync.
async fn me(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Me>> {
    let user = caller.user.clone();
    let folders = blocking(move || app.repo.with_meta(|m| m.folders_of(&user))).await?;
    Ok(Json(Me {
        user: caller.user,
        folders,
    }))
}

async fn missing(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(allowed): Extension<Allowed>,
    Path(ns): Path<String>,
    Json(req): Json<HashList>,
) -> ApiResult<Response> {
    // Only what is about to be uploaded is asked about.
    if let Some(r) = allowed.refuses(Access::Write, &ns, &caller) {
        return Ok(r);
    }
    let hashes = blocking(move || app.repo.missing(&ns, &req.hashes)).await?;
    Ok(Json(HashList { hashes }).into_response())
}

async fn upload(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(allowed): Extension<Allowed>,
    Path(ns): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    if let Some(r) = allowed.refuses(Access::Write, &ns, &caller) {
        return Ok(r);
    }
    let stored = blocking(move || {
        let Ok(frames) = wire::read_frames(&body) else {
            return Ok(false);
        };
        // Synced together, and every one durable before the answer.
        app.repo.chunks(&ns)?.put_all(&frames)?;
        Ok(true)
    })
    .await?;
    Ok(if stored {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::BAD_REQUEST
    }
    .into_response())
}

async fn download(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
    Json(req): Json<HashList>,
) -> ApiResult<Response> {
    let body = blocking(move || {
        let store = app.repo.chunks(&ns)?;
        let mut out = Vec::new();
        for hash in &req.hashes {
            match store.get(hash)? {
                Some(data) => wire::write_frame(&mut out, hash, &data),
                None => return Ok(Err(*hash)),
            }
        }
        Ok(Ok(out))
    })
    .await?;
    Ok(match body {
        Ok(bytes) => bytes.into_response(),
        Err(hash) => (StatusCode::NOT_FOUND, format!("no chunk {hash}")).into_response(),
    })
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

async fn commit(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(allowed): Extension<Allowed>,
    Path(ns): Path<String>,
    Json(req): Json<CommitRequest>,
) -> ApiResult<Response> {
    if let Some(r) = allowed.refuses(Access::Write, &ns, &caller) {
        return Ok(r);
    }
    let repo = app.clone();
    let result = blocking(move || repo.repo.commit(&ns, &req, now(), Some(&caller.user))).await?;
    // Durable by now, so a device that wakes reads it. A retried commit wakes them for nothing.
    app.commits.send_modify(|n| *n = n.wrapping_add(1));
    // The acknowledgement goes out only now that the commit is durable.
    cairn_failpoint::hit("server.commit.before_ack");
    Ok(Json(result).into_response())
}

#[derive(Deserialize)]
struct ChangesQuery {
    #[serde(default)]
    after: u64,
    limit: Option<u32>,
    /// Hold the answer for up to this many seconds until the head is no longer `after`.
    #[serde(default)]
    wait: u64,
    /// `false`: each entry without its operations, for a client that only checks where it is.
    #[serde(default = "yes")]
    ops: bool,
}

fn yes() -> bool {
    true
}

/// Until the head differs from `after`, the time is up, or the server is stopping. A head behind
/// `after` also answers at once: that device is ahead of a server restored from a backup.
async fn wait_for_change(app: &Arc<App>, ns: &str, after: u64, secs: u64) -> ApiResult<()> {
    // Subscribed before the head is read, so a commit between the two still wakes this.
    let mut commits = app.commits.subscribe();
    let mut closing = app.closing.subscribe();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let (repo, name) = (app.clone(), ns.to_string());
        if blocking(move || repo.repo.head(&name)).await? != after || *closing.borrow() {
            return Ok(());
        }
        tokio::select! {
            _ = commits.changed() => {}
            _ = closing.changed() => {}
            _ = tokio::time::sleep_until(deadline) => return Ok(()),
        }
    }
}

async fn changes(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
    Query(q): Query<ChangesQuery>,
) -> ApiResult<Response> {
    let limit = q.limit.unwrap_or(1000).min(10_000);
    if q.wait > 0 {
        wait_for_change(&app, &ns, q.after, q.wait.min(MAX_WAIT_SECS)).await?;
    }
    let budget = cairn_failpoint::tuned("server.changes_bytes", CHANGES_BYTES as u64) as usize;
    let (changes, head) = blocking(move || {
        if q.ops {
            app.repo.changes(&ns, q.after, limit, budget)
        } else {
            app.repo.changes_without_ops(&ns, q.after, limit)
        }
    })
    .await?;
    cairn_failpoint::hit("server.changes.answered");
    // A proxy that caches must not answer a later ask from this one.
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(ChangesResponse { changes, head }),
    )
        .into_response())
}

async fn conflicts(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
) -> ApiResult<Json<Vec<wire::ConflictEntry>>> {
    let list = blocking(move || app.repo.conflicts(&ns)).await?;
    Ok(Json(list))
}

async fn members(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
) -> ApiResult<Json<Vec<Member>>> {
    Ok(Json(
        blocking(move || app.repo.with_meta(|m| m.members(&ns))).await?,
    ))
}

/// A manager changes who has access, but never so that nobody manages the folder any more: the
/// server's administrator could mend that, a manager could not.
async fn set_member(
    app: Arc<App>,
    caller: Caller,
    allowed: Allowed,
    ns: String,
    user: String,
    access: Option<Access>,
) -> ApiResult<Response> {
    if let Some(r) = allowed.refuses(Access::Manage, &ns, &caller) {
        return Ok(r);
    }
    let answer = blocking(move || {
        app.repo.with_meta(|m| {
            if !m.users()?.iter().any(|u| u.name == user) {
                return Ok((StatusCode::NOT_FOUND, format!("no user {user:?}")).into_response());
            }
            let members = m.members(&ns)?;
            let now = members.iter().find(|x| x.user == user).map(|x| x.access);
            if access.is_none() && now.is_none() {
                return Ok((
                    StatusCode::NOT_FOUND,
                    format!("user {user:?} has no access to folder {ns:?}"),
                )
                    .into_response());
            }
            let managers = members.iter().filter(|x| x.access == Access::Manage).count();
            if now == Some(Access::Manage) && access != Some(Access::Manage) && managers == 1 {
                return Ok((
                    StatusCode::CONFLICT,
                    format!("user {user:?} is the last manager of folder {ns:?}: make another one first"),
                )
                    .into_response());
            }
            m.set_access(&ns, &user, access)?;
            Ok(StatusCode::NO_CONTENT.into_response())
        })
    })
    .await?;
    Ok(answer)
}

async fn grant(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(allowed): Extension<Allowed>,
    Path((ns, user)): Path<(String, String)>,
    Json(grant): Json<Grant>,
) -> ApiResult<Response> {
    set_member(app, caller, allowed, ns, user, Some(grant.access)).await
}

async fn revoke(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(allowed): Extension<Allowed>,
    Path((ns, user)): Path<(String, String)>,
) -> ApiResult<Response> {
    set_member(app, caller, allowed, ns, user, None).await
}

fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/info", get(info))
        .route("/chunks/missing", post(missing))
        .route("/chunks/upload", post(upload))
        .route("/chunks/download", post(download))
        .route("/commit", post(commit))
        .route("/changes", get(changes))
        .route("/conflicts", get(conflicts))
        .route("/access", get(members))
        .route("/access/{user}", put(grant).delete(revoke))
        .layer(DefaultBodyLimit::max(MAX_CHUNK_BATCH_BYTES * 2))
        .merge(web::namespace_routes())
        .route_layer(middleware::from_fn_with_state(app.clone(), check_namespace))
        .with_state(app.clone());
    Router::new()
        .route("/v1/me", get(me))
        .with_state(app.clone())
        .nest("/v1/ns/{ns}", api)
        .merge(web::user_routes(&app))
        .fallback(|| async { (StatusCode::NOT_FOUND, "no such endpoint") })
        .layer(middleware::from_fn_with_state(app.clone(), authorize))
        // After the layer, so outside it: what answers without a token.
        .merge(web::public_routes(&app))
        .layer(middleware::map_response(web::no_referrer))
}

#[derive(Parser)]
#[command(name = "cairn-server", version, about = "The Cairn file sync server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a data directory, with the user `admin`, who manages the folder `default`, and that
    /// user's token in the directory's `token` file.
    Init {
        #[arg(long)]
        data: PathBuf,
    },
    /// Users, each with tokens of their own and access to the folders given them. These commands
    /// work while the server runs, which sees each change on its next request.
    User {
        #[command(subcommand)]
        what: admin::UserCommand,
    },
    /// Tokens: one per device is best, so that a lost one is taken back alone.
    Token {
        #[command(subcommand)]
        what: admin::TokenCommand,
    },
    /// Folders, each its own tree, history and set of chunks, synced by a client as one root.
    Folder {
        #[command(subcommand)]
        what: admin::FolderCommand,
    },
    /// Give a user access to a folder: read, write, or manage, which is write and also saying who
    /// else has access. Replaces the level the user had.
    Grant {
        folder: String,
        user: String,
        access: Access,
        #[arg(long)]
        data: PathBuf,
    },
    /// Take all of a user's access to a folder away. What the user's devices hold of it stays on them.
    Revoke {
        folder: String,
        user: String,
        #[arg(long)]
        data: PathBuf,
    },
    /// Serve a data directory.
    Serve {
        #[arg(long)]
        data: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8733")]
        listen: SocketAddr,
        /// Listen beyond loopback. This server speaks plain HTTP, so the token and all data need a
        /// TLS-terminating reverse proxy in front of it.
        #[arg(long)]
        behind_tls_proxy: bool,
        /// The address public links are opened at, such as a proxy's port that passes only them, open
        /// to anyone while the server's own address is not: each link is this followed by `/s/<link>`.
        #[arg(long, value_name = "URL")]
        link_url: Option<String>,
    },
    /// Check every chunk hash, every manifest reference, and the tables against the journal.
    Fsck {
        #[arg(long)]
        data: PathBuf,
    },
}

/// A new file only this user reads, holding `text`, synced. Fails if the file is there already.
pub(crate) fn write_secret(path: &FsPath, text: &str) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    // Windows has no mode: who reads the token is up to the directory's ACL.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

pub(crate) fn new_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

fn init(data: &FsPath) -> Result<()> {
    std::fs::create_dir_all(data)?;
    let token_path = data.join("token");
    if token_path.exists() {
        bail!("{} already has a token", data.display());
    }
    if data.join("meta.sqlite").exists() {
        bail!(
            "{} already holds a server's data: add users and tokens with `cairn-server user add` and `cairn-server token add`",
            data.display()
        );
    }
    // The token first: the database, made with accounts, makes it the first user's.
    write_secret(&token_path, &new_token())?;
    Repo::open(data)?;
    println!("initialized {}", data.display());
    println!(
        "user {} manages folder default; its token file: {}",
        cairn_store::FIRST_USER,
        token_path.display()
    );
    Ok(())
}

async fn shutdown_signal(app: Arc<App>) {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    // Windows has no SIGTERM: a console's Ctrl-C or Ctrl-Break, or a kill.
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    app.closing.send_replace(true);
}

/// A `--link-url` as links are made from it: `http(s)://host[:port][/path]`, without a trailing `/`.
fn link_base(url: &str) -> Result<String> {
    let base = url.trim().trim_end_matches('/');
    let rest = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .unwrap_or("");
    if rest.is_empty()
        || rest.starts_with('/')
        || base.contains(['?', '#', ' ', '"', '<', '>', '\\'])
    {
        bail!("--link-url {url:?} is not an http:// or https:// address, such as https://files.example:8443");
    }
    Ok(base.to_string())
}

async fn serve(
    data: PathBuf,
    listen: SocketAddr,
    behind_tls_proxy: bool,
    link_url: Option<String>,
) -> Result<()> {
    let link_url = link_url.as_deref().map(link_base).transpose()?;
    if !listen.ip().is_loopback() && !behind_tls_proxy {
        bail!(
            "{listen} is beyond loopback and this server speaks plain HTTP: put a TLS proxy in front and pass --behind-tls-proxy"
        );
    }
    if !data.join("meta.sqlite").exists() && !data.join("token").exists() {
        bail!(
            "{} holds no server: run `cairn-server init` first",
            data.display()
        );
    }
    let repo = Repo::open(&data)?;
    if repo.with_meta(|m| m.users())?.is_empty() {
        eprintln!(
            "cairn-server: there are no users, so every request is refused: add one with `cairn-server user add`"
        );
    }
    let app = Arc::new(App {
        repo,
        uploads: web::upload::Uploads::open(&data)?,
        sessions: web::session::Sessions::open(&data)?,
        deleted: web::deleted::Indexes::default(),
        link_url,
        commits: tokio::sync::watch::channel(0).0,
        closing: tokio::sync::watch::channel(false).0,
    });
    // Uploads given up on are removed, with their bytes.
    let sweeper = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            let app = sweeper.clone();
            let _ = tokio::task::spawn_blocking(move || app.uploads.sweep()).await;
        }
    });
    // What is deleted, replayed from every namespace's journal now rather than when someone first
    // asks, and kept up after each commit, a page at a time while devices sync.
    let keeper = app.clone();
    tokio::spawn(async move {
        let mut commits = keeper.commits.subscribe();
        let mut every_namespace = true;
        loop {
            let app = keeper.clone();
            let kept =
                tokio::task::spawn_blocking(move || web::deleted::keep_up(&app, every_namespace));
            match kept.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("error: {e:#}"),
                Err(e) => eprintln!("error: the index of what is deleted: {e}"),
            }
            every_namespace = false;
            if commits.changed().await.is_err() {
                break;
            }
        }
    });
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    println!("listening on {}", listener.local_addr()?);
    std::io::stdout().flush()?;
    axum::serve(listener, router(app.clone()))
        .with_graceful_shutdown(shutdown_signal(app))
        .await?;
    Ok(())
}

/// The manifest a web upload makes of these bytes: for the failure tests, which hold it to a client's.
#[doc(hidden)]
pub fn web_manifest(data: &[u8]) -> cairn_protocol::Manifest {
    web::upload::chunk_file(data, data.len() as u64, &mut |_, _| Ok(()))
        .expect("bytes in memory are read whole")
}

/// The whole server program; `main` of the release binary and of the harness's build.
pub fn run() -> std::process::ExitCode {
    match run_inner() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("cairn-server: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_inner() -> Result<std::process::ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { data } => init(&data)?,
        Command::User { what } => admin::user(what)?,
        Command::Token { what } => admin::token(what)?,
        Command::Folder { what } => admin::folder(what)?,
        Command::Grant {
            folder,
            user,
            access,
            data,
        } => admin::grant(&data, &folder, &user, Some(access))?,
        Command::Revoke { folder, user, data } => admin::grant(&data, &folder, &user, None)?,
        Command::Serve {
            data,
            listen,
            behind_tls_proxy,
            link_url,
        } => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(serve(data, listen, behind_tls_proxy, link_url))?;
        }
        Command::Fsck { data } => {
            let repo = Repo::open(&data)?;
            let report = cairn_store::fsck::fsck(&repo)?;
            println!(
                "{} namespace(s), {} commits, {} revisions, {} chunks, {} unreferenced",
                report.namespaces,
                report.commits,
                report.revisions,
                report.chunks,
                report.unreferenced
            );
            for e in &report.errors {
                println!("error: {e}");
            }
            if !report.is_clean() {
                println!("{} error(s)", report.errors.len());
                return Ok(std::process::ExitCode::FAILURE);
            }
            println!("clean");
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::link_base;

    #[test]
    fn a_link_url_is_an_address_links_go_on_from() {
        assert_eq!(
            link_base("https://files.example:8443/").unwrap(),
            "https://files.example:8443"
        );
        assert_eq!(link_base(" http://h/share ").unwrap(), "http://h/share");
        for bad in [
            "files.example",
            "https://",
            "https:///x",
            "ftp://h",
            "https://h/?a",
            "https://h#x",
        ] {
            assert!(link_base(bad).is_err(), "{bad}");
        }
    }
}
