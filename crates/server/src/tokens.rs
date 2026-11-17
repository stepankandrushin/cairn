//! A user's own tokens through the API (docs/protocol.md "Tokens"): listed, made, relabelled and taken
//! back, each user only their own, so a person signs out a lost phone or an old browser from any of
//! their devices without the server's administrator. A token's last use is kept to the hour.

use crate::web::session::Presented;
use crate::{blocking, new_token, now, ApiResult, App, Caller};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use cairn_protocol::wire::{
    check_token_label, MeToken, NewToken, TokenKind, TokenLabel, TokenView, Tokens,
};
use cairn_protocol::Hash;
use cairn_store::meta::TokenInfo;
use cairn_store::{token_hash, token_id};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The hour each token was last said to be used in, so that a token used all day writes once an hour.
#[derive(Default)]
pub(crate) struct Touched(Mutex<HashMap<Hash, i64>>);

impl Touched {
    /// Whether a request with this token, now, is the first in its hour.
    fn first_in_hour(&self, hash: &Hash, hour: i64) -> bool {
        let mut seen = self.0.lock().expect("a panic aborts the process");
        if seen.len() > 100_000 {
            seen.clear();
        }
        seen.insert(*hash, hour) != Some(hour)
    }
}

/// Says the token was used this hour, once an hour, without holding the request up.
pub(crate) fn touch(app: &Arc<App>, hash: Hash) {
    let hour = now() / 3600 * 3600;
    if !app.touched.first_in_hour(&hash, hour) {
        return;
    }
    let app = app.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = app.repo.with_meta(|m| m.touch_token(&hash, hour)) {
            eprintln!("error: say a token was used: {e:#}");
        }
    });
}

fn view(t: &TokenInfo, current: &Hash) -> TokenView {
    TokenView {
        id: t.id.clone(),
        label: t.label.clone(),
        kind: t.kind,
        created: t.created,
        used: t.used,
        current: t.hash == *current,
    }
}

/// The caller's tokens, oldest first.
fn mine(app: &App, user: &str) -> anyhow::Result<Vec<TokenInfo>> {
    let all = app.repo.with_meta(|m| m.tokens())?;
    Ok(all.into_iter().filter(|t| t.user == user).collect())
}

/// The token a request came with, for `GET /v1/me`.
pub(crate) fn current(app: &App, user: &str, hash: &Hash) -> anyhow::Result<Option<MeToken>> {
    Ok(mine(app, user)?
        .into_iter()
        .find(|t| t.hash == *hash)
        .map(|t| MeToken {
            id: t.id,
            label: t.label,
            kind: t.kind,
        }))
}

pub(crate) async fn list(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(Presented(current)): Extension<Presented>,
) -> ApiResult<Json<Tokens>> {
    let tokens = blocking(move || mine(&app, &caller.user)).await?;
    Ok(Json(Tokens {
        tokens: tokens.iter().map(|t| view(t, &current)).collect(),
    }))
}

/// A new token of the caller's, a device's, shown this once.
pub(crate) async fn add(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<TokenLabel>,
) -> ApiResult<Response> {
    if let Err(why) = check_token_label(&body.label) {
        return Ok((StatusCode::BAD_REQUEST, why).into_response());
    }
    let made = issue(app, caller.user, body.label, TokenKind::Device).await?;
    Ok((StatusCode::CREATED, Json(made)).into_response())
}

/// Makes a token of the user's, of this kind: what only this answer holds, and its ID.
pub(crate) async fn issue(
    app: Arc<App>,
    user: String,
    label: String,
    kind: TokenKind,
) -> ApiResult<NewToken> {
    let token = new_token();
    let hash = token_hash(&token);
    blocking(move || {
        app.repo
            .with_meta(|m| m.add_token(&user, &hash, &label, kind, now()))
    })
    .await?;
    Ok(NewToken {
        id: token_id(&hash),
        token,
    })
}

/// The caller's token of this ID: another user's is not there.
fn find(tokens: Vec<TokenInfo>, id: &str) -> Option<TokenInfo> {
    tokens.into_iter().find(|t| t.id == id.to_ascii_lowercase())
}

fn no_such_token() -> Response {
    (StatusCode::NOT_FOUND, "no such token").into_response()
}

pub(crate) async fn relabel(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(Presented(current)): Extension<Presented>,
    Path(id): Path<String>,
    Json(body): Json<TokenLabel>,
) -> ApiResult<Response> {
    if let Err(why) = check_token_label(&body.label) {
        return Ok((StatusCode::BAD_REQUEST, why).into_response());
    }
    let done = blocking(move || {
        let Some(found) = find(mine(&app, &caller.user)?, &id) else {
            return Ok(None);
        };
        app.repo
            .with_meta(|m| m.set_token_label(&found.hash, &body.label))?;
        Ok(Some(TokenInfo {
            label: body.label,
            ..found
        }))
    })
    .await?;
    Ok(match done {
        Some(t) => Json(view(&t, &current)).into_response(),
        None => no_such_token(),
    })
}

/// Takes one of the caller's tokens back, the one this request came with too: every device and browser
/// it signed in is signed out at its next request.
pub(crate) async fn remove(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let done = blocking(move || {
        let Some(found) = find(mine(&app, &caller.user)?, &id) else {
            return Ok(false);
        };
        app.repo.with_meta(|m| m.remove_token(&found.hash))?;
        Ok(true)
    })
    .await?;
    Ok(if done {
        StatusCode::NO_CONTENT.into_response()
    } else {
        no_such_token()
    })
}
