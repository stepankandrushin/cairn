//! Resumable uploads for `PUT /file` (docs/protocol.md "Resumable uploads"), as the IETF's resumable
//! uploads draft says, in the interop versions the phones' systems speak: 3 (draft -01, `Upload-Incomplete`)
//! and 5 and 6 (drafts -03 to -05, `Upload-Complete`). A request that asks for it is told where it can be
//! resumed by a 104 while its body comes, its bytes are received into a file of their own, and once the
//! last of them is in they are stored and committed as any `PUT /file` is, with the same `op_id`: so a
//! video cut off at 90% sends its last 10% only. What is waiting is kept in memory and in
//! `web/resumable/` which a start empties: after a restart a client starts again from nothing.

use crate::conn::Interim;
use crate::file::{commit_file, failed, refuse, Asked};
use crate::web::upload::store_bytes;
use crate::{App, Caller};
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use cairn_protocol::RevisionId;
use cairn_store::chunks::Held;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

/// The interop versions spoken here.
pub(crate) const INTEROP: [&str; 3] = ["3", "5", "6"];
pub(crate) const VERSION: &str = "upload-draft-interop-version";
const OFFSET: &str = "upload-offset";
const COMPLETE: &str = "upload-complete";
const INCOMPLETE: &str = "upload-incomplete";
const LENGTH: &str = "upload-length";
/// An upload left alone this long is given up; one committed is remembered this long, for a client
/// asking where it stands after its answer was lost.
const IDLE: Duration = Duration::from_secs(24 * 3600);
const DONE: Duration = Duration::from_secs(3600);

/// The draft's interop version a request names, if it is one spoken here.
pub(crate) fn interop(headers: &HeaderMap) -> Option<&'static str> {
    let v = headers.get(VERSION)?.to_str().ok()?.trim();
    INTEROP.iter().copied().find(|&known| known == v)
}

fn sf_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    match headers.get(name)?.as_bytes() {
        b"?1" => Some(true),
        b"?0" => Some(false),
        _ => None,
    }
}

/// Whether the request says its body ends the upload, as its version says it; `None` if it says
/// nothing of it, which makes it no resumable upload.
pub(crate) fn completes(headers: &HeaderMap, version: &str) -> Option<bool> {
    if version == "3" {
        sf_bool(headers, INCOMPLETE).map(|more| !more)
    } else {
        sf_bool(headers, COMPLETE)
    }
}

/// The upload's whole length, when the client says it.
pub(crate) fn length(headers: &HeaderMap) -> Option<u64> {
    headers.get(LENGTH)?.to_str().ok()?.trim().parse().ok()
}

fn offset(headers: &HeaderMap) -> Option<u64> {
    headers.get(OFFSET)?.to_str().ok()?.trim().parse().ok()
}

struct Pending {
    /// What the creating request asked, its size the bytes received so far.
    asked: Asked,
    version: &'static str,
    spool: PathBuf,
    length: Option<u64>,
    /// Moved by each request that sends bytes, so that one given up for a newer writes nothing more.
    turn: u64,
    touched: Instant,
    /// Committed: whole, and nothing more taken.
    done: bool,
}

/// The uploads that may be resumed, by their revision, each one user's.
pub(crate) struct Resumable {
    dir: PathBuf,
    map: Mutex<HashMap<RevisionId, Arc<tokio::sync::Mutex<Pending>>>>,
}

impl Resumable {
    pub(crate) fn open(data: &FsPath) -> Result<Self> {
        let dir = data.join("web").join("resumable");
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("empty {}", dir.display()))?;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Resumable {
            dir,
            map: Mutex::new(HashMap::new()),
        })
    }

    fn get(&self, id: &RevisionId) -> Option<Arc<tokio::sync::Mutex<Pending>>> {
        self.map
            .lock()
            .expect("a panic aborts the process")
            .get(id)
            .cloned()
    }

    fn forget(&self, id: &RevisionId) {
        self.map
            .lock()
            .expect("a panic aborts the process")
            .remove(id);
        let _ = std::fs::remove_file(self.dir.join(id.to_string()));
    }

    /// Gives up what was left alone too long, and forgets what was committed a while ago.
    pub(crate) async fn sweep(&self) {
        let all: Vec<_> = self
            .map
            .lock()
            .expect("a panic aborts the process")
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        for (id, p) in all {
            let Ok(p) = p.try_lock() else { continue };
            let age = p.touched.elapsed();
            if age > IDLE || (p.done && age > DONE) {
                drop(p);
                self.forget(&id);
            }
        }
    }
}

fn location(a: &Asked) -> String {
    format!("/v1/ns/{}/file/uploads/{}", a.ns, a.q.revision)
}

/// The headers every answer about an upload carries: its version, where it stands, and whether it is
/// whole, as that version says it.
fn standing(r: &mut Response, version: &str, received: u64, complete: Option<bool>) {
    let h = r.headers_mut();
    let mut put = |k: &'static str, v: String| {
        if let Ok(v) = HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
    };
    put(VERSION, version.to_string());
    put(OFFSET, received.to_string());
    if let Some(whole) = complete {
        if version == "3" {
            put(INCOMPLETE, if whole { "?0" } else { "?1" }.into());
        } else {
            put(COMPLETE, if whole { "?1" } else { "?0" }.into());
        }
    }
}

/// Appends the body to the upload's file, as long as it is this request's turn; how much came, and
/// whether the body ended as sent.
async fn receive(
    pending: &tokio::sync::Mutex<Pending>,
    turn: u64,
    body: Body,
    mut interim: Option<(Interim, String)>,
) -> Result<bool> {
    let spool = pending.lock().await.spool.clone();
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&spool)
        .await
        .with_context(|| format!("open {}", spool.display()))?;
    let mut stream = body.into_data_stream();
    while let Some(next) = stream.next().await {
        let Ok(bytes) = next else {
            return Ok(false);
        };
        {
            let mut p = pending.lock().await;
            if p.turn != turn {
                // A newer request goes on from where this one stood: nothing more of this one.
                return Ok(false);
            }
            file.write_all(&bytes).await?;
            p.asked.size += bytes.len() as u64;
            p.touched = Instant::now();
        }
        // Where it can be resumed, once the first of the body has come, after any 100 Continue.
        if let Some((i, at)) = interim.take() {
            let version = pending.lock().await.version;
            if let Err(e) = i
                .send(
                    104,
                    "Upload Resumption Supported",
                    &[("Upload-Draft-Interop-Version", version), ("Location", &at)],
                )
                .await
            {
                eprintln!("cairn-server: the 104 of {at}: {e}");
            }
        }
    }
    file.flush().await?;
    Ok(true)
}

/// The upload's bytes stored and committed, once all of them are in; the answer, kept for a client
/// that asks again.
async fn finish(app: Arc<App>, pending: Arc<tokio::sync::Mutex<Pending>>) -> Response {
    let (asked, version, spool) = {
        let p = pending.lock().await;
        (p.asked.clone(), p.version, p.spool.clone())
    };
    let received = asked.size;
    let a = Arc::new(asked);
    let (app2, a2) = (app.clone(), a.clone());
    let stored = tokio::task::spawn_blocking(move || -> Result<(cairn_protocol::Hash, Held)> {
        let chunks = app2.repo.chunks(&a2.ns)?;
        let file = std::fs::File::open(&spool)?;
        store_bytes(&chunks, std::io::BufReader::new(file), a2.size, &mut |_| {})
    })
    .await;
    // Held until the commit returns, as any upload's chunks are.
    let (manifest, held) = match stored {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => return failed(e),
        Err(e) => return failed(e.into()),
    };
    cairn_failpoint::hit("server.resumable.stored");
    let mut answer = commit_file(app.clone(), a.clone(), manifest, held).await;
    standing(&mut answer, version, received, None);
    if answer.status().is_success() {
        {
            let mut p = pending.lock().await;
            p.done = true;
            p.touched = Instant::now();
        }
        let _ = std::fs::remove_file(app.resumable.dir.join(a.q.revision.to_string()));
    }
    answer
}

/// A `PUT /file` that asks to be resumable: its upload made, told where it is by a 104, and received.
pub(crate) async fn create(
    app: Arc<App>,
    asked: Asked,
    version: &'static str,
    complete: bool,
    length: Option<u64>,
    interim: Option<Interim>,
    body: Body,
) -> Response {
    let id = asked.q.revision;
    let spool = app.resumable.dir.join(id.to_string());
    if let Err(e) = std::fs::File::create(&spool) {
        return failed(e.into());
    }
    let at = location(&asked);
    let mut asked = asked;
    asked.size = 0;
    let pending = Arc::new(tokio::sync::Mutex::new(Pending {
        asked,
        version,
        spool,
        length,
        turn: 1,
        touched: Instant::now(),
        done: false,
    }));
    app.resumable
        .map
        .lock()
        .expect("a panic aborts the process")
        .insert(id, pending.clone());
    let whole = match receive(&pending, 1, body, interim.map(|i| (i, at.clone()))).await {
        Ok(w) => w,
        Err(e) => return failed(e),
    };
    respond(app, pending, whole, complete, Some(at)).await
}

/// After a request's bytes are in: committed if it ended the upload, else where it stands.
async fn respond(
    app: Arc<App>,
    pending: Arc<tokio::sync::Mutex<Pending>>,
    whole: bool,
    complete: bool,
    at: Option<String>,
) -> Response {
    let (version, received, length) = {
        let p = pending.lock().await;
        (p.version, p.asked.size, p.length)
    };
    if whole && complete {
        if length.is_some_and(|l| l != received) {
            let mut r = refuse(
                StatusCode::BAD_REQUEST,
                format!(
                    "the upload is {received} bytes, not the {} it said",
                    length.unwrap_or(0)
                ),
            );
            standing(&mut r, version, received, Some(false));
            return r;
        }
        let mut r = finish(app, pending).await;
        if let Some(at) = at {
            if let Ok(v) = HeaderValue::from_str(&at) {
                r.headers_mut().insert(axum::http::header::LOCATION, v);
            }
        }
        return r;
    }
    // Not whole yet: where it stands, to go on from.
    let mut r = StatusCode::CREATED.into_response();
    if let Some(at) = at {
        if let Ok(v) = HeaderValue::from_str(&at) {
            r.headers_mut().insert(axum::http::header::LOCATION, v);
        }
    }
    standing(&mut r, version, received, None);
    r
}

#[derive(serde::Deserialize)]
pub(crate) struct UploadPath {
    ns: String,
    id: String,
}

/// The caller's upload of this ID in this namespace: another's is not there.
fn mine(
    app: &App,
    caller: &Caller,
    p: &UploadPath,
) -> Option<(RevisionId, Arc<tokio::sync::Mutex<Pending>>)> {
    let id: RevisionId = p.id.parse().ok()?;
    let pending = app.resumable.get(&id)?;
    let ok = pending
        .try_lock()
        .map(|g| g.asked.user == caller.user && g.asked.ns == p.ns)
        .unwrap_or(true);
    ok.then_some((id, pending))
}

fn not_there() -> Response {
    refuse(StatusCode::NOT_FOUND, "no such upload: start it again")
}

fn has_upload_headers(headers: &HeaderMap) -> bool {
    [OFFSET, COMPLETE, INCOMPLETE, LENGTH]
        .iter()
        .any(|h| headers.contains_key(*h))
}

/// `HEAD …/file/uploads/{id}`: where an upload stands.
pub(crate) async fn offset_of(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Path(p): Path<UploadPath>,
    headers: HeaderMap,
) -> Response {
    if has_upload_headers(&headers) {
        return refuse(
            StatusCode::BAD_REQUEST,
            "an offset is asked for with no upload headers",
        );
    }
    let Some((_, pending)) = mine(&app, &caller, &p) else {
        return not_there();
    };
    let p = pending.lock().await;
    if p.asked.user != caller.user {
        return not_there();
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    standing(&mut r, p.version, p.asked.size, Some(p.done));
    if let Some(l) = p.length {
        if let Ok(v) = HeaderValue::from_str(&l.to_string()) {
            r.headers_mut().insert(LENGTH, v);
        }
    }
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    r
}

/// `PATCH …/file/uploads/{id}`: more of an upload, from where it stands.
pub(crate) async fn append(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Path(p): Path<UploadPath>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some((_, pending)) = mine(&app, &caller, &p) else {
        return not_there();
    };
    let (turn, version) = {
        let mut g = pending.lock().await;
        if g.asked.user != caller.user {
            return not_there();
        }
        let version = g.version;
        let Some(complete) = completes(&headers, version) else {
            return refuse(
                StatusCode::BAD_REQUEST,
                "an append says whether it ends the upload",
            );
        };
        if g.done {
            let mut r = refuse(StatusCode::BAD_REQUEST, "that upload is complete");
            standing(&mut r, version, g.asked.size, Some(true));
            return r;
        }
        if offset(&headers) != Some(g.asked.size) {
            let mut r = refuse(
                StatusCode::CONFLICT,
                format!("the upload stands at {}", g.asked.size),
            );
            standing(&mut r, version, g.asked.size, Some(false));
            return r;
        }
        if let Some(l) = length(&headers) {
            g.length = Some(l);
        }
        g.turn += 1;
        g.touched = Instant::now();
        ((g.turn, complete), version)
    };
    let (turn, complete) = turn;
    let _ = version;
    let whole = match receive(&pending, turn, body, None).await {
        Ok(w) => w,
        Err(e) => return failed(e),
    };
    respond(app, pending, whole, complete, None).await
}

/// `DELETE …/file/uploads/{id}`: given up, and its bytes removed.
pub(crate) async fn cancel(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Path(p): Path<UploadPath>,
    headers: HeaderMap,
) -> Response {
    if headers.contains_key(OFFSET)
        || headers.contains_key(COMPLETE)
        || headers.contains_key(INCOMPLETE)
    {
        return refuse(StatusCode::BAD_REQUEST, "a cancel has no upload headers");
    }
    let Some((id, pending)) = mine(&app, &caller, &p) else {
        return not_there();
    };
    {
        let mut g = pending.lock().await;
        if g.asked.user != caller.user {
            return not_there();
        }
        g.turn += 1;
    }
    app.resumable.forget(&id);
    StatusCode::NO_CONTENT.into_response()
}
