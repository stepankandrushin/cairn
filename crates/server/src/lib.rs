//! The Cairn server: Axum over the store. Blocking storage work runs off the async threads.

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cairn_protocol::wire::{
    self, ChangesResponse, CommitRequest, HashList, Info, MAX_CHUNK_BATCH_BYTES,
};
use cairn_store::{is_disk_full, ChunkStore, Repo};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

struct App {
    repo: Repo,
    token: String,
}

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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn authorize(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), app.token.as_bytes()) => {
            next.run(req).await
        }
        _ => (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response(),
    }
}

async fn check_namespace(
    State(app): State<Arc<App>>,
    Path(params): Path<Vec<(String, String)>>,
    req: Request,
    next: Next,
) -> Response {
    let ns = params
        .iter()
        .find(|(k, _)| k == "ns")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    match app.repo.namespace_exists(&ns) {
        Ok(true) => next.run(req).await,
        Ok(false) => (StatusCode::NOT_FOUND, format!("no namespace {ns:?}")).into_response(),
        Err(e) => ApiError(e).into_response(),
    }
}

async fn info(State(app): State<Arc<App>>, Path(ns): Path<String>) -> ApiResult<Json<Info>> {
    let head = blocking(move || app.repo.head(&ns)).await?;
    Ok(Json(Info {
        protocol: cairn_protocol::PROTOCOL_VERSION,
        server_version: env!("CARGO_PKG_VERSION").into(),
        head,
        max_batch_bytes: MAX_CHUNK_BATCH_BYTES as u64,
    }))
}

async fn missing(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
    Json(req): Json<HashList>,
) -> ApiResult<Json<HashList>> {
    let hashes = blocking(move || app.repo.missing(&ns, &req.hashes)).await?;
    Ok(Json(HashList { hashes }))
}

async fn upload(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    if wire::read_frames(&body).is_err() {
        return Ok(StatusCode::BAD_REQUEST);
    }
    blocking(move || {
        let store = app.repo.chunks(&ns)?;
        for (hash, data) in wire::read_frames(&body)? {
            store.put(&hash, data)?;
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
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
    Path(ns): Path<String>,
    Json(req): Json<CommitRequest>,
) -> ApiResult<Response> {
    let result = blocking(move || app.repo.commit(&ns, &req, now())).await?;
    // The acknowledgement goes out only now that the commit is durable.
    cairn_failpoint::hit("server.commit.before_ack");
    Ok(Json(result).into_response())
}

#[derive(Deserialize)]
struct ChangesQuery {
    #[serde(default)]
    after: u64,
    limit: Option<u32>,
}

async fn changes(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
    Query(q): Query<ChangesQuery>,
) -> ApiResult<Json<ChangesResponse>> {
    let limit = q.limit.unwrap_or(1000).min(10_000);
    let (changes, head) = blocking(move || app.repo.changes(&ns, q.after, limit)).await?;
    Ok(Json(ChangesResponse { changes, head }))
}

async fn conflicts(
    State(app): State<Arc<App>>,
    Path(ns): Path<String>,
) -> ApiResult<Json<Vec<wire::ConflictEntry>>> {
    let list = blocking(move || app.repo.conflicts(&ns)).await?;
    Ok(Json(list))
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
        .route_layer(middleware::from_fn_with_state(app.clone(), check_namespace))
        .layer(DefaultBodyLimit::max(MAX_CHUNK_BATCH_BYTES * 2))
        .with_state(app.clone());
    Router::new()
        .nest("/v1/ns/{ns}", api)
        .layer(middleware::from_fn_with_state(app, authorize))
}

#[derive(Parser)]
#[command(name = "cairn-server", version, about = "The Cairn file sync server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a data directory and its access token.
    Init {
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
    },
    /// Check every chunk hash, every manifest reference, and the tables against the journal.
    Fsck {
        #[arg(long)]
        data: PathBuf,
    },
}

fn init(data: &FsPath) -> Result<()> {
    std::fs::create_dir_all(data)?;
    let token_path = data.join("token");
    if token_path.exists() {
        bail!("{} already has a token", data.display());
    }
    Repo::open(data)?;
    let token = hex::encode(rand::random::<[u8; 32]>());
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&token_path)?;
    f.write_all(token.as_bytes())?;
    f.sync_all()?;
    println!("initialized {}", data.display());
    println!("token file: {}", token_path.display());
    Ok(())
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

async fn serve(data: PathBuf, listen: SocketAddr, behind_tls_proxy: bool) -> Result<()> {
    if !listen.ip().is_loopback() && !behind_tls_proxy {
        bail!(
            "{listen} is beyond loopback and this server speaks plain HTTP: put a TLS proxy in front and pass --behind-tls-proxy"
        );
    }
    let token = std::fs::read_to_string(data.join("token"))
        .with_context(|| {
            format!(
                "read {}: run `cairn-server init` first",
                data.join("token").display()
            )
        })?
        .trim()
        .to_string();
    let repo = Repo::open(&data)?;
    let app = Arc::new(App { repo, token });
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    println!("listening on {}", listener.local_addr()?);
    std::io::stdout().flush()?;
    axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
        Command::Serve {
            data,
            listen,
            behind_tls_proxy,
        } => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(serve(data, listen, behind_tls_proxy))?;
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
