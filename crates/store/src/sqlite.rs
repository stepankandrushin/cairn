//! The default store: one SQLite file in WAL mode with full synchronous commits.

use crate::meta::{CommitOutcome, MetaStore, StoredRevision, TreeState};
use anyhow::{anyhow, bail, Context, Result};
use cairn_failpoint::{check, hit};
use cairn_protocol::wire::{Change, CommitRequest, ConflictEntry};
use cairn_protocol::{
    check_and_apply, Content, Link, Metadata, ObjectId, ObjectKind, Op, OpReject, RevisionBody,
    RevisionId, TreeStore,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::path::Path;

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE namespaces (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, head INTEGER NOT NULL DEFAULT 0);
CREATE TABLE objects (
    ns INTEGER NOT NULL, id BLOB NOT NULL, kind TEXT NOT NULL, head BLOB NOT NULL,
    PRIMARY KEY (ns, id)
) WITHOUT ROWID;
CREATE TABLE entries (
    ns INTEGER NOT NULL, parent BLOB NOT NULL, name BLOB NOT NULL, object BLOB NOT NULL,
    PRIMARY KEY (ns, parent, name)
) WITHOUT ROWID;
CREATE INDEX entries_object ON entries (ns, object);
CREATE TABLE revisions (
    ns INTEGER NOT NULL, id BLOB NOT NULL, object BLOB NOT NULL, base BLOB, seq INTEGER NOT NULL, body TEXT NOT NULL,
    PRIMARY KEY (ns, id)
) WITHOUT ROWID;
CREATE INDEX revisions_object ON revisions (ns, object, seq);
CREATE TABLE journal (
    ns INTEGER NOT NULL, seq INTEGER NOT NULL, op_id BLOB NOT NULL, device BLOB NOT NULL,
    device_name TEXT NOT NULL, time INTEGER NOT NULL, ops TEXT NOT NULL,
    PRIMARY KEY (ns, seq)
) WITHOUT ROWID;
CREATE UNIQUE INDEX journal_op ON journal (ns, op_id);
CREATE TABLE conflicts (
    ns INTEGER NOT NULL, seq INTEGER NOT NULL, copy BLOB NOT NULL, object BLOB NOT NULL, competing BLOB NOT NULL,
    PRIMARY KEY (ns, seq, copy)
) WITHOUT ROWID;
"#];

pub struct SqliteStore {
    conn: Connection,
}

fn blob16(v: Vec<u8>) -> Result<[u8; 16]> {
    v.try_into()
        .map_err(|_| anyhow!("a stored ID is not 16 bytes"))
}

fn root_body() -> RevisionBody {
    RevisionBody {
        content: Content::Dir,
        metadata: Metadata::default(),
    }
}

fn kind_name(k: ObjectKind) -> &'static str {
    match k {
        ObjectKind::File => "file",
        ObjectKind::Dir => "dir",
        ObjectKind::Symlink => "symlink",
    }
}

fn kind_of(s: &str) -> Result<ObjectKind> {
    Ok(match s {
        "file" => ObjectKind::File,
        "dir" => ObjectKind::Dir,
        "symlink" => ObjectKind::Symlink,
        other => bail!("unknown object kind {other:?} in the database"),
    })
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::with_connection(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // FULL: an acknowledged commit survives power loss (invariant 1).
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > MIGRATIONS.len() {
            bail!(
                "this database has schema {version}, newer than this binary's {}: upgrade the binary",
                MIGRATIONS.len()
            );
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i + 1)?;
            tx.commit()?;
        }
        let mut store = Self { conn };
        store.create_namespace("default")?;
        Ok(store)
    }

    fn ns_id(&self, ns: &str) -> Result<i64> {
        self.conn
            .query_row("SELECT id FROM namespaces WHERE name = ?1", [ns], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| anyhow!("no namespace {ns:?}"))
    }
}

enum CommitErr {
    Reject(OpReject),
    Store(anyhow::Error),
}

impl From<OpReject> for CommitErr {
    fn from(r: OpReject) -> Self {
        CommitErr::Reject(r)
    }
}
impl From<rusqlite::Error> for CommitErr {
    fn from(e: rusqlite::Error) -> Self {
        CommitErr::Store(e.into())
    }
}
impl From<anyhow::Error> for CommitErr {
    fn from(e: anyhow::Error) -> Self {
        CommitErr::Store(e)
    }
}
impl From<serde_json::Error> for CommitErr {
    fn from(e: serde_json::Error) -> Self {
        CommitErr::Store(e.into())
    }
}

/// The op rules, applied inside the commit's transaction.
struct TxView<'a> {
    tx: &'a Transaction<'a>,
    ns: i64,
    seq: u64,
}

impl TreeStore for TxView<'_> {
    type Err = CommitErr;

    fn object(&mut self, o: ObjectId) -> Result<Option<(ObjectKind, RevisionId)>, CommitErr> {
        let row: Option<(String, Vec<u8>)> = self
            .tx
            .prepare_cached("SELECT kind, head FROM objects WHERE ns = ?1 AND id = ?2")?
            .query_row(params![self.ns, o.0], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(match row {
            Some((kind, head)) => Some((kind_of(&kind)?, RevisionId(blob16(head)?))),
            None => None,
        })
    }

    fn link_count(&mut self, o: ObjectId) -> Result<u64, CommitErr> {
        let n: i64 = self
            .tx
            .prepare_cached("SELECT count(*) FROM entries WHERE ns = ?1 AND object = ?2")?
            .query_row(params![self.ns, o.0], |r| r.get(0))?;
        Ok(n as u64)
    }

    fn entry(&mut self, link: &Link) -> Result<Option<ObjectId>, CommitErr> {
        let row: Option<Vec<u8>> = self
            .tx
            .prepare_cached(
                "SELECT object FROM entries WHERE ns = ?1 AND parent = ?2 AND name = ?3",
            )?
            .query_row(params![self.ns, link.parent.0, link.name.as_bytes()], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(match row {
            Some(o) => Some(ObjectId(blob16(o)?)),
            None => None,
        })
    }

    fn has_children(&mut self, dir: ObjectId) -> Result<bool, CommitErr> {
        let found: Option<i64> = self
            .tx
            .prepare_cached("SELECT 1 FROM entries WHERE ns = ?1 AND parent = ?2 LIMIT 1")?
            .query_row(params![self.ns, dir.0], |r| r.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    fn dir_parent(&mut self, dir: ObjectId) -> Result<Option<ObjectId>, CommitErr> {
        let row: Option<Vec<u8>> = self
            .tx
            .prepare_cached("SELECT parent FROM entries WHERE ns = ?1 AND object = ?2 LIMIT 1")?
            .query_row(params![self.ns, dir.0], |r| r.get(0))
            .optional()?;
        Ok(match row {
            Some(p) => Some(ObjectId(blob16(p)?)),
            None => None,
        })
    }

    fn insert_object(
        &mut self,
        o: ObjectId,
        rev: RevisionId,
        body: &RevisionBody,
    ) -> Result<(), CommitErr> {
        self.tx
            .prepare_cached("INSERT INTO objects (ns, id, kind, head) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![self.ns, o.0, kind_name(body.content.kind()), rev.0])?;
        self.tx
            .prepare_cached("INSERT INTO revisions (ns, id, object, base, seq, body) VALUES (?1, ?2, ?3, NULL, ?4, ?5)")?
            .execute(params![self.ns, rev.0, o.0, self.seq, serde_json::to_string(body)?])?;
        Ok(())
    }

    fn set_head(
        &mut self,
        o: ObjectId,
        base: RevisionId,
        rev: RevisionId,
        body: &RevisionBody,
    ) -> Result<(), CommitErr> {
        self.tx
            .prepare_cached("UPDATE objects SET head = ?3 WHERE ns = ?1 AND id = ?2")?
            .execute(params![self.ns, o.0, rev.0])?;
        self.tx
            .prepare_cached("INSERT INTO revisions (ns, id, object, base, seq, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
            .execute(params![self.ns, rev.0, o.0, base.0, self.seq, serde_json::to_string(body)?])?;
        Ok(())
    }

    fn add_link(&mut self, o: ObjectId, link: &Link) -> Result<(), CommitErr> {
        self.tx
            .prepare_cached(
                "INSERT INTO entries (ns, parent, name, object) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![self.ns, link.parent.0, link.name.as_bytes(), o.0])?;
        Ok(())
    }

    fn remove_link(&mut self, _o: ObjectId, link: &Link) -> Result<(), CommitErr> {
        self.tx
            .prepare_cached("DELETE FROM entries WHERE ns = ?1 AND parent = ?2 AND name = ?3")?
            .execute(params![self.ns, link.parent.0, link.name.as_bytes()])?;
        Ok(())
    }
}

impl MetaStore for SqliteStore {
    fn head(&mut self, ns: &str) -> Result<u64> {
        let head: i64 =
            self.conn
                .query_row("SELECT head FROM namespaces WHERE name = ?1", [ns], |r| {
                    r.get(0)
                })?;
        Ok(head as u64)
    }

    fn namespace_exists(&mut self, ns: &str) -> Result<bool> {
        Ok(self.ns_id(ns).is_ok())
    }

    fn create_namespace(&mut self, ns: &str) -> Result<()> {
        if self.ns_id(ns).is_ok() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO namespaces (name) VALUES (?1)", [ns])?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO objects (ns, id, kind, head) VALUES (?1, ?2, 'dir', ?3)",
            params![id, ObjectId::ROOT.0, RevisionId::ZERO.0],
        )?;
        tx.execute(
            "INSERT INTO revisions (ns, id, object, base, seq, body) VALUES (?1, ?2, ?3, NULL, 0, ?4)",
            params![id, RevisionId::ZERO.0, ObjectId::ROOT.0, serde_json::to_string(&root_body())?],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn commit(&mut self, ns: &str, req: &CommitRequest, time: i64) -> Result<CommitOutcome> {
        let ns_id = self.ns_id(ns)?;
        check("server.commit.begin")?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let earlier: Option<i64> = tx
            .query_row(
                "SELECT seq FROM journal WHERE ns = ?1 AND op_id = ?2",
                params![ns_id, req.op_id.0],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(seq) = earlier {
            return Ok(CommitOutcome::Committed {
                seq: seq as u64,
                replayed: true,
            });
        }
        let head: i64 =
            tx.query_row("SELECT head FROM namespaces WHERE id = ?1", [ns_id], |r| {
                r.get(0)
            })?;
        let seq = head + 1;
        let mut view = TxView {
            tx: &tx,
            ns: ns_id,
            seq: seq as u64,
        };
        for (i, op) in req.ops.iter().enumerate() {
            match check_and_apply(&mut view, op) {
                Ok(()) => {}
                Err(CommitErr::Reject(reason)) => {
                    // Dropping `tx` rolls the whole batch back.
                    return Ok(CommitOutcome::Rejected {
                        op_index: i,
                        reason,
                    });
                }
                Err(CommitErr::Store(e)) => return Err(e),
            }
            if let Op::Create {
                object,
                conflict_of: Some(c),
                ..
            } = op
            {
                tx.execute(
                    "INSERT INTO conflicts (ns, seq, copy, object, competing) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![ns_id, seq, object.0, c.object.0, c.competing_revision.0],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO journal (ns, seq, op_id, device, device_name, time, ops) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ns_id, seq, req.op_id.0, req.device.0, req.device_name, time, serde_json::to_string(&req.ops)?],
        )?;
        tx.execute(
            "UPDATE namespaces SET head = ?2 WHERE id = ?1",
            params![ns_id, seq],
        )?;
        hit("server.commit.before_commit");
        check("server.commit.write")?;
        tx.commit()?;
        hit("server.commit.durable");
        Ok(CommitOutcome::Committed {
            seq: seq as u64,
            replayed: false,
        })
    }

    fn changes(&mut self, ns: &str, after: u64, limit: u32) -> Result<Vec<Change>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT seq, op_id, device, device_name, time, ops FROM journal WHERE ns = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![ns_id, after as i64, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, op_id, device, device_name, time, ops) = row?;
            out.push(Change {
                seq: seq as u64,
                op_id: cairn_protocol::OpId(blob16(op_id)?),
                device: cairn_protocol::DeviceId(blob16(device)?),
                device_name,
                time,
                ops: serde_json::from_str(&ops)?,
            });
        }
        Ok(out)
    }

    fn conflicts(&mut self, ns: &str) -> Result<Vec<ConflictEntry>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.seq, c.copy, c.object, c.competing, j.device_name, j.time
             FROM conflicts c JOIN journal j ON j.ns = c.ns AND j.seq = c.seq
             WHERE c.ns = ?1 ORDER BY c.seq, c.copy",
        )?;
        let rows = stmt.query_map([ns_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, copy, object, competing, device_name, time) = row?;
            out.push(ConflictEntry {
                seq: seq as u64,
                copy: ObjectId(blob16(copy)?),
                object: ObjectId(blob16(object)?),
                competing_revision: RevisionId(blob16(competing)?),
                device_name,
                time,
            });
        }
        Ok(out)
    }

    fn revisions(&mut self, ns: &str) -> Result<Vec<StoredRevision>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self
            .conn
            .prepare("SELECT id, object, seq, body FROM revisions WHERE ns = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([ns_id], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, object, seq, body) = row?;
            out.push(StoredRevision {
                revision: RevisionId(blob16(id)?),
                object: ObjectId(blob16(object)?),
                seq: seq as u64,
                body: serde_json::from_str(&body)?,
            });
        }
        Ok(out)
    }

    fn tree_state(&mut self, ns: &str) -> Result<TreeState> {
        let ns_id = self.ns_id(ns)?;
        let mut state = TreeState::default();
        let mut stmt = self.conn.prepare(
            "SELECT parent, name, object FROM entries WHERE ns = ?1 ORDER BY parent, name",
        )?;
        let rows = stmt.query_map([ns_id], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        for row in rows {
            let (parent, name, object) = row?;
            state
                .entries
                .push((ObjectId(blob16(parent)?), name, ObjectId(blob16(object)?)));
        }
        let mut stmt = self
            .conn
            .prepare("SELECT id, head FROM objects WHERE ns = ?1 ORDER BY id")?;
        let rows = stmt.query_map([ns_id], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (id, head) = row?;
            state
                .heads
                .push((ObjectId(blob16(id)?), RevisionId(blob16(head)?)));
        }
        Ok(state)
    }

    fn namespaces(&mut self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM namespaces ORDER BY name")?;
        let names = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_passes_the_store_suite() {
        crate::meta::suite::run(&mut SqliteStore::in_memory().unwrap());
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_guessed_at() {
        let dir = std::env::temp_dir().join(format!(
            "cairn-schema-{}",
            hex::encode(rand::random::<[u8; 8]>())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("db");
        SqliteStore::open(&db).unwrap();
        Connection::open(&db)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        let err = SqliteStore::open(&db).err().expect("must refuse");
        assert!(err.to_string().contains("newer"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
