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
use cairn_store::chunks::Held;
use cairn_store::MetaStore;
use futures_util::StreamExt;
use std::io::{self, Read};
use std::sync::Arc;

/// How many times a commit refused for a name taken meanwhile is planned again under `rename`.
const RENAME_TRIES: u32 = 5;

/// What a client that asks first, as the Photos system does, is told: resumable uploads to one that
/// names an interop version of the draft spoken here, none to anyone else.
pub(crate) async fn options(headers: HeaderMap) -> Response {
    match crate::resumable::interop(&headers) {
        Some(v) => (
            StatusCode::OK,
            [
                (crate::resumable::VERSION, v),
                ("upload-limit", "min-size=0"),
            ],
        )
            .into_response(),
        None => (StatusCode::NOT_IMPLEMENTED, "no resumable uploads").into_response(),
    }
}

/// A refusal, its reason in the body and in a header, for a sender that keeps only headers.
pub(crate) fn refuse(status: StatusCode, why: impl Into<String>) -> Response {
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
pub(crate) fn failed(e: anyhow::Error) -> Response {
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
    if a.conflict {
        put(f::HEADER_CONFLICT, "1".into());
    }
    (status, headers, Json(a)).into_response()
}

/// What the request asks, checked before a byte of the body is read.
#[derive(Clone)]
pub(crate) struct Asked {
    pub(crate) ns: String,
    pub(crate) user: String,
    parent: ObjectId,
    folders: Vec<Name>,
    /// The new file's name; `None` for a change, whose file is named by its ID.
    name: Option<Name>,
    pub(crate) size: u64,
    metadata: Metadata,
    pub(crate) q: FileQuery,
}

fn asked(
    ns: String,
    caller: &Caller,
    headers: &HeaderMap,
    q: FileQuery,
) -> std::result::Result<Asked, Refusal> {
    let (names, name) = if q.base.is_some() {
        // A change: the file is named by its ID, and stays where it is.
        if !q.path.is_empty() {
            return Err(no(
                StatusCode::BAD_REQUEST,
                "a change of a file names it by object, with no path",
            ));
        }
        (Vec::new(), None)
    } else {
        let Some(mut names) = f::path_names(&q.path) else {
            return Err(no(
                StatusCode::BAD_REQUEST,
                "the path is not names joined by `/`",
            ));
        };
        let name = names.pop().expect("a path has a name");
        (names, Some(name))
    };
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
    // A change that found the file changed made a copy beside it, a new object with this revision, by
    // this request's own commit.
    let copy = rev.object != a.q.object
        && a.q.base.is_some()
        && m.committed(&a.ns, &OpId(a.q.revision.0))? == Some(rev.seq);
    if rev.object != a.q.object && !copy {
        return Ok(Some(Err(no(
            StatusCode::CONFLICT,
            "that revision is another object's",
        ))));
    }
    let name = match m.links_of(&a.ns, rev.object)?.into_iter().next() {
        Some(l) => l.name,
        None => match &a.name {
            Some(n) => n.clone(),
            // Deleted since: named by its ID.
            None => Name::new(rev.object.to_string().into_bytes())?,
        },
    };
    Ok(Some(Ok(FileAnswer {
        object: rev.object,
        revision: rev.revision,
        seq: rev.seq,
        name,
        existed: false,
        conflict: copy,
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
    /// A change made a conflict copy beside the file it was meant for.
    Copy(Vec<Op>, Name),
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
    let asked = a.name.clone().expect("a new file has a name");
    let mut name = asked.clone();
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
                    conflict: false,
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
                .map(|n| f::numbered(&asked, n))
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

/// A change of a file already there, against `base`, the revision the client saw: an update keeping
/// everything of its metadata but its mtime (invariant 2) while it is still that revision; beside it a
/// conflict copy once another changed it, never in its place (invariant 3), or with `stale=fail` nothing;
/// and once deleted, brought back where it was last, as itself or, changed since, as a copy.
fn plan_change(
    m: &mut dyn MetaStore,
    a: &Asked,
    base: RevisionId,
    manifest: Hash,
    last: Option<(Link, u64)>,
) -> Result<std::result::Result<Plan, Refusal>> {
    let refused = |s, why: &str| Ok(Err(no(s, why)));
    let content = Content::File {
        manifest,
        size: a.size,
    };
    let Some(head) = m.object_head(&a.ns, a.q.object)? else {
        return refused(StatusCode::NOT_FOUND, "no such file");
    };
    if head.kind != ObjectKind::File {
        return refused(StatusCode::CONFLICT, "that is not a file");
    }
    let current = head.revision == base;
    if !current && a.q.stale == f::Stale::Fail {
        return refused(
            StatusCode::CONFLICT,
            "the file changed since that revision, and was left as it is",
        );
    }
    // Everything it has but its mtime, as the web's replace keeps it.
    let mut kept = head.body.metadata.clone();
    kept.mtime = a.metadata.mtime;
    kept.time_precision_ns = a.metadata.time_precision_ns;
    let update = Op::Update {
        object: a.q.object,
        base,
        revision: a.q.revision,
        body: RevisionBody {
            content: content.clone(),
            metadata: kept,
        },
        restored_from: None,
    };
    let links = m.links_of(&a.ns, a.q.object)?;
    if let Some(at) = links.into_iter().next() {
        if head.body.content == content {
            let seq = m.revision(&a.ns, head.revision)?.map_or(0, |r| r.seq);
            return Ok(Ok(Plan::Existed(FileAnswer {
                object: a.q.object,
                revision: head.revision,
                seq,
                name: at.name,
                existed: true,
                conflict: false,
            })));
        }
        if current {
            return Ok(Ok(Plan::Commit(vec![update], at.name)));
        }
        let name = conflict_beside(m, a, at.parent, &at.name, None)?;
        return Ok(Ok(copy(a, &head, content, at.parent, name)));
    }
    // Deleted: where it was last, if that folder is there.
    let Some((was, seq)) = last else {
        return refused(
            StatusCode::CONFLICT,
            "the file is deleted, and where it was is not known: send it as a new file",
        );
    };
    let live = was.parent == ObjectId::ROOT || !m.links_of(&a.ns, was.parent)?.is_empty();
    if !live {
        return refused(
            StatusCode::CONFLICT,
            "the file is deleted, and the folder it was in too: send it as a new file",
        );
    }
    let free = !m
        .children(&a.ns, was.parent)?
        .iter()
        .any(|c| c.name == was.name);
    if current {
        let name = if free {
            was.name.clone()
        } else {
            conflict_beside(m, a, was.parent, &was.name, Some(seq))?
        };
        let link = Op::Link {
            object: a.q.object,
            link: Link {
                parent: was.parent,
                name: name.clone(),
            },
        };
        return Ok(Ok(Plan::Commit(vec![link, update], name)));
    }
    let name = conflict_beside(m, a, was.parent, &was.name, None)?;
    Ok(Ok(copy(a, &head, content, was.parent, name)))
}

/// The bytes as a new file beside the one they were meant to change, a conflict of it.
fn copy(
    a: &Asked,
    head: &cairn_store::Head,
    content: Content,
    parent: ObjectId,
    name: Name,
) -> Plan {
    let mut metadata = a.metadata.clone();
    metadata.mode = head.body.metadata.mode;
    Plan::Copy(
        vec![Op::Create {
            object: ObjectId::random(),
            revision: a.q.revision,
            body: RevisionBody { content, metadata },
            link: Link {
                parent,
                name: name.clone(),
            },
            conflict_of: Some(cairn_protocol::ConflictOf {
                object: a.q.object,
                competing_revision: head.revision,
            }),
        }],
        name,
    )
}

/// A free name beside `name` in `parent` for a conflict copy made by this request's device today, or,
/// given the sequence that deleted it, for a file brought back where its name was taken since.
fn conflict_beside(
    m: &mut dyn MetaStore,
    a: &Asked,
    parent: ObjectId,
    name: &Name,
    restored: Option<u64>,
) -> Result<Name> {
    let taken: std::collections::HashSet<Name> = m
        .children(&a.ns, parent)?
        .into_iter()
        .map(|c| c.name)
        .collect();
    // The names of the folders above it, for git's own folders, whose copies are named apart.
    let mut folder = Vec::new();
    let mut at = parent;
    while at != ObjectId::ROOT && folder.len() < 4096 {
        let Some(l) = m.links_of(&a.ns, at)?.into_iter().next() else {
            break;
        };
        folder.push(l.name);
        at = l.parent;
    }
    folder.reverse();
    let in_git = cairn_protocol::name::in_git_dir(&folder);
    let (host, date) = match restored {
        Some(seq) => ("restored".to_string(), format!("seq {seq}")),
        None => (a.q.device_name.clone(), crate::admin::date(now())),
    };
    let mut attempt = 0;
    loop {
        let n = cairn_protocol::name::conflict_name(name, &host, &date, attempt, in_git);
        if !taken.contains(&n)
            && !(parent == ObjectId::ROOT && n.as_bytes() == cairn_protocol::STATE_DIR)
        {
            return Ok(n);
        }
        attempt += 1;
        if attempt > 10_000 {
            anyhow::bail!("no free name for a conflict copy beside {name}");
        }
    }
}

// An axum handler: each argument is one thing it takes from the request.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn put(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(allowed): Extension<Allowed>,
    interim: Option<Extension<crate::conn::Interim>>,
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

    // One that may be resumed: received whole first, then stored and committed as below.
    if let Some(version) = crate::resumable::interop(&headers) {
        if let Some(complete) = crate::resumable::completes(&headers, version) {
            let length = crate::resumable::length(&headers);
            return crate::resumable::create(
                app,
                (*a).clone(),
                version,
                complete,
                length,
                interim.map(|Extension(i)| i),
                body,
            )
            .await;
        }
    }

    // The bytes, cut and stored as they come.
    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(8);
    let (app2, a2) = (app.clone(), a.clone());
    let storing = tokio::task::spawn_blocking(move || -> Result<(Hash, Held)> {
        let chunks = app2.repo.chunks(&a2.ns)?;
        let reader = BodyReader {
            rx,
            cur: Bytes::new(),
        };
        store_bytes(&chunks, reader, a2.size, &mut |_| {})
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
    // The chunks are held until the commit that names them returns: no collection takes one between.
    let (manifest, held) = match storing.await {
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
    commit_file(app, a, manifest, held).await
}

/// The file's bytes stored as `manifest` committed as the request asks, or refused: the answer. `held`
/// keeps its chunks from any collection until the commit returns.
pub(crate) async fn commit_file(
    app: Arc<App>,
    a: Arc<Asked>,
    manifest: Hash,
    held: Held,
) -> Response {
    let (app2, a2) = (app.clone(), a.clone());
    let done = crate::blocking(move || {
        let _held = held;
        for _ in 0..RENAME_TRIES {
            // Where the file was last, should it be deleted: from the index, outside the store's lock,
            // which the index takes to catch up.
            let last = match a2.q.base {
                Some(_) => crate::web::deleted::last_link(&app2, &a2.ns, a2.q.object)?,
                None => None,
            };
            let planned = app2.repo.with_meta(|m| match a2.q.base {
                Some(base) => plan_change(m, &a2, base, manifest, last),
                None => plan(m, &a2, manifest),
            })?;
            let (ops, name) = match planned {
                Ok(Plan::Commit(ops, name) | Plan::Copy(ops, name)) => (ops, name),
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
                } if a2.q.taken == Taken::Rename || a2.q.base.is_some() => continue,
                // Changed or deleted between the plan and the commit: planned again against that.
                CommitResponse::Rejected {
                    reject:
                        RejectReason::Op {
                            reason: OpReject::StaleBase(_) | OpReject::Deleted(_),
                            ..
                        },
                } if a2.q.base.is_some() => continue,
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
