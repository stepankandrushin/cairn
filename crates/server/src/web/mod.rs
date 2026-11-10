//! What the web interface asks of the server, and any other client may (docs/protocol.md, "Web"): three
//! views of a tree, by a folder's grant, a share with a user, or a public link, each reaching no further
//! than its root; sessions; and the page itself.

use crate::{blocking, now, ApiResult, App, Caller};
use anyhow::{anyhow, Result};
use axum::extract::{DefaultBodyLimit, Extension, Path, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use cairn_protocol::web::{self as w, Item, Step};
use cairn_protocol::wire::Access;
use cairn_protocol::{name::display_bytes, Content, DeviceId, Hash, Name, ObjectId, ObjectKind};
use cairn_store::{Head, MetaStore, ShareRow};
use serde::Deserialize;
use std::sync::Arc;

mod assets;
mod content;
mod deleted;
mod edit;
mod shares;
pub(crate) mod upload;
mod zip;

/// How a request reaches a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    /// A user given the folder.
    Member,
    /// A share with the user asking, by its ID.
    Share([u8; 16]),
    /// A public link.
    Link,
}

/// One view of a namespace: what a request may reach and do there.
#[derive(Clone, Debug)]
pub(crate) struct View {
    pub ns: String,
    /// The namespace's root, or the shared object: nothing above it is reached or named.
    pub root: ObjectId,
    pub access: Access,
    /// Who is asking; nobody, by a public link.
    pub user: Option<String>,
    pub via: Via,
}

impl View {
    pub fn member(ns: &str, user: &str, access: Access) -> Self {
        View {
            ns: ns.to_string(),
            root: ObjectId::ROOT,
            access,
            user: Some(user.to_string()),
            via: Via::Member,
        }
    }

    /// A 403 to answer with, unless this view may at least `want`.
    pub fn refuses(&self, want: Access) -> Option<Response> {
        if self.access >= want {
            return None;
        }
        let why = match (&self.via, want) {
            (Via::Link, _) => "a public link only reads".to_string(),
            (_, Access::Manage) => format!(
                "only a manager of folder {:?} shares what is in it",
                self.ns
            ),
            (Via::Share(_), _) => "this was shared with you to read only".to_string(),
            (Via::Member, _) => format!(
                "folder {:?} is read-only for user {:?}",
                self.ns,
                self.user.as_deref().unwrap_or("")
            ),
        };
        Some((StatusCode::FORBIDDEN, why).into_response())
    }

    /// The user a commit is made as. A link never commits.
    pub fn committer(&self) -> Result<&str> {
        self.user
            .as_deref()
            .ok_or_else(|| anyhow!("a public link changes nothing"))
    }
}

/// Every web commit's device, one per user: no client's random ID can be it.
pub(crate) fn web_device(user: &str) -> DeviceId {
    let h = Hash::of(format!("cairn web device {user}").as_bytes());
    DeviceId(h.0[..16].try_into().expect("16 of 32 bytes"))
}

/// An answer of 404, the same whether the thing is not there, deleted, or out of this view.
pub(crate) fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("no {what} here")).into_response()
}

pub(crate) fn bad_request(why: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, why.into()).into_response()
}

pub(crate) fn conflict(why: impl Into<String>) -> Response {
    (StatusCode::CONFLICT, why.into()).into_response()
}

/// An `{object}` in a path: an `ObjectId`, or `root` for the view's own root.
pub(crate) fn object_param(view: &View, s: &str) -> Option<ObjectId> {
    if s == "root" {
        return Some(view.root);
    }
    s.parse().ok()
}

/// The way from a view's root down to an object: each object with the name it has there, the root's
/// none when it is a namespace's root.
pub(crate) type Chain = Vec<(ObjectId, Option<Name>)>;

#[derive(Deserialize)]
pub(crate) struct ObjectPath {
    pub object: String,
}

/// The way from a view's root to an object: each folder and the name it has there, the root first and
/// the object last, along the first of the object's entries that lies inside the view. `None` when the
/// object is deleted, not there, or outside the view.
pub(crate) fn path_in_view(
    m: &mut dyn MetaStore,
    view: &View,
    object: ObjectId,
) -> Result<Option<Chain>> {
    const MAX_DEPTH: usize = 4096;
    if object == view.root {
        return Ok(Some(vec![(object, root_name(m, view)?)]));
    }
    if object == ObjectId::ROOT {
        return Ok(None);
    }
    'links: for link in m.links_of(&view.ns, object)? {
        let mut chain = vec![(object, Some(link.name.clone()))];
        let mut at = link.parent;
        loop {
            if at == view.root {
                chain.push((at, root_name(m, view)?));
                chain.reverse();
                return Ok(Some(chain));
            }
            if at == ObjectId::ROOT || chain.len() > MAX_DEPTH {
                continue 'links;
            }
            // A folder has one entry, and none once deleted.
            let Some(up) = m.links_of(&view.ns, at)?.into_iter().next() else {
                continue 'links;
            };
            chain.push((at, Some(up.name)));
            at = up.parent;
        }
    }
    Ok(None)
}

/// The name a view's root goes by: none for a namespace's root, else its first entry's.
fn root_name(m: &mut dyn MetaStore, view: &View) -> Result<Option<Name>> {
    if view.root == ObjectId::ROOT {
        return Ok(None);
    }
    Ok(m.links_of(&view.ns, view.root)?
        .into_iter()
        .next()
        .map(|l| l.name))
}

/// How a name shows: the namespace's own for its root.
pub(crate) fn display_name(view: &View, name: Option<&Name>) -> String {
    match name {
        Some(n) => display_bytes(n.as_bytes()),
        None => view.ns.clone(),
    }
}

pub(crate) fn steps(view: &View, chain: &[(ObjectId, Option<Name>)]) -> Vec<Step> {
    chain
        .iter()
        .map(|(object, name)| Step {
            object: *object,
            display: display_name(view, name.as_ref()),
        })
        .collect()
}

pub(crate) fn item(view: &View, object: ObjectId, name: Option<Name>, head: &Head) -> Item {
    let meta = &head.body.metadata;
    let (size, target) = match &head.body.content {
        Content::File { size, .. } => (Some(*size), None),
        Content::Symlink { target } => (None, Some(target.clone())),
        Content::Dir => (None, None),
    };
    Item {
        object,
        revision: head.revision,
        kind: head.kind,
        display: display_name(view, name.as_ref()),
        name,
        size,
        mtime: meta.mtime,
        mode: meta.mode,
        target_display: target.as_ref().map(|t| display_bytes(&t.0)),
        target,
    }
}

/// An object of the view, with its way from the view's root and its head: `None` if it is not in the view.
pub(crate) fn lookup(
    m: &mut dyn MetaStore,
    view: &View,
    object: ObjectId,
) -> Result<Option<(Chain, Head)>> {
    let Some(chain) = path_in_view(m, view, object)? else {
        return Ok(None);
    };
    let Some(head) = m.object_head(&view.ns, object)? else {
        return Ok(None);
    };
    Ok(Some((chain, head)))
}

/// A path as text, from the namespace's root: `a/b/c`, empty for the root.
pub(crate) fn ns_path(m: &mut dyn MetaStore, ns: &str, object: ObjectId) -> Result<Option<String>> {
    let view = View::member(ns, "", Access::Read);
    Ok(path_in_view(m, &view, object)?.map(|chain| {
        chain
            .iter()
            .skip(1)
            .map(|(_, n)| display_name(&view, n.as_ref()))
            .collect::<Vec<_>>()
            .join("/")
    }))
}

async fn item_view(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<ObjectPath>,
) -> ApiResult<Response> {
    let Some(object) = object_param(&view, &p.object) else {
        return Ok(not_found("such object"));
    };
    let found = blocking(move || {
        app.repo.with_meta(|m| {
            let Some((chain, head)) = lookup(m, &view, object)? else {
                return Ok(None);
            };
            let name = chain.last().and_then(|(_, n)| n.clone());
            Ok(Some(w::ItemView {
                item: item(&view, object, name, &head),
                path: steps(&view, &chain),
                access: view.access,
            }))
        })
    })
    .await?;
    Ok(match found {
        Some(v) => Json(v).into_response(),
        None => not_found("such object"),
    })
}

async fn dir_view(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<ObjectPath>,
) -> ApiResult<Response> {
    let Some(object) = object_param(&view, &p.object) else {
        return Ok(not_found("such folder"));
    };
    let found = blocking(move || {
        app.repo.with_meta(|m| {
            let Some((chain, head)) = lookup(m, &view, object)? else {
                return Ok(Err(not_found("such folder")));
            };
            if head.kind != ObjectKind::Dir {
                return Ok(Err(bad_request("that is not a folder")));
            }
            let entries = m
                .children(&view.ns, object)?
                .into_iter()
                .map(|c| item(&view, c.object, Some(c.name), &c.head))
                .collect();
            let name = chain.last().and_then(|(_, n)| n.clone());
            Ok(Ok(w::Listing {
                dir: item(&view, object, name, &head),
                path: steps(&view, &chain),
                entries,
                access: view.access,
            }))
        })
    })
    .await?;
    Ok(match found {
        Ok(listing) => Json(listing).into_response(),
        Err(r) => r,
    })
}

async fn history(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<ObjectPath>,
) -> ApiResult<Response> {
    // A link gives what is there now, and no more.
    if view.via == Via::Link {
        return Ok(not_found("history"));
    }
    let Some(object) = object_param(&view, &p.object) else {
        return Ok(not_found("such object"));
    };
    let found = blocking(move || {
        app.repo.with_meta(|m| {
            if path_in_view(m, &view, object)?.is_none() {
                return Ok(None);
            }
            let revisions = m
                .history(&view.ns, object)?
                .into_iter()
                .map(|r| {
                    let body = &r.revision.body;
                    w::HistoryEntry {
                        revision: r.revision.revision,
                        seq: r.revision.seq,
                        time: r.time,
                        user: r.user,
                        device_name: r.device_name,
                        kind: body.content.kind(),
                        size: match &body.content {
                            Content::File { size, .. } => Some(*size),
                            _ => None,
                        },
                        mode: body.metadata.mode,
                        mtime: body.metadata.mtime,
                    }
                })
                .collect();
            Ok(Some(w::History { revisions }))
        })
    })
    .await?;
    Ok(match found {
        Some(h) => Json(h).into_response(),
        None => not_found("such object"),
    })
}

/// Whether a share works now: not expired, its object not deleted, and made by someone who still
/// manages the folder, as only a manager may say who else has access.
pub(crate) fn share_alive(m: &mut dyn MetaStore, share: &ShareRow, now: i64) -> Result<bool> {
    if share.expires.is_some_and(|e| e <= now) {
        return Ok(false);
    }
    if m.access(&share.ns, &share.by)? != Some(Access::Manage) {
        return Ok(false);
    }
    Ok(share.object == ObjectId::ROOT || !m.links_of(&share.ns, share.object)?.is_empty())
}

pub(crate) fn share_view(share: &ShareRow) -> View {
    View {
        ns: share.ns.clone(),
        root: share.object,
        // A link only reads, whatever the row says.
        access: if share.link.is_some() {
            Access::Read
        } else {
            share.access.min(Access::Write)
        },
        user: share.user.clone(),
        via: if share.link.is_some() {
            Via::Link
        } else {
            Via::Share(share.id)
        },
    }
}

#[derive(Deserialize)]
struct SharePath {
    share: String,
}

/// `/v1/shared/{share}`: the share must be the caller's, and work now.
async fn check_share(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Path(p): Path<SharePath>,
    mut req: Request,
    next: Next,
) -> Response {
    let mut id = [0u8; 16];
    if hex::decode_to_slice(&p.share, &mut id).is_err() {
        return not_found("such share");
    }
    let found = blocking(move || {
        app.repo.with_meta(|m| {
            let Some(share) = m.share(&id)? else {
                return Ok(None);
            };
            if share.user.as_deref() != Some(caller.user.as_str())
                || !share_alive(m, &share, now())?
            {
                return Ok(None);
            }
            Ok(Some(share_view(&share)))
        })
    })
    .await;
    match found {
        Ok(Some(view)) => {
            req.extensions_mut().insert(view);
            next.run(req).await
        }
        Ok(None) => not_found("such share"),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct LinkPath {
    link: String,
}

/// `/v1/links/{link}`, without a token: the link is the key.
async fn check_link(
    State(app): State<Arc<App>>,
    Path(p): Path<LinkPath>,
    mut req: Request,
    next: Next,
) -> Response {
    let found = blocking(move || {
        app.repo.with_meta(|m| {
            let Some(share) = m.share_by_link(&p.link)? else {
                return Ok(None);
            };
            if !share_alive(m, &share, now())? {
                return Ok(None);
            }
            Ok(Some((share_view(&share), share.expires)))
        })
    })
    .await;
    match found {
        Ok(Some((view, expires))) => {
            req.extensions_mut().insert(view);
            req.extensions_mut().insert(LinkExpires(expires));
            next.run(req).await
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            "this link does not exist, or has expired",
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Clone, Copy)]
struct LinkExpires(Option<i64>);

async fn link_info(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Extension(LinkExpires(expires)): Extension<LinkExpires>,
) -> ApiResult<Response> {
    let found = blocking(move || {
        app.repo.with_meta(|m| {
            let root = view.root;
            Ok(lookup(m, &view, root)?.map(|(chain, head)| {
                let name = chain.last().and_then(|(_, n)| n.clone());
                w::LinkInfo {
                    item: item(&view, root, name, &head),
                    expires,
                }
            }))
        })
    })
    .await?;
    Ok(match found {
        Some(info) => Json(info).into_response(),
        None => not_found("such link"),
    })
}

/// What every view has: reading, and for one that may write, changing.
fn tree_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/item/{object}", get(item_view))
        .route("/dir/{object}", get(dir_view))
        .route("/content/{object}", get(content::content))
        .route("/zip/{object}", get(content::zip))
        .route("/history/{object}", get(history))
        .route("/mkdir", post(edit::mkdir))
        .route("/move", post(edit::move_entry))
        .route("/delete", post(edit::delete))
        .route("/restore", post(edit::restore))
        .route("/uploads", post(upload::start))
        .route(
            "/uploads/{upload}",
            put(upload::part).get(upload::status).delete(upload::cancel),
        )
        .route("/uploads/{upload}/commit", post(upload::commit_upload))
        .layer(DefaultBodyLimit::max(w::MAX_UPLOAD_PART))
}

/// The web routes under `/v1/ns/{ns}`, after its grant is checked.
pub(crate) fn namespace_routes() -> Router<Arc<App>> {
    tree_routes()
        .route("/deleted", get(deleted::list))
        .route("/undelete", post(deleted::undelete))
        .route("/shares", get(shares::list).post(shares::create))
        .route("/shares/{share}", axum::routing::delete(shares::remove))
}

/// The routes that need a token: a user's shares, and the users there are.
pub(crate) fn user_routes(app: &Arc<App>) -> Router {
    Router::new()
        .route("/v1/users", get(shares::users))
        .route("/v1/shared", get(shares::shared_with_me))
        .nest(
            "/v1/shared/{share}",
            tree_routes().route_layer(middleware::from_fn_with_state(app.clone(), check_share)),
        )
        .with_state(app.clone())
}

/// The routes that answer without a token: the page, signing in, and public links.
pub(crate) fn public_routes(app: &Arc<App>) -> Router {
    Router::new()
        .route("/", get(assets::page))
        .route("/s/{link}", get(assets::page))
        .route("/web/{file}", get(assets::file))
        .route("/v1/session", post(sign_in).delete(sign_out))
        .nest(
            "/v1/links/{link}",
            Router::new()
                .route("/", get(link_info))
                .merge(tree_routes())
                .route_layer(middleware::from_fn_with_state(app.clone(), check_link)),
        )
        .with_state(app.clone())
}

const SESSION_DAYS: u64 = 30;

fn session_cookie(value: &str, max_age: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{}={value}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict",
        w::SESSION_COOKIE
    ))
    .expect("a token is hex")
}

/// A token as a cookie, so that a browser's downloads and previews are signed in too. Only a token the
/// server knows is set, so the cookie is always plain hex.
async fn sign_in(State(app): State<Arc<App>>, Json(req): Json<w::SignIn>) -> ApiResult<Response> {
    let token = req.token.trim().to_string();
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Ok((StatusCode::UNAUTHORIZED, "missing or wrong token").into_response());
    }
    let hash = cairn_store::token_hash(&token);
    let user = blocking(move || app.repo.with_meta(|m| m.token_user(&hash))).await?;
    if user.is_none() {
        return Ok((StatusCode::UNAUTHORIZED, "missing or wrong token").into_response());
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            session_cookie(&token, SESSION_DAYS * 86_400),
        )],
    )
        .into_response())
}

async fn sign_out() -> Response {
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session_cookie("", 0))],
    )
        .into_response()
}

/// The session cookie's token, if the request has one.
pub(crate) fn cookie_token(req: &Request) -> Option<String> {
    let prefix = format!("{}=", w::SESSION_COOKIE);
    req.headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .map(str::trim)
        .find_map(|c| c.strip_prefix(&prefix))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// A request signed in by the cookie changes something only with the header no other site can send.
pub(crate) fn cookie_request_refused(req: &Request) -> Option<Response> {
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return None;
    }
    if req
        .headers()
        .get(w::WEB_HEADER)
        .is_some_and(|v| v.as_bytes() == b"1")
    {
        return None;
    }
    Some(
        (
            StatusCode::FORBIDDEN,
            format!(
                "a request signed in by the session cookie needs the header {}: 1",
                w::WEB_HEADER
            ),
        )
            .into_response(),
    )
}

/// Every answer: no page of this server sends its address, and a link's token with it, to another.
pub(crate) async fn no_referrer(mut resp: Response) -> Response {
    resp.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    resp
}
