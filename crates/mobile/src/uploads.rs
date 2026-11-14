//! Uploads of photos and videos, queued in a database the app and its extensions share, so that one
//! killed at any step neither loses a queued photo nor sends it twice (invariant 4).
//!
//! A resource is planned first, durably, with the IDs of the file it will be: its request, whoever
//! sends it and however often, is the same one, and the server takes it twice as once. Only an answer
//! from the server makes it done, and a refusal keeps its reason for the app to show (invariant 8).
//!
//! The database is opened for each call and closed after it: iOS ends a suspended app that holds a
//! lock on a file in a shared container.

use crate::folder::{read_answer, Folder, Sent};
use crate::naming::{dest_path, file_name, NameStyle};
use crate::{bad, CairnError, Header, HttpRequest, Progress, Result};
use cairn_protocol::file::{self as f, FileQuery, Taken};
use cairn_protocol::{Name, ObjectId, RevisionId, Timestamp};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS upload (
    id INTEGER PRIMARY KEY,
    asset TEXT NOT NULL,
    kind INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    original TEXT NOT NULL,
    folder TEXT NOT NULL,
    path TEXT NOT NULL,
    object TEXT NOT NULL,
    revision TEXT NOT NULL,
    mtime TEXT NOT NULL,
    btime TEXT,
    state TEXT NOT NULL,
    job TEXT,
    tries INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    name TEXT,
    seq INTEGER,
    done_object TEXT,
    updated INTEGER NOT NULL,
    UNIQUE (asset, kind, idx)
);
CREATE INDEX IF NOT EXISTS upload_state ON upload (state, id);
CREATE INDEX IF NOT EXISTS upload_path ON upload (folder, path);
CREATE INDEX IF NOT EXISTS upload_job ON upload (job);
CREATE INDEX IF NOT EXISTS upload_object ON upload (folder, done_object);
CREATE TABLE IF NOT EXISTS skipped (
    folder TEXT NOT NULL,
    object TEXT NOT NULL,
    display TEXT NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (folder, object)
);
";

/// One resource of a photo library asset: its photo, its Live Photo's video, a RAW beside a JPEG.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AssetResource {
    /// The asset's local identifier.
    pub asset: String,
    /// The resource's type, as PhotoKit numbers it.
    pub kind: i32,
    /// Which of the asset's resources of that type.
    pub idx: u32,
    /// Its name on the phone, `IMG_0003.HEIC`.
    pub original_name: String,
    /// When it was taken.
    pub created_sec: i64,
    pub created_nsec: u32,
    /// The phone's offset from UTC then, in seconds east, for the name's local time.
    pub utc_offset: i32,
}

/// Where uploads go and what they are called.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Destination {
    pub folder: String,
    /// A path in the folder, `Photos/iPhone`.
    pub path: String,
    pub style: NameStyle,
    /// `YYYY/MM` below `path`.
    pub subfolders: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum UploadState {
    /// Queued, its request not sent yet.
    Planned,
    /// Its request may be on its way, or was lost: sent again as it is.
    Sent,
    /// The server has it.
    Done,
    /// The server refused it, for `error`'s reason: Retry plans it again.
    Failed,
    /// Not sent, for `error`'s reason: the phone could not read it.
    Skipped,
}

impl UploadState {
    fn text(self) -> &'static str {
        match self {
            UploadState::Planned => "planned",
            UploadState::Sent => "sent",
            UploadState::Done => "done",
            UploadState::Failed => "failed",
            UploadState::Skipped => "skipped",
        }
    }

    fn of(s: &str) -> Self {
        match s {
            "sent" => UploadState::Sent,
            "done" => UploadState::Done,
            "failed" => UploadState::Failed,
            "skipped" => UploadState::Skipped,
            _ => UploadState::Planned,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Upload {
    pub id: i64,
    pub asset: String,
    pub kind: i32,
    pub idx: u32,
    pub original_name: String,
    pub folder: String,
    pub path: String,
    pub object: String,
    pub revision: String,
    pub state: UploadState,
    /// The Photos system's job sending it, if one is.
    pub job: Option<String>,
    pub tries: u32,
    /// Why it failed or was skipped, or what went wrong with the last try.
    pub error: Option<String>,
    /// The name it has on the server, once done.
    pub name: Option<String>,
    pub seq: Option<u64>,
    /// The file on the server it is: its own, or one with its bytes that was there already.
    pub done_object: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct UploadStats {
    pub planned: u64,
    pub sent: u64,
    pub done: u64,
    pub failed: u64,
    pub skipped: u64,
}

/// What the Files app was not shown, and why.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SkippedEntry {
    pub folder: String,
    pub object: String,
    pub display: String,
    pub reason: String,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

const COLUMNS: &str = "id, asset, kind, idx, original, folder, path, object, revision, state, job, tries, error, name, seq, done_object";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Upload> {
    Ok(Upload {
        id: r.get(0)?,
        asset: r.get(1)?,
        kind: r.get(2)?,
        idx: r.get(3)?,
        original_name: r.get(4)?,
        folder: r.get(5)?,
        path: r.get(6)?,
        object: r.get(7)?,
        revision: r.get(8)?,
        state: UploadState::of(&r.get::<_, String>(9)?),
        job: r.get(10)?,
        tries: r.get(11)?,
        error: r.get(12)?,
        name: r.get(13)?,
        seq: r.get::<_, Option<i64>>(14)?.map(|s| s as u64),
        done_object: r.get(15)?,
    })
}

#[derive(uniffi::Object)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    fn db(&self) -> Result<Connection> {
        let c = Connection::open(&self.path)?;
        c.busy_timeout(Duration::from_secs(10))?;
        // No WAL: its shared memory file is a lock iOS may find a suspended process holding.
        c.pragma_update(None, "journal_mode", "DELETE")?;
        c.pragma_update(None, "synchronous", "FULL")?;
        Ok(c)
    }

    fn one(&self, c: &Connection, id: i64) -> Result<Upload> {
        c.query_row(
            &format!("SELECT {COLUMNS} FROM upload WHERE id = ?1"),
            [id],
            row,
        )
        .optional()?
        .ok_or_else(|| bad(format!("no upload {id}")))
    }

    fn query(&self, row_of: &Upload, folder: &Folder) -> Result<FileQuery> {
        let mtime = self.db()?.query_row(
            "SELECT mtime, btime FROM upload WHERE id = ?1",
            [row_of.id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )?;
        Ok(FileQuery {
            parent: None,
            path: row_of.path.clone(),
            object: row_of
                .object
                .parse()
                .map_err(|_| bad("a bad object ID in the database"))?,
            revision: row_of
                .revision
                .parse()
                .map_err(|_| bad("a bad revision ID in the database"))?,
            device: folder.device,
            device_name: folder.device_name.clone(),
            mtime: mtime.0,
            btime: mtime.1,
            taken: Taken::Rename,
        })
    }

    fn record(&self, id: i64, result: &Result<Sent>) -> Result<Upload> {
        let c = self.db()?;
        match result {
            Ok(s) => {
                c.execute(
                    "UPDATE upload SET state = 'done', error = NULL, name = ?2, seq = ?3, done_object = ?4, updated = ?5 WHERE id = ?1",
                    params![id, s.name, s.seq as i64, s.object, now()],
                )?;
            }
            Err(CairnError::Refused { reason, .. } | CairnError::NotFound { reason }) => {
                c.execute(
                    "UPDATE upload SET state = 'failed', error = ?2, updated = ?3 WHERE id = ?1",
                    params![id, reason, now()],
                )?;
            }
            // Not answered, or stopped: it goes again as it is.
            Err(e) => {
                c.execute(
                    "UPDATE upload SET error = ?2, updated = ?3 WHERE id = ?1 AND state != 'done'",
                    params![id, e.to_string(), now()],
                )?;
            }
        }
        self.one(&c, id)
    }
}

#[uniffi::export]
impl Store {
    /// The database at `path`, made if it is not there.
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Arc<Self>> {
        let s = Self {
            path: PathBuf::from(path),
        };
        s.db()?.execute_batch(SCHEMA)?;
        Ok(Arc::new(s))
    }

    pub fn get(&self, key: String) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db()?
            .query_row("SELECT v FROM kv WHERE k = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set(&self, key: String, value: Option<Vec<u8>>) -> Result<()> {
        let c = self.db()?;
        match value {
            Some(v) => c.execute(
                "INSERT INTO kv (k, v) VALUES (?1, ?2) ON CONFLICT (k) DO UPDATE SET v = ?2",
                params![key, v],
            )?,
            None => c.execute("DELETE FROM kv WHERE k = ?1", [key])?,
        };
        Ok(())
    }

    pub fn get_text(&self, key: String) -> Result<Option<String>> {
        Ok(self
            .get(key)?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    pub fn set_text(&self, key: String, value: Option<String>) -> Result<()> {
        self.set(key, value.map(String::into_bytes))
    }

    /// Each resource queued, once: one queued already is answered as it stands. A name another queued
    /// resource has in the same place is numbered, `name (2).ext`. Durable before this returns.
    pub fn plan(&self, dest: Destination, resources: Vec<AssetResource>) -> Result<Vec<Upload>> {
        let mut c = self.db()?;
        let tx = c.transaction()?;
        let mut out = Vec::new();
        for r in resources {
            let found = tx
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM upload WHERE asset = ?1 AND kind = ?2 AND idx = ?3"
                    ),
                    params![r.asset, r.kind, r.idx],
                    row,
                )
                .optional()?;
            if let Some(u) = found {
                out.push(u);
                continue;
            }
            let name = file_name(dest.style, &r.original_name, r.created_sec, r.utc_offset);
            let first = Name::new(name.as_bytes().to_vec())
                .map_err(|e| bad(format!("{name:?} is not a name: {e}")))?;
            let mut n = 1;
            let path = loop {
                let candidate = if n == 1 {
                    first.clone()
                } else {
                    f::numbered(&first, n)
                };
                let p = dest_path(
                    &dest.path,
                    dest.subfolders,
                    &String::from_utf8_lossy(candidate.as_bytes()),
                    r.created_sec,
                    r.utc_offset,
                );
                let taken: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM upload WHERE folder = ?1 AND path = ?2)",
                    params![dest.folder, p],
                    |x| x.get(0),
                )?;
                if !taken {
                    break p;
                }
                n += 1;
            };
            if f::path_names(&path).is_none() {
                return Err(bad(format!("{path:?} is not a path of names")));
            }
            let created = Timestamp {
                sec: r.created_sec,
                nsec: r.created_nsec,
            };
            // PhotoKit keeps a time to the millisecond.
            let time = f::format_time(created, 1_000_000);
            tx.execute(
                "INSERT INTO upload (asset, kind, idx, original, folder, path, object, revision, mtime, btime, state, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, 'planned', ?10)",
                params![
                    r.asset,
                    r.kind,
                    r.idx,
                    r.original_name,
                    dest.folder,
                    path,
                    ObjectId::random().to_string(),
                    RevisionId::random().to_string(),
                    time,
                    now()
                ],
            )?;
            let id = tx.last_insert_rowid();
            out.push(tx.query_row(
                &format!("SELECT {COLUMNS} FROM upload WHERE id = ?1"),
                [id],
                row,
            )?);
        }
        tx.commit()?;
        cairn_failpoint::hit("mobile.upload.planned");
        Ok(out)
    }

    pub fn upload(&self, id: i64) -> Result<Upload> {
        self.one(&self.db()?, id)
    }

    /// The upload a Photos system job sends.
    pub fn by_job(&self, job: String) -> Result<Option<Upload>> {
        Ok(self
            .db()?
            .query_row(
                &format!("SELECT {COLUMNS} FROM upload WHERE job = ?1"),
                [job],
                row,
            )
            .optional()?)
    }

    /// The upload of a resource, if it was queued.
    pub fn by_resource(&self, asset: String, kind: i32, idx: u32) -> Result<Option<Upload>> {
        Ok(self
            .db()?
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM upload WHERE asset = ?1 AND kind = ?2 AND idx = ?3"
                ),
                params![asset, kind, idx],
                row,
            )
            .optional()?)
    }

    /// The request that sends an upload, for the Photos system to send with the resource's bytes:
    /// the same for every try.
    pub fn request_for(&self, id: i64, folder: Arc<Folder>) -> Result<HttpRequest> {
        let u = self.upload(id)?;
        let q = self.query(&u, &folder)?;
        Ok(HttpRequest {
            method: "PUT".into(),
            url: folder
                .client
                .url_of(&format!("/file?{}", f::query_string(&q))),
            headers: vec![
                Header {
                    name: "Authorization".into(),
                    value: folder.client.authorization().to_string(),
                },
                Header {
                    name: "Content-Type".into(),
                    value: "application/octet-stream".into(),
                },
            ],
        })
    }

    /// Its request is on its way, by the Photos system's `job` or by the app.
    pub fn mark_sent(&self, id: i64, job: Option<String>) -> Result<Upload> {
        let c = self.db()?;
        c.execute(
            "UPDATE upload SET state = 'sent', job = COALESCE(?2, job), tries = tries + 1, updated = ?3 WHERE id = ?1 AND state IN ('planned', 'sent')",
            params![id, job, now()],
        )?;
        self.one(&c, id)
    }

    /// The server's answer to its request, as the Photos system kept it: done, failed with the
    /// server's reason, or, for an answer that says nothing of it, to be sent again.
    pub fn finish(&self, id: i64, status: u16, headers: Vec<Header>) -> Result<Upload> {
        let headers: Vec<(String, String)> =
            headers.into_iter().map(|h| (h.name, h.value)).collect();
        let result = read_answer(status, &headers);
        self.record(id, &result)
    }

    /// Its request got no answer (`reason`): it is sent again as it is.
    pub fn not_answered(&self, id: i64, reason: String) -> Result<Upload> {
        self.record(id, &Err(CairnError::Unreachable { reason }))
    }

    /// Not to be sent, for `reason`: the phone could not read it.
    pub fn skip(&self, id: i64, reason: String) -> Result<Upload> {
        let c = self.db()?;
        c.execute(
            "UPDATE upload SET state = 'skipped', error = ?2, updated = ?3 WHERE id = ?1 AND state != 'done'",
            params![id, reason, now()],
        )?;
        self.one(&c, id)
    }

    /// A failed or skipped upload queued again, as the same request.
    pub fn retry(&self, id: i64) -> Result<Upload> {
        let c = self.db()?;
        c.execute(
            "UPDATE upload SET state = 'planned', job = NULL, updated = ?2 WHERE id = ?1 AND state IN ('failed', 'skipped')",
            params![id, now()],
        )?;
        self.one(&c, id)
    }

    /// An upload sent by the app itself, its bytes read from `file`, a copy the app made of the
    /// resource: the same request the Photos system would send. Answered with where it stands.
    pub fn upload_now(
        &self,
        id: i64,
        folder: Arc<Folder>,
        file: String,
        progress: Option<Arc<dyn Progress>>,
    ) -> Result<Upload> {
        let u = self.upload(id)?;
        if matches!(u.state, UploadState::Done) {
            return Ok(u);
        }
        if !matches!(u.state, UploadState::Planned | UploadState::Sent) {
            return Err(bad(format!(
                "upload {id} is {}, not queued",
                u.state.text()
            )));
        }
        let q = self.query(&u, &folder)?;
        let len = std::fs::metadata(&file)?.len();
        self.mark_sent(id, None)?;
        cairn_failpoint::hit("mobile.upload.sending");
        let result = folder.send(&q, &|| std::fs::File::open(&file), len, progress.as_ref());
        cairn_failpoint::hit("mobile.upload.answered");
        self.record(id, &result)
    }

    /// Uploads queued or on their way, oldest first.
    pub fn pending(&self, limit: u32) -> Result<Vec<Upload>> {
        self.in_states(&["planned", "sent"], limit)
    }

    pub fn failed(&self, limit: u32) -> Result<Vec<Upload>> {
        self.in_states(&["failed", "skipped"], limit)
    }

    /// The last done, newest first.
    pub fn recent(&self, limit: u32) -> Result<Vec<Upload>> {
        let c = self.db()?;
        let mut s = c.prepare(&format!(
            "SELECT {COLUMNS} FROM upload WHERE state = 'done' ORDER BY updated DESC, id DESC LIMIT ?1"
        ))?;
        let rows = s.query_map([limit], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn stats(&self) -> Result<UploadStats> {
        let c = self.db()?;
        let mut s = c.prepare("SELECT state, COUNT(*) FROM upload GROUP BY state")?;
        let mut out = UploadStats::default();
        for r in s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (state, n) = r?;
            let n = n as u64;
            match UploadState::of(&state) {
                UploadState::Planned => out.planned = n,
                UploadState::Sent => out.sent = n,
                UploadState::Done => out.done = n,
                UploadState::Failed => out.failed = n,
                UploadState::Skipped => out.skipped = n,
            }
        }
        Ok(out)
    }

    /// The photo library asset a file on the server came from, if this phone sent it: its thumbnail
    /// is on the phone.
    pub fn asset_of(&self, folder: String, object: String) -> Result<Option<String>> {
        Ok(self
            .db()?
            .query_row(
                "SELECT asset FROM upload WHERE folder = ?1 AND done_object = ?2 LIMIT 1",
                params![folder, object],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// What the Files app was not shown, and why (invariant 8).
    pub fn note_skipped(&self, entries: Vec<SkippedEntry>) -> Result<()> {
        let mut c = self.db()?;
        let tx = c.transaction()?;
        for e in entries {
            tx.execute(
                "INSERT INTO skipped (folder, object, display, reason) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (folder, object) DO UPDATE SET display = ?3, reason = ?4",
                params![e.folder, e.object, e.display, e.reason],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn skipped(&self) -> Result<Vec<SkippedEntry>> {
        let c = self.db()?;
        let mut s = c.prepare(
            "SELECT folder, object, display, reason FROM skipped ORDER BY folder, display",
        )?;
        let rows = s.query_map([], |r| {
            Ok(SkippedEntry {
                folder: r.get(0)?,
                object: r.get(1)?,
                display: r.get(2)?,
                reason: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Everything forgotten, as at signing out: what was queued and not sent is not sent.
    pub fn forget_all(&self) -> Result<()> {
        self.db()?
            .execute_batch("DELETE FROM upload; DELETE FROM skipped; DELETE FROM kv;")?;
        Ok(())
    }
}

impl Store {
    fn in_states(&self, states: &[&str], limit: u32) -> Result<Vec<Upload>> {
        let c = self.db()?;
        let marks = states
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut s = c.prepare(&format!(
            "SELECT {COLUMNS} FROM upload WHERE state IN ({marks}) ORDER BY id LIMIT ?1"
        ))?;
        let rows = s.query_map([limit], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempdir::Dir, Arc<Store>) {
        let d = tempdir::Dir::new();
        let s = Store::open(d.0.join("cairn.db").to_string_lossy().into_owned()).unwrap();
        (d, s)
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir()
                    .join(format!("cairn-mobile-test-{}", hex::encode(rand_bytes())));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        fn rand_bytes() -> [u8; 8] {
            cairn_protocol::OpId::random().0[..8].try_into().unwrap()
        }
    }

    fn res(asset: &str, kind: i32, name: &str, t: i64) -> AssetResource {
        AssetResource {
            asset: asset.into(),
            kind,
            idx: 0,
            original_name: name.into(),
            created_sec: t,
            created_nsec: 250_000_000,
            utc_offset: 7 * 3600,
        }
    }

    fn dest() -> Destination {
        Destination {
            folder: "cloud".into(),
            path: "phone/iPhone".into(),
            style: NameStyle::Nextcloud,
            subfolders: false,
        }
    }

    #[test]
    fn a_resource_is_planned_once_and_keeps_its_ids() {
        let (_d, s) = store();
        let t = 1_691_920_214;
        let first = s
            .plan(
                dest(),
                vec![
                    res("A", 1, "IMG_0003.HEIC", t),
                    res("A", 9, "IMG_0003.MOV", t),
                ],
            )
            .unwrap();
        assert_eq!(first[0].path, "phone/iPhone/23-08-13 16-50-14 0003.heic");
        assert_eq!(first[1].path, "phone/iPhone/23-08-13 16-50-14 0003.mov");
        let again = s
            .plan(dest(), vec![res("A", 1, "IMG_0003.HEIC", t)])
            .unwrap();
        assert_eq!(again[0], first[0]);
        assert_eq!(s.stats().unwrap().planned, 2);
    }

    #[test]
    fn a_clash_in_the_queue_is_numbered() {
        let (_d, s) = store();
        let t = 1_691_920_214;
        let a = s
            .plan(dest(), vec![res("A", 1, "IMG_0003.HEIC", t)])
            .unwrap();
        let b = s
            .plan(dest(), vec![res("B", 1, "IMG_0003.HEIC", t)])
            .unwrap();
        assert_eq!(a[0].path, "phone/iPhone/23-08-13 16-50-14 0003.heic");
        assert_eq!(b[0].path, "phone/iPhone/23-08-13 16-50-14 0003 (2).heic");
    }

    #[test]
    fn answers_make_it_done_failed_or_sent_again() {
        let (_d, s) = store();
        let u = &s
            .plan(dest(), vec![res("A", 1, "IMG_0001.JPG", 0)])
            .unwrap()[0];
        s.mark_sent(u.id, Some("job-1".into())).unwrap();
        assert_eq!(s.by_job("job-1".into()).unwrap().unwrap().id, u.id);
        // A proxy's 502 says nothing of the request: it goes again.
        let x = s.finish(u.id, 502, vec![]).unwrap();
        assert_eq!(x.state, UploadState::Sent);
        let x = s
            .finish(
                u.id,
                409,
                vec![Header {
                    name: "X-Cairn-Error".into(),
                    value: f::header_text("phone in the path is a file, not a folder"),
                }],
            )
            .unwrap();
        assert_eq!(x.state, UploadState::Failed);
        assert_eq!(
            x.error.as_deref(),
            Some("phone in the path is a file, not a folder")
        );
        let x = s.retry(u.id).unwrap();
        assert_eq!(x.state, UploadState::Planned);
        assert_eq!(x.object, u.object);
        use base64::Engine;
        let ok = vec![
            Header {
                name: "x-cairn-object".into(),
                value: u.object.clone(),
            },
            Header {
                name: "x-cairn-revision".into(),
                value: u.revision.clone(),
            },
            Header {
                name: "x-cairn-seq".into(),
                value: "7".into(),
            },
            Header {
                name: "x-cairn-name".into(),
                value: base64::engine::general_purpose::STANDARD
                    .encode("70-01-01 07-00-00 0001.jpg"),
            },
        ];
        let x = s.finish(u.id, 201, ok).unwrap();
        assert_eq!(x.state, UploadState::Done);
        assert_eq!(x.seq, Some(7));
        assert_eq!(
            s.asset_of("cloud".into(), u.object.clone())
                .unwrap()
                .as_deref(),
            Some("A")
        );
    }
}
