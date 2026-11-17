//! Users' passwords, for signing in to the web on a computer that syncs nothing (docs/protocol.md
//! "Passwords"): kept as argon2id hashes, checked in the same time whether the user has one or not, and
//! each sign-in by one making a token of kind `browser` for that browser alone, which the user can take
//! back alone, and which signing out takes back.

use crate::web::session::Presented;
use crate::{blocking, ApiResult, App, Caller};
use argon2::password_hash::{PasswordHash, SaltString};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use cairn_protocol::wire::{check_password, NewPassword, TokenKind};
use cairn_protocol::Hash;
use std::sync::{Arc, OnceLock};

/// A password's hash, as a PHC string.
pub(crate) fn hash(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>())
        .map_err(|e| anyhow::anyhow!("make a salt: {e}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hash a password: {e}"))?
        .to_string())
}

/// Whether `password` is the one `phc` was made of. With no hash to check against, one made of nothing
/// anyone types is checked all the same, so that the answer takes as long for a user with no password,
/// or none at all, as for a wrong one.
pub(crate) fn verify(phc: Option<&str>, password: &str) -> bool {
    static NOBODY: OnceLock<String> = OnceLock::new();
    let nobody = NOBODY.get_or_init(|| hash("\0 no password \0").expect("hashing works"));
    let (phc, real) = match phc {
        Some(p) => (p, true),
        None => (nobody.as_str(), false),
    };
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    let ok = Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok();
    ok && real
}

/// Sets a user's password, or with `None` takes it away, and signs out every browser a password of
/// theirs signed in, but `keep`: a password changed is one somebody else may have known.
pub(crate) fn set(
    repo: &cairn_store::Repo,
    user: &str,
    password: Option<&str>,
    keep: Option<&Hash>,
) -> anyhow::Result<usize> {
    let phc = password.map(hash).transpose()?;
    repo.with_meta(|m| m.set_password(user, phc.as_deref()))?;
    let browsers: Vec<_> = repo
        .with_meta(|m| m.tokens())?
        .into_iter()
        .filter(|t| t.user == user && t.kind == TokenKind::Browser && Some(&t.hash) != keep)
        .collect();
    for t in &browsers {
        repo.with_meta(|m| m.remove_token(&t.hash))?;
    }
    Ok(browsers.len())
}

/// `PUT /v1/me/password`: the caller's own password set, changed or taken away. A user who has one
/// gives it as `current` to change it; one who forgot it has the server's administrator set another.
pub(crate) async fn change(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(Presented(presented)): Extension<Presented>,
    Json(req): Json<NewPassword>,
) -> ApiResult<Response> {
    if let Some(new) = &req.new {
        if let Err(why) = check_password(new) {
            return Ok((StatusCode::BAD_REQUEST, why).into_response());
        }
    }
    let answer = blocking(move || {
        let had = app.repo.with_meta(|m| m.password(&caller.user))?;
        if had.is_some() {
            let given = req.current.as_deref().unwrap_or("");
            if !verify(had.as_deref(), given) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "that is not your password now: give it as current to change it",
                )
                    .into_response());
            }
        }
        set(
            &app.repo,
            &caller.user,
            req.new.as_deref(),
            Some(&presented),
        )?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await?;
    Ok(answer)
}

/// A password read for the administrator's command: from a terminal without showing it, twice, or the
/// first line of whatever else stdin is.
pub(crate) fn read_from_stdin() -> anyhow::Result<String> {
    use std::io::{BufRead, IsTerminal};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_string());
    }
    let first = prompt("new password: ")?;
    let again = prompt("the same again: ")?;
    if first != again {
        anyhow::bail!("the two were not the same: nothing changed");
    }
    Ok(first)
}

#[cfg(unix)]
fn prompt(what: &str) -> anyhow::Result<String> {
    use rustix::termios::{tcgetattr, tcsetattr, LocalModes, OptionalActions};
    use std::io::{BufRead, Write};
    eprint!("{what}");
    std::io::stderr().flush()?;
    let stdin = std::io::stdin();
    let before = tcgetattr(&stdin)?;
    let mut quiet = before.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    tcsetattr(&stdin, OptionalActions::Now, &quiet)?;
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    tcsetattr(&stdin, OptionalActions::Now, &before)?;
    eprintln!();
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(not(unix))]
fn prompt(what: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, Write};
    eprint!("{what}(shown as typed) ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}
