//! The client's durable state, one SQLite file per sync root: the base tree and cursor for three-way
//! merge, what each local object looked like, and the queue of commits not yet acknowledged.

use anyhow::{bail, Context, Result};
use cairn_protocol::wire::CommitRequest;
use cairn_protocol::{
    Content, Hash, Link, Metadata, Node, ObjectId, OpId, RevisionBody, Timestamp, Tree,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE base (object BLOB PRIMARY KEY, node TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE rows (object BLOB PRIMARY KEY, row TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE manifests (hash BLOB PRIMARY KEY, bytes BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE outbox (op_id BLOB PRIMARY KEY, request TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE intents (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
"#];

/// Every write to the state moves the generation, by triggers in the file itself, so that whatever
/// writes it, this build or another, a sync can tell that nothing in it has changed since one that had
/// nothing to do. Made once, all together, in one transaction; not a migration, so an older build still
/// opens the file, and its writes move the generation too.
const GENERATION_TRIGGERS: &[(&str, &str, &str)] = &[
    ("base", "INSERT", ""),
    ("base", "UPDATE", "WHEN OLD.node IS NOT NEW.node"),
    ("base", "DELETE", ""),
    ("rows", "INSERT", ""),
    ("rows", "UPDATE", "WHEN OLD.row IS NOT NEW.row"),
    ("rows", "DELETE", ""),
    ("manifests", "INSERT", ""),
    ("manifests", "UPDATE", "WHEN OLD.bytes IS NOT NEW.bytes"),
    ("manifests", "DELETE", ""),
    ("outbox", "INSERT", ""),
    ("outbox", "UPDATE", ""),
    ("outbox", "DELETE", ""),
    ("intents", "INSERT", ""),
    ("intents", "UPDATE", "WHEN OLD.value IS NOT NEW.value"),
    ("intents", "DELETE", ""),
    (
        "kv",
        "INSERT",
        "WHEN NEW.key NOT IN ('generation', 'steady')",
    ),
    (
        "kv",
        "UPDATE",
        "WHEN NEW.key NOT IN ('generation', 'steady') AND (OLD.value IS NOT NEW.value OR OLD.key IS NOT NEW.key)",
    ),
    (
        "kv",
        "DELETE",
        "WHEN OLD.key NOT IN ('generation', 'steady')",
    ),
];

const KEY_GENERATION: &str = "generation";
/// The last sync that had nothing to do and changed nothing, for the next to compare itself with.
const KEY_STEADY: &str = "steady";

fn generation_triggers() -> Vec<(String, String)> {
    GENERATION_TRIGGERS
        .iter()
        .map(|(table, op, when)| {
            let name = format!("generation_{table}_{}", op.to_lowercase());
            let sql = format!(
                "CREATE TRIGGER {name} AFTER {op} ON {table} {when} BEGIN \
                 UPDATE kv SET value = CAST(value AS INTEGER) + 1 WHERE key = '{KEY_GENERATION}'; END"
            );
            (name, sql)
        })
        .collect()
}

fn ensure_generation(conn: &mut Connection) -> Result<()> {
    let want = generation_triggers();
    let have: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND name GLOB 'generation_*' ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let counted = conn
        .query_row("SELECT 1 FROM kv WHERE key = ?1", [KEY_GENERATION], |_| {
            Ok(())
        })
        .optional()?
        .is_some();
    let mut sorted = want.clone();
    sorted.sort();
    if counted && have == sorted {
        return Ok(());
    }
    // Missing, partial or different: whatever was written meanwhile may have gone uncounted, so no
    // earlier sync with nothing to do says anything about now.
    let tx = conn.transaction()?;
    for (name, _) in &have {
        tx.execute_batch(&format!("DROP TRIGGER \"{name}\";"))?;
    }
    for (_, sql) in &want {
        tx.execute_batch(&format!("{sql};"))?;
    }
    tx.execute("DELETE FROM kv WHERE key = ?1", [KEY_STEADY])?;
    tx.execute(
        "INSERT INTO kv (key, value) VALUES (?1, '1') ON CONFLICT (key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
        [KEY_GENERATION],
    )?;
    tx.commit()?;
    Ok(())
}

/// Enough of `stat` to tell that a file has not changed since it was read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btime: Option<Timestamp>,
}

impl Fingerprint {
    /// As this build records one: a birth time this platform does not count on is left out, so that a
    /// row written before still matches.
    pub fn current(&self) -> Fingerprint {
        Fingerprint {
            btime: crate::fsops::birth_mark(self.btime),
            ..self.clone()
        }
    }
}

/// State this client shares with the server, ahead of the base tree: it pushed it or applied it. The
/// common ancestor once it is known to have reached the server, or while the head equals it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Synced {
    pub body: RevisionBody,
    pub links: BTreeSet<Link>,
    /// What the filesystem held at that moment, the pair of `body.metadata`.
    pub applied: Metadata,
    /// The commit that carries this state, which proves nothing until it is seen in the journal: it
    /// may have been refused. `None` when this state was read from the journal in the first place.
    #[serde(default)]
    pub commit: Option<OpId>,
    /// This state arrived and the filesystem is still to be brought to `body.metadata`: a folder made
    /// private until the last pass of a run that was killed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unapplied: bool,
    /// A killed run moved this folder's timestamp from this, which is to go back on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_time: Option<Timestamp>,
}

/// What this client knows about one local object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub fp: Fingerprint,
    /// The timestamps were too close to the read to prove that a later write would move them, so
    /// the next scan must read the file again.
    pub racy: bool,
    pub content: Content,
    /// What the last scan read, reused while the fingerprint holds.
    pub scan: Metadata,
    /// What the filesystem held when the base tree last matched it: the pair of the base's stored
    /// metadata, from which local changes are computed. `None` before the first full sync.
    #[serde(default)]
    pub applied: Option<Metadata>,
    #[serde(default)]
    pub synced: Option<Synced>,
    /// Some of `scan` could not be read: it proves nothing and is no baseline.
    #[serde(default)]
    pub incomplete: bool,
    /// The entries this device holds. Only what is here can be deleted here, so an entry missing from
    /// disk and from this set was never here, or this device removed it itself, and if the server
    /// has it, it comes down. Given up before removing, and taken back if a scan still finds it.
    /// `None` in a row from before this field: all that the base says.
    #[serde(default)]
    pub held: Option<BTreeSet<Link>>,
}

impl Row {
    pub fn known(&self) -> Known<'_> {
        Known {
            fp: &self.fp,
            racy: self.racy,
            incomplete: self.incomplete,
            content: self.content.clone(),
            scan: Some(&self.scan),
        }
    }
}

/// What a scan needs to know of a row: enough to tell an unchanged file without reading it.
#[derive(Clone, Debug)]
pub struct Known<'a> {
    pub fp: &'a Fingerprint,
    pub racy: bool,
    pub incomplete: bool,
    pub content: Content,
    /// What the last scan read, which an unchanged file is given. `None` where only a glance was read.
    pub scan: Option<&'a Metadata>,
}

impl Known<'_> {
    /// Whether this row speaks for a file found with this fingerprint, which is then not read again.
    pub fn vouches_for(&self, fp: &Fingerprint) -> bool {
        self.fp.current() == *fp
            && !self.racy
            && !self.incomplete
            && matches!(self.content, Content::File { .. })
    }
}

/// A row as a sync with nothing to do reads it: what the scan needs, without the rest.
#[derive(Debug, Deserialize)]
pub struct Glance {
    pub fp: Fingerprint,
    pub racy: bool,
    pub content: Content,
    #[serde(default)]
    pub incomplete: bool,
}

impl Glance {
    pub fn known(&self) -> Known<'_> {
        Known {
            fp: &self.fp,
            racy: self.racy,
            incomplete: self.incomplete,
            content: self.content.clone(),
            scan: None,
        }
    }
}

pub struct State {
    conn: Connection,
}

const KEY_CURSOR: &str = "cursor";
const KEY_CURSOR_OP: &str = "cursor_op";
pub const KEY_ROOT_ID: &str = "root_id";

impl State {
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn =
            Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // FULL: a queued commit or an intent survives power loss, not only a killed process.
        conn.pragma_update(None, "synchronous", "FULL")?;
        // On macOS that needs this too, or SQLite's fsync stops at the drive's cache. Elsewhere a no-op.
        conn.pragma_update(None, "fullfsync", "ON")?;
        conn.pragma_update(None, "checkpoint_fullfsync", "ON")?;
        let version: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > MIGRATIONS.len() {
            bail!("this root's state is schema {version}, newer than this binary: upgrade cairn");
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i + 1)?;
            tx.commit()?;
        }
        ensure_generation(&mut conn)?;
        Ok(Self { conn })
    }

    /// Moved by every write to the state; `None` if this file cannot say.
    pub fn generation(&self) -> Result<Option<u64>> {
        Ok(self.get(KEY_GENERATION)?.and_then(|s| s.parse().ok()))
    }

    /// The record of the last sync that had nothing to do, as text: `steady.rs` reads it.
    pub fn steady(&self) -> Result<Option<String>> {
        self.get(KEY_STEADY)
    }

    /// Written without moving the generation, which it is a statement about.
    pub fn set_steady(&self, text: Option<&str>) -> Result<()> {
        match text {
            Some(text) => self.set(KEY_STEADY, text),
            None => {
                self.conn
                    .execute("DELETE FROM kv WHERE key = ?1", [KEY_STEADY])?;
                Ok(())
            }
        }
    }

    /// The base's nodes for these objects and everything above them: enough of the tree to find
    /// their paths, without the rest.
    pub fn base_subset(&self, objects: impl IntoIterator<Item = ObjectId>) -> Result<Tree> {
        let mut nodes = BTreeMap::new();
        let mut want: Vec<ObjectId> = objects.into_iter().collect();
        let mut stmt = self
            .conn
            .prepare_cached("SELECT node FROM base WHERE object = ?1")?;
        while let Some(id) = want.pop() {
            if nodes.contains_key(&id) {
                continue;
            }
            let Some(text) = stmt
                .query_row([id.0], |r| r.get::<_, String>(0))
                .optional()?
            else {
                continue;
            };
            let node: Node = serde_json::from_str(&text)?;
            want.extend(node.links.iter().map(|l| l.parent));
            nodes.insert(id, node);
        }
        Ok(Tree::from_nodes(nodes))
    }

    /// Whether a mark or a queued commit waits: something a run began, which only the full sync settles.
    pub fn pending(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM intents) OR EXISTS (SELECT 1 FROM outbox)",
            [],
            |r| r.get(0),
        )?)
    }

    /// Every row, as a scan needs it and no more, in the order of `load_rows`. Read here and parsed on
    /// as many threads as there are, in batches put back in order.
    pub fn glance_rows(&self) -> Result<Vec<(ObjectId, Glance)>> {
        const BATCH: usize = 8_192;
        type Parsed = Vec<(usize, Vec<(ObjectId, Glance)>)>;
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().clamp(1, 8));
        let mut stmt = self
            .conn
            .prepare("SELECT object, row FROM rows ORDER BY object")?;
        let mut rows = stmt.query([])?;
        let (tx, rx) =
            std::sync::mpsc::sync_channel::<(usize, Vec<(ObjectId, String)>)>(threads * 2);
        let rx = std::sync::Mutex::new(rx);
        let mut parsed: Parsed = std::thread::scope(|s| -> Result<Parsed> {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| -> Result<Parsed> {
                        let mut done = Vec::new();
                        // A row that does not parse is said once all are read: the reader must not wait
                        // on workers that have stopped taking batches.
                        let mut failed = None;
                        loop {
                            let next = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                            let Ok((i, batch)) = next else {
                                break;
                            };
                            if failed.is_some() {
                                continue;
                            }
                            let parsed: serde_json::Result<Vec<(ObjectId, Glance)>> = batch
                                .into_iter()
                                .map(|(id, text)| Ok((id, serde_json::from_str::<Glance>(&text)?)))
                                .collect();
                            match parsed {
                                Ok(out) => done.push((i, out)),
                                Err(e) => failed = Some(e),
                            }
                        }
                        match failed {
                            Some(e) => Err(e.into()),
                            None => Ok(done),
                        }
                    })
                })
                .collect();
            let read = (|| -> Result<()> {
                let mut batch = Vec::with_capacity(BATCH);
                let mut n = 0;
                while let Some(row) = rows.next()? {
                    let id: [u8; 16] = row
                        .get_ref(0)?
                        .as_blob()?
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("a stored ID is not 16 bytes"))?;
                    batch.push((ObjectId(id), row.get_ref(1)?.as_str()?.to_string()));
                    if batch.len() == BATCH {
                        tx.send((n, std::mem::take(&mut batch)))?;
                        n += 1;
                    }
                }
                if !batch.is_empty() {
                    tx.send((n, batch))?;
                }
                Ok(())
            })();
            drop(tx);
            let mut all = Vec::new();
            for w in workers {
                all.extend(w.join().unwrap_or_else(|e| std::panic::resume_unwind(e))?);
            }
            read?;
            Ok(all)
        })?;
        parsed.sort_by_key(|(i, _)| *i);
        Ok(parsed.into_iter().flat_map(|(_, batch)| batch).collect())
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// The operation at the cursor, to tell later that the server still has this history.
    pub fn cursor_op(&self) -> Result<Option<OpId>> {
        Ok(self.get(KEY_CURSOR_OP)?.and_then(|s| s.parse().ok()))
    }

    pub fn cursor(&self) -> Result<u64> {
        Ok(self.get(KEY_CURSOR)?.map_or(Ok(0), |s| s.parse())?)
    }

    pub fn load_base(&self) -> Result<Tree> {
        let mut nodes = BTreeMap::new();
        let mut stmt = self.conn.prepare("SELECT object, node FROM base")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, node) = row?;
            let id: [u8; 16] = id
                .try_into()
                .map_err(|_| anyhow::anyhow!("a stored ID is not 16 bytes"))?;
            nodes.insert(ObjectId(id), serde_json::from_str::<Node>(&node)?);
        }
        Ok(Tree::from_nodes(nodes))
    }

    /// Make `new` the base at `cursor`, with the rows measured against it, in one transaction: a
    /// baseline paired with the wrong base would misread what changed. Writes only what differs.
    pub fn store_base(
        &mut self,
        old: &Tree,
        new: &Tree,
        cursor: u64,
        cursor_op: Option<OpId>,
        rows: &[(ObjectId, &Row)],
        gone: &[ObjectId],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        put_rows(&tx, rows, gone)?;
        {
            let mut put = tx.prepare_cached(
                "INSERT INTO base (object, node) VALUES (?1, ?2) ON CONFLICT (object) DO UPDATE SET node = excluded.node",
            )?;
            // The same tree when nothing came from the server: nothing to compare.
            let same = std::ptr::eq(old, new);
            for (id, node) in new.nodes.iter().filter(|_| !same) {
                if old.nodes.get(id) != Some(node) {
                    put.execute(params![id.0, serde_json::to_string(node)?])?;
                }
            }
        }
        tx.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                        params![KEY_CURSOR, cursor.to_string()],
        )?;
        if let Some(op) = cursor_op {
            tx.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![KEY_CURSOR_OP, op.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_rows(&self) -> Result<BTreeMap<ObjectId, Row>> {
        let mut out = BTreeMap::new();
        let mut stmt = self.conn.prepare("SELECT object, row FROM rows")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, value) = row?;
            let id: [u8; 16] = id
                .try_into()
                .map_err(|_| anyhow::anyhow!("a stored ID is not 16 bytes"))?;
            out.insert(ObjectId(id), serde_json::from_str::<Row>(&value)?);
        }
        Ok(out)
    }

    /// Write changed rows and delete the ones named, in one transaction.
    pub fn store_rows(&mut self, changed: &[(ObjectId, &Row)], gone: &[ObjectId]) -> Result<()> {
        if changed.is_empty() && gone.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        put_rows(&tx, changed, gone)?;
        tx.commit()?;
        Ok(())
    }

    pub fn put_manifests(&mut self, manifests: &[(Hash, Vec<u8>)]) -> Result<()> {
        if manifests.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut put =
                tx.prepare_cached("INSERT OR IGNORE INTO manifests (hash, bytes) VALUES (?1, ?2)")?;
            for (h, b) in manifests {
                put.execute(params![h.0, b])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn manifest(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT bytes FROM manifests WHERE hash = ?1",
                [hash.0],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn has_manifest(&self, hash: &Hash) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM manifests WHERE hash = ?1", [hash.0], |_| {
                Ok(())
            })
            .optional()?
            .is_some())
    }

    /// Drop every manifest not in `keep`.
    pub fn retain_manifests(&mut self, keep: &BTreeSet<Hash>) -> Result<()> {
        let tx = self.conn.transaction()?;
        let mut gone = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT hash FROM manifests")?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            for row in rows {
                let bytes: [u8; 32] = row?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("a stored hash is not 32 bytes"))?;
                if !keep.contains(&Hash(bytes)) {
                    gone.push(bytes);
                }
            }
        }
        {
            let mut del = tx.prepare_cached("DELETE FROM manifests WHERE hash = ?1")?;
            for h in gone {
                del.execute([h])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn outbox(&self) -> Result<Vec<CommitRequest>> {
        let mut stmt = self.conn.prepare("SELECT request FROM outbox")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    /// Queue a commit durably, with the state it will leave, before the first attempt to send it, and
    /// drop the marks it carries out.
    pub fn enqueue(
        &mut self,
        req: &CommitRequest,
        rows: &[(ObjectId, &Row)],
        marks: &[String],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO outbox (op_id, request) VALUES (?1, ?2)",
            params![req.op_id.0, serde_json::to_string(req)?],
        )?;
        put_rows(&tx, rows, &[])?;
        {
            let mut del = tx.prepare_cached("DELETE FROM intents WHERE key = ?1")?;
            for key in marks {
                del.execute([key])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn dequeue(&self, op: &OpId) -> Result<()> {
        self.conn
            .execute("DELETE FROM outbox WHERE op_id = ?1", [op.0])?;
        Ok(())
    }

    pub fn put_intent<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.conn.execute(
            "INSERT INTO intents (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    pub fn drop_intent(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM intents WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn drop_intents(&mut self, keys: &[String]) -> Result<()> {
        self.store_rows_dropping(&[], keys)
    }

    /// Several marks in one transaction, so one fsync however many there are.
    pub fn put_intents<T: Serialize>(&mut self, marks: &[(String, T)]) -> Result<()> {
        if marks.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut put = tx.prepare_cached(
                "INSERT INTO intents (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )?;
            for (key, value) in marks {
                put.execute(params![key, serde_json::to_string(value)?])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A mark and the rows it goes with, in one transaction, so that no kill leaves one without the other.
    pub fn put_intent_with_rows<T: Serialize>(
        &mut self,
        key: &str,
        value: &T,
        changed: &[(ObjectId, &Row)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        put_rows(&tx, changed, &[])?;
        tx.execute(
            "INSERT INTO intents (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Rows and the marks they answer, in one transaction, so that dying here repeats nothing.
    pub fn store_rows_dropping(
        &mut self,
        changed: &[(ObjectId, &Row)],
        marks: &[String],
    ) -> Result<()> {
        if changed.is_empty() && marks.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        put_rows(&tx, changed, &[])?;
        {
            let mut del = tx.prepare_cached("DELETE FROM intents WHERE key = ?1")?;
            for key in marks {
                del.execute([key])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every intent under a key prefix, oldest key order.
    pub fn intents<T: for<'de> Deserialize<'de>>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM intents WHERE key >= ?1 ORDER BY key")?;
        let rows = stmt.query_map([prefix], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, value) = row?;
            if !key.starts_with(prefix) {
                break;
            }
            out.push((key, serde_json::from_str(&value)?));
        }
        Ok(out)
    }
}

fn put_rows(
    tx: &rusqlite::Transaction<'_>,
    changed: &[(ObjectId, &Row)],
    gone: &[ObjectId],
) -> Result<()> {
    {
        let mut put = tx.prepare_cached(
            "INSERT INTO rows (object, row) VALUES (?1, ?2) ON CONFLICT (object) DO UPDATE SET row = excluded.row",
        )?;
        for (id, row) in changed {
            put.execute(params![id.0, serde_json::to_string(row)?])?;
        }
    }
    let mut del = tx.prepare_cached("DELETE FROM rows WHERE object = ?1")?;
    for id in gone {
        del.execute([id.0])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(size: u64) -> Row {
        Row {
            fp: Fingerprint {
                dev: 1,
                ino: 2,
                size,
                mtime: Timestamp::default(),
                ctime: Timestamp::default(),
                btime: None,
            },
            racy: false,
            content: Content::Dir,
            scan: Metadata::default(),
            applied: None,
            synced: None,
            incomplete: false,
            held: None,
        }
    }

    fn fresh() -> (std::path::PathBuf, State) {
        let dir = std::env::temp_dir().join(format!("cairn-state-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = State::open(&dir.join("state.sqlite")).unwrap();
        (dir, state)
    }

    /// The generation is what a sync with nothing to do trusts: any write moves it, whatever makes it,
    /// and only a write that changes something.
    #[test]
    fn every_write_to_the_state_moves_the_generation_and_nothing_else_does() {
        let (dir, mut state) = fresh();
        let id = ObjectId::random();
        let mut last = state.generation().unwrap().unwrap();
        let mut moved = |state: &State, what: &str, expect: bool| {
            let now = state.generation().unwrap().unwrap();
            assert_eq!(now != last, expect, "{what}: {last} -> {now}");
            last = now;
        };
        state.store_rows(&[(id, &row(1))], &[]).unwrap();
        moved(&state, "a new row", true);
        state.store_rows(&[(id, &row(1))], &[]).unwrap();
        moved(&state, "the same row again", false);
        state.store_rows(&[(id, &row(2))], &[]).unwrap();
        moved(&state, "a changed row", true);
        state.store_rows(&[], &[id]).unwrap();
        moved(&state, "a row gone", true);
        state
            .put_manifests(&[(Hash::of(b"m"), b"m".to_vec())])
            .unwrap();
        moved(&state, "a manifest", true);
        state
            .put_manifests(&[(Hash::of(b"m"), b"m".to_vec())])
            .unwrap();
        moved(&state, "the same manifest", false);
        state.retain_manifests(&BTreeSet::new()).unwrap();
        moved(&state, "manifests dropped", true);
        state.put_intent("x/1", &1).unwrap();
        moved(&state, "an intent", true);
        state.put_intent("x/1", &1).unwrap();
        moved(&state, "the same intent", false);
        state.drop_intent("x/1").unwrap();
        moved(&state, "an intent dropped", true);
        state.set("cursor", "7").unwrap();
        moved(&state, "the cursor", true);
        state.set("cursor", "7").unwrap();
        moved(&state, "the same cursor", false);
        state.set_steady(Some("{}")).unwrap();
        moved(&state, "the record of a sync with nothing to do", false);
        let old = Tree::new();
        let mut new = Tree::new();
        new.nodes.insert(
            id,
            Node {
                rev: cairn_protocol::RevisionId::random(),
                body: RevisionBody {
                    content: Content::Dir,
                    metadata: Metadata::default(),
                },
                links: BTreeSet::new(),
            },
        );
        state.store_base(&old, &new, 7, None, &[], &[]).unwrap();
        moved(&state, "the base", true);
        state.store_base(&new, &new, 7, None, &[], &[]).unwrap();
        moved(&state, "the same base", false);
        // Written by anything else than these methods, as an older build would: still counted.
        state
            .conn
            .execute("UPDATE rows SET row = row || ' '", [])
            .unwrap();
        state.store_rows(&[(id, &row(3))], &[]).unwrap();
        moved(&state, "a row", true);
        state
            .conn
            .execute("UPDATE rows SET row = row || ' '", [])
            .unwrap();
        moved(&state, "a row written by hand", true);
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The glance, parsed in batches on several threads, is every row in `load_rows`' order.
    #[test]
    fn a_glance_is_every_row_in_order() {
        let (dir, mut state) = fresh();
        let rows: Vec<(ObjectId, Row)> = (0..20_000u64)
            .map(|i| {
                let mut r = row(i);
                r.fp.ino = i % 7;
                r.racy = i % 3 == 0;
                (ObjectId::random(), r)
            })
            .collect();
        let list: Vec<(ObjectId, &Row)> = rows.iter().map(|(id, r)| (*id, r)).collect();
        state.store_rows(&list, &[]).unwrap();
        let loaded = state.load_rows().unwrap();
        let glanced = state.glance_rows().unwrap();
        assert_eq!(glanced.len(), loaded.len());
        for ((gid, g), (lid, l)) in glanced.iter().zip(&loaded) {
            assert_eq!(gid, lid);
            assert_eq!(
                (&g.fp, g.racy, &g.content, g.incomplete),
                (&l.fp, l.racy, &l.content, l.incomplete)
            );
        }
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A trigger gone or changed means writes may have gone uncounted: no record made before then
    /// stands.
    #[test]
    fn a_missing_trigger_is_made_again_and_the_record_dropped() {
        let (dir, state) = fresh();
        state.set_steady(Some("{}")).unwrap();
        let before = state.generation().unwrap().unwrap();
        drop(state);
        let state = State::open(&dir.join("state.sqlite")).unwrap();
        assert_eq!(
            state.steady().unwrap().as_deref(),
            Some("{}"),
            "opened again"
        );
        assert_eq!(state.generation().unwrap(), Some(before));
        state
            .conn
            .execute_batch("DROP TRIGGER generation_rows_update;")
            .unwrap();
        drop(state);
        let state = State::open(&dir.join("state.sqlite")).unwrap();
        assert_eq!(state.steady().unwrap(), None);
        assert!(state.generation().unwrap().unwrap() > before);
        let triggers: i64 = state
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'trigger'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(triggers as usize, GENERATION_TRIGGERS.len());
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }
}
