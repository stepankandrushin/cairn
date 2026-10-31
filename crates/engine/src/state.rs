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
        Ok(Self { conn })
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
            for (id, node) in &new.nodes {
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

    /// Queue a commit durably, with the state it will leave, before the first attempt to send it.
    pub fn enqueue(&mut self, req: &CommitRequest, rows: &[(ObjectId, &Row)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO outbox (op_id, request) VALUES (?1, ?2)",
            params![req.op_id.0, serde_json::to_string(req)?],
        )?;
        put_rows(&tx, rows, &[])?;
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
