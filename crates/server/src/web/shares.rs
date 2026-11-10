//! Sharing what is in a folder: with a user of the server, or by a public link. Only a manager says who
//! else has access, so only a manager shares, and a share stops working once its maker no longer
//! manages the folder.

use super::{item, lookup, not_found, ns_path, path_in_view, share_alive, share_view, View};
use crate::{blocking, now, ApiResult, App, Caller};
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use cairn_protocol::web::{self as w};
use cairn_protocol::wire::Access;
use cairn_protocol::ObjectId;
use cairn_store::{MetaStore, ShareRow};
use serde::Deserialize;
use std::sync::Arc;

fn shown(
    m: &mut dyn MetaStore,
    row: &ShareRow,
    link_url: Option<&str>,
) -> anyhow::Result<w::Share> {
    let kind = m
        .object_head(&row.ns, row.object)?
        .map(|h| h.kind)
        .unwrap_or(cairn_protocol::ObjectKind::File);
    Ok(w::Share {
        id: hex::encode(row.id),
        object: row.object,
        kind,
        path: ns_path(m, &row.ns, row.object)?,
        user: row.user.clone(),
        access: row.access,
        by: row.by.clone(),
        created: row.created,
        expires: row.expires,
        url: row
            .link
            .as_ref()
            .and_then(|l| link_url.map(|base| format!("{base}/s/{l}"))),
        link: row.link.clone(),
    })
}

#[derive(Deserialize)]
pub(super) struct SharesQuery {
    object: Option<String>,
}

pub(super) async fn list(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Query(q): Query<SharesQuery>,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Manage) {
        return Ok(r);
    }
    let object = match q.object.as_deref() {
        None | Some("") => None,
        Some("root") => Some(ObjectId::ROOT),
        Some(o) => match o.parse() {
            Ok(o) => Some(o),
            Err(_) => return Ok(not_found("such object")),
        },
    };
    let shares = blocking(move || {
        let base = app.link_url.as_deref();
        app.repo.with_meta(|m| {
            let rows = m.shares(&view.ns, object)?;
            rows.iter()
                .map(|r| shown(m, r, base))
                .collect::<anyhow::Result<Vec<_>>>()
        })
    })
    .await?;
    Ok(Json(w::Shares { shares }).into_response())
}

pub(super) async fn create(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Json(req): Json<w::NewShare>,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Manage) {
        return Ok(r);
    }
    let access = req.access.unwrap_or(Access::Read);
    if access == Access::Manage {
        return Ok(super::bad_request(
            "a share reads or writes; to have someone manage the folder, give them the folder",
        ));
    }
    if req.user.is_none() && access != Access::Read {
        return Ok(super::bad_request("a public link only reads"));
    }
    if req.expires.is_some_and(|e| e <= now()) {
        return Ok(super::bad_request("that time has passed"));
    }
    let answer = blocking(move || {
        app.repo.with_meta(|m| {
            if path_in_view(m, &view, req.object)?.is_none() {
                return Ok(not_found("such object"));
            }
            if let Some(user) = &req.user {
                if !m.users()?.iter().any(|u| &u.name == user) {
                    return Ok((StatusCode::NOT_FOUND, format!("no user {user:?}")).into_response());
                }
            }
            let link = req.user.is_none().then(|| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
            });
            let row = ShareRow {
                id: rand::random(),
                ns: view.ns.clone(),
                object: req.object,
                user: req.user.clone(),
                link,
                access,
                by: view.committer()?.to_string(),
                created: now(),
                expires: req.expires,
            };
            m.add_share(&row)?;
            let shown = shown(m, &row, app.link_url.as_deref())?;
            Ok((StatusCode::CREATED, Json(shown)).into_response())
        })
    })
    .await?;
    Ok(answer)
}

#[derive(Deserialize)]
pub(super) struct SharePath {
    share: String,
}

pub(super) async fn remove(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<SharePath>,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Manage) {
        return Ok(r);
    }
    let mut id = [0u8; 16];
    if hex::decode_to_slice(&p.share, &mut id).is_err() {
        return Ok(not_found("such share"));
    }
    let removed = blocking(move || app.repo.with_meta(|m| m.remove_share(&view.ns, &id))).await?;
    Ok(if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found("such share")
    })
}

pub(super) async fn shared_with_me(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Response> {
    let shares = blocking(move || {
        app.repo.with_meta(|m| {
            let mut out = Vec::new();
            let t = now();
            for row in m.shares_to(&caller.user)? {
                if !share_alive(m, &row, t)? {
                    continue;
                }
                let view = share_view(&row);
                let Some((chain, head)) = lookup(m, &view, view.root)? else {
                    continue;
                };
                let name = chain.last().and_then(|(_, n)| n.clone());
                out.push(w::SharedItem {
                    id: hex::encode(row.id),
                    item: item(&view, view.root, name, &head),
                    by: row.by.clone(),
                    access: view.access,
                    expires: row.expires,
                });
            }
            Ok(out)
        })
    })
    .await?;
    Ok(Json(w::SharedWithMe { shares }).into_response())
}

/// Every user's name, for a manager choosing whom to share with.
pub(super) async fn users(State(app): State<Arc<App>>) -> ApiResult<Response> {
    let users = blocking(move || {
        Ok(app
            .repo
            .with_meta(|m| m.users())?
            .into_iter()
            .map(|u| u.name)
            .collect())
    })
    .await?;
    Ok(Json(w::Users { users }).into_response())
}
