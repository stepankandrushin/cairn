//! Changes made from the web: each one commit, as a device's are, validated by the same rules, and
//! refused rather than forced when the tree is not as the request saw it.

use super::{bad_request, conflict, not_found, path_in_view, web_device, View};
use crate::{blocking, now, ApiResult, App};
use anyhow::Result;
use axum::extract::{Extension, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use cairn_protocol::web::{self as w, WEB_DEVICE};
use cairn_protocol::wire::{Access, CommitRequest, CommitResponse, RejectReason};
use cairn_protocol::{
    Content, Link, Metadata, ObjectId, ObjectKind, Op, OpId, OpReject, RevisionBody, RevisionId,
    Timestamp,
};
use cairn_store::MetaStore;
use std::sync::Arc;

/// Why a commit was refused, in words a person acts on.
pub(super) fn refusal(reject: &RejectReason) -> String {
    match reject {
        RejectReason::Op { reason, .. } => match reason {
            OpReject::NameTaken(_) => "that name is taken: reload to see what has it".into(),
            OpReject::StaleBase(_) => {
                "it has changed since you looked: reload, and do it again if it still applies"
                    .into()
            }
            OpReject::Deleted(_) | OpReject::NoSuchEntry(_) | OpReject::NoSuchObject(_) => {
                "it has been moved or deleted since you looked: reload".into()
            }
            OpReject::BadParent(_) => "the folder it would go in is gone: reload".into(),
            OpReject::Cycle(_) => "a folder cannot go inside itself".into(),
            OpReject::NotEmpty(_) => "the folder is not empty".into(),
            OpReject::ReservedName(_) => {
                "`.cairn` in a folder's root holds each device's own state, and cannot be used"
                    .into()
            }
            other => other.to_string(),
        },
        RejectReason::MissingChunk { hash } => format!("chunk {hash} is not on the server"),
        RejectReason::BadManifest { message, .. } => message.clone(),
    }
}

/// One commit of `ops` as the view's user, from the web's device: `Err` why it was refused.
pub(super) fn commit(app: &App, view: &View, ops: Vec<Op>) -> Result<Result<u64, String>> {
    let user = view.committer()?;
    let req = CommitRequest {
        op_id: OpId::random(),
        device: web_device(user),
        device_name: WEB_DEVICE.into(),
        ops,
    };
    match app.repo.commit(&view.ns, &req, now(), Some(user))? {
        CommitResponse::Committed { seq } => {
            // Durable now: what waits on the change feed may read it.
            app.commits.send_modify(|n| *n = n.wrapping_add(1));
            Ok(Ok(seq))
        }
        CommitResponse::Rejected { reject } => Ok(Err(refusal(&reject))),
    }
}

/// Now, as a revision's time.
pub(super) fn now_ts() -> Timestamp {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp {
        sec: d.as_secs() as i64,
        nsec: d.subsec_nanos(),
    }
}

/// A folder of the view that something may go in: live, a directory, inside the view.
pub(super) fn folder_in_view(
    m: &mut dyn MetaStore,
    view: &View,
    dir: ObjectId,
) -> Result<Option<Response>> {
    if path_in_view(m, view, dir)?.is_none() {
        return Ok(Some(not_found("such folder")));
    }
    match m.object_head(&view.ns, dir)? {
        Some(h) if h.kind == ObjectKind::Dir => Ok(None),
        Some(_) => Ok(Some(bad_request("that is not a folder"))),
        None => Ok(Some(not_found("such folder"))),
    }
}

pub(super) async fn mkdir(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Json(req): Json<w::Mkdir>,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Write) {
        return Ok(r);
    }
    let answer = blocking(move || {
        if let Some(r) = app
            .repo
            .with_meta(|m| folder_in_view(m, &view, req.parent))?
        {
            return Ok(r);
        }
        let object = ObjectId::random();
        let op = Op::Create {
            object,
            revision: RevisionId::random(),
            body: RevisionBody {
                content: Content::Dir,
                metadata: Metadata {
                    mode: 0o755,
                    mtime: now_ts(),
                    time_precision_ns: 1,
                    ..Default::default()
                },
            },
            link: Link {
                parent: req.parent,
                name: req.name,
            },
            conflict_of: None,
        };
        Ok(match commit(&app, &view, vec![op])? {
            Ok(seq) => Json(w::Made { object, seq }).into_response(),
            Err(why) => conflict(why),
        })
    })
    .await?;
    Ok(answer)
}

/// Whether an entry is the object's and lies inside the view, not as the view's own root.
fn entry_in_view(
    m: &mut dyn MetaStore,
    view: &View,
    object: ObjectId,
    link: &Link,
) -> Result<bool> {
    if object == view.root || object == ObjectId::ROOT {
        return Ok(false);
    }
    if !m.links_of(&view.ns, object)?.contains(link) {
        return Ok(false);
    }
    Ok(path_in_view(m, view, link.parent)?.is_some())
}

pub(super) async fn move_entry(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Json(req): Json<w::MoveRequest>,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Write) {
        return Ok(r);
    }
    let answer = blocking(move || {
        let refused = app.repo.with_meta(|m| {
            if !entry_in_view(m, &view, req.object, &req.from)? {
                return Ok(Some(not_found("such entry")));
            }
            folder_in_view(m, &view, req.to.parent)
        })?;
        if let Some(r) = refused {
            return Ok(r);
        }
        if req.from == req.to {
            return Ok(bad_request("that is where it is already"));
        }
        let op = Op::Move {
            object: req.object,
            from: req.from,
            to: req.to,
        };
        Ok(match commit(&app, &view, vec![op])? {
            Ok(seq) => Json(w::Done { seq }).into_response(),
            Err(why) => conflict(why),
        })
    })
    .await?;
    Ok(answer)
}

/// The unlinks that delete an entry and, for a folder, everything in it, deepest first, each against the
/// head it has now: an edit committed after this is read makes the commit refused, and survives.
fn unlinks(
    m: &mut dyn MetaStore,
    ns: &str,
    object: ObjectId,
    link: Link,
    base: Option<RevisionId>,
) -> Result<Vec<Op>> {
    let mut ops = Vec::new();
    // (object, the entry to remove, base), each folder's contents after it, then reversed.
    let mut stack = vec![(object, link, base)];
    while let Some((o, l, b)) = stack.pop() {
        let head = m.object_head(ns, o)?;
        if head.as_ref().is_some_and(|h| h.kind == ObjectKind::Dir) {
            for c in m.children(ns, o)? {
                stack.push((
                    c.object,
                    Link {
                        parent: o,
                        name: c.name,
                    },
                    Some(c.head.revision),
                ));
            }
        }
        ops.push(Op::Unlink {
            object: o,
            link: l,
            base: b,
        });
    }
    ops.reverse();
    Ok(ops)
}

pub(super) async fn delete(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Json(req): Json<w::DeleteRequest>,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Write) {
        return Ok(r);
    }
    let answer = blocking(move || {
        let planned = app.repo.with_meta(|m| {
            if !entry_in_view(m, &view, req.object, &req.link)? {
                return Ok(Err(not_found("such entry")));
            }
            Ok(Ok(unlinks(
                m,
                &view.ns,
                req.object,
                req.link.clone(),
                req.base,
            )?))
        })?;
        let ops = match planned {
            Ok(ops) => ops,
            Err(r) => return Ok(r),
        };
        let objects = ops.len() as u64;
        Ok(match commit(&app, &view, ops)? {
            Ok(seq) => Json(w::DeleteDone { seq, objects }).into_response(),
            Err(why) => conflict(why),
        })
    })
    .await?;
    Ok(answer)
}

pub(super) async fn restore(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Json(req): Json<w::RestoreRequest>,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Write) {
        return Ok(r);
    }
    let answer = blocking(move || {
        let found = app.repo.with_meta(|m| {
            if path_in_view(m, &view, req.object)?.is_none() {
                return Ok(Err(not_found("such object")));
            }
            let Some(head) = m.object_head(&view.ns, req.object)? else {
                return Ok(Err(not_found("such object")));
            };
            match m.revision(&view.ns, req.revision)? {
                Some(old) if old.object == req.object => Ok(Ok((head.revision, old.body))),
                _ => Ok(Err(not_found("such revision"))),
            }
        })?;
        let (base, body) = match found {
            Ok(f) => f,
            Err(r) => return Ok(r),
        };
        if base == req.revision {
            return Ok(bad_request("that is the revision it has now"));
        }
        let revision = RevisionId::random();
        // The old revision whole, its metadata too, as `cairn-sync restore` brings it back.
        let op = Op::Update {
            object: req.object,
            base,
            revision,
            body,
            restored_from: Some(req.revision),
        };
        Ok(match commit(&app, &view, vec![op])? {
            Ok(seq) => Json(w::Restored { seq, revision }).into_response(),
            Err(why) => conflict(why),
        })
    })
    .await?;
    Ok(answer)
}
