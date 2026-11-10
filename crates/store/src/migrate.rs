//! Moving the database between schema 3, which kept every body twice, in `journal` and in `revisions`,
//! as JSON text, and schema 4, which keeps it once, in `revisions`, with bodies and operations as binary
//! records (docs/format.md, "Metadata"); and giving back the pages a move leaves free. Each move is one
//! transaction: a kill or a full disk at any point leaves one schema or the other whole.

use crate::codec::{decode_body, decode_ops, encode_body, encode_ops};
use crate::sqlite::ops_of;
use anyhow::{bail, Context, Result};
use cairn_failpoint::{check, hit, tuned};
use cairn_protocol::local::bytes;
use cairn_protocol::{Op, RevisionBody};
use rusqlite::types::ToSqlOutput;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// A database's file by its pages: their size, how many, and how many are free, kept by SQLite for
/// reuse but never given back to the filesystem.
#[derive(Clone, Copy, Debug)]
pub struct Pages {
    pub size: u64,
    pub count: u64,
    pub free: u64,
}

impl Pages {
    pub fn file(&self) -> u64 {
        self.size * self.count
    }

    /// What the pages in use hold: about what a compacted file comes to.
    pub fn live(&self) -> u64 {
        self.size * self.count.saturating_sub(self.free)
    }

    /// More than half of the file free, and enough of it to matter.
    fn due(&self, floor: u64) -> bool {
        self.free * 2 > self.count && self.free * self.size >= floor
    }
}

pub fn pages(conn: &Connection) -> Result<Pages> {
    let get = |pragma: &str| -> Result<u64> {
        let n: i64 = conn.pragma_query_value(None, pragma, |r| r.get(0))?;
        Ok(n.max(0) as u64)
    };
    Ok(Pages {
        size: get("page_size")?,
        count: get("page_count")?,
        free: get("freelist_count")?,
    })
}

/// Rows or operations moved between two of a move's failpoints.
const STEP: u64 = 256;

/// The page cache a move works with, in KiB: it looks up a revision for each create and update.
const MOVE_CACHE_KIB: u64 = 128 << 10;

/// Free pages below this are not worth a compaction.
const COMPACT_FLOOR: u64 = 16 << 20;

const TABLES_4: &str = "
CREATE TABLE revisions_4 (
    ns INTEGER NOT NULL, id BLOB NOT NULL, object BLOB NOT NULL, base BLOB, seq INTEGER NOT NULL, body BLOB NOT NULL,
    PRIMARY KEY (ns, id)
) WITHOUT ROWID;
CREATE TABLE journal_4 (
    ns INTEGER NOT NULL, seq INTEGER NOT NULL, op_id BLOB NOT NULL, device BLOB NOT NULL,
    device_name TEXT NOT NULL, time INTEGER NOT NULL, user TEXT, ops BLOB NOT NULL, json_len INTEGER NOT NULL,
    PRIMARY KEY (ns, seq)
) WITHOUT ROWID;
";

const TABLES_3: &str = "
CREATE TABLE revisions_3 (
    ns INTEGER NOT NULL, id BLOB NOT NULL, object BLOB NOT NULL, base BLOB, seq INTEGER NOT NULL, body TEXT NOT NULL,
    PRIMARY KEY (ns, id)
) WITHOUT ROWID;
CREATE TABLE journal_3 (
    ns INTEGER NOT NULL, seq INTEGER NOT NULL, op_id BLOB NOT NULL, device BLOB NOT NULL,
    device_name TEXT NOT NULL, time INTEGER NOT NULL, ops TEXT NOT NULL, user TEXT,
    PRIMARY KEY (ns, seq)
) WITHOUT ROWID;
";

/// The new tables in place of the old, under the old names, with their indexes.
fn swap_tables(tx: &Connection, suffix: &str) -> Result<()> {
    tx.execute_batch(&format!(
        "DROP TABLE journal;
         DROP TABLE revisions;
         ALTER TABLE journal_{suffix} RENAME TO journal;
         ALTER TABLE revisions_{suffix} RENAME TO revisions;
         CREATE UNIQUE INDEX journal_op ON journal (ns, op_id);
         CREATE INDEX revisions_object ON revisions (ns, object, seq);"
    ))?;
    Ok(())
}

fn checkpoint(conn: &Connection) -> Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        r.get::<_, i64>(0)
    })?;
    Ok(())
}

/// The bytes free to this user on the filesystem holding `path`, if it says.
fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let s = rustix::fs::statvfs(path).ok()?;
        Some(s.f_bavail.saturating_mul(s.f_frsize))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The folder a database file is in.
fn folder_of(file: &Path) -> PathBuf {
    match file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Where SQLite puts a temporary database: its unix VFS's first of these that is a folder it may
/// write in.
fn sqlite_temp_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let from_env = ["SQLITE_TMPDIR", "TMPDIR"]
            .into_iter()
            .filter_map(|v| std::env::var_os(v).map(PathBuf::from));
        let fixed = ["/var/tmp", "/usr/tmp", "/tmp", "."]
            .into_iter()
            .map(PathBuf::from);
        from_env.chain(fixed).find(|d| {
            d.is_dir()
                && rustix::fs::access(
                    d.as_path(),
                    rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
                )
                .is_ok()
        })
    }
    #[cfg(not(unix))]
    {
        Some(std::env::temp_dir())
    }
}

fn same_filesystem(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev(),
            _ => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        true
    }
}

/// Where a move whose new tables take at most `new` bytes lacks room, if anywhere: they go into the
/// WAL as they are written, and from it into the file at the checkpoint, into the pages the file has
/// free and past its end for the rest. With an eighth and 8 MiB to spare. The old tables' pages are
/// only freed, which the compaction after it gives back.
fn move_short_of_room(file: &Path, p: &Pages, new: u64, knob: &str) -> Option<(PathBuf, u64, u64)> {
    let growth = new.saturating_sub(p.free * p.size);
    let need = (new + growth + new / 8 + (8 << 20)) * tuned(knob, 1);
    if need == 0 {
        return None;
    }
    let dir = folder_of(file);
    let free = free_bytes(&dir)?;
    (free < need).then_some((dir, need, free))
}

/// What a move did.
#[derive(Default)]
struct Moved {
    commits: u64,
    ops: u64,
    revisions: u64,
    /// Bodies the journal keeps itself, as `revisions` did not hold them for that commit.
    bodies_here: u64,
    /// Commits whose JSON, made again, is not as long as the move from schema 3 found it.
    json_differs: u64,
}

/// Moves a database of schema 3 to 4, in one transaction. Checks for room first. `quiet` for a
/// database made just now, which has nothing in it to move.
pub(crate) fn to_4(conn: &mut Connection, file: Option<&Path>, quiet: bool) -> Result<()> {
    let started = Instant::now();
    let before = pages(conn)?;
    hit("server.migrate.due");
    if let Some(file) = file {
        // The new tables take at most what the whole database holds now.
        if let Some((place, need, free)) =
            move_short_of_room(file, &before, before.live(), "server.migrate_room")
        {
            bail!(
                "this database has schema 3, which this binary moves to schema 4 before it serves: that takes {} where {} is, which has {} free. It stays at schema 3: make room, or go on with the binary from before",
                bytes(need),
                place.display(),
                bytes(free)
            );
        }
    }
    let cache: i64 = conn.pragma_query_value(None, "cache_size", |r| r.get(0))?;
    let kib = tuned("server.migrate_cache_kib", MOVE_CACHE_KIB);
    conn.pragma_update(None, "cache_size", -(kib as i64))?;
    let moved = move_to_4(conn);
    let _ = conn.pragma_update(None, "cache_size", cache);
    if moved.is_err() {
        // What the transaction wrote into the WAL before it failed holds room it no longer needs.
        let _ = checkpoint(conn);
    }
    let moved = match moved {
        Ok(m) => m,
        Err(e) if crate::is_disk_full(&e) => bail!(
            "moving this database from schema 3 to 4 ran out of room on the disk that holds it ({e:#}). It stays at schema 3: make room, or go on with the binary from before"
        ),
        Err(e) => {
            return Err(e.context("move the database from schema 3 to 4; it stays at schema 3"))
        }
    };
    hit("server.migrate.committed");
    let _ = checkpoint(conn);
    if !quiet {
        let after = pages(conn)?;
        eprintln!(
            "cairn-server: moved the database from schema 3 to 4 in {:.1} s: {} commits, {} operations, {} revisions{}; {} in use before, {} now",
            started.elapsed().as_secs_f64(),
            moved.commits,
            moved.ops,
            moved.revisions,
            match moved.bodies_here {
                0 => String::new(),
                n => format!(", {n} bodies kept in the journal, as `revisions` did not hold them for that commit"),
            },
            bytes(before.live()),
            bytes(after.live())
        );
    }
    Ok(())
}

fn move_to_4(conn: &mut Connection) -> Result<Moved> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if tx.pragma_query_value(None, "user_version", |r| r.get::<_, usize>(0))? != 3 {
        bail!("the database's schema changed while it was being moved");
    }
    hit("server.migrate.begun");
    tx.execute_batch(TABLES_4)?;
    let mut moved = Moved::default();
    {
        // In key order, so that each new table is written from its first page to its last.
        let mut read =
            tx.prepare("SELECT ns, id, object, base, seq, body FROM revisions ORDER BY ns, id")?;
        let mut put = tx.prepare(
            "INSERT INTO revisions_4 (ns, id, object, base, seq, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut rows = read.query([])?;
        while let Some(row) = rows.next()? {
            let id = row.get_ref(1)?.as_blob()?;
            let text = row.get_ref(5)?.as_str()?;
            let body: RevisionBody = serde_json::from_str(text)
                .with_context(|| format!("revision {} in the database", hex::encode(id)))?;
            let record = encode_body(&body);
            if decode_body(&record)? != body {
                bail!("revision {} would not read back as it is", hex::encode(id));
            }
            put.execute(params![
                ToSqlOutput::Borrowed(row.get_ref(0)?),
                id,
                ToSqlOutput::Borrowed(row.get_ref(2)?),
                ToSqlOutput::Borrowed(row.get_ref(3)?),
                ToSqlOutput::Borrowed(row.get_ref(4)?),
                record
            ])?;
            moved.revisions += 1;
            if moved.revisions % STEP == 0 {
                hit("server.migrate.revisions");
            }
        }
    }
    {
        let mut read = tx.prepare(
            "SELECT ns, seq, op_id, device, device_name, time, ops, user FROM journal ORDER BY ns, seq",
        )?;
        let mut stored = tx
            .prepare("SELECT object, base, seq, body FROM revisions_4 WHERE ns = ?1 AND id = ?2")?;
        let mut put = tx.prepare(
            "INSERT INTO journal_4 (ns, seq, op_id, device, device_name, time, user, ops, json_len)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        let mut rows = read.query([])?;
        while let Some(row) = rows.next()? {
            let ns: i64 = row.get(0)?;
            let seq: i64 = row.get(1)?;
            let text = row.get_ref(6)?.as_str()?;
            let ops: Vec<Op> = serde_json::from_str(text)
                .with_context(|| format!("commit {seq} in the database"))?;
            // A body stays in the commit unless `revisions` holds that very body, for that object,
            // base and commit.
            let mut here = vec![false; ops.len()];
            for (i, op) in ops.iter().enumerate() {
                moved.ops += 1;
                if moved.ops % STEP == 0 {
                    hit("server.migrate.journal");
                }
                let (object, revision, base, body) = match op {
                    Op::Create {
                        object,
                        revision,
                        body,
                        ..
                    } => (object, revision, None, body),
                    Op::Update {
                        object,
                        base,
                        revision,
                        body,
                        ..
                    } => (object, revision, Some(base), body),
                    _ => continue,
                };
                let row = stored
                    .query_row(params![ns, revision.0], |r| {
                        Ok((
                            r.get::<_, Vec<u8>>(0)?,
                            r.get::<_, Option<Vec<u8>>>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, Vec<u8>>(3)?,
                        ))
                    })
                    .optional()?;
                let agrees = row.is_some_and(|(o, b, s, record)| {
                    o == object.0
                        && b.as_deref() == base.map(|b| &b.0[..])
                        && s == seq
                        && record == encode_body(body)
                });
                if !agrees {
                    here[i] = true;
                    moved.bodies_here += 1;
                }
            }
            let record = encode_ops(&ops, |i| here[i]);
            // Read back as the server reads it, each body it names from `revisions`, which holds
            // just that body: it must be what the commit was.
            let back = decode_ops(&record)?;
            let mut back_ops = back.ops;
            for i in back.bodies_wanted {
                back_ops[i] = ops[i].clone();
            }
            if back_ops != ops {
                bail!("commit {seq}'s operations would not read back as they are");
            }
            put.execute(params![
                ns,
                seq,
                ToSqlOutput::Borrowed(row.get_ref(2)?),
                ToSqlOutput::Borrowed(row.get_ref(3)?),
                ToSqlOutput::Borrowed(row.get_ref(4)?),
                ToSqlOutput::Borrowed(row.get_ref(5)?),
                ToSqlOutput::Borrowed(row.get_ref(7)?),
                record,
                text.len() as i64
            ])?;
            moved.commits += 1;
        }
    }
    hit("server.migrate.moved");
    swap_tables(&tx, "4")?;
    tx.pragma_update(None, "user_version", 4)?;
    hit("server.migrate.commit");
    check("server.migrate.write")?;
    tx.commit()?;
    Ok(moved)
}

/// Moves a database of schema 4 back to 3, which a binary from before schema 4 reads: every body
/// written out as JSON in the commit that made it and again in `revisions`, as such a binary wrote
/// them. One transaction, so a kill or a full disk leaves schema 4 or 3 whole; with no server on it,
/// which the caller sees to by holding the data directory's lock.
pub fn to_3(file: &Path) -> Result<String> {
    let mut conn = Connection::open(file).with_context(|| format!("open {}", file.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let version: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != 4 {
        bail!("this database has schema {version}: only one of schema 4 is moved back to 3");
    }
    let started = Instant::now();
    let before = pages(&conn)?;
    // JSON takes three to five times what the records do.
    if let Some((place, need, free)) =
        move_short_of_room(file, &before, before.live() * 5, "server.downgrade_room")
    {
        bail!(
            "moving the database back to schema 3 takes {} where {} is, which has {} free; it stays at schema 4",
            bytes(need),
            place.display(),
            bytes(free)
        );
    }
    let kib = tuned("server.migrate_cache_kib", MOVE_CACHE_KIB);
    conn.pragma_update(None, "cache_size", -(kib as i64))?;
    let moved = move_to_3(&mut conn);
    if moved.is_err() {
        let _ = checkpoint(&conn);
    }
    let moved = match moved {
        Ok(m) => m,
        Err(e) if crate::is_disk_full(&e) => bail!(
            "moving the database back to schema 3 ran out of room on the disk that holds it ({e:#}); it stays at schema 4"
        ),
        Err(e) => return Err(e.context("move the database back to schema 3; it stays at schema 4")),
    };
    checkpoint(&conn)?;
    let after = pages(&conn)?;
    Ok(format!(
        "moved {} back to schema 3 in {:.1} s: {} commits, {} operations, {} revisions; {} in use before, {} now{}. A binary from before schema 4 reads it, and this one moves it to 4 again when it opens it",
        file.display(),
        started.elapsed().as_secs_f64(),
        moved.commits,
        moved.ops,
        moved.revisions,
        bytes(before.live()),
        bytes(after.live()),
        match moved.json_differs {
            0 => String::new(),
            n => format!("; {n} commits' JSON is not as long as when they were moved to schema 4"),
        }
    ))
}

fn move_to_3(conn: &mut Connection) -> Result<Moved> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if tx.pragma_query_value(None, "user_version", |r| r.get::<_, usize>(0))? != 4 {
        bail!("the database's schema changed while it was being moved");
    }
    hit("server.downgrade.begun");
    tx.execute_batch(TABLES_3)?;
    let mut moved = Moved::default();
    {
        let mut read =
            tx.prepare("SELECT ns, id, object, base, seq, body FROM revisions ORDER BY ns, id")?;
        let mut put = tx.prepare(
            "INSERT INTO revisions_3 (ns, id, object, base, seq, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut rows = read.query([])?;
        while let Some(row) = rows.next()? {
            let id = row.get_ref(1)?.as_blob()?;
            let body = decode_body(row.get_ref(5)?.as_blob()?)
                .with_context(|| format!("revision {}", hex::encode(id)))?;
            put.execute(params![
                ToSqlOutput::Borrowed(row.get_ref(0)?),
                id,
                ToSqlOutput::Borrowed(row.get_ref(2)?),
                ToSqlOutput::Borrowed(row.get_ref(3)?),
                ToSqlOutput::Borrowed(row.get_ref(4)?),
                serde_json::to_string(&body)?
            ])?;
            moved.revisions += 1;
            if moved.revisions % STEP == 0 {
                hit("server.downgrade.revisions");
            }
        }
    }
    {
        let mut read = tx.prepare(
            "SELECT ns, seq, op_id, device, device_name, time, ops, user, json_len FROM journal ORDER BY ns, seq",
        )?;
        let mut put = tx.prepare(
            "INSERT INTO journal_3 (ns, seq, op_id, device, device_name, time, ops, user)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut rows = read.query([])?;
        while let Some(row) = rows.next()? {
            let ns: i64 = row.get(0)?;
            let seq: i64 = row.get(1)?;
            let ops = ops_of(&tx, ns, seq, row.get_ref(6)?.as_blob()?)?;
            let text = serde_json::to_string(&ops)?;
            if text.len() as i64 != row.get::<_, i64>(8)? {
                moved.json_differs += 1;
            }
            moved.ops += ops.len() as u64;
            put.execute(params![
                ns,
                seq,
                ToSqlOutput::Borrowed(row.get_ref(2)?),
                ToSqlOutput::Borrowed(row.get_ref(3)?),
                ToSqlOutput::Borrowed(row.get_ref(4)?),
                ToSqlOutput::Borrowed(row.get_ref(5)?),
                text,
                ToSqlOutput::Borrowed(row.get_ref(7)?)
            ])?;
            moved.commits += 1;
            hit("server.downgrade.journal");
        }
    }
    swap_tables(&tx, "3")?;
    tx.pragma_update(None, "user_version", 3)?;
    hit("server.downgrade.commit");
    tx.commit()?;
    Ok(moved)
}

/// Rewrites the database with every table in key order and no page free, if more than half of it is
/// free pages, as a move to schema 4 leaves it: SQLite's VACUUM, through the WAL as any transaction,
/// so a kill or a full disk at any point leaves the database as it was or as it is after. Short of
/// room for it, or should it fail, the server goes on with the database as it is, and it is tried
/// again at the next start.
pub(crate) fn compact_if_due(conn: &Connection, file: &Path) -> Result<()> {
    let before = pages(conn)?;
    if !before.due(tuned("server.compact_floor", COMPACT_FLOOR)) {
        return Ok(());
    }
    hit("server.compact.due");
    // The rewrite goes into the WAL, and SQLite's temporary copy takes as much again.
    let live = before.live();
    let each = (live + live / 8 + (8 << 20)) * tuned("server.compact_room", 1);
    let dir = folder_of(file);
    let mut places = vec![(dir.clone(), each)];
    match sqlite_temp_dir() {
        Some(t) if same_filesystem(&dir, &t) => places[0].1 += each,
        Some(t) => places.push((t, each)),
        None => {}
    }
    let short = places.into_iter().find_map(|(place, need)| {
        let free = free_bytes(&place)?;
        (free < need).then_some((place, need, free))
    });
    if let Some((place, need, free)) = short {
        eprintln!(
            "cairn-server: {} of the database's {} is free pages, which a compaction gives back: it takes {} where {} is, which has {} free, so it is tried again at the next start",
            bytes(before.free * before.size),
            bytes(before.file()),
            bytes(need),
            place.display(),
            bytes(free)
        );
        return Ok(());
    }
    let started = Instant::now();
    conn.pragma_update(None, "temp_store", "FILE")?;
    #[cfg(feature = "failpoints")]
    conn.progress_handler(
        1000,
        Some(|| {
            hit("server.compact.vacuuming");
            false
        }),
    );
    let vacuumed = conn.execute_batch("VACUUM");
    #[cfg(feature = "failpoints")]
    conn.progress_handler(0, None::<fn() -> bool>);
    let emptied = checkpoint(conn);
    let _ = conn.pragma_update(None, "temp_store", "DEFAULT");
    if let Err(e) = vacuumed {
        eprintln!(
            "cairn-server: the compaction of the database did not finish ({e}); it is as it was, and is tried again at the next start"
        );
        return Ok(());
    }
    emptied?;
    hit("server.compact.done");
    eprintln!(
        "cairn-server: compacted the database, {} of its {} free: {} now, in {:.1} s",
        bytes(before.free * before.size),
        bytes(before.file()),
        bytes(std::fs::metadata(file).map_or(0, |m| m.len())),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::MetaStore;
    use crate::sqlite::SqliteStore;
    use cairn_protocol::wire::CommitRequest;
    use cairn_protocol::{
        Bytes, ConflictOf, Content, DeviceId, Hash, Link, Metadata, Name, ObjectId, OpId, Owner,
        RevisionId, Timestamp,
    };

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cairn-move-{}",
            hex::encode(rand::random::<[u8; 8]>())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn link(parent: ObjectId, name: &str) -> Link {
        Link {
            parent,
            name: Name::new(name.as_bytes().to_vec()).unwrap(),
        }
    }

    fn meta(mode: u32, unknown: bool) -> Metadata {
        let mut m = Metadata {
            mode,
            owner: Some(Owner {
                uid: 1000,
                gid: 1000,
                user: Some(Bytes(b"s".to_vec())),
                group: Some(Bytes(b"s".to_vec())),
            }),
            mtime: Timestamp {
                sec: 1_700_000_000,
                nsec: 123_456_789,
            },
            btime: Some(Timestamp { sec: 1, nsec: 2 }),
            ..Default::default()
        };
        if unknown {
            m.unknown.insert(
                "from_the_future".into(),
                serde_json::json!({"x": [1, 2.5, "three"], "y": null}),
            );
        }
        m
    }

    fn file(n: u8, mode: u32) -> RevisionBody {
        RevisionBody {
            content: Content::File {
                manifest: Hash::of(&[n]),
                size: u64::from(n) * 1000,
            },
            metadata: meta(mode, n % 3 == 0),
        }
    }

    fn commit(s: &mut SqliteStore, ns: &str, ops: Vec<Op>) {
        let req = CommitRequest {
            op_id: OpId::random(),
            device: DeviceId::random(),
            device_name: "device".into(),
            ops,
        };
        let out = s.commit(ns, &req, 1_700_000_000, Some("u")).unwrap();
        assert!(
            matches!(out, crate::meta::CommitOutcome::Committed { .. }),
            "{out:?}"
        );
    }

    /// Two namespaces, every kind of operation, conflict copies, restores, hard links, notes that
    /// repeat as a join's do, and metadata a newer build wrote.
    fn rich(db: &Path) -> Vec<ObjectId> {
        let mut s = SqliteStore::open(db).unwrap();
        s.create_namespace("team").unwrap();
        let mut objects = Vec::new();
        for ns in ["default", "team"] {
            let d = ObjectId::random();
            let (f, g) = (ObjectId::random(), ObjectId::random());
            let (rf, rg) = (RevisionId::random(), RevisionId::random());
            let mut ops = vec![Op::Create {
                object: d,
                revision: RevisionId::random(),
                body: RevisionBody {
                    content: Content::Dir,
                    metadata: meta(0o775, false),
                },
                link: link(ObjectId::ROOT, "dir"),
                conflict_of: None,
            }];
            for (o, r, name, n) in [(f, rf, "f", 1u8), (g, rg, "g", 3)] {
                ops.push(Op::Create {
                    object: o,
                    revision: r,
                    body: file(n, 0o664),
                    link: link(d, name),
                    conflict_of: None,
                });
            }
            for i in 10..50u8 {
                let o = ObjectId::random();
                ops.push(Op::Create {
                    object: o,
                    revision: RevisionId::random(),
                    body: file(i, 0o644),
                    link: link(d, &format!("many-{i}")),
                    conflict_of: None,
                });
            }
            commit(&mut s, ns, ops);
            let notes: Vec<Op> = [f, g, d, f]
                .iter()
                .map(|o| {
                    Op::note(
                        *o,
                        "mode: kept 664 from the server; this device had 644".into(),
                    )
                })
                .chain([
                    Op::note(g, "owner: kept a from the server; this device had b".into()),
                    Op::Note {
                        object: d,
                        text: "owner: kept a from the server; this device had c".into(),
                        also: vec![f, g],
                    },
                ])
                .collect();
            commit(&mut s, ns, notes);
            let rf2 = RevisionId::random();
            let copy = ObjectId::random();
            commit(
                &mut s,
                ns,
                vec![
                    Op::Update {
                        object: f,
                        base: rf,
                        revision: rf2,
                        body: file(2, 0o600),
                        restored_from: None,
                    },
                    Op::Create {
                        object: copy,
                        revision: RevisionId::random(),
                        body: file(4, 0o644),
                        link: link(d, "f (conflict)"),
                        conflict_of: Some(ConflictOf {
                            object: f,
                            competing_revision: rf2,
                        }),
                    },
                    Op::Link {
                        object: g,
                        link: link(ObjectId::ROOT, "hard"),
                    },
                ],
            );
            commit(
                &mut s,
                ns,
                vec![
                    Op::Update {
                        object: f,
                        base: rf2,
                        revision: RevisionId::random(),
                        body: file(1, 0o664),
                        restored_from: Some(rf),
                    },
                    Op::Move {
                        object: copy,
                        from: link(d, "f (conflict)"),
                        to: link(ObjectId::ROOT, "moved"),
                    },
                    Op::Unlink {
                        object: g,
                        link: link(ObjectId::ROOT, "hard"),
                        base: Some(rg),
                    },
                    Op::Unlink {
                        object: g,
                        link: link(d, "g"),
                        base: None,
                    },
                ],
            );
            objects.extend([ObjectId::ROOT, d, f, g, copy]);
        }
        objects
    }

    /// Everything a server reads from the store, as it would send it.
    fn reads(db: &Path, objects: &[ObjectId]) -> String {
        let mut s = SqliteStore::open(db).unwrap();
        let mut out = String::new();
        for ns in ["default", "team"] {
            let changes = s.changes(ns, 0, 1000, usize::MAX).unwrap();
            out += &serde_json::to_string(&changes).unwrap();
            let head = s.head(ns).unwrap();
            for max in [1, 400, 3000] {
                let pages: Vec<u64> = s
                    .changes(ns, 0, 1000, max)
                    .unwrap()
                    .iter()
                    .map(|c| c.seq)
                    .collect();
                out += &format!("{max}: {pages:?}\n");
                let before = s.changes_before(ns, head + 1, 1000, max).unwrap();
                out += &serde_json::to_string(&before).unwrap();
            }
            out += &serde_json::to_string(&s.conflicts(ns).unwrap()).unwrap();
            for r in s.revisions(ns).unwrap() {
                out += &format!("{r:?}\n");
                out += &format!("{:?}\n", s.revision(ns, r.revision).unwrap());
            }
            for o in objects {
                out += &format!("{:?}\n", s.object_head(ns, *o).unwrap());
                out += &format!("{:?}\n", s.children(ns, *o).unwrap());
                out += &format!("{:?}\n", s.links_of(ns, *o).unwrap());
                out += &format!("{:?}\n", s.history(ns, *o).unwrap());
            }
            out += &format!("{:?}\n", s.tree_state(ns).unwrap());
        }
        out
    }

    /// Every table's rows, sorted, and the names of the tables and indexes.
    fn rows(db: &Path) -> Vec<(String, Vec<String>)> {
        let conn = Connection::open(db).unwrap();
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        let names: Vec<String> = conn
            .prepare("SELECT type || ' ' || name FROM sqlite_master ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let mut all = vec![("schema".to_string(), names.clone())];
        for t in names.iter().filter_map(|n| n.strip_prefix("table ")) {
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
            let n = stmt.column_count();
            let mut got: Vec<String> = stmt
                .query_map([], |r| {
                    (0..n)
                        .map(|i| r.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .map(|r| format!("{:?}", r.unwrap()))
                .collect();
            got.sort();
            all.push((t.to_string(), got));
        }
        all
    }

    fn version(db: &Path) -> usize {
        Connection::open(db)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// Schema 4 moved back to 3 holds what a binary of schema 3 wrote, each body as JSON in its commit
    /// and again in `revisions`, and the journal pages by that JSON's length; moved to 4 again, every
    /// read is as it was, and moved back once more, every row is the same, byte for byte.
    #[test]
    fn a_database_moved_back_to_schema_3_and_on_to_4_reads_and_holds_the_same() {
        let dir = scratch();
        let db = dir.join("meta.sqlite");
        let objects = rich(&db);
        assert_eq!(version(&db), 4);
        let want = reads(&db, &objects);
        assert!(want.contains("from_the_future"), "{want}");

        to_3(&db).unwrap();
        assert_eq!(version(&db), 3);
        let three = rows(&db);
        {
            let conn = Connection::open(&db).unwrap();
            let mut stmt = conn
                .prepare("SELECT ops FROM journal WHERE ns = 1 ORDER BY seq")
                .unwrap();
            let texts: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let joined = format!("[{}]", texts.join(","));
            let ops: Vec<Vec<Op>> = serde_json::from_str(&joined).unwrap();
            assert_eq!(ops.len(), 4);
            assert!(texts[0].contains(r#""body":{"content""#), "{}", texts[0]);
            let body: String = conn
                .query_row(
                    "SELECT body FROM revisions WHERE ns = 1 AND seq = 1 LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str::<RevisionBody>(&body).unwrap();
        }

        // The binary of schema 4 moves it when it opens it, and reads it as before.
        assert_eq!(reads(&db, &objects), want);
        assert_eq!(version(&db), 4);
        let four = rows(&db);
        let journal = four.iter().find(|(t, _)| t == "journal").unwrap();
        assert!(journal.1.iter().all(|r| r.contains("Blob(")), "{journal:?}");
        to_3(&db).unwrap();
        assert!(rows(&db) == three, "not the same rows the second time");
        SqliteStore::open(&db).unwrap();
        assert!(rows(&db) == four, "not the same rows at schema 4 again");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A schema-3 database whose `revisions` does not hold a commit's body, changed or missing: the
    /// body stays in the commit, so the journal says what it said, and `revisions` what it held.
    #[test]
    fn a_body_revisions_does_not_hold_for_its_commit_stays_in_it() {
        let dir = scratch();
        let db = dir.join("meta.sqlite");
        let objects = rich(&db);
        to_3(&db).unwrap();
        let (changed, gone): (Vec<u8>, Vec<u8>) = {
            let conn = Connection::open(&db).unwrap();
            let mut stmt = conn
                .prepare("SELECT id FROM revisions WHERE ns = 1 AND seq = 1 ORDER BY id LIMIT 2")
                .unwrap();
            let ids: Vec<Vec<u8>> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let other = serde_json::to_string(&file(99, 0o600)).unwrap();
            conn.execute(
                "UPDATE revisions SET body = ?1 WHERE id = ?2",
                params![other, ids[0]],
            )
            .unwrap();
            conn.execute("DELETE FROM revisions WHERE id = ?1", [&ids[1]])
                .unwrap();
            (ids[0].clone(), ids[1].clone())
        };
        let journal_then: String = {
            let conn = Connection::open(&db).unwrap();
            conn.query_row(
                "SELECT ops FROM journal WHERE ns = 1 AND seq = 1",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        let mut s = SqliteStore::open(&db).unwrap();
        let first = s.changes("default", 0, 1, usize::MAX).unwrap();
        assert_eq!(serde_json::to_string(&first[0].ops).unwrap(), journal_then);
        let held = s
            .revision("default", RevisionId(changed.try_into().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(held.body, file(99, 0o600));
        assert!(s
            .revision("default", RevisionId(gone.try_into().unwrap()))
            .unwrap()
            .is_none());
        drop(s);
        let _ = reads(&db, &objects);
        // And back, the journal as it was.
        to_3(&db).unwrap();
        let conn = Connection::open(&db).unwrap();
        let again: String = conn
            .query_row(
                "SELECT ops FROM journal WHERE ns = 1 AND seq = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(again, journal_then);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A commit's operations are only named in the journal now: a revision that is not there for the
    /// commit that names it is an error when it is read, never another body.
    #[test]
    fn a_revision_missing_or_moved_is_an_error_when_the_journal_is_read() {
        let dir = scratch();
        let db = dir.join("meta.sqlite");
        rich(&db);
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE revisions SET seq = 3 WHERE ns = 1 AND seq = 1 AND id = (SELECT id FROM revisions WHERE ns = 1 AND seq = 1 ORDER BY id LIMIT 1)",
            [],
        )
        .unwrap();
        drop(conn);
        let mut s = SqliteStore::open(&db).unwrap();
        let err = s.changes("default", 0, 1000, usize::MAX).unwrap_err();
        assert!(format!("{err:#}").contains("at commit 3"), "{err:#}");
        drop(s);
        let conn = Connection::open(&db).unwrap();
        conn.execute("DELETE FROM revisions WHERE ns = 1 AND seq = 3", [])
            .unwrap();
        drop(conn);
        let mut s = SqliteStore::open(&db).unwrap();
        let err = s.changes("default", 0, 1000, usize::MAX).unwrap_err();
        assert!(
            format!("{err:#}").contains("which the database does not hold"),
            "{err:#}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A move leaves the old tables' pages free, and the next opening that finds more than half of
    /// the file free gives them back.
    #[test]
    fn the_pages_a_move_frees_are_given_back() {
        let dir = scratch();
        let db = dir.join("meta.sqlite");
        let mut s = SqliteStore::open(&db).unwrap();
        let mut ops = Vec::new();
        for i in 0..20_000u32 {
            ops.push(Op::Create {
                object: ObjectId::random(),
                revision: RevisionId::random(),
                body: file((i % 200) as u8, 0o644),
                link: link(ObjectId::ROOT, &format!("file-{i}")),
                conflict_of: None,
            });
        }
        commit(&mut s, "default", ops);
        drop(s);
        to_3(&db).unwrap();
        let three = std::fs::metadata(&db).unwrap().len();
        SqliteStore::open(&db).unwrap();
        let four = std::fs::metadata(&db).unwrap().len();
        let conn = Connection::open(&db).unwrap();
        let p = pages(&conn).unwrap();
        assert_eq!(p.free, 0, "{p:?}");
        assert!(four * 2 < three, "{three} bytes at schema 3, {four} at 4");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
