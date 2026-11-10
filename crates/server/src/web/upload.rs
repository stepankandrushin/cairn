//! Uploads from the web, in parts small enough for any proxy: received into a file of their own, then
//! cut and hashed as a client reads a file without holes, the chunks made durable, and committed against
//! what the browser saw, so nothing it did not see is replaced (invariants 1 and 3).

use super::edit::{commit, folder_in_view, now_ts};
use super::{bad_request, conflict, not_found, View};
use crate::{blocking, ApiResult, App};
use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use cairn_protocol::manifest::{CHUNK_AVG, CHUNK_MAX, CHUNK_MIN};
use cairn_protocol::web::{self as w, UploadState};
use cairn_protocol::wire::Access;
use cairn_protocol::{
    ChunkRef, Content, Hash, Link, Manifest, Metadata, ObjectId, ObjectKind, Op, RevisionBody,
    RevisionId, Segment,
};
use cairn_store::ChunkStore;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// An upload nobody has touched for this long is given up, its bytes removed.
const IDLE: Duration = Duration::from_secs(6 * 3600);
/// A finished one is kept this long for its status to be read.
const KEEP_DONE: Duration = Duration::from_secs(600);
/// Chunks go to the store in batches of about this much, each durable before the next.
const BATCH_BYTES: usize = 32 * 1024 * 1024;

struct Upload {
    /// The view it was begun in: only that view sees it.
    ns: String,
    root: ObjectId,
    user: Option<String>,
    start: w::UploadStart,
    file: PathBuf,
    received: u64,
    state: UploadState,
    stored: u64,
    done: Option<w::UploadDone>,
    error: Option<String>,
    touched: Instant,
}

impl Upload {
    fn status(&self, id: &str) -> w::UploadStatus {
        w::UploadStatus {
            id: id.to_string(),
            size: self.start.size,
            received: self.received,
            state: self.state,
            stored: self.stored,
            done: self.done.clone(),
            error: self.error.clone(),
        }
    }

    fn sees(&self, view: &View) -> bool {
        self.ns == view.ns && self.root == view.root && self.user == view.user
    }
}

/// The uploads in progress, by ID, and the folder their bytes are in.
pub(crate) struct Uploads {
    dir: PathBuf,
    map: Mutex<HashMap<String, Arc<Mutex<Upload>>>>,
}

impl Uploads {
    /// Under the data directory, emptied: an upload from before a restart is begun again.
    pub(crate) fn open(data: &FsPath) -> Result<Self> {
        let dir = data.join("web").join("uploads");
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("empty {}", dir.display()))?;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Uploads {
            dir,
            map: Mutex::new(HashMap::new()),
        })
    }

    fn get(&self, id: &str, view: &View) -> Option<Arc<Mutex<Upload>>> {
        let map = self.map.lock().expect("a panic aborts the process");
        let up = map.get(id)?.clone();
        let sees = up.lock().expect("a panic aborts the process").sees(view);
        sees.then_some(up)
    }

    fn forget(&self, id: &str) {
        let removed = self
            .map
            .lock()
            .expect("a panic aborts the process")
            .remove(id);
        if let Some(up) = removed {
            let up = up.lock().expect("a panic aborts the process");
            let _ = std::fs::remove_file(&up.file);
        }
    }

    /// Give up what has lain idle, and forget what finished a while ago.
    pub(crate) fn sweep(&self) {
        let ids: Vec<String> = {
            let map = self.map.lock().expect("a panic aborts the process");
            map.iter()
                .filter(|(_, up)| {
                    let up = up.lock().expect("a panic aborts the process");
                    let idle = up.touched.elapsed();
                    match up.state {
                        UploadState::Storing => false,
                        UploadState::Receiving => idle > IDLE,
                        UploadState::Done | UploadState::Failed => idle > KEEP_DONE,
                    }
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.forget(&id);
        }
    }
}

pub(super) async fn start(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Json(req): Json<w::UploadStart>,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Write) {
        return Ok(r);
    }
    let answer = blocking(move || {
        // Checked now so that a person hears at once; the commit checks again.
        let refused = app.repo.with_meta(|m| {
            if let Some(r) = folder_in_view(m, &view, req.parent)? {
                return Ok(Some(r));
            }
            let at = m
                .children(&view.ns, req.parent)?
                .into_iter()
                .find(|c| c.name == req.name);
            Ok(match (at, req.replace) {
                (None, None) => None,
                (Some(_), None) => Some(conflict("that name is taken")),
                (None, Some(_)) => Some(conflict("what it would replace is not there now: reload")),
                (Some(c), Some(rev)) if c.head.kind != ObjectKind::File => {
                    let _ = rev;
                    Some(conflict("only a file is replaced by an upload"))
                }
                (Some(c), Some(rev)) if c.head.revision != rev => Some(conflict(
                    "the file has changed since you looked: reload, and choose again",
                )),
                (Some(_), Some(_)) => None,
            })
        })?;
        if let Some(r) = refused {
            return Ok(r);
        }
        let id = hex::encode(rand::random::<[u8; 16]>());
        let file = app.uploads.dir.join(&id);
        std::fs::File::create(&file).with_context(|| format!("create {}", file.display()))?;
        let up = Upload {
            ns: view.ns.clone(),
            root: view.root,
            user: view.user.clone(),
            start: req,
            file,
            received: 0,
            state: UploadState::Receiving,
            stored: 0,
            done: None,
            error: None,
            touched: Instant::now(),
        };
        app.uploads
            .map
            .lock()
            .expect("a panic aborts the process")
            .insert(id.clone(), Arc::new(Mutex::new(up)));
        Ok((StatusCode::CREATED, Json(w::UploadId { id })).into_response())
    })
    .await?;
    Ok(answer)
}

#[derive(Deserialize)]
pub(super) struct UploadPath {
    upload: String,
}

#[derive(Deserialize)]
pub(super) struct PartQuery {
    offset: u64,
}

pub(super) async fn part(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<UploadPath>,
    Query(q): Query<PartQuery>,
    body: Bytes,
) -> ApiResult<Response> {
    let Some(up) = app.uploads.get(&p.upload, &view) else {
        return Ok(not_found("such upload"));
    };
    let answer = blocking(move || {
        let mut up = up.lock().expect("a panic aborts the process");
        up.touched = Instant::now();
        if up.state != UploadState::Receiving {
            return Ok(conflict("this upload has all its bytes"));
        }
        if q.offset != up.received {
            return Ok((
                StatusCode::CONFLICT,
                Json(w::Received {
                    received: up.received,
                }),
            )
                .into_response());
        }
        if up.received + body.len() as u64 > up.start.size {
            return Ok(bad_request("more bytes than the upload said it has"));
        }
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&up.file)
            .with_context(|| format!("open {}", up.file.display()))?;
        let written =
            cairn_failpoint::check("server.web.upload.part").and_then(|()| f.write_all(&body));
        if let Err(e) = written {
            // What was written of this part is not counted: cut back to where it began.
            let _ = f.set_len(up.received);
            return Err(e.into());
        }
        up.received += body.len() as u64;
        Ok(Json(w::Received {
            received: up.received,
        })
        .into_response())
    })
    .await?;
    Ok(answer)
}

pub(super) async fn status(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<UploadPath>,
) -> ApiResult<Response> {
    let Some(up) = app.uploads.get(&p.upload, &view) else {
        return Ok(not_found("such upload"));
    };
    let status = up
        .lock()
        .expect("a panic aborts the process")
        .status(&p.upload);
    Ok(Json(status).into_response())
}

pub(super) async fn cancel(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<UploadPath>,
) -> ApiResult<Response> {
    let Some(up) = app.uploads.get(&p.upload, &view) else {
        return Ok(not_found("such upload"));
    };
    if up.lock().expect("a panic aborts the process").state == UploadState::Storing {
        return Ok(conflict(
            "this upload is being stored, and finishes on its own",
        ));
    }
    app.uploads.forget(&p.upload);
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) async fn commit_upload(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<UploadPath>,
) -> ApiResult<Response> {
    let Some(up) = app.uploads.get(&p.upload, &view) else {
        return Ok(not_found("such upload"));
    };
    {
        let mut u = up.lock().expect("a panic aborts the process");
        u.touched = Instant::now();
        match u.state {
            UploadState::Receiving if u.received == u.start.size => u.state = UploadState::Storing,
            UploadState::Receiving => {
                return Ok(conflict(format!(
                    "{} of its {} bytes have come",
                    u.received, u.start.size
                )))
            }
            state => {
                return Ok((
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({ "state": state })),
                )
                    .into_response())
            }
        }
    }
    let id = p.upload.clone();
    tokio::task::spawn_blocking(move || {
        let result = store(&app, &view, &up);
        let mut u = up.lock().expect("a panic aborts the process");
        u.touched = Instant::now();
        match result {
            Ok(Ok(done)) => {
                u.state = UploadState::Done;
                u.done = Some(done);
            }
            Ok(Err(why)) => {
                u.state = UploadState::Failed;
                u.error = Some(why);
            }
            Err(e) => {
                eprintln!("error: upload {id}: {e:#}");
                u.state = UploadState::Failed;
                u.error = Some(if cairn_store::is_disk_full(&e) {
                    "the server's disk is full".into()
                } else {
                    format!("{e:#}")
                });
            }
        }
        let _ = std::fs::remove_file(&u.file);
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "state": UploadState::Storing })),
    )
        .into_response())
}

/// The manifest of a file's bytes as a client makes it of a file with no holes: one run, cut by FastCDC
/// with the format's bounds, a run of at most the minimum one chunk. Each chunk is handed to `put` as
/// it is cut.
pub(crate) fn chunk_file(
    mut file: impl Read,
    size: u64,
    put: &mut dyn FnMut(Hash, Vec<u8>) -> Result<()>,
) -> Result<Manifest> {
    if size == 0 {
        return Ok(Manifest::new(0, Vec::new()));
    }
    let mut chunks = Vec::new();
    if size <= u64::from(CHUNK_MIN) {
        let mut buf = vec![0u8; size as usize];
        file.read_exact(&mut buf)?;
        let hash = Hash::of(&buf);
        chunks.push(ChunkRef {
            hash,
            len: size as u32,
        });
        put(hash, buf)?;
    } else {
        let mut total = 0u64;
        for c in fastcdc::v2020::StreamCDC::new(file.take(size), CHUNK_MIN, CHUNK_AVG, CHUNK_MAX) {
            let c = c.map_err(|e| anyhow::anyhow!("reading the upload: {e}"))?;
            let hash = Hash::of(&c.data);
            chunks.push(ChunkRef {
                hash,
                len: c.length as u32,
            });
            total += c.length as u64;
            put(hash, c.data)?;
        }
        if total != size {
            bail!("the upload holds {total} bytes, not {size}");
        }
    }
    Ok(Manifest::new(size, vec![Segment { offset: 0, chunks }]))
}

/// Cut, store and commit an upload: `Ok(Err)` is a refusal a person reads.
fn store(app: &App, view: &View, up: &Mutex<Upload>) -> Result<Result<w::UploadDone, String>> {
    let (path, start) = {
        let u = up.lock().expect("a panic aborts the process");
        (u.file.clone(), u.start.clone())
    };
    let chunks = app.repo.chunks(&view.ns)?;
    let file = std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
    if file.metadata()?.len() != start.size {
        bail!("the upload's file is not the size it was given");
    }
    let mut batch: Vec<(Hash, Vec<u8>)> = Vec::new();
    let mut pending = 0usize;
    let flush = |batch: &mut Vec<(Hash, Vec<u8>)>, pending: &mut usize| -> Result<()> {
        let frames: Vec<(Hash, &[u8])> = batch.iter().map(|(h, d)| (*h, d.as_slice())).collect();
        chunks.put_all(&frames)?;
        let n: u64 = batch.iter().map(|(_, d)| d.len() as u64).sum();
        batch.clear();
        *pending = 0;
        up.lock().expect("a panic aborts the process").stored += n;
        Ok(())
    };
    let manifest = chunk_file(
        std::io::BufReader::with_capacity(1 << 20, file),
        start.size,
        &mut |h, d| {
            pending += d.len();
            batch.push((h, d));
            if pending >= BATCH_BYTES {
                flush(&mut batch, &mut pending)?;
            }
            Ok(())
        },
    )?;
    flush(&mut batch, &mut pending)?;
    let encoded = manifest.encode();
    let mhash = Hash::of(&encoded);
    chunks.put(&mhash, &encoded)?;
    cairn_failpoint::hit("server.web.upload.stored");

    // Every chunk is durable; now the commit, against what the browser saw.
    let mtime = start.mtime.unwrap_or_else(now_ts);
    let content = Content::File {
        manifest: mhash,
        size: start.size,
    };
    let planned = app.repo.with_meta(|m| {
        if folder_in_view(m, view, start.parent)?.is_some() {
            return Ok(Err("the folder it was going in is gone".to_string()));
        }
        let at = m
            .children(&view.ns, start.parent)?
            .into_iter()
            .find(|c| c.name == start.name);
        let revision = RevisionId::random();
        Ok(match (at, start.replace) {
            (None, None) => {
                let object = ObjectId::random();
                Ok((
                    object,
                    revision,
                    Op::Create {
                        object,
                        revision,
                        body: RevisionBody {
                            content,
                            metadata: Metadata {
                                mode: 0o644,
                                mtime,
                                // A browser gives milliseconds.
                                time_precision_ns: 1_000_000,
                                ..Default::default()
                            },
                        },
                        link: Link {
                            parent: start.parent,
                            name: start.name.clone(),
                        },
                        conflict_of: None,
                    },
                ))
            }
            (Some(_), None) => {
                Err("the name was taken while it uploaded: nothing was replaced".to_string())
            }
            (None, Some(_)) => {
                Err("what it would replace was moved or deleted while it uploaded".to_string())
            }
            (Some(c), Some(base)) => {
                if c.head.kind != ObjectKind::File {
                    Err("only a file is replaced by an upload".to_string())
                } else if c.head.revision != base {
                    Err("the file changed while it uploaded: nothing was replaced".to_string())
                } else {
                    // Everything it had but its bytes and time stays: modes, owners, attributes and
                    // streams that the browser never saw (invariant 2).
                    let mut metadata = c.head.body.metadata.clone();
                    metadata.mtime = mtime;
                    metadata.time_precision_ns = metadata.time_precision_ns.max(1_000_000);
                    Ok((
                        c.object,
                        revision,
                        Op::Update {
                            object: c.object,
                            base,
                            revision,
                            body: RevisionBody { content, metadata },
                            restored_from: None,
                        },
                    ))
                }
            }
        })
    })?;
    let (object, revision, op) = match planned {
        Ok(p) => p,
        Err(why) => return Ok(Err(why)),
    };
    match commit(app, view, vec![op])? {
        Ok(seq) => Ok(Ok(w::UploadDone {
            object,
            revision,
            seq,
        })),
        Err(why) => Ok(Err(format!("nothing was replaced: {why}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise, so that the chunker finds cut points.
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            out.extend_from_slice(&seed.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn an_upload_is_cut_as_a_client_cuts_the_same_bytes() {
        for len in [
            0usize,
            1,
            CHUNK_MIN as usize,
            CHUNK_MIN as usize + 1,
            20 << 20,
        ] {
            let data = noise(len, 7);
            let mut got = Vec::new();
            let m = chunk_file(&data[..], len as u64, &mut |h, d| {
                assert_eq!(Hash::of(&d), h);
                got.extend_from_slice(&d);
                Ok(())
            })
            .unwrap();
            assert_eq!(got, data);
            assert_eq!(m.size, len as u64);
            m.validate().unwrap();
            if len == 0 {
                assert!(m.segments.is_empty());
            } else {
                assert_eq!(m.segments.len(), 1);
                assert_eq!(m.segments[0].offset, 0);
            }
            if len <= CHUNK_MIN as usize && len > 0 {
                assert_eq!(m.chunks().count(), 1);
            }
            if len == 20 << 20 {
                assert!(m.chunks().count() > 2);
            }
        }
        // Fewer bytes than said is an error, never a short file.
        assert!(chunk_file(&[0u8; 10][..], 11, &mut |_, _| Ok(())).is_err());
        assert!(chunk_file(
            &noise(CHUNK_MIN as usize + 10, 1)[..],
            CHUNK_MIN as u64 + 11,
            &mut |_, _| Ok(())
        )
        .is_err());
    }
}
