//! Pairing a device without pasting a token (docs/protocol.md "Pairing"): a signed-in device asks for a
//! code, shows it as text and a QR code, and the new device trades it for a token of the same user's, of
//! its own, which that user can take back alone. A code is 60 random bits, taken once, within ten
//! minutes, and kept in memory only: a restart forgets the codes waiting.

use crate::{blocking, tokens, ApiResult, App, Caller};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use cairn_protocol::wire::{
    check_origin, check_token_label, pair_code, pair_display, pair_uri, PairClaim, PairRequest,
    Paired, Pairing, TokenKind, PAIR_ALPHABET, PAIR_SECS,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// At most this many codes wait at once, for all users together.
const MAX_WAITING: usize = 256;

struct Waiting {
    user: String,
    label: Option<String>,
    until: Instant,
}

/// The codes waiting to be taken.
#[derive(Default)]
pub(crate) struct Codes(Mutex<HashMap<String, Waiting>>);

impl Codes {
    fn issue(&self, user: &str, label: Option<String>) -> Option<String> {
        let mut waiting = self.0.lock().expect("a panic aborts the process");
        let now = Instant::now();
        waiting.retain(|_, w| w.until > now);
        if waiting.len() >= MAX_WAITING {
            return None;
        }
        let bits: u64 = rand::random();
        let code: String = (0..12)
            .map(|i| PAIR_ALPHABET[((bits >> (i * 5)) & 31) as usize] as char)
            .collect();
        waiting.insert(
            code.clone(),
            Waiting {
                user: user.to_string(),
                label,
                // A test makes it short, to see one run out.
                until: now
                    + Duration::from_secs(cairn_failpoint::tuned("server.pair_secs", PAIR_SECS)),
            },
        );
        Some(code)
    }

    /// The code's user and label, once: a code is gone after any try with it, in time or not.
    fn take(&self, code: &str) -> Option<(String, Option<String>)> {
        let mut waiting = self.0.lock().expect("a panic aborts the process");
        let w = waiting.remove(code)?;
        (w.until > Instant::now()).then_some((w.user, w.label))
    }
}

/// `POST /v1/pair`, signed in.
pub(crate) async fn issue(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Json(req): Json<PairRequest>,
) -> ApiResult<Response> {
    if let Some(label) = &req.label {
        if let Err(why) = check_token_label(label) {
            return Ok((StatusCode::BAD_REQUEST, why).into_response());
        }
    }
    if let Some(origin) = &req.origin {
        if let Err(why) = check_origin(origin) {
            return Ok((StatusCode::BAD_REQUEST, why).into_response());
        }
    }
    let Some(code) = app.pairing.issue(&caller.user, req.label) else {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            "too many pairing codes are waiting: try again in ten minutes",
        )
            .into_response());
    };
    let uri = req
        .origin
        .as_deref()
        .map(|o| pair_uri(o.trim_end_matches('/'), &code));
    let qr_svg = match &uri {
        Some(uri) => Some(qr(uri)?),
        None => None,
    };
    Ok(Json(Pairing {
        code: pair_display(&code),
        expires_in: PAIR_SECS,
        uri,
        qr_svg,
    })
    .into_response())
}

/// The QR code of a link, as an SVG a page shows as it is.
fn qr(text: &str) -> anyhow::Result<String> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M)?;
    Ok(code
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build())
}

/// `POST /v1/pair/claim`, with no token: the code traded for a token of its user's.
pub(crate) async fn claim(
    State(app): State<Arc<App>>,
    Json(req): Json<PairClaim>,
) -> ApiResult<Response> {
    if let Some(label) = &req.label {
        if let Err(why) = check_token_label(label) {
            return Ok((StatusCode::BAD_REQUEST, why).into_response());
        }
    }
    let wrong = || {
        (
            StatusCode::UNAUTHORIZED,
            "no such pairing code: it is mistyped, used, or older than ten minutes",
        )
            .into_response()
    };
    let Some(code) = pair_code(&req.code) else {
        return Ok(wrong());
    };
    let Some((user, label)) = app.pairing.take(&code) else {
        return Ok(wrong());
    };
    let label = req
        .label
        .or(label)
        .unwrap_or_else(|| "paired device".to_string());
    let still = blocking({
        let (app, user) = (app.clone(), user.clone());
        move || {
            Ok(app
                .repo
                .with_meta(|m| m.users())?
                .iter()
                .any(|u| u.name == user))
        }
    })
    .await?;
    if !still {
        return Ok(wrong());
    }
    let made = tokens::issue(app, user.clone(), label, TokenKind::Paired).await?;
    Ok((
        StatusCode::CREATED,
        Json(Paired {
            user,
            id: made.id,
            token: made.token,
        }),
    )
        .into_response())
}
