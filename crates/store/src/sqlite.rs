//! The default store: one SQLite file in WAL mode with full synchronous commits.

use crate::codec::{decode_body, decode_ops, encode_body, encode_ops, json_len};
use crate::meta::{
    Child, CommitOutcome, Head, MetaStore, Revisioned, ShareRow, StoredRevision, TokenInfo,
    TreeState, UserInfo,
};
use anyhow::{anyhow, bail, Context, Result};
use cairn_failpoint::{check, hit};
use cairn_protocol::wire::{Access, Change, CommitRequest, ConflictEntry, FolderAccess, Member};
use cairn_protocol::{
    check_and_apply, Content, Hash, Link, Metadata, Name, ObjectId, ObjectKind, Op, OpReject,
    RevisionBody, RevisionId, TreeStore,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::path::Path;

/// Schemas 1 to 3, each the SQL that moves the one before it to it. Schema 4 is `migrate::to_4`'s.
pub(crate) const MIGRATIONS: &[&str] = &[
    r#"
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
"#,
    r#"
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, created INTEGER NOT NULL);
CREATE TABLE tokens (
    hash BLOB PRIMARY KEY, user INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    label TEXT NOT NULL, created INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX tokens_user ON tokens (user);
CREATE TABLE grants (
    ns INTEGER NOT NULL REFERENCES namespaces (id), user INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    access TEXT NOT NULL,
    PRIMARY KEY (ns, user)
) WITHOUT ROWID;
CREATE INDEX grants_user ON grants (user);
ALTER TABLE journal ADD COLUMN user TEXT;
"#,
    r#"
CREATE TABLE shares (
    id BLOB PRIMARY KEY, ns INTEGER NOT NULL REFERENCES namespaces (id), object BLOB NOT NULL,
    recipient INTEGER REFERENCES users (id) ON DELETE CASCADE, link TEXT UNIQUE,
    access TEXT NOT NULL,
    creator INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created INTEGER NOT NULL, expires INTEGER,
    CHECK ((recipient IS NULL) <> (link IS NULL))
) WITHOUT ROWID;
CREATE INDEX shares_object ON shares (ns, object);
CREATE INDEX shares_recipient ON shares (recipient);
"#,
];

/// Schema 5's, after 4: the names a renamed namespace had, which still reach it and no other may take,
/// and each user's own name for a namespace.
pub(crate) const MIGRATION_5: &str = r#"
CREATE TABLE former_names (
    name TEXT PRIMARY KEY, ns INTEGER NOT NULL REFERENCES namespaces (id)
) WITHOUT ROWID;
CREATE INDEX former_names_ns ON former_names (ns);
CREATE TABLE folder_names (
    ns INTEGER NOT NULL REFERENCES namespaces (id), user INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    PRIMARY KEY (ns, user)
) WITHOUT ROWID;
CREATE INDEX folder_names_user ON folder_names (user);
"#;

/// The schema this binary writes: `PRAGMA user_version` once every migration has run. Schema 4 keeps
/// each revision's body once, in `revisions`, and bodies and operations as binary records
/// (`crate::codec`), with what is sent made from them as the JSON it always was; schema 5 adds
/// [`MIGRATION_5`]'s tables.
pub const SCHEMA: usize = MIGRATIONS.len() + 2;

/// The user a data directory's one token from before accounts becomes, managing every namespace.
pub const FIRST_USER: &str = "admin";

/// The namespace a data directory with none gets: what its first devices sync, `cloud` as people call
/// the folder they keep there. Directories from before it keep their `default`.
pub const FIRST_FOLDER: &str = "cloud";

/// How to open a database.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opening<'a> {
    /// The data directory's `token`, if it has one: the moment the schema gains accounts, it becomes
    /// [`FIRST_USER`]'s, who manages every namespace there is, so that nothing that used it stops.
    pub first_token: Option<&'a str>,
    /// Another process serves this database: take it as it is, at this binary's schema or not at all,
    /// and write nothing on opening. What the server reads per request then sees each change at once.
    pub beside_server: bool,
}

/// The hash a token is kept and looked up by: the token itself is never stored.
pub fn token_hash(token: &str) -> Hash {
    Hash::of(token.trim().as_bytes())
}

/// How a token is named in lists: the first 12 digits of its hash, which say nothing of the token.
pub fn token_id(hash: &Hash) -> String {
    hash.to_string()[..12].to_string()
}

fn access_of(s: &str) -> Result<Access> {
    s.parse()
        .map_err(|e: String| anyhow!("in the database: {e}"))
}

/// How a schema keeps a revision's body: JSON text up to schema 3, a binary record from 4 on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bodies {
    Json,
    Binary,
}

/// The namespace by that name, made if it is not there yet: its root directory at revision zero.
pub(crate) fn ensure_namespace(conn: &Connection, ns: &str, bodies: Bodies) -> Result<i64> {
    if let Some(id) = conn
        .query_row("SELECT id FROM namespaces WHERE name = ?1", [ns], |r| {
            r.get(0)
        })
        .optional()?
    {
        return Ok(id);
    }
    conn.execute("INSERT INTO namespaces (name) VALUES (?1)", [ns])?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO objects (ns, id, kind, head) VALUES (?1, ?2, 'dir', ?3)",
        params![id, ObjectId::ROOT.0, RevisionId::ZERO.0],
    )?;
    let body = match bodies {
        Bodies::Json => rusqlite::types::Value::Text(serde_json::to_string(&root_body())?),
        Bodies::Binary => rusqlite::types::Value::Blob(encode_body(&root_body())),
    };
    conn.execute(
        "INSERT INTO revisions (ns, id, object, base, seq, body) VALUES (?1, ?2, ?3, NULL, 0, ?4)",
        params![id, RevisionId::ZERO.0, ObjectId::ROOT.0, body],
    )?;
    Ok(id)
}

/// Schema 2's own step, in its transaction: the one token there was becomes the first user's.
fn adopt_first_token(tx: &Transaction<'_>, token: Option<&str>, time: i64) -> Result<()> {
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(());
    };
    // A store from before accounts has its `default` already, which every open of it made.
    if no_namespace(tx)? {
        ensure_namespace(tx, FIRST_FOLDER, Bodies::Json)?;
    }
    tx.execute(
        "INSERT INTO users (name, created) VALUES (?1, ?2)",
        params![FIRST_USER, time],
    )?;
    let user = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO tokens (hash, user, label, created) VALUES (?1, ?2, 'token file', ?3)",
        params![token_hash(token).0, user, time],
    )?;
    tx.execute(
        "INSERT INTO grants (ns, user, access) SELECT id, ?1, 'manage' FROM namespaces",
        [user],
    )?;
    Ok(())
}

/// The name now of the namespace that `name` was the name of, if it was one's.
fn former_of(conn: &Connection, name: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT n.name FROM former_names f JOIN namespaces n ON n.id = f.ns WHERE f.name = ?1",
            [name],
            |r| r.get(0),
        )
        .optional()?)
}

fn no_namespace(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM namespaces LIMIT 1", [], |_| Ok(()))
        .optional()?
        .is_none())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub struct SqliteStore {
    conn: Connection,
}

fn blob16(v: Vec<u8>) -> Result<[u8; 16]> {
    v.try_into()
        .map_err(|_| anyhow!("a stored ID is not 16 bytes"))
}

pub(crate) fn root_body() -> RevisionBody {
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

/// A name read back from the database, where only valid ones are ever written.
fn stored_name(bytes: Vec<u8>) -> Result<Name> {
    Name::new(bytes).map_err(|e| anyhow!("a stored name is not valid: {e}"))
}

const SHARE_SELECT: &str =
    "SELECT s.id, n.name, s.object, r.name, s.link, s.access, c.name, s.created, s.expires
     FROM shares s JOIN namespaces n ON n.id = s.ns JOIN users c ON c.id = s.creator
     LEFT JOIN users r ON r.id = s.recipient";

/// A share's row as SQLite gives it, before its IDs and level are checked.
struct RawShare {
    id: Vec<u8>,
    ns: String,
    object: Vec<u8>,
    user: Option<String>,
    link: Option<String>,
    access: String,
    by: String,
    created: i64,
    expires: Option<i64>,
}

fn share_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawShare> {
    Ok(RawShare {
        id: r.get(0)?,
        ns: r.get(1)?,
        object: r.get(2)?,
        user: r.get(3)?,
        link: r.get(4)?,
        access: r.get(5)?,
        by: r.get(6)?,
        created: r.get(7)?,
        expires: r.get(8)?,
    })
}

impl RawShare {
    fn into_row(self) -> Result<ShareRow> {
        Ok(ShareRow {
            id: blob16(self.id)?,
            ns: self.ns,
            object: ObjectId(blob16(self.object)?),
            user: self.user,
            link: self.link,
            access: access_of(&self.access)?,
            by: self.by,
            created: self.created,
            expires: self.expires,
        })
    }
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, Opening::default())
    }

    pub fn open_with(path: &Path, how: Opening<'_>) -> Result<Self> {
        if how.beside_server && !path.exists() {
            bail!("{} does not exist", path.display());
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::with_connection(conn, how, Some(path))
    }

    pub fn in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?, Opening::default(), None)
    }

    fn with_connection(
        mut conn: Connection,
        how: Opening<'_>,
        file: Option<&Path>,
    ) -> Result<Self> {
        // An administrator's command beside the server writes now and then: each waits for the other
        // rather than failing.
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // FULL: an acknowledged commit survives power loss (invariant 1).
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA {
            bail!(
                "this database has schema {version}, newer than this binary's {SCHEMA}: upgrade the binary"
            );
        }
        if how.beside_server {
            if version != SCHEMA {
                bail!(
                    "this database has schema {version} and this binary writes {SCHEMA}: run this with the binary the server runs, or stop the server and run it again, which brings the database up to date"
                );
            }
            return Ok(Self { conn });
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            if i == 1 {
                adopt_first_token(&tx, how.first_token, now())?;
            }
            tx.pragma_update(None, "user_version", i + 1)?;
            tx.commit()?;
        }
        if version < 4 {
            crate::migrate::to_4(&mut conn, file, version == 0)?;
        }
        // Before schema 5's step, so that a compaction killed leaves schema 4 as its move did.
        if let Some(file) = file {
            crate::migrate::compact_if_due(&conn, file)?;
        }
        if version < 5 {
            crate::migrate::to_5(&mut conn)?;
        }
        let mut store = Self { conn };
        if no_namespace(&store.conn)? {
            store.create_namespace(FIRST_FOLDER)?;
        }
        Ok(store)
    }

    fn user_id(&self, name: &str) -> Result<i64> {
        self.conn
            .query_row("SELECT id FROM users WHERE name = ?1", [name], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow!("no user {name:?}"))
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
            .execute(params![self.ns, rev.0, o.0, self.seq, encode_body(body)])?;
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
            .execute(params![self.ns, rev.0, o.0, base.0, self.seq, encode_body(body)])?;
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

/// A commit's operations as `journal` keeps them, each create's and update's body put in from
/// `revisions`, where the revision it names must be that object's and made by this commit.
pub(crate) fn ops_of(conn: &Connection, ns: i64, seq: i64, stored: &[u8]) -> Result<Vec<Op>> {
    let decoded = decode_ops(stored).with_context(|| format!("commit {seq}"))?;
    let mut ops = decoded.ops;
    if decoded.bodies_wanted.is_empty() {
        return Ok(ops);
    }
    let mut stmt =
        conn.prepare_cached("SELECT object, seq, body FROM revisions WHERE ns = ?1 AND id = ?2")?;
    for i in decoded.bodies_wanted {
        let (object, revision, body) = match &mut ops[i] {
            Op::Create {
                object,
                revision,
                body,
                ..
            }
            | Op::Update {
                object,
                revision,
                body,
                ..
            } => (*object, *revision, body),
            _ => bail!("commit {seq}: a body is wanted for an operation that has none"),
        };
        let row = stmt
            .query_row(params![ns, revision.0], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })
            .optional()?;
        let Some((stored_object, stored_seq, stored_body)) = row else {
            bail!("commit {seq} names revision {revision}, which the database does not hold");
        };
        if stored_object != object.0 || stored_seq != seq {
            bail!(
                "commit {seq} names revision {revision} of object {object}, and the database holds it for object {} at commit {stored_seq}",
                hex::encode(&stored_object)
            );
        }
        *body = decode_body(&stored_body).with_context(|| format!("revision {revision}"))?;
    }
    Ok(ops)
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
        if let Some(now) = former_of(&tx, ns)? {
            bail!(
                "{ns:?} was the name of the folder {now:?}, which it still reaches: pick another"
            );
        }
        ensure_namespace(&tx, ns, Bodies::Binary)?;
        tx.commit()?;
        Ok(())
    }

    fn resolve_namespace(&mut self, name: &str) -> Result<Option<String>> {
        if self.ns_id(name).is_ok() {
            return Ok(Some(name.to_string()));
        }
        former_of(&self.conn, name)
    }

    fn rename_namespace(&mut self, from: &str, to: &str) -> Result<()> {
        if !cairn_protocol::wire::valid_account_name(to) {
            bail!("{to:?} is no folder name: 1 to 64 of ASCII letters, digits, '.', '_' and '-', not starting with '.' or '-'");
        }
        let id = self.ns_id(from)?;
        if from == to {
            bail!("the folder {from:?} has that name already");
        }
        let tx = self.conn.transaction()?;
        if tx
            .query_row("SELECT 1 FROM namespaces WHERE name = ?1", [to], |_| Ok(()))
            .optional()?
            .is_some()
        {
            bail!("there is a folder {to:?} already");
        }
        let owner: Option<i64> = tx
            .query_row("SELECT ns FROM former_names WHERE name = ?1", [to], |r| {
                r.get(0)
            })
            .optional()?;
        if owner.is_some_and(|o| o != id) {
            let now = former_of(&tx, to)?.unwrap_or_default();
            bail!(
                "{to:?} was the name of the folder {now:?}, which it still reaches: pick another"
            );
        }
        tx.execute("DELETE FROM former_names WHERE name = ?1", [to])?;
        tx.execute(
            "UPDATE namespaces SET name = ?1 WHERE id = ?2",
            params![to, id],
        )?;
        tx.execute(
            "INSERT INTO former_names (name, ns) VALUES (?1, ?2)",
            params![from, id],
        )?;
        hit("server.rename.commit");
        check("server.rename.write")?;
        tx.commit()?;
        Ok(())
    }

    fn former_names(&mut self, ns: &str) -> Result<Vec<String>> {
        let id = self.ns_id(ns)?;
        let mut stmt = self
            .conn
            .prepare_cached("SELECT name FROM former_names WHERE ns = ?1 ORDER BY name")?;
        let names = stmt
            .query_map([id], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(names)
    }

    fn set_folder_name(
        &mut self,
        ns: &str,
        user: &str,
        name: Option<&str>,
        if_none: bool,
    ) -> Result<Option<String>> {
        let ns_id = self.ns_id(ns)?;
        let user_id = self.user_id(user)?;
        match name {
            None => {
                self.conn.execute(
                    "DELETE FROM folder_names WHERE ns = ?1 AND user = ?2",
                    params![ns_id, user_id],
                )?;
            }
            Some(name) => {
                if !cairn_protocol::wire::valid_folder_name(name) {
                    bail!("{name:?} is no name for a folder: 1 to 64 characters, no control character, and no space at either end");
                }
                let sql = if if_none {
                    "INSERT INTO folder_names (ns, user, name) VALUES (?1, ?2, ?3)
                     ON CONFLICT (ns, user) DO NOTHING"
                } else {
                    "INSERT INTO folder_names (ns, user, name) VALUES (?1, ?2, ?3)
                     ON CONFLICT (ns, user) DO UPDATE SET name = excluded.name"
                };
                self.conn.execute(sql, params![ns_id, user_id, name])?;
            }
        }
        Ok(self
            .conn
            .query_row(
                "SELECT name FROM folder_names WHERE ns = ?1 AND user = ?2",
                params![ns_id, user_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn add_user(&mut self, name: &str, time: i64) -> Result<()> {
        if self.user_id(name).is_ok() {
            bail!("there is a user {name:?} already");
        }
        self.conn.execute(
            "INSERT INTO users (name, created) VALUES (?1, ?2)",
            params![name, time],
        )?;
        Ok(())
    }

    fn remove_user(&mut self, name: &str) -> Result<()> {
        let id = self.user_id(name)?;
        // Its tokens and grants go with it. The journal keeps its name on what it committed.
        self.conn.execute("DELETE FROM users WHERE id = ?1", [id])?;
        Ok(())
    }

    fn users(&mut self) -> Result<Vec<UserInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT u.name, u.created, (SELECT count(*) FROM tokens t WHERE t.user = u.id)
             FROM users u ORDER BY u.name",
        )?;
        let users = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut out = Vec::new();
        for (name, created, tokens) in users {
            let folders = self.folders_of(&name)?;
            out.push(UserInfo {
                name,
                created,
                tokens: tokens as u64,
                folders,
            });
        }
        Ok(out)
    }

    fn add_token(&mut self, user: &str, hash: &Hash, label: &str, time: i64) -> Result<()> {
        let id = self.user_id(user)?;
        self.conn.execute(
            "INSERT INTO tokens (hash, user, label, created) VALUES (?1, ?2, ?3, ?4)",
            params![hash.0, id, label, time],
        )?;
        Ok(())
    }

    fn tokens(&mut self) -> Result<Vec<TokenInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.hash, u.name, t.label, t.created FROM tokens t JOIN users u ON u.id = t.user
             ORDER BY u.name, t.created, t.hash",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (hash, user, label, created) = row?;
            let hash = Hash(
                hash.try_into()
                    .map_err(|_| anyhow!("a stored token hash is not 32 bytes"))?,
            );
            out.push(TokenInfo {
                id: token_id(&hash),
                hash,
                user,
                label,
                created,
            });
        }
        Ok(out)
    }

    fn remove_token(&mut self, hash: &Hash) -> Result<()> {
        if self
            .conn
            .execute("DELETE FROM tokens WHERE hash = ?1", [hash.0])?
            == 0
        {
            bail!("no such token");
        }
        Ok(())
    }

    fn token_user(&mut self, hash: &Hash) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT u.name FROM tokens t JOIN users u ON u.id = t.user WHERE t.hash = ?1",
            )?
            .query_row([hash.0], |r| r.get(0))
            .optional()?)
    }

    fn access(&mut self, ns: &str, user: &str) -> Result<Option<Access>> {
        let level: Option<String> = self
            .conn
            .prepare_cached(
                "SELECT g.access FROM grants g
                 JOIN namespaces n ON n.id = g.ns JOIN users u ON u.id = g.user
                 WHERE n.name = ?1 AND u.name = ?2",
            )?
            .query_row([ns, user], |r| r.get(0))
            .optional()?;
        level.as_deref().map(access_of).transpose()
    }

    fn set_access(&mut self, ns: &str, user: &str, access: Option<Access>) -> Result<()> {
        let ns_id = self.ns_id(ns)?;
        let user_id = self.user_id(user)?;
        match access {
            Some(a) => self.conn.execute(
                "INSERT INTO grants (ns, user, access) VALUES (?1, ?2, ?3)
                 ON CONFLICT (ns, user) DO UPDATE SET access = excluded.access",
                params![ns_id, user_id, a.as_str()],
            )?,
            None => self.conn.execute(
                "DELETE FROM grants WHERE ns = ?1 AND user = ?2",
                params![ns_id, user_id],
            )?,
        };
        Ok(())
    }

    fn members(&mut self, ns: &str) -> Result<Vec<Member>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT u.name, g.access FROM grants g JOIN users u ON u.id = g.user
             WHERE g.ns = ?1 ORDER BY u.name",
        )?;
        let rows = stmt.query_map([ns_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (user, access) = row?;
            out.push(Member {
                user,
                access: access_of(&access)?,
            });
        }
        Ok(out)
    }

    fn folders_of(&mut self, user: &str) -> Result<Vec<FolderAccess>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT n.name, g.access, f.name FROM grants g
             JOIN namespaces n ON n.id = g.ns JOIN users u ON u.id = g.user
             LEFT JOIN folder_names f ON f.ns = g.ns AND f.user = g.user
             WHERE u.name = ?1 ORDER BY n.name",
        )?;
        let rows = stmt.query_map([user], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (folder, access, name) = row?;
            out.push(FolderAccess {
                folder,
                access: access_of(&access)?,
                name,
            });
        }
        Ok(out)
    }

    fn commit(
        &mut self,
        ns: &str,
        req: &CommitRequest,
        time: i64,
        user: Option<&str>,
    ) -> Result<CommitOutcome> {
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
        // Each body is in `revisions` now, under its revision, and only named here.
        tx.execute(
            "INSERT INTO journal (ns, seq, op_id, device, device_name, time, user, ops, json_len) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![ns_id, seq, req.op_id.0, req.device.0, req.device_name, time, user, encode_ops(&req.ops, |_| false), json_len(&req.ops)? as i64],
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

    fn changes(
        &mut self,
        ns: &str,
        after: u64,
        limit: u32,
        max_bytes: usize,
    ) -> Result<Vec<Change>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT seq, op_id, device, device_name, time, ops, user, json_len FROM journal WHERE ns = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![ns_id, after as i64, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        let mut bytes = 0usize;
        for row in rows {
            if bytes >= max_bytes {
                break;
            }
            let (seq, op_id, device, device_name, time, ops, user, json_len) = row?;
            bytes = bytes.saturating_add(usize::try_from(json_len).unwrap_or(usize::MAX));
            out.push(Change {
                seq: seq as u64,
                op_id: cairn_protocol::OpId(blob16(op_id)?),
                device: cairn_protocol::DeviceId(blob16(device)?),
                device_name,
                time,
                ops: ops_of(&self.conn, ns_id, seq, &ops)?,
                user,
            });
        }
        Ok(out)
    }

    fn changes_without_ops(&mut self, ns: &str, after: u64, limit: u32) -> Result<Vec<Change>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT seq, op_id, device, device_name, time, user FROM journal WHERE ns = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![ns_id, after as i64, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, op_id, device, device_name, time, user) = row?;
            out.push(Change {
                seq: seq as u64,
                op_id: cairn_protocol::OpId(blob16(op_id)?),
                device: cairn_protocol::DeviceId(blob16(device)?),
                device_name,
                time,
                ops: Vec::new(),
                user,
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
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, object, seq, body) = row?;
            out.push(StoredRevision {
                revision: RevisionId(blob16(id)?),
                object: ObjectId(blob16(object)?),
                seq: seq as u64,
                body: decode_body(&body)?,
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

    fn children(&mut self, ns: &str, dir: ObjectId) -> Result<Vec<Child>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT e.name, e.object, o.kind, o.head, r.body FROM entries e
             JOIN objects o ON o.ns = e.ns AND o.id = e.object
             JOIN revisions r ON r.ns = o.ns AND r.id = o.head
             WHERE e.ns = ?1 AND e.parent = ?2 ORDER BY e.name",
        )?;
        let rows = stmt.query_map(params![ns_id, dir.0], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, Vec<u8>>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (name, object, kind, head, body) = row?;
            out.push(Child {
                name: stored_name(name)?,
                object: ObjectId(blob16(object)?),
                head: Head {
                    kind: kind_of(&kind)?,
                    revision: RevisionId(blob16(head)?),
                    body: decode_body(&body)?,
                },
            });
        }
        Ok(out)
    }

    fn object_head(&mut self, ns: &str, object: ObjectId) -> Result<Option<Head>> {
        let ns_id = self.ns_id(ns)?;
        let row = self
            .conn
            .prepare_cached(
                "SELECT o.kind, o.head, r.body FROM objects o
                 JOIN revisions r ON r.ns = o.ns AND r.id = o.head
                 WHERE o.ns = ?1 AND o.id = ?2",
            )?
            .query_row(params![ns_id, object.0], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })
            .optional()?;
        let Some((kind, head, body)) = row else {
            return Ok(None);
        };
        Ok(Some(Head {
            kind: kind_of(&kind)?,
            revision: RevisionId(blob16(head)?),
            body: decode_body(&body)?,
        }))
    }

    fn links_of(&mut self, ns: &str, object: ObjectId) -> Result<Vec<Link>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT parent, name FROM entries WHERE ns = ?1 AND object = ?2 ORDER BY parent, name",
        )?;
        let rows = stmt.query_map(params![ns_id, object.0], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (parent, name) = row?;
            out.push(Link {
                parent: ObjectId(blob16(parent)?),
                name: stored_name(name)?,
            });
        }
        Ok(out)
    }

    fn revision(&mut self, ns: &str, revision: RevisionId) -> Result<Option<StoredRevision>> {
        let ns_id = self.ns_id(ns)?;
        let row = self
            .conn
            .prepare_cached("SELECT object, seq, body FROM revisions WHERE ns = ?1 AND id = ?2")?
            .query_row(params![ns_id, revision.0], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })
            .optional()?;
        let Some((object, seq, body)) = row else {
            return Ok(None);
        };
        Ok(Some(StoredRevision {
            revision,
            object: ObjectId(blob16(object)?),
            seq: seq as u64,
            body: decode_body(&body)?,
        }))
    }

    fn history(&mut self, ns: &str, object: ObjectId) -> Result<Vec<Revisioned>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT r.id, r.seq, r.body, j.time, j.device_name, j.user FROM revisions r
             LEFT JOIN journal j ON j.ns = r.ns AND j.seq = r.seq
             WHERE r.ns = ?1 AND r.object = ?2 ORDER BY r.seq DESC, r.id",
        )?;
        let rows = stmt.query_map(params![ns_id, object.0], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, seq, body, time, device_name, user) = row?;
            out.push(Revisioned {
                revision: StoredRevision {
                    revision: RevisionId(blob16(id)?),
                    object,
                    seq: seq as u64,
                    body: decode_body(&body)?,
                },
                time: time.unwrap_or(0),
                device_name: device_name.unwrap_or_default(),
                user,
            });
        }
        Ok(out)
    }

    fn changes_before(
        &mut self,
        ns: &str,
        before: u64,
        limit: u32,
        max_bytes: usize,
    ) -> Result<Vec<Change>> {
        let ns_id = self.ns_id(ns)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT seq, op_id, device, device_name, time, ops, user, json_len FROM journal
             WHERE ns = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![ns_id, before.min(i64::MAX as u64) as i64, limit],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Vec<u8>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        let mut bytes = 0usize;
        for row in rows {
            if bytes >= max_bytes {
                break;
            }
            let (seq, op_id, device, device_name, time, ops, user, json_len) = row?;
            bytes = bytes.saturating_add(usize::try_from(json_len).unwrap_or(usize::MAX));
            out.push(Change {
                seq: seq as u64,
                op_id: cairn_protocol::OpId(blob16(op_id)?),
                device: cairn_protocol::DeviceId(blob16(device)?),
                device_name,
                time,
                ops: ops_of(&self.conn, ns_id, seq, &ops)?,
                user,
            });
        }
        Ok(out)
    }

    fn add_share(&mut self, share: &ShareRow) -> Result<()> {
        let ns_id = self.ns_id(&share.ns)?;
        let creator = self.user_id(&share.by)?;
        let recipient = share.user.as_deref().map(|u| self.user_id(u)).transpose()?;
        if recipient.is_some() == share.link.is_some() {
            bail!("a share names a user or is a link, one or the other");
        }
        self.conn.execute(
            "INSERT INTO shares (id, ns, object, recipient, link, access, creator, created, expires)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                share.id,
                ns_id,
                share.object.0,
                recipient,
                share.link,
                share.access.as_str(),
                creator,
                share.created,
                share.expires
            ],
        )?;
        Ok(())
    }

    fn shares(&mut self, ns: &str, object: Option<ObjectId>) -> Result<Vec<ShareRow>> {
        let ns_id = self.ns_id(ns)?;
        let sql = format!(
            "{SHARE_SELECT} WHERE s.ns = ?1 AND (?2 IS NULL OR s.object = ?2) ORDER BY s.created, s.id"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![ns_id, object.map(|o| o.0)], share_row)?;
        rows.map(|r| r?.into_row()).collect()
    }

    fn share(&mut self, id: &[u8; 16]) -> Result<Option<ShareRow>> {
        let sql = format!("{SHARE_SELECT} WHERE s.id = ?1");
        let row = self
            .conn
            .prepare_cached(&sql)?
            .query_row([&id[..]], share_row)
            .optional()?;
        row.map(RawShare::into_row).transpose()
    }

    fn share_by_link(&mut self, link: &str) -> Result<Option<ShareRow>> {
        let sql = format!("{SHARE_SELECT} WHERE s.link = ?1");
        let row = self
            .conn
            .prepare_cached(&sql)?
            .query_row([link], share_row)
            .optional()?;
        row.map(RawShare::into_row).transpose()
    }

    fn shares_to(&mut self, user: &str) -> Result<Vec<ShareRow>> {
        let sql = format!("{SHARE_SELECT} WHERE r.name = ?1 ORDER BY s.created, s.id");
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([user], share_row)?;
        rows.map(|r| r?.into_row()).collect()
    }

    fn remove_share(&mut self, ns: &str, id: &[u8; 16]) -> Result<bool> {
        let ns_id = self.ns_id(ns)?;
        Ok(self.conn.execute(
            "DELETE FROM shares WHERE ns = ?1 AND id = ?2",
            params![ns_id, &id[..]],
        )? > 0)
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

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cairn-schema-{}",
            hex::encode(rand::random::<[u8; 8]>())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A database as a release before accounts left it: schema 1, with namespaces in it.
    fn schema_1(db: &Path, namespaces: &[&str]) {
        let conn = Connection::open(db).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        for ns in namespaces {
            ensure_namespace(&conn, ns, Bodies::Json).unwrap();
        }
        conn.pragma_update(None, "user_version", 1).unwrap();
    }

    #[test]
    fn the_token_from_before_accounts_becomes_the_first_users_with_every_namespace() {
        let dir = scratch();
        let db = dir.join("db");
        schema_1(&db, &["default", "team"]);
        let how = Opening {
            first_token: Some("abc123\n"),
            beside_server: false,
        };
        let mut s = SqliteStore::open_with(&db, how).unwrap();
        assert_eq!(
            s.token_user(&token_hash("abc123")).unwrap().as_deref(),
            Some(FIRST_USER)
        );
        for ns in ["default", "team"] {
            assert_eq!(s.access(ns, FIRST_USER).unwrap(), Some(Access::Manage));
        }
        // Its folders as they were, and no first folder of a new store beside them.
        assert_eq!(s.namespaces().unwrap(), ["default", "team"]);
        drop(s);
        // Only the move to accounts does that: a token file seen again later changes nothing.
        let mut s = SqliteStore::open_with(
            &db,
            Opening {
                first_token: Some("other"),
                beside_server: false,
            },
        )
        .unwrap();
        assert_eq!(s.users().unwrap().len(), 1);
        assert_eq!(s.token_user(&token_hash("other")).unwrap(), None);
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_new_database_with_a_token_gives_its_user_the_first_folder() {
        let dir = scratch();
        let mut s = SqliteStore::open_with(
            &dir.join("db"),
            Opening {
                first_token: Some("t"),
                beside_server: false,
            },
        )
        .unwrap();
        assert_eq!(s.namespaces().unwrap(), [FIRST_FOLDER]);
        assert_eq!(
            s.access(FIRST_FOLDER, FIRST_USER).unwrap(),
            Some(Access::Manage)
        );
        let mut none = SqliteStore::open(&dir.join("other")).unwrap();
        assert!(none.users().unwrap().is_empty(), "no token, no user");
        drop((s, none));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn beside_a_server_only_this_schema_is_taken_and_nothing_is_migrated() {
        let dir = scratch();
        let db = dir.join("db");
        let beside = Opening {
            first_token: None,
            beside_server: true,
        };
        assert!(
            SqliteStore::open_with(&db, beside).is_err(),
            "no database yet"
        );
        assert!(!db.exists(), "and none made");
        schema_1(&db, &["default"]);
        let err = SqliteStore::open_with(&db, beside)
            .err()
            .expect("must refuse");
        assert!(
            err.to_string().contains("the binary the server runs"),
            "{err}"
        );
        let version: usize = Connection::open(&db)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1, "left as it was");
        SqliteStore::open(&db).unwrap();
        let mut s = SqliteStore::open_with(&db, beside).unwrap();
        s.add_user("u", 0).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
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
