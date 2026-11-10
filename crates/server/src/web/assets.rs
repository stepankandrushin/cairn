//! The web interface's own files, in the binary: one page, its script, style and icon. They name no
//! version, user or folder, so they answer without a token.

use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

/// What the page may load: only this server's own files, and nothing may frame it.
const PAGE_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; connect-src 'self'; frame-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

const INDEX: &str = include_str!("../../web/index.html");

/// Each file under `/web/`, with its type.
const FILES: &[(&str, &str, &str)] = &[
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_str!("../../web/app.js"),
    ),
    (
        "app.css",
        "text/css; charset=utf-8",
        include_str!("../../web/app.css"),
    ),
    (
        "icon.svg",
        "image/svg+xml",
        include_str!("../../web/icon.svg"),
    ),
];

/// `GET /` and `GET /s/{link}`: the one page, which reads where it was opened.
pub(super) async fn page() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, PAGE_POLICY),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        INDEX,
    )
        .into_response()
}

pub(super) async fn file(Path(name): Path<String>) -> Response {
    let Some((_, kind, body)) = FILES.iter().find(|(n, _, _)| *n == name) else {
        return (StatusCode::NOT_FOUND, "no such file").into_response();
    };
    (
        [
            (header::CONTENT_TYPE, *kind),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            // Checked again on every load, so a server upgraded never runs an old script with a new page.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        *body,
    )
        .into_response()
}
