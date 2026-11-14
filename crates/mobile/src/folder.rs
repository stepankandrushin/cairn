//! One folder of the server, as it is now: listed when opened, a file downloaded when opened, and new
//! files and folders sent. Nothing of it is kept here but what is asked for.

use crate::session::access_text;
use crate::{
    bad, object_of, revision_of, CairnError, Header, HttpRequest, Result, TransferProgress,
};
use cairn_engine::client::{Client, RawAnswer};
use cairn_protocol::file::{self as f, FileQuery, Taken};
use cairn_protocol::web;
use cairn_protocol::wire::{Access, CommitRequest, CommitResponse, RejectReason};
use cairn_protocol::{
    Content, DeviceId, Link, Metadata, Name, ObjectId, ObjectKind, Op, OpId, OpReject,
    RevisionBody, RevisionId, Timestamp,
};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How many times a transfer the network broke is taken up again.
const TRIES: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

impl From<ObjectKind> for EntryKind {
    fn from(k: ObjectKind) -> Self {
        match k {
            ObjectKind::File => EntryKind::File,
            ObjectKind::Dir => EntryKind::Dir,
            ObjectKind::Symlink => EntryKind::Symlink,
        }
    }
}

/// An entry of a folder, as `GET …/dir` gives it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Entry {
    pub object: String,
    pub revision: String,
    pub kind: EntryKind,
    /// The name's bytes; empty for the namespace's root.
    pub name: Vec<u8>,
    /// The name as text, a byte that is not UTF-8 as `\xNN`.
    pub display: String,
    /// Whether `display` is the name itself: not where the name is not UTF-8.
    pub utf8: bool,
    pub size: Option<u64>,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub mode: u32,
    pub target: Option<String>,
}

impl Entry {
    pub(crate) fn of(i: &web::Item) -> Self {
        let name = i
            .name
            .as_ref()
            .map(|n| n.as_bytes().to_vec())
            .unwrap_or_default();
        Self {
            object: i.object.to_string(),
            revision: i.revision.to_string(),
            kind: i.kind.into(),
            utf8: name.is_empty() || std::str::from_utf8(&name).is_ok(),
            name,
            display: i.display.clone(),
            size: i.size,
            mtime_sec: i.mtime.sec,
            mtime_nsec: i.mtime.nsec,
            mode: i.mode,
            target: i.target_display.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Step {
    pub object: String,
    pub display: String,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Listing {
    pub dir: Entry,
    /// From the folder's root to `dir` itself.
    pub path: Vec<Step>,
    /// In name-byte order.
    pub entries: Vec<Entry>,
    pub access: String,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ItemView {
    pub item: Entry,
    pub path: Vec<Step>,
    pub access: String,
}

/// A file the server has, as it answered `PUT /file`.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Sent {
    pub object: String,
    pub revision: String,
    pub seq: u64,
    pub name: String,
    /// It was there already, with the same bytes; nothing was committed.
    pub existed: bool,
}

pub(crate) fn steps(path: &[web::Step]) -> Vec<Step> {
    path.iter()
        .map(|s| Step {
            object: s.object.to_string(),
            display: s.display.clone(),
        })
        .collect()
}

/// An answer to `PUT /file`, from its headers alone, as the Photos system keeps them: the file, or
/// why not. `Unreachable` for an answer that says nothing about the request (a proxy's 502), which is
/// sent again as it was.
pub fn read_answer(status: u16, headers: &[(String, String)]) -> Result<Sent> {
    let get = |k: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    if (200..300).contains(&status) {
        let parsed = (|| {
            let object: ObjectId = get(f::HEADER_OBJECT)?.parse().ok()?;
            let revision: RevisionId = get(f::HEADER_REVISION)?.parse().ok()?;
            let seq: u64 = get(f::HEADER_SEQ)?.parse().ok()?;
            use base64::Engine;
            let name = base64::engine::general_purpose::STANDARD
                .decode(get(f::HEADER_NAME)?)
                .ok()?;
            Some(Sent {
                object: object.to_string(),
                revision: revision.to_string(),
                seq,
                name: cairn_protocol::name::display_bytes(&name),
                existed: get(f::HEADER_EXISTED) == Some("1"),
            })
        })();
        return parsed.ok_or_else(|| {
            bad(format!(
                "the server answered {status} without saying what it stored: is it older than this app?"
            ))
        });
    }
    let reason = get(f::HEADER_ERROR)
        .map(f::from_header_text)
        .unwrap_or_else(|| format!("the server answered {status}"));
    Err(match status {
        502..=504 => CairnError::Unreachable { reason },
        404 => CairnError::NotFound { reason },
        401 => CairnError::Refused {
            status,
            reason: "the server refused the token: sign in again".into(),
        },
        _ => CairnError::Refused { status, reason },
    })
}

fn wait(tries: u32) {
    std::thread::sleep(Duration::from_millis(
        cairn_failpoint::tuned("mobile.retry_ms", 1000) * u64::from(1u32 << tries.min(5)),
    ));
}

fn name_of(s: &str) -> Result<Name> {
    Name::new(s.as_bytes().to_vec()).map_err(|e| bad(format!("{s:?} is not a name: {e}")))
}

fn sync_parent(p: &Path) -> std::io::Result<()> {
    if let Some(dir) = p.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[derive(uniffi::Object)]
pub struct Folder {
    pub(crate) ns: String,
    pub(crate) client: Client,
    pub(crate) device: DeviceId,
    pub(crate) device_name: String,
}

impl Folder {
    pub(crate) fn new(ns: String, client: Client, device: DeviceId, device_name: String) -> Self {
        Self {
            ns,
            client,
            device,
            device_name,
        }
    }

    /// `PUT /file` of `len` bytes from `open`, the same request again while the network breaks it:
    /// the server takes it twice as once.
    pub(crate) fn send(
        &self,
        q: &FileQuery,
        open: &dyn Fn() -> std::io::Result<File>,
        len: u64,
        progress: Option<&Arc<dyn TransferProgress>>,
    ) -> Result<Sent> {
        let query = f::query_string(q);
        let mut tries = 0;
        loop {
            let file = open()?;
            let mut body = Counting {
                inner: file,
                done: 0,
                total: len,
                progress: progress.cloned(),
                stopped: false,
            };
            let answer = self.client.put_file(&query, &mut body, len);
            if body.stopped {
                return Err(CairnError::Cancelled);
            }
            let result = match answer {
                Ok(RawAnswer {
                    status, headers, ..
                }) => read_answer(status, &headers),
                Err(e) => Err(CairnError::from(e)),
            };
            match result {
                Err(CairnError::Unreachable { .. }) if tries + 1 < TRIES => {
                    tries += 1;
                    wait(tries);
                }
                other => return other,
            }
        }
    }
}

/// A file being sent, counted and stoppable.
struct Counting {
    inner: File,
    done: u64,
    total: u64,
    progress: Option<Arc<dyn TransferProgress>>,
    stopped: bool,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(p) = &self.progress {
            if !p.progress(self.done, self.total) {
                self.stopped = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "stopped",
                ));
            }
        }
        let n = self.inner.read(buf)?;
        self.done += n as u64;
        Ok(n)
    }
}

#[uniffi::export]
impl Folder {
    /// The namespace, as the server names it.
    pub fn name(&self) -> String {
        self.ns.clone()
    }

    /// What this token may do here: `read`, `write` or `manage`.
    pub fn access(&self) -> String {
        access_text(self.client.access().unwrap_or(Access::Manage))
    }

    /// The journal's head now: where following the change feed starts.
    pub fn head(&self) -> Result<u64> {
        Ok(self.client.info()?.head)
    }

    /// The head once it is past `after`, or after `seconds` (at most 120) whatever it is: what the
    /// app waits on in the foreground, to tell the Files app something changed.
    pub fn wait_for_head(&self, after: u64, seconds: u32) -> Result<u64> {
        Ok(self
            .client
            .wait_for_head(after, Duration::from_secs(u64::from(seconds.min(120))))?)
    }

    /// A folder's entries now; `dir` is an object's hex, or `root`.
    pub fn list(&self, dir: String) -> Result<Listing> {
        let o = object_of(&dir)?;
        let l = self.client.listing((o != ObjectId::ROOT).then_some(o))?;
        Ok(Listing {
            dir: Entry::of(&l.dir),
            path: steps(&l.path),
            entries: l.entries.iter().map(Entry::of).collect(),
            access: access_text(l.access),
        })
    }

    /// An object as it is now, with its path from the root: `None` once it is deleted.
    pub fn item(&self, object: String) -> Result<Option<ItemView>> {
        Ok(self.client.item(object_of(&object)?)?.map(|v| ItemView {
            item: Entry::of(&v.item),
            path: steps(&v.path),
            access: access_text(v.access),
        }))
    }

    /// A file's bytes at `revision` into `dest`: written beside it first and synced, then renamed, so
    /// that nothing half-written is ever at `dest` (invariant 4). A broken connection takes up where
    /// it broke. `size`, if not 0, is what the listing said, and anything else is refused.
    pub fn download(
        &self,
        object: String,
        revision: String,
        dest: String,
        size: u64,
        progress: Option<Arc<dyn TransferProgress>>,
    ) -> Result<u64> {
        let object = object_of(&object)?;
        let revision = revision_of(&revision)?;
        let dest = PathBuf::from(dest);
        let file_name = dest
            .file_name()
            .ok_or_else(|| bad("the destination has no file name"))?
            .to_string_lossy()
            .into_owned();
        let part = dest.with_file_name(format!(".{file_name}.part"));
        let mut out = File::create(&part)?;
        let mut tries = 0;
        let mut stopped = false;
        loop {
            let from = out.seek(SeekFrom::End(0))?;
            let mut keep_going = |n: u64| {
                let go = progress.as_ref().is_none_or(|p| p.progress(from + n, size));
                stopped = !go;
                go
            };
            match self
                .client
                .content(object, revision, from, &mut out, &mut keep_going)
            {
                Ok(_) => break,
                Err(_) if stopped => {
                    drop(out);
                    let _ = std::fs::remove_file(&part);
                    return Err(CairnError::Cancelled);
                }
                Err(e) => {
                    let e = CairnError::from(e);
                    if matches!(e, CairnError::Unreachable { .. }) && tries + 1 < TRIES {
                        tries += 1;
                        wait(tries);
                        continue;
                    }
                    drop(out);
                    let _ = std::fs::remove_file(&part);
                    return Err(e);
                }
            }
        }
        out.flush()?;
        out.sync_all()?;
        let got = out.metadata()?.len();
        drop(out);
        if size != 0 && got != size {
            let _ = std::fs::remove_file(&part);
            return Err(bad(format!(
                "the download was {got} bytes, and the file has {size}: try again"
            )));
        }
        cairn_failpoint::hit("mobile.download.written");
        std::fs::rename(&part, &dest)?;
        sync_parent(&dest)?;
        Ok(got)
    }

    /// The request for a file's bytes at `revision`, for a player that streams it.
    pub fn content_request(&self, object: String, revision: String) -> Result<HttpRequest> {
        let object = object_of(&object)?;
        let revision = revision_of(&revision)?;
        Ok(HttpRequest {
            method: "GET".into(),
            url: self
                .client
                .url_of(&format!("/content/{object}?revision={revision}")),
            headers: vec![Header {
                name: "Authorization".into(),
                value: self.client.authorization().to_string(),
            }],
        })
    }

    /// A new folder `name` in `parent` (hex, or `root`), committed as this device.
    pub fn mkdir(&self, parent: String, name: String) -> Result<Entry> {
        let parent = object_of(&parent)?;
        let name = name_of(&name)?;
        let object = ObjectId::random();
        let revision = RevisionId::random();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let mtime = Timestamp {
            sec: now.as_secs() as i64,
            nsec: now.subsec_nanos(),
        };
        let req = CommitRequest {
            op_id: OpId::random(),
            device: self.device,
            device_name: self.device_name.clone(),
            ops: vec![Op::Create {
                object,
                revision,
                body: RevisionBody {
                    content: Content::Dir,
                    metadata: Metadata {
                        mode: 0o755,
                        mtime,
                        time_precision_ns: 1,
                        ..Default::default()
                    },
                },
                link: Link {
                    parent,
                    name: name.clone(),
                },
                conflict_of: None,
            }],
        };
        match self.client.commit(&req)? {
            CommitResponse::Committed { .. } => Ok(Entry {
                object: object.to_string(),
                revision: revision.to_string(),
                kind: EntryKind::Dir,
                display: name.to_string(),
                utf8: true,
                name: name.as_bytes().to_vec(),
                size: None,
                mtime_sec: mtime.sec,
                mtime_nsec: mtime.nsec,
                mode: 0o755,
                target: None,
            }),
            CommitResponse::Rejected { reject } => Err(CairnError::Refused {
                status: 409,
                reason: match reject {
                    RejectReason::Op {
                        reason: OpReject::NameTaken(_),
                        ..
                    } => "that name is taken".into(),
                    RejectReason::Op {
                        reason: OpReject::BadParent(_),
                        ..
                    } => "the folder it would go in is gone".into(),
                    other => format!("{other:?}"),
                },
            }),
        }
    }

    /// The file at `local` sent as `name` in `parent` (hex, or `root`). A name taken is refused
    /// unless `rename`, which numbers it; the same bytes there already are that file.
    #[allow(clippy::too_many_arguments)]
    pub fn put_file(
        &self,
        parent: String,
        name: String,
        local: String,
        mtime_sec: i64,
        mtime_nsec: u32,
        rename: bool,
        progress: Option<Arc<dyn TransferProgress>>,
    ) -> Result<Sent> {
        let parent = object_of(&parent)?;
        name_of(&name)?;
        let len = std::fs::metadata(&local)?.len();
        let q = FileQuery {
            parent: (parent != ObjectId::ROOT).then_some(parent),
            path: name,
            object: ObjectId::random(),
            revision: RevisionId::random(),
            device: self.device,
            device_name: self.device_name.clone(),
            mtime: f::format_time(
                Timestamp {
                    sec: mtime_sec,
                    nsec: mtime_nsec,
                },
                1,
            ),
            btime: None,
            taken: if rename { Taken::Rename } else { Taken::Fail },
        };
        self.send(&q, &|| File::open(&local), len, progress.as_ref())
    }
}
