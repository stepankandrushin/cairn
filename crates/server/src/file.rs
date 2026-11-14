//! `PUT /v1/ns/{ns}/file`: one file's bytes in one request, cut and stored as they arrive, every chunk
//! durable before the commit that names them (invariant 1), and committed as the device that sent it
//! (docs/protocol.md, "One-request uploads"). A name taken by other bytes is refused or numbered, never
//! replaced (invariant 3); a path through a symbolic link is refused (invariant 6). The commit's
//! `op_id` is the revision the client chose, so a retry of a request whose answer was lost is the same
//! commit, and is answered as the first was.

use crate::web::edit::refusal;
use crate::web::upload::store_bytes;
use crate::{now, Allowed, App, Caller};
use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use cairn_protocol::file::{self as f, FileAnswer, FileQuery, Taken};
use cairn_protocol::wire::{Access, CommitRequest, CommitResponse, RejectReason};
use cairn_protocol::{
    Content, Hash, Link, Metadata, Name, ObjectId, ObjectKind, Op, OpId, OpReject, RevisionBody,
    RevisionId,
};
use cairn_store::MetaStore;
use futures_util::StreamExt;
use std::io::{self, Read};
use std::sync::Arc;

/// How many times a commit refused for a name taken meanwhile is planned again under `rename`.
const RENAME_TRIES: u32 = 5;

/// No resumable uploads: what a client that asks first, as the Photos system does, is told.
pub(crate) async fn options() -> Response {
    (StatusCode::NOT_IMPLEMENTED, "no resumable uploads").into_response()
}

/// A refusal, its reason in the body and in a header, for a sender that keeps only headers.
fn refuse(status: StatusCode, why: impl Into<String>) -> Response {
    let why = why.into();
    let mut r = (status, why.clone()).into_response();
    if let Ok(v) = HeaderValue::from_str(&f::header_text(&why)) {
        r.headers_mut().insert(f::HEADER_ERROR, v);
    }
    r
}

/// A refusal not yet made a response: a status and its reason.
struct Refusal(StatusCode, String);

fn no(status: StatusCode, why: impl Into<String>) -> Refusal {
    Refusal(status, why.into())
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        refuse(self.0, self.1)
    }
}

/// An error as a refusal: a full disk says so, as the rest of the API does.
fn failed(e: anyhow::Error) -> Response {
    if cairn_store::is_disk_full(&e) {
        return refuse(
            StatusCode::INSUFFICIENT_STORAGE,
            "the server's disk is full",
        );
    }
    eprintln!("error: {e:#}");
    refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn answer(status: StatusCode, a: FileAnswer) -> Response {
    let mut headers = HeaderMap::new();
    let mut put = |k: &'static str, v: String| {
        if let Ok(v) = HeaderValue::from_str(&v) {
            headers.insert(k, v);
        }
    };
    put(f::HEADER_OBJECT, a.object.to_string());
    put(f::HEADER_REVISION, a.revision.to_string());
    put(f::HEADER_SEQ, a.seq.to_string());
    put(
        f::HEADER_NAME,
        base64::engine::general_purpose::STANDARD.encode(a.name.as_bytes()),
    );
    if a.existed {
        put(f::HEADER_EXISTED, "1".into());
    }
    (status, headers, Json(a)).into_response()
}

/// What the request asks, checked before a byte of the body is read.
struct Asked {
    ns: String,
    user: String,
    parent: ObjectId,
    folders: Vec<Name>,
    name: Name,
    size: u64,
    metadata: Metadata,
    q: FileQuery,
}

fn asked(
    ns: String,
    caller: &Caller,
    headers: &HeaderMap,
    q: FileQuery,
) -> std::result::Result<Asked, Refusal> {
    let Some(mut names) = f::path_names(&q.path) else {
        return Err(no(
            StatusCode::BAD_REQUEST,
            "the path is not names joined by `/`",
        ));
    };
    let name = names.pop().expect("a path has a name");
    let Some((mtime, precision)) = f::parse_time(&q.mtime) else {
        return Err(no(StatusCode::BAD_REQUEST, "mtime is not a time"));
    };
    let btime = match q.btime.as_deref().map(f::parse_time) {
        None => None,
        Some(Some((t, _))) => Some(t),
        Some(None) => return Err(no(StatusCode::BAD_REQUEST, "btime is not a time")),
    };
    if q.device_name.is_empty() {
        return Err(no(StatusCode::BAD_REQUEST, "device_name is empty"));
    }
    let Some(size) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return Err(no(
            StatusCode::LENGTH_REQUIRED,
            "a file is sent with its Content-Length",
        ));
    };
    Ok(Asked {
        ns,
        user: caller.user.clone(),
        parent: q.parent.unwrap_or(ObjectId::ROOT),
        folders: names,
        name,
        size,
        metadata: Metadata {
            mode: 0o644,
            mtime,
            btime,
            time_precision_ns: precision,
            ..Default::default()
        },
        q,
    })
}

/// The answer for a revision already committed: this request's own, by a retry.
fn committed(
    m: &mut dyn MetaStore,
    a: &Asked,
) -> Result<Option<std::result::Result<FileAnswer, Refusal>>> {
    let Some(rev) = m.revision(&a.ns, a.q.revision)? else {
        return Ok(None);
    };
    if rev.object != a.q.object {
        return Ok(Some(Err(no(
            StatusCode::CONFLICT,
            "that revision is another object's",
        ))));
    }
    let name = m
        .links_of(&a.ns, rev.object)?
        .into_iter()
        .next()
        .map_or_else(|| a.name.clone(), |l| l.name);
    Ok(Some(Ok(FileAnswer {
        object: rev.object,
        revision: rev.revision,
        seq: rev.seq,
        name,
        existed: false,
    })))
}

/// The request body as a blocking reader, fed by the async side through a small channel.
struct BodyReader {
    rx: tokio::sync::mpsc::Receiver<io::Result<Bytes>>,
    cur: Bytes,
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.cur.is_empty() {
            match self.rx.blocking_recv() {
                None => return Ok(0),
                Some(Ok(b)) => self.cur = b,
                Some(Err(e)) => return Err(e),
            }
        }
        let n = buf.len().min(self.cur.len());
        buf[..n].copy_from_slice(&self.cur.split_to(n));
        Ok(n)
    }
}

/// What the commit does, against the tree as it is now.
enum Plan {
    Commit(Vec<Op>, Name),
    /// The file there already has these bytes: nothing to commit.
    Existed(FileAnswer),
}

fn plan(
    m: &mut dyn MetaStore,
    a: &Asked,
    manifest: Hash,
) -> Result<std::result::Result<Plan, Refusal>> {
    let refused = |s, why: &str| Ok(Err(no(s, why)));
    let live = a.parent == ObjectId::ROOT || !m.links_of(&a.ns, a.parent)?.is_empty();
    match m.object_head(&a.ns, a.parent)? {
        Some(h) if live && h.kind == ObjectKind::Dir => {}
        None if a.parent == ObjectId::ROOT => {}
        Some(h) if live && h.kind == ObjectKind::Symlink => {
            return refused(StatusCode::CONFLICT, "the parent is a symbolic link")
        }
        Some(_) if live => return refused(StatusCode::CONFLICT, "the parent is not a folder"),
        _ => return refused(StatusCode::NOT_FOUND, "no such folder"),
    }
    let mut ops = Vec::new();
    let mut dir = a.parent;
    // Whether `dir` is one this commit makes, so that nothing is in it yet.
    let mut made = false;
    for folder in &a.folders {
        let found = if made {
            None
        } else {
            m.children(&a.ns, dir)?
                .into_iter()
                .find(|c| c.name == *folder)
        };
        match found {
            Some(c) if c.head.kind == ObjectKind::Dir => dir = c.object,
            Some(c) if c.head.kind == ObjectKind::Symlink => {
                return refused(
                    StatusCode::CONFLICT,
                    &format!("{folder} in the path is a symbolic link, which nothing goes through"),
                )
            }
            Some(_) => {
                return refused(
                    StatusCode::CONFLICT,
                    &format!("{folder} in the path is a file, not a folder"),
                )
            }
            None => {
                let object = ObjectId::random();
                ops.push(Op::Create {
                    object,
                    revision: RevisionId::random(),
                    body: RevisionBody {
                        content: Content::Dir,
                        metadata: Metadata {
                            mode: 0o755,
                            mtime: crate::web::edit::now_ts(),
                            time_precision_ns: 1,
                            ..Default::default()
                        },
                    },
                    link: Link {
                        parent: dir,
                        name: folder.clone(),
                    },
                    conflict_of: None,
                });
                dir = object;
                made = true;
            }
        }
    }
    let content = Content::File {
        manifest,
        size: a.size,
    };
    let mut name = a.name.clone();
    if !made {
        let children = m.children(&a.ns, dir)?;
        if let Some(c) = children.iter().find(|c| c.name == name) {
            if c.head.body.content == content {
                let seq = m.revision(&a.ns, c.head.revision)?.map_or(0, |r| r.seq);
                return Ok(Ok(Plan::Existed(FileAnswer {
                    object: c.object,
                    revision: c.head.revision,
                    seq,
                    name,
                    existed: true,
                })));
            }
            if a.q.taken == Taken::Fail {
                return refused(
                    StatusCode::CONFLICT,
                    "that name is taken by something else, which was left as it is",
                );
            }
            let taken: std::collections::HashSet<&Name> =
                children.iter().map(|c| &c.name).collect();
            name = (2..)
                .map(|n| f::numbered(&a.name, n))
                .find(|n| !taken.contains(n))
                .expect("a free name is found among the numbered ones");
        }
    }
    ops.push(Op::Create {
        object: a.q.object,
        revision: a.q.revision,
        body: RevisionBody {
            content,
            metadata: a.metadata.clone(),
        },
        link: Link {
            parent: dir,
            name: name.clone(),
        },
        conflict_of: None,
    });
    Ok(Ok(Plan::Commit(ops, name)))
}

pub(crate) async fn put(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(allowed): Extension<Allowed>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    query: std::result::Result<Query<FileQuery>, QueryRejection>,
    body: Body,
) -> Response {
    if allowed.0 < Access::Write {
        return refuse(
            StatusCode::FORBIDDEN,
            format!("folder {ns:?} is read-only for user {:?}", caller.user),
        );
    }
    let q = match query {
        Ok(Query(q)) => q,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let a = match asked(ns, &caller, &headers, q) {
        Ok(a) => a,
        Err(r) => return r.into_response(),
    };
    let a = Arc::new(a);

    // A retry of a request that committed is answered without its bytes.
    let (app2, a2) = (app.clone(), a.clone());
    match crate::blocking(move || app2.repo.with_meta(|m| committed(m, &a2))).await {
        Ok(Some(Ok(done))) => {
            // Read to its end, so that a client still sending is not cut off mid-body and takes the
            // answer for a lost connection.
            let mut stream = body.into_data_stream();
            while let Some(Ok(_)) = stream.next().await {}
            return answer(StatusCode::CREATED, done);
        }
        Ok(Some(Err(r))) => return r.into_response(),
        Ok(None) => {}
        Err(e) => return failed(e.0),
    }

    // The bytes, cut and stored as they come.
    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(8);
    let (app2, a2) = (app.clone(), a.clone());
    let storing = tokio::task::spawn_blocking(move || -> Result<Hash> {
        let chunks = app2.repo.chunks(&a2.ns)?;
        let reader = BodyReader {
            rx,
            cur: Bytes::new(),
        };
        let manifest = store_bytes(&*chunks, reader, a2.size, &mut |_| {})?;
        Ok(manifest)
    });
    let mut stream = body.into_data_stream();
    while let Some(next) = stream.next().await {
        let item =
            next.map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string()));
        let stop = item.is_err();
        if tx.send(item).await.is_err() || stop {
            break;
        }
    }
    drop(tx);
    let manifest = match storing.await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) if cairn_store::is_disk_full(&e) => return failed(e),
        Ok(Err(e)) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                format!("the file did not arrive whole, and nothing was committed: {e:#}"),
            )
        }
        Err(e) => return failed(e.into()),
    };
    cairn_failpoint::hit("server.file.stored");

    let (app2, a2) = (app.clone(), a.clone());
    let done = crate::blocking(move || {
        for _ in 0..RENAME_TRIES {
            let planned = app2.repo.with_meta(|m| plan(m, &a2, manifest))?;
            let (ops, name) = match planned {
                Ok(Plan::Commit(ops, name)) => (ops, name),
                Ok(Plan::Existed(e)) => return Ok(Ok((StatusCode::OK, e))),
                Err(r) => return Ok(Err(r)),
            };
            let req = CommitRequest {
                op_id: OpId(a2.q.revision.0),
                device: a2.q.device,
                device_name: a2.q.device_name.clone(),
                ops,
            };
            match app2.repo.commit(&a2.ns, &req, now(), Some(&a2.user))? {
                CommitResponse::Committed { .. } => {
                    app2.commits.send_modify(|n| *n = n.wrapping_add(1));
                    // From the store, not the plan: a commit with this op made by another attempt is
                    // what stands.
                    return match app2.repo.with_meta(|m| committed(m, &a2))? {
                        Some(Ok(done)) => Ok(Ok((StatusCode::CREATED, done))),
                        Some(Err(r)) => Ok(Err(r)),
                        None => anyhow::bail!(
                            "committed {name}, and its revision {} is not in the store",
                            a2.q.revision
                        ),
                    };
                }
                CommitResponse::Rejected {
                    reject:
                        RejectReason::Op {
                            reason: OpReject::NameTaken(_),
                            ..
                        },
                } if a2.q.taken == Taken::Rename => continue,
                CommitResponse::Rejected { reject } => {
                    return Ok(Err(no(StatusCode::CONFLICT, refusal(&reject))))
                }
            }
        }
        Ok(Err(no(
            StatusCode::CONFLICT,
            "the folder's names kept changing while this was committed: send it again",
        )))
    })
    .await;
    // The commit is durable; only now the answer.
    cairn_failpoint::hit("server.file.before_ack");
    match done {
        Ok(Ok((status, a))) => answer(status, a),
        Ok(Err(r)) => r.into_response(),
        Err(e) => failed(e.0),
    }
}
