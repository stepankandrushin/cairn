//! Thumbnails (docs/protocol.md "Thumbnails"): a JPEG of a picture, made on the first ask from a JPEG,
//! PNG, WebP or GIF file and kept under the data directory's `thumbs/`; and for what the server does not
//! decode, HEIC and video, the one a client made of it and sent, which every size is made from. Kept by
//! the file's manifest, so every revision and copy with the same bytes shares them, and a collection that
//! takes a manifest's chunks takes its thumbnails with them.

use super::content::{file_at, manifest_of, write_range};
use super::{bad_request, not_found, object_param, ObjectPath, View};
use crate::{blocking, ApiResult, App};
use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use cairn_protocol::wire::Access;
use cairn_protocol::{Hash, RevisionId};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::Deserialize;
use std::io::Cursor;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

/// The sizes a thumbnail is made in: its longer side, in pixels.
pub(crate) const SIZES: [u32; 4] = [128, 256, 512, 1024];
/// A file larger than this is not read for a thumbnail.
const MAX_SOURCE: u64 = 64 * 1024 * 1024;
/// A client's own thumbnail: at most this many bytes, and 1024 pixels on its longer side.
pub(crate) const MAX_CLIENT_BYTES: usize = 512 * 1024;
const MAX_CLIENT_SIDE: u32 = 1024;
/// Decodes at once: each may hold a picture's pixels, about 50 MB for 12 megapixels.
const DECODES: usize = 2;

pub(crate) struct Thumbs {
    dir: PathBuf,
    decoding: tokio::sync::Semaphore,
}

impl Thumbs {
    pub(crate) fn open(data: &FsPath) -> Result<Self> {
        let dir = data.join("thumbs");
        std::fs::create_dir_all(&dir).with_context(|| format!("make {}", dir.display()))?;
        Ok(Thumbs {
            dir,
            decoding: tokio::sync::Semaphore::new(DECODES),
        })
    }

    /// Where a manifest's thumbnails are kept, in a folder of its namespace's chunk directory's name.
    fn folder(&self, chunk_dir: &str, manifest: &Hash) -> PathBuf {
        let hex = manifest.to_string();
        self.dir.join(chunk_dir).join(&hex[..2])
    }

    fn file(&self, chunk_dir: &str, manifest: &Hash, what: &str) -> PathBuf {
        self.folder(chunk_dir, manifest)
            .join(format!("{manifest}-{what}.jpg"))
    }

    /// Takes every thumbnail of these manifests away, as a collection does with their chunks.
    pub(crate) fn forget(&self, chunk_dir: &str, manifests: &[Hash]) -> Result<()> {
        for m in manifests {
            for what in SIZES
                .iter()
                .map(|s| s.to_string())
                .chain(["client".to_string()])
            {
                match std::fs::remove_file(self.file(chunk_dir, m, &what)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(())
    }
}

/// Written whole or not at all: a reader never sees half of one.
fn put_file(path: &FsPath, bytes: &[u8]) -> Result<()> {
    let folder = path.parent().expect("a thumbnail is in a folder");
    std::fs::create_dir_all(folder)?;
    let tmp = folder.join(format!(".{:016x}.tmp", rand::random::<u64>()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn read_if_there(path: &FsPath) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// What the server decodes itself, by the file's first bytes, never by its name.
fn format_of(head: &[u8]) -> Option<ImageFormat> {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if head.starts_with(b"GIF8") {
        Some(ImageFormat::Gif)
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some(ImageFormat::WebP)
    } else {
        None
    }
}

/// A picture decoded within limits, turned as its EXIF says, and made a JPEG no larger than `size`.
fn make(bytes: &[u8], format: ImageFormat, size: u32) -> Result<Vec<u8>> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut picture = DynamicImage::from_decoder(decoder)?;
    picture.apply_orientation(orientation);
    jpeg_of(&picture, size)
}

fn jpeg_of(picture: &DynamicImage, size: u32) -> Result<Vec<u8>> {
    let small = if picture.width().max(picture.height()) > size {
        picture.thumbnail(size, size)
    } else {
        picture.clone()
    };
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80);
    DynamicImage::ImageRgb8(small.to_rgb8()).write_with_encoder(encoder)?;
    Ok(out)
}

#[derive(Deserialize)]
pub(super) struct ThumbQuery {
    revision: Option<String>,
    size: Option<u32>,
}

/// The revision asked for: `Some(None)` for none, `None` for one that is no revision.
fn revision_of(q: &ThumbQuery) -> Option<Option<RevisionId>> {
    match q.revision.as_deref().filter(|r| !r.is_empty()) {
        None => Some(None),
        Some(r) => r.parse().ok().map(Some),
    }
}

fn jpeg(bytes: Vec<u8>, manifest: &Hash, size: u32, fixed: bool, headers: &HeaderMap) -> Response {
    let etag = format!("\"{manifest}-{size}\"");
    let cache = if fixed {
        "private, max-age=31536000, immutable"
    } else {
        "private, no-cache"
    };
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v.as_bytes() == etag.as_bytes())
    {
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag), (header::CACHE_CONTROL, cache.into())],
        )
            .into_response();
    }
    let mut r = (StatusCode::OK, bytes).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    r
}

fn no_thumbnail() -> Response {
    (StatusCode::NOT_FOUND, "no thumbnail").into_response()
}

/// `GET B/thumb/{object}?revision=R&size=S`
pub(super) async fn get(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<ObjectPath>,
    Query(q): Query<ThumbQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let Some(object) = object_param(&view, &p.object) else {
        return Ok(not_found("such file"));
    };
    let size = q.size.unwrap_or(256);
    if !SIZES.contains(&size) {
        return Ok(bad_request("a thumbnail's size is 128, 256, 512 or 1024"));
    }
    let Some(revision) = revision_of(&q) else {
        return Ok(not_found("such revision"));
    };
    let fixed = revision.is_some();
    let (app2, view2) = (app.clone(), view.clone());
    let found = blocking(move || {
        let file = file_at(&app2, &view2, object, revision)?;
        let dir = app2.repo.with_meta(|m| m.chunk_dir(&view2.ns))?;
        Ok((file, dir))
    })
    .await?;
    let (file, dir) = match found {
        (Ok(f), dir) => (f, dir),
        (Err(r), _) => return Ok(r),
    };
    let manifest = file.manifest;
    let made = app.thumbs.file(&dir, &manifest, &size.to_string());
    if let Some(bytes) = read_if_there(&made)? {
        return Ok(jpeg(bytes, &manifest, size, fixed, &headers));
    }
    let client = app.thumbs.file(&dir, &manifest, "client");
    let _decoding = app
        .thumbs
        .decoding
        .acquire()
        .await
        .map_err(anyhow::Error::from)?;
    let out = blocking(move || -> Result<Option<Vec<u8>>> {
        // A client's own, for what the server does not decode.
        if let Some(theirs) = read_if_there(&client)? {
            let picture = image::load_from_memory_with_format(&theirs, ImageFormat::Jpeg)?;
            let out = jpeg_of(&picture, size)?;
            put_file(&made, &out)?;
            return Ok(Some(out));
        }
        if file.size > MAX_SOURCE {
            return Ok(None);
        }
        let m = manifest_of(&file.store, &file.manifest)?;
        let mut bytes = Vec::with_capacity(file.size as usize);
        write_range(&file.store, &m, 0, file.size, &mut |b| {
            bytes.extend_from_slice(b);
            Ok(())
        })?;
        let Some(format) = format_of(&bytes[..bytes.len().min(16)]) else {
            return Ok(None);
        };
        // A picture that does not decode has no thumbnail; it is no error of the server's.
        let Ok(out) = make(&bytes, format, size) else {
            return Ok(None);
        };
        put_file(&made, &out)?;
        Ok(Some(out))
    })
    .await?;
    Ok(match out {
        Some(bytes) => jpeg(bytes, &manifest, size, fixed, &headers),
        None => no_thumbnail(),
    })
}

/// `PUT B/thumb/{object}?revision=R`: a client's own thumbnail of a file the server does not decode, a
/// JPEG of at most 512 KB and 1024 pixels on its longer side, which every size is then made from.
pub(super) async fn put(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<ObjectPath>,
    Query(q): Query<ThumbQuery>,
    body: Bytes,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Write) {
        return Ok(r);
    }
    let Some(object) = object_param(&view, &p.object) else {
        return Ok(not_found("such file"));
    };
    let Some(revision) = revision_of(&q) else {
        return Ok(not_found("such revision"));
    };
    if body.len() > MAX_CLIENT_BYTES {
        return Ok((
            StatusCode::PAYLOAD_TOO_LARGE,
            "a thumbnail is at most 512 KB",
        )
            .into_response());
    }
    let picture = match image::load_from_memory_with_format(&body, ImageFormat::Jpeg) {
        Ok(p) => p,
        Err(_) => return Ok(bad_request("a thumbnail is a JPEG")),
    };
    let side = picture.width().max(picture.height());
    if side > MAX_CLIENT_SIDE || picture.width().min(picture.height()) == 0 {
        return Ok(bad_request(
            "a thumbnail is at most 1024 pixels on its longer side",
        ));
    }
    let (app2, view2) = (app.clone(), view.clone());
    let found = blocking(move || {
        let file = file_at(&app2, &view2, object, revision)?;
        let dir = app2.repo.with_meta(|m| m.chunk_dir(&view2.ns))?;
        Ok((file, dir))
    })
    .await?;
    let (manifest, dir) = match found {
        (Ok(f), dir) => (f.manifest, dir),
        (Err(r), _) => return Ok(r),
    };
    blocking(move || {
        // The sizes made before are made again from this one.
        app.thumbs.forget(&dir, &[manifest])?;
        put_file(&app.thumbs.file(&dir, &manifest, "client"), &body)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
