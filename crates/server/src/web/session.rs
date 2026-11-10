//! Signing a browser in, by a token typed once or by a ticket a program with a token asked for. Either
//! sets the same cookie: a session the server signed with a key of its own, naming the token's hash and
//! when it ends. The token is never in the browser, and removing it signs out every browser it signed in.

use crate::{blocking, now, ApiResult, App, Caller};
use anyhow::{bail, Context, Result};
use axum::extract::{Extension, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use cairn_protocol::web as w;
use cairn_protocol::Hash;
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The most tickets waiting at once: each is asked for with a token, and none waits long.
const MAX_TICKETS: usize = 1024;

/// What the server signs sessions with, in `web/session.key`, made at its first start. Removing it signs
/// every browser out at the next start.
pub(crate) struct Sessions {
    key: [u8; 32],
    tickets: Mutex<HashMap<String, Ticket>>,
}

struct Ticket {
    token: Hash,
    until: Instant,
}

impl Sessions {
    pub(crate) fn open(data: &FsPath) -> Result<Self> {
        let dir = data.join("web");
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join("session.key");
        if !path.exists() {
            make_key(&path)?;
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let key = hex::decode(text.trim())
            .ok()
            .and_then(|k| <[u8; 32]>::try_from(k).ok());
        let Some(key) = key else {
            bail!(
                "{} is not a session key (64 hex digits): remove it, and a new one is made, which signs every browser out",
                path.display()
            );
        };
        Ok(Sessions {
            key,
            tickets: Mutex::new(HashMap::new()),
        })
    }

    fn mac(&self, token: &Hash, expires: u64) -> blake3::Hash {
        let mut h = blake3::Hasher::new_keyed(&self.key);
        h.update(b"cairn web session\0");
        h.update(&token.0);
        h.update(&expires.to_le_bytes());
        h.finalize()
    }

    /// The cookie's value for a session of `token` that ends at `expires`.
    fn cookie(&self, token: &Hash, expires: u64) -> String {
        format!(
            "{}.{expires}.{}",
            hex::encode(token.0),
            self.mac(token, expires).to_hex()
        )
    }

    /// The token a cookie's session names, if the server signed it and it has not ended.
    pub(crate) fn verify(&self, cookie: &str) -> Option<Hash> {
        let mut parts = cookie.split('.');
        let (token, expires, mac) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let token = Hash(hex::decode(token).ok()?.try_into().ok()?);
        let expires: u64 = expires.parse().ok()?;
        let mac = blake3::Hash::from_hex(mac).ok()?;
        // blake3's equality takes the same time wherever the two differ.
        (mac == self.mac(&token, expires) && expires > now() as u64).then_some(token)
    }

    fn issue(&self, token: Hash) -> Option<String> {
        let mut tickets = self.tickets.lock().expect("a panic aborts the process");
        let at = Instant::now();
        tickets.retain(|_, t| t.until > at);
        if tickets.len() >= MAX_TICKETS {
            return None;
        }
        let ticket =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        tickets.insert(
            ticket.clone(),
            Ticket {
                token,
                until: at + Duration::from_secs(w::TICKET_SECS),
            },
        );
        Some(ticket)
    }

    /// Once: a ticket taken is gone, whether it was in time or not.
    fn take(&self, ticket: &str) -> Option<Hash> {
        let t = self
            .tickets
            .lock()
            .expect("a panic aborts the process")
            .remove(ticket)?;
        (t.until > Instant::now()).then_some(t.token)
    }
}

fn make_key(path: &FsPath) -> Result<()> {
    let new: PathBuf = path.with_extension("key.new");
    let _ = std::fs::remove_file(&new);
    crate::write_secret(&new, &hex::encode(rand::random::<[u8; 32]>()))?;
    std::fs::rename(&new, path).with_context(|| format!("make {}", path.display()))?;
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("sync {}", dir.display()))?;
    }
    Ok(())
}

/// What a request presented: the hash a token is known by. The handler that issues tickets needs it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Presented(pub(crate) Hash);

fn session_cookie(value: &str, max_age: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{}={value}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict",
        w::SESSION_COOKIE
    ))
    .expect("a session is hex, digits and dots")
}

fn refused(why: &str) -> Response {
    (StatusCode::UNAUTHORIZED, why.to_string()).into_response()
}

/// `POST /v1/session`: a token the server knows, or a ticket in time, sets the session cookie, so that a
/// browser's downloads and previews are signed in too.
pub(crate) async fn sign_in(
    State(app): State<Arc<App>>,
    Json(req): Json<w::SignIn>,
) -> ApiResult<Response> {
    let token = match (req.token.as_deref().map(str::trim), req.ticket.as_deref()) {
        (Some(token), None) => {
            if token.is_empty() || !token.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Ok(refused("missing or wrong token"));
            }
            cairn_store::token_hash(token)
        }
        (None, Some(ticket)) => match app.sessions.take(ticket) {
            Some(token) => token,
            None => return Ok(refused(
                "that sign-in link has been used or has expired: open the page from Cairn again",
            )),
        },
        _ => return Ok(refused("sign in with a token or a ticket, one of them")),
    };
    let user = blocking({
        let app = app.clone();
        move || app.repo.with_meta(|m| m.token_user(&token))
    })
    .await?;
    if user.is_none() {
        return Ok(refused("missing or wrong token"));
    }
    let expires = now() as u64 + w::SESSION_SECS;
    Ok((
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            session_cookie(&app.sessions.cookie(&token, expires), w::SESSION_SECS),
        )],
    )
        .into_response())
}

pub(crate) async fn sign_out() -> Response {
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session_cookie("", 0))],
    )
        .into_response()
}

/// `POST /v1/session/ticket`, with a token: a ticket that signs a browser in once as that token.
pub(crate) async fn ticket(
    State(app): State<Arc<App>>,
    Extension(_caller): Extension<Caller>,
    Extension(presented): Extension<Presented>,
) -> Response {
    match app.sessions.issue(presented.0) {
        Some(ticket) => Json(w::SignInTicket {
            ticket,
            expires_in: w::TICKET_SECS,
        })
        .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many sign-in tickets are waiting: try again in two minutes",
        )
            .into_response(),
    }
}

/// The session cookie's value, if the request has one.
pub(crate) fn cookie_value(req: &Request) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions() -> Sessions {
        Sessions {
            key: [7; 32],
            tickets: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn a_session_holds_only_what_the_server_signed_and_until_it_ends() {
        let s = sessions();
        let token = Hash::of(b"a token");
        let later = now() as u64 + 60;
        let cookie = s.cookie(&token, later);
        assert!(!cookie.contains(&hex::encode(b"a token")));
        assert_eq!(s.verify(&cookie).map(|h| h.0), Some(token.0));
        // A later end, another token, or another key's signature is no session.
        let (hash, _) = cookie.split_once('.').unwrap();
        let mac = cookie.rsplit('.').next().unwrap();
        assert!(s.verify(&format!("{hash}.{}.{mac}", later + 1)).is_none());
        let other = Hash::of(b"another");
        assert!(s
            .verify(&format!("{}.{later}.{mac}", hex::encode(other.0)))
            .is_none());
        let mut k = sessions();
        k.key = [8; 32];
        assert!(k.verify(&cookie).is_none());
        assert!(s.verify(&s.cookie(&token, now() as u64 - 1)).is_none());
        for junk in ["", "a.b.c", &format!("{cookie}.x"), "0123"] {
            assert!(s.verify(junk).is_none(), "{junk}");
        }
    }

    #[test]
    fn a_ticket_is_taken_once_and_only_in_time() {
        let s = sessions();
        let token = Hash::of(b"t");
        let ticket = s.issue(token).unwrap();
        assert_eq!(ticket.len(), 43);
        assert_eq!(s.take(&ticket).map(|h| h.0), Some(token.0));
        assert!(s.take(&ticket).is_none());
        let late = s.issue(token).unwrap();
        s.tickets.lock().unwrap().get_mut(&late).unwrap().until = Instant::now();
        assert!(s.take(&late).is_none());
        assert!(s.take("nothing").is_none());
    }

    #[test]
    fn the_key_is_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("cairn-session-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = Sessions::open(&dir).unwrap();
        let again = Sessions::open(&dir).unwrap();
        assert_eq!(first.key, again.key);
        let path = dir.join("web/session.key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&path, "not a key").unwrap();
        let e = Sessions::open(&dir).err().unwrap();
        assert!(format!("{e:#}").contains("not a session key"), "{e:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
