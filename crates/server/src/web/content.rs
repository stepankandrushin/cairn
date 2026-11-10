//! A file's bytes, or a folder's as a zip, streamed from the chunks as they are read: a chunk is checked
//! against its hash as it is read, and one that is missing or damaged ends the answer short of its
//! length, so a download fails rather than holds something else.

use super::zip::{Crc, ZipWriter};
use super::{bad_request, lookup, not_found, object_param, ObjectPath, Via, View};
use crate::{blocking, ApiResult, App};
use anyhow::{anyhow, bail, Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use cairn_protocol::name::display_bytes;
use cairn_protocol::{Content, Hash, Manifest, ObjectId, ObjectKind, RevisionId};
use cairn_store::{ChunkStore, FsChunkStore};
use serde::Deserialize;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;

/// What a stream is sent in: about this much at a time, a few pieces ahead of the reader.
const PIECE: usize = 256 * 1024;
const AHEAD: usize = 8;

type Piece = Result<Bytes, std::io::Error>;

/// A body fed from a blocking thread, piece by piece.
fn channel_body() -> (mpsc::Sender<Piece>, Body) {
    let (tx, rx) = mpsc::channel::<Piece>(AHEAD);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|piece| (piece, rx))
    });
    (tx, Body::from_stream(stream))
}

/// Writes into a channel body; fails once the reader has gone, which ends the thread's work.
struct ChannelWriter {
    tx: mpsc::Sender<Piece>,
    buf: Vec<u8>,
}

impl ChannelWriter {
    fn new(tx: mpsc::Sender<Piece>) -> Self {
        ChannelWriter {
            tx,
            buf: Vec::with_capacity(PIECE),
        }
    }

    fn send(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let piece = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(PIECE)));
        self.tx.blocking_send(Ok(piece)).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the reader went away")
        })
    }

    /// Ends the body with an error, so the connection is cut rather than the answer looking whole.
    fn fail(&self, e: &anyhow::Error) {
        eprintln!("error: a download ended short: {e:#}");
        let _ = self
            .tx
            .blocking_send(Err(std::io::Error::other(format!("{e:#}"))));
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= PIECE {
            self.send()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.send()
    }
}

fn manifest_of(store: &FsChunkStore, hash: &Hash) -> Result<Manifest> {
    let bytes = store
        .get(hash)?
        .ok_or_else(|| anyhow!("manifest {hash} is not on the server"))?;
    Manifest::decode(&bytes).with_context(|| format!("manifest {hash}"))
}

/// Bytes `[from, to)` of a file, holes as zeros, each chunk read and checked as it comes.
fn write_range(
    store: &FsChunkStore,
    manifest: &Manifest,
    from: u64,
    to: u64,
    out: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let zeros = vec![0u8; PIECE];
    let mut at = from;
    let fill = |at: &mut u64, upto: u64, out: &mut dyn FnMut(&[u8]) -> Result<()>| -> Result<()> {
        while *at < upto {
            let n = (upto - *at).min(PIECE as u64) as usize;
            out(&zeros[..n])?;
            *at += n as u64;
        }
        Ok(())
    };
    for (offset, chunk) in manifest.placed_chunks() {
        let end = offset + u64::from(chunk.len);
        if end <= at {
            continue;
        }
        if offset >= to {
            break;
        }
        fill(&mut at, offset.min(to), out)?;
        let data = store
            .get(&chunk.hash)?
            .ok_or_else(|| anyhow!("chunk {} is not on the server", chunk.hash))?;
        if data.len() as u64 != u64::from(chunk.len) {
            bail!("chunk {} is not the length its manifest says", chunk.hash);
        }
        let lo = (at - offset) as usize;
        let hi = (to.min(end) - offset) as usize;
        out(&data[lo..hi])?;
        at = offset + hi as u64;
    }
    fill(&mut at, to, out)
}

#[derive(Deserialize)]
pub(super) struct ContentQuery {
    revision: Option<String>,
    #[serde(default)]
    download: Option<String>,
}

/// One range of a `Range: bytes=` header against a size: `None` to answer it whole, as for several
/// ranges or one that does not parse; `Some(Err)` when nothing of the file is in it.
fn range(headers: &HeaderMap, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = headers.get(header::RANGE)?.to_str().ok()?.trim();
    let spec = spec.strip_prefix("bytes=")?;
    if spec.contains(',') || size == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let (from, to) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return Some(Err(()));
        }
        (size.saturating_sub(n), size)
    } else {
        let from: u64 = a.parse().ok()?;
        let to = if b.is_empty() {
            size
        } else {
            let last: u64 = b.parse().ok()?;
            if last < from {
                return None;
            }
            last.saturating_add(1).min(size)
        };
        (from, to)
    };
    if from >= size {
        return Some(Err(()));
    }
    Some(Ok((from, to)))
}

/// The type a browser is told, by the name's extension. What could run script in a page of this
/// server is sent sandboxed (`content_policy`).
pub(super) fn content_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "heic" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" | "md" | "markdown" | "rs" | "py" | "go" | "c" | "h" | "cpp" | "hpp" | "cc"
        | "java" | "kt" | "swift" | "sh" | "bash" | "zsh" | "toml" | "yaml" | "yml" | "ini"
        | "cfg" | "conf" | "log" | "csv" | "tsv" | "xml" | "css" | "js" | "mjs" | "ts" | "tsx"
        | "jsx" | "sql" | "rb" | "pl" | "lua" | "php" | "r" | "tex" | "srt" | "vtt" | "diff"
        | "patch" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A user's file opened in a page of this server could otherwise run script as the page: everything
/// is sandboxed, which gives it an origin of its own, but a PDF, whose viewer runs apart from the page
/// and which some browsers will not show sandboxed.
fn content_policy(kind: &str) -> Option<&'static str> {
    if kind == "application/pdf" {
        return None;
    }
    Some("sandbox; default-src 'none'; img-src 'self' data: blob:; media-src 'self' blob:; style-src 'unsafe-inline'; frame-ancestors 'self'")
}

/// `Content-Disposition` with the name in UTF-8, and a plain fallback for old readers.
pub(super) fn disposition(how: &str, name: &str) -> HeaderValue {
    let plain: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    HeaderValue::from_str(&format!(
        "{how}; filename=\"{plain}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// What a request for a file's bytes is answered from.
struct FileAt {
    name: String,
    revision: RevisionId,
    manifest: Hash,
    size: u64,
    store: Arc<FsChunkStore>,
}

fn file_at(
    app: &App,
    view: &View,
    object: ObjectId,
    revision: Option<RevisionId>,
) -> Result<Result<FileAt, Response>> {
    let found = app.repo.with_meta(|m| {
        let Some((chain, head)) = lookup(m, view, object)? else {
            return Ok(None);
        };
        let name = chain
            .last()
            .and_then(|(_, n)| n.as_ref())
            .map(|n| display_bytes(n.as_bytes()))
            .unwrap_or_else(|| view.ns.clone());
        let (revision, body) = match revision {
            None => (head.revision, head.body),
            Some(r) if r == head.revision => (head.revision, head.body),
            // A link gives what is there now, and no more.
            Some(_) if view.via == Via::Link => return Ok(None),
            Some(r) => match m.revision(&view.ns, r)? {
                Some(stored) if stored.object == object => (r, stored.body),
                _ => return Ok(None),
            },
        };
        Ok(Some((name, revision, body.content)))
    })?;
    let Some((name, revision, content)) = found else {
        return Ok(Err(not_found("such file")));
    };
    let Content::File { manifest, size } = content else {
        return Ok(Err(bad_request("that is not a file")));
    };
    Ok(Ok(FileAt {
        name,
        revision,
        manifest,
        size,
        store: app.repo.chunks(&view.ns)?,
    }))
}

pub(super) async fn content(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<ObjectPath>,
    Query(q): Query<ContentQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let Some(object) = object_param(&view, &p.object) else {
        return Ok(not_found("such file"));
    };
    let revision = match q.revision.as_deref().filter(|r| !r.is_empty()) {
        None => None,
        Some(r) => match r.parse() {
            Ok(r) => Some(r),
            Err(_) => return Ok(not_found("such revision")),
        },
    };
    let app2 = app.clone();
    let found = blocking(move || file_at(&app2, &view, object, revision)).await?;
    let file = match found {
        Ok(f) => f,
        Err(r) => return Ok(r),
    };
    let kind = content_type(&file.name);
    let (status, from, to) = match range(&headers, file.size) {
        None => (StatusCode::OK, 0, file.size),
        Some(Ok((from, to))) => (StatusCode::PARTIAL_CONTENT, from, to),
        Some(Err(())) => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{}", file.size))],
            )
                .into_response())
        }
    };
    let how = if q.download.as_deref().is_some_and(|d| d == "1") {
        "attachment"
    } else {
        "inline"
    };
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    resp_headers.insert(header::CONTENT_LENGTH, HeaderValue::from(to - from));
    resp_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    resp_headers.insert(header::CONTENT_DISPOSITION, disposition(how, &file.name));
    resp_headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp_headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    resp_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    if let Ok(etag) = HeaderValue::from_str(&format!("\"{}\"", file.revision)) {
        resp_headers.insert(header::ETAG, etag);
    }
    if let Some(policy) = content_policy(kind) {
        resp_headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(policy),
        );
    }
    if status == StatusCode::PARTIAL_CONTENT {
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {from}-{}/{}", to - 1, file.size)) {
            resp_headers.insert(header::CONTENT_RANGE, v);
        }
    }
    let (tx, body) = channel_body();
    tokio::task::spawn_blocking(move || {
        let mut out = ChannelWriter::new(tx);
        let result = manifest_of(&file.store, &file.manifest).and_then(|manifest| {
            if manifest.size != file.size {
                bail!(
                    "manifest {} says {} bytes, its revision {}",
                    file.manifest,
                    manifest.size,
                    file.size
                );
            }
            write_range(&file.store, &manifest, from, to, &mut |b| {
                Ok(out.write_all(b)?)
            })?;
            Ok(out.flush()?)
        });
        if let Err(e) = result {
            if !e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            }) {
                out.fail(&e);
            }
        }
    });
    Ok((status, resp_headers, body).into_response())
}

/// Files up to this size are read once, into memory, for their CRC and then sent; a bigger one is
/// read twice, as a zip's local header, which every reader can use, holds the CRC before the bytes.
const READ_ONCE: u64 = 32 * 1024 * 1024;

/// A name in the zip: the bytes as UTF-8, any that are not shown as U+FFFD, and a backslash, which a Linux
/// name may hold, as `_`: an unzip on Windows takes it for a folder's end, and `..\` for a way out.
fn zip_name(parts: &[Vec<u8>]) -> Vec<u8> {
    parts
        .iter()
        .map(|p| String::from_utf8_lossy(p).replace('\\', "_"))
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes()
}

fn write_zip(
    app: &App,
    view: &View,
    top: ObjectId,
    top_name: Vec<u8>,
    out: ChannelWriter,
) -> Result<()> {
    let store = app.repo.chunks(&view.ns)?;
    let mut zip = ZipWriter::new(out);
    // Depth first, each folder's entries read as it is reached, so a big tree is never held whole.
    let mut stack: Vec<(ObjectId, Vec<Vec<u8>>)> = vec![(top, vec![top_name])];
    while let Some((dir, path)) = stack.pop() {
        let (head, children) = app
            .repo
            .with_meta(|m| Ok((m.object_head(&view.ns, dir)?, m.children(&view.ns, dir)?)))?;
        let Some(head) = head else { continue };
        let mut dname = zip_name(&path);
        dname.push(b'/');
        zip.dir(
            &dname,
            head.body.metadata.mode,
            head.body.metadata.mtime.sec,
        )?;
        let mut dirs = Vec::new();
        for child in children {
            let mut cpath = path.clone();
            cpath.push(child.name.as_bytes().to_vec());
            let meta = &child.head.body.metadata;
            match (&child.head.kind, &child.head.body.content) {
                (ObjectKind::Dir, _) => dirs.push((child.object, cpath)),
                (ObjectKind::File, Content::File { manifest, size }) => {
                    let manifest = manifest_of(&store, manifest)?;
                    if manifest.size != *size {
                        bail!("a manifest is not the size its revision says");
                    }
                    let name = zip_name(&cpath);
                    if *size <= READ_ONCE {
                        let mut bytes = Vec::with_capacity(*size as usize);
                        write_range(&store, &manifest, 0, *size, &mut |b| {
                            bytes.extend_from_slice(b);
                            Ok(())
                        })?;
                        let mut crc = Crc::new();
                        crc.update(&bytes);
                        zip.file_header(&name, meta.mode, meta.mtime.sec, crc.value(), *size)?;
                        zip.data(&bytes)?;
                    } else {
                        let mut crc = Crc::new();
                        write_range(&store, &manifest, 0, *size, &mut |b| {
                            crc.update(b);
                            Ok(())
                        })?;
                        zip.file_header(&name, meta.mode, meta.mtime.sec, crc.value(), *size)?;
                        write_range(&store, &manifest, 0, *size, &mut |b| Ok(zip.data(b)?))?;
                    }
                }
                // Links are left out: a zip has no portable way to hold one.
                _ => {}
            }
        }
        // Popped in name order.
        stack.extend(dirs.into_iter().rev());
    }
    zip.finish()?.flush()?;
    Ok(())
}

pub(super) async fn zip(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Path(p): Path<ObjectPath>,
) -> ApiResult<Response> {
    let Some(object) = object_param(&view, &p.object) else {
        return Ok(not_found("such folder"));
    };
    let (app2, view2) = (app.clone(), view.clone());
    let found = blocking(move || {
        app2.repo.with_meta(|m| {
            let Some((chain, head)) = lookup(m, &view2, object)? else {
                return Ok(Err(not_found("such folder")));
            };
            if head.kind != ObjectKind::Dir {
                return Ok(Err(bad_request("only a folder is sent as a zip")));
            }
            let name = chain
                .last()
                .and_then(|(_, n)| n.as_ref())
                .map(|n| n.as_bytes().to_vec())
                .unwrap_or_else(|| view2.ns.clone().into_bytes());
            Ok(Ok(name))
        })
    })
    .await?;
    let name = match found {
        Ok(n) => n,
        Err(r) => return Ok(r),
    };
    let filename = format!("{}.zip", String::from_utf8_lossy(&name));
    let (tx, body) = channel_body();
    tokio::task::spawn_blocking(move || {
        let out = ChannelWriter::new(tx.clone());
        let failer = ChannelWriter::new(tx);
        if let Err(e) = write_zip(&app, &view, object, name, out) {
            if !e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            }) {
                failer.fail(&e);
            }
        }
    });
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            ),
            (
                header::CONTENT_DISPOSITION,
                disposition("attachment", &filename),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(range: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(header::RANGE, HeaderValue::from_str(range).unwrap());
        m
    }

    #[test]
    fn ranges() {
        assert_eq!(range(&HeaderMap::new(), 10), None);
        assert_eq!(range(&h("bytes=0-3"), 10), Some(Ok((0, 4))));
        assert_eq!(range(&h("bytes=4-"), 10), Some(Ok((4, 10))));
        assert_eq!(range(&h("bytes=-3"), 10), Some(Ok((7, 10))));
        assert_eq!(range(&h("bytes=-30"), 10), Some(Ok((0, 10))));
        assert_eq!(range(&h("bytes=5-100"), 10), Some(Ok((5, 10))));
        assert_eq!(range(&h("bytes=10-"), 10), Some(Err(())));
        assert_eq!(range(&h("bytes=0-1,4-5"), 10), None);
        assert_eq!(range(&h("bytes=5-2"), 10), None);
        assert_eq!(range(&h("items=0-1"), 10), None);
        assert_eq!(range(&h("bytes=0-1"), 0), None);
    }

    #[test]
    fn a_zip_name_cannot_climb_out_on_windows() {
        let parts = vec![b"top".to_vec(), b"..\\..\\evil".to_vec(), vec![0xff, b'x']];
        assert_eq!(zip_name(&parts), "top/.._.._evil/\u{fffd}x".as_bytes());
    }

    #[test]
    fn names_in_a_disposition_are_safe_and_whole() {
        let v = disposition("attachment", "a \"b\"\\ é.txt");
        let s = v.to_str().unwrap();
        assert!(
            s.starts_with("attachment; filename=\"a _b__ _.txt\""),
            "{s}"
        );
        assert!(
            s.ends_with("filename*=UTF-8''a%20%22b%22%5C%20%C3%A9.txt"),
            "{s}"
        );
        assert!(content_policy(content_type("x.HTML")).is_some());
        assert!(content_policy(content_type("x.svg")).is_some());
        assert!(content_policy(content_type("x.pdf")).is_none());
    }
}
