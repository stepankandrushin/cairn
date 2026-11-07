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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// What the last agreement was, per object, for a sync that carries a change (incr.rs), and every
/// object whose base node or row has been written since, by triggers in the file itself, whatever
/// build writes it. Not a migration, like the generation: an older build opens the file as before, and
/// its writes mark what they touch too.
const LEDGER_TABLES: &str = "
CREATE TABLE IF NOT EXISTS ledger (object BLOB PRIMARY KEY, entry BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS dirty (object BLOB PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS ledger_anchor (k INTEGER PRIMARY KEY, v TEXT NOT NULL);
";

/// Table, operation, condition, and the objects it marks.
const DIRTY_TRIGGERS: &[(&str, &str, &str, &[&str])] = &[
    ("base", "INSERT", "", &["NEW"]),
    (
        "base",
        "UPDATE",
        "WHEN OLD.node IS NOT NEW.node OR OLD.object IS NOT NEW.object",
        &["OLD", "NEW"],
    ),
    ("base", "DELETE", "", &["OLD"]),
    ("rows", "INSERT", "", &["NEW"]),
    (
        "rows",
        "UPDATE",
        "WHEN OLD.row IS NOT NEW.row OR OLD.object IS NOT NEW.object",
        &["OLD", "NEW"],
    ),
    ("rows", "DELETE", "", &["OLD"]),
];

fn dirty_triggers() -> Vec<(String, String)> {
    DIRTY_TRIGGERS
        .iter()
        .map(|(table, op, when, marks)| {
            let name = format!("dirty_{table}_{}", op.to_lowercase());
            let body: String = marks
                .iter()
                // Not `OR IGNORE`: an outer statement's conflict clause overrides a trigger's.
                .map(|m| {
                    format!(
                        "INSERT INTO dirty (object) SELECT {m}.object WHERE NOT EXISTS \
                         (SELECT 1 FROM dirty WHERE object = {m}.object); "
                    )
                })
                .collect();
            let sql = format!("CREATE TRIGGER {name} AFTER {op} ON {table} {when} BEGIN {body}END");
            (name, sql)
        })
        .collect()
}

fn ensure_ledger(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(LEDGER_TABLES)?;
    let want = dirty_triggers();
    let have: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND name GLOB 'dirty_*' ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut sorted = want.clone();
    sorted.sort();
    if have == sorted {
        return Ok(());
    }
    // Missing, partial or different: what was written meanwhile may have gone unmarked, so the ledger
    // speaks for nothing until a full sync writes it again.
    let tx = conn.transaction()?;
    for (name, _) in &have {
        tx.execute_batch(&format!("DROP TRIGGER \"{name}\";"))?;
    }
    for (_, sql) in &want {
        tx.execute_batch(&format!("{sql};"))?;
    }
    tx.execute_batch("DELETE FROM ledger_anchor; DELETE FROM ledger; DELETE FROM dirty;")?;
    tx.commit()?;
    Ok(())
}

/// A table the ledger is rebuilt from.
#[derive(Clone, Copy)]
pub enum StateTable {
    Base,
    Rows,
}

/// Every entry of a ledger besides those a finalize puts, each to the writer as it comes.
pub type LedgerRest<'a> =
    Box<dyn Fn(&mut dyn FnMut(ObjectId, Vec<u8>) -> Result<()>) -> Result<()> + 'a>;

/// What a finalize leaves in the ledger, in its transaction.
#[derive(Default)]
pub struct LedgerWrite<'a> {
    /// The whole ledger anew: everything not put, marked or in `rest` here goes.
    pub whole: bool,
    pub put: Vec<(ObjectId, Vec<u8>)>,
    pub drop: Vec<ObjectId>,
    /// No longer written since its entry.
    pub clean: Vec<ObjectId>,
    pub dirty: Vec<ObjectId>,
    pub anchor: Option<String>,
    /// With `whole`, a ledger rebuilt from the state: its entries for what the round did not take,
    /// written before those the round put.
    pub rest: Option<LedgerRest<'a>>,
}

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

/// The page cache of a state's connection, in KiB.
const STATE_CACHE_KIB: i64 = 32 * 1024;

/// What `warm` reads at a time.
const WARM_PIECE: usize = 1 << 20;

/// Reads a state's file, and the WAL beside it if a run left one, once from start to end in pieces of
/// a MiB, so that the system caches what a sync is about to read most of. SQLite reads a page at a
/// time in the order of a table's keys, and in a state that syncs have rewritten that is not the file's
/// order: a row rewritten moves, and the file keeps the pages that frees. With the cache cold, as it is
/// at each sync on a device with little memory to spare, since the daemon holds nothing between syncs,
/// every page was a seek: about 150 a second on a hard disk, where this goes at the disk's sequential
/// rate. What is cached already costs a copy. Not for a file over a quarter of this machine's memory,
/// which the cache would not keep. Before SQLite opens the file: closing another descriptor of it would
/// drop every POSIX lock this process holds on it, SQLite's too. Only a hint, so a read that fails ends
/// it, and SQLite says what it then cannot read itself. Returns the bytes read.
pub fn warm(path: &Path) -> u64 {
    use std::io::Read;
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let files: Vec<(std::path::PathBuf, u64)> = [path.to_path_buf(), wal.into()]
        .into_iter()
        .filter_map(|p| {
            let len = std::fs::metadata(&p).ok()?.len();
            (len > 0).then_some((p, len))
        })
        .collect();
    let total: u64 = files.iter().map(|(_, len)| len).sum();
    if total == 0 || memory().is_some_and(|m| total > m / 4) {
        return 0;
    }
    let mut piece = vec![0u8; WARM_PIECE];
    let mut read = 0;
    for (p, _) in &files {
        let Ok(mut file) = std::fs::File::open(p) else {
            continue;
        };
        // Twice the read-ahead; the other systems see a sequential reader for themselves.
        #[cfg(target_os = "linux")]
        let _ = rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::Sequential);
        loop {
            match file.read(&mut piece) {
                Ok(0) => break,
                Ok(n) => read += n as u64,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }
    read
}

/// This machine's memory, in bytes.
#[cfg(target_os = "linux")]
fn memory() -> Option<u64> {
    let info = rustix::system::sysinfo();
    Some(info.totalram as u64 * u64::from(info.mem_unit.max(1)))
}

#[cfg(target_os = "macos")]
fn memory() -> Option<u64> {
    let mut bytes: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a 64-bit integer, written into `bytes`, whose size `len` gives.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut bytes as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && bytes > 0).then_some(bytes)
}

#[cfg(windows)]
fn memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: a zeroed MEMORYSTATUSEX is valid, and the call fills the one it is given, of the length
    // it is told.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;
    (ok && status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn memory() -> Option<u64> {
    None
}

/// Free pages under this are not worth rewriting the state for: a small tree's would churn for nothing.
const COMPACT_FLOOR: u64 = 4 << 20;

/// A state's file by its pages: their size, how many, and how many of them are free, kept by SQLite
/// for reuse but never given back to the filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    fn free_percent(&self) -> u64 {
        (self.free * 100).checked_div(self.count).unwrap_or(0)
    }
}

/// What `State::compact` found, or did.
#[derive(Debug)]
pub enum Compaction {
    NotDue(Pages),
    /// Rewritten in key order with no page free: the file's bytes after, and how long it took.
    Done {
        before: Pages,
        after: u64,
        took: Duration,
    },
    /// Due, but the filesystem holding `place` has `free` bytes where the rewrite takes `need`.
    NoRoom {
        pages: Pages,
        place: PathBuf,
        need: u64,
        free: u64,
    },
}

impl std::fmt::Display for Compaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use cairn_protocol::local::bytes;
        match self {
            Compaction::NotDue(p) => write!(
                f,
                "the state was not compacted: {} % of its {} is free pages",
                p.free_percent(),
                bytes(p.file())
            ),
            Compaction::Done {
                before,
                after,
                took,
            } => write!(
                f,
                "compacted the state, {} % of it free pages: {} to {}, in {:.1} s",
                before.free_percent(),
                bytes(before.file()),
                bytes(*after),
                took.as_secs_f64()
            ),
            Compaction::NoRoom {
                pages,
                place,
                need,
                free,
            } => write!(
                f,
                "the state was not compacted, though {} % of its {} is free pages: rewriting it takes {} \
                 where {} is, which has {} free; it stays as it was, and is tried again after a later sync",
                pages.free_percent(),
                bytes(pages.file()),
                bytes(*need),
                place.display(),
                bytes(*free)
            ),
        }
    }
}

/// Where SQLite puts a temporary database: its unix VFS's first of these that is a folder it may write
/// in, Windows's temporary folder there.
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
    #[cfg(windows)]
    {
        Some(std::env::temp_dir())
    }
}

/// The bytes free to this user on the filesystem holding `path`, if it says.
fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let s = rustix::fs::statvfs(path).ok()?;
        Some(s.f_bavail.saturating_mul(s.f_frsize))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut free: u64 = 0;
        // SAFETY: a NUL-terminated path, and one out-parameter, the others null as the call allows.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } != 0;
        ok.then_some(free)
    }
}

/// Whether two folders are on one filesystem, so that what is written in each comes out of one room.
fn same_filesystem(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev(),
            _ => true,
        }
    }
    #[cfg(windows)]
    {
        let prefix = |p: &Path| {
            std::path::absolute(p)
                .ok()
                .and_then(|p| match p.components().next() {
                    Some(std::path::Component::Prefix(x)) => {
                        Some(x.as_os_str().to_string_lossy().to_lowercase())
                    }
                    _ => None,
                })
        };
        prefix(a) == prefix(b)
    }
}

/// Where a compaction of a state whose pages in use hold `live` bytes lacks room, if anywhere: the WAL
/// beside `file` takes about that much as the rewrite goes into it, and SQLite's temporary database as
/// much again, each with an eighth and 8 MiB to spare. The WAL then goes back into the file, which only
/// shrinks, and both are emptied.
fn short_of_room(file: &Path, live: u64) -> Option<(PathBuf, u64, u64)> {
    let each = (live + live / 8 + (8 << 20)) * cairn_failpoint::tuned("state.compact_room", 1);
    if each == 0 {
        return None;
    }
    let state_dir = file.parent()?.to_path_buf();
    let temp = sqlite_temp_dir();
    let mut places = vec![(state_dir.clone(), each)];
    match temp {
        Some(t) if same_filesystem(&state_dir, &t) => places[0].1 += each,
        Some(t) => places.push((t, each)),
        None => {}
    }
    places.into_iter().find_map(|(place, need)| {
        let free = free_bytes(&place)?;
        (free < need).then_some((place, need, free))
    })
}

/// What a transaction writes, in the order of the table's keys: each page is then dirtied once, and
/// written once, however far the transaction outgrows the page cache. In the order given, as random as
/// object IDs are, a page goes to the WAL again each time it is dirtied again after a spill.
fn in_key_order<T, K: Ord + ?Sized>(items: &[T], key: impl Fn(&T) -> &K) -> Vec<&T> {
    let mut out: Vec<&T> = items.iter().collect();
    out.sort_by(|a, b| key(a).cmp(key(b)));
    out
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
        // Every statement on a table with triggers keeps a journal of the pages it touches, to undo it
        // alone. A few pages each, but in a temporary file, from the first that passes 64 KiB, for the
        // rest of the transaction: a join wrote its state several times over into those.
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // Pages held between steps, in KiB: enough for the inner pages of a state of millions of entries,
        // so a lookup costs a leaf and not a walk from the top, and for what one transaction dirties in
        // key order before it spills. Taken only as pages are read; the default is 2 MiB.
        conn.pragma_update(None, "cache_size", -STATE_CACHE_KIB)?;
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
        ensure_ledger(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn pages(&self) -> Result<Pages> {
        let get = |pragma: &str| -> Result<u64> {
            let n: i64 = self.conn.pragma_query_value(None, pragma, |r| r.get(0))?;
            Ok(n.max(0) as u64)
        };
        Ok(Pages {
            size: get("page_size")?,
            count: get("page_count")?,
            free: get("freelist_count")?,
        })
    }

    /// Whether `compact` would rewrite the state now. A state it cannot ask is not.
    pub fn compaction_due(&self) -> bool {
        self.pages()
            .is_ok_and(|p| p.due(cairn_failpoint::tuned("state.compact_floor", COMPACT_FLOOR)))
    }

    /// Rewrites the state, the file at `file`, with every table in key order and no page free, if more
    /// than half of it is free pages: what a join or any big rewrite leaves, which SQLite reuses but never
    /// gives back, and which each sync's warm-up reads and a cold read of a table seeks across. Its page
    /// size and `auto_vacuum` stay as they were, so nothing about the format changes. SQLite's VACUUM,
    /// through the WAL as any transaction: a kill or a full disk at any point leaves the state as it
    /// was, or as it is after. Its temporary copy goes to a file, not to memory, where it would take as
    /// much as the state holds. The WAL is emptied after it either way, so that what a VACUUM that failed
    /// wrote does not hold the room.
    pub fn compact(&self, file: &Path) -> Result<Compaction> {
        let before = self.pages()?;
        if !before.due(cairn_failpoint::tuned("state.compact_floor", COMPACT_FLOOR)) {
            return Ok(Compaction::NotDue(before));
        }
        cairn_failpoint::hit("state.compact.due");
        if let Some((place, need, free)) = short_of_room(file, before.live()) {
            return Ok(Compaction::NoRoom {
                pages: before,
                place,
                need,
                free,
            });
        }
        let started = Instant::now();
        self.conn.pragma_update(None, "temp_store", "FILE")?;
        // Points to kill it at, all the way through: one every thousand of SQLite's steps.
        #[cfg(feature = "failpoints")]
        self.conn.progress_handler(
            1000,
            Some(|| {
                cairn_failpoint::hit("state.compact.vacuuming");
                false
            }),
        );
        let vacuumed = self.conn.execute_batch("VACUUM");
        #[cfg(feature = "failpoints")]
        self.conn.progress_handler(0, None::<fn() -> bool>);
        cairn_failpoint::hit("state.compact.vacuumed");
        let emptied = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            });
        let restored = self.conn.pragma_update(None, "temp_store", "MEMORY");
        vacuumed.context("rewrite the state (VACUUM)")?;
        emptied.context("empty the state's WAL")?;
        restored?;
        cairn_failpoint::hit("state.compact.done");
        let after = std::fs::metadata(file).map_or(0, |m| m.len());
        Ok(Compaction::Done {
            before,
            after,
            took: started.elapsed(),
        })
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

    /// How many objects the base holds, the root and those deleted among them.
    pub fn base_len(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM base", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Whether the base holds anything besides the root: a first sync's has nothing.
    pub fn has_base(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM base WHERE object != ?1)",
            [ObjectId::ROOT.0],
            |r| r.get(0),
        )?)
    }

    /// Every base node, then every row, as text, in the order of the objects, in batches to `each`:
    /// what the ledger is rebuilt from, read once, with no more of it held than a batch.
    pub fn each_in_batches(
        &self,
        table: StateTable,
        batch: usize,
        each: &mut dyn FnMut(Vec<(ObjectId, String)>) -> Result<()>,
    ) -> Result<()> {
        let sql = match table {
            StateTable::Base => "SELECT object, node FROM base ORDER BY object",
            StateTable::Rows => "SELECT object, row FROM rows ORDER BY object",
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::with_capacity(batch);
        while let Some(row) = rows.next()? {
            let id: [u8; 16] = row
                .get_ref(0)?
                .as_blob()?
                .try_into()
                .map_err(|_| anyhow::anyhow!("a stored ID is not 16 bytes"))?;
            out.push((ObjectId(id), row.get_ref(1)?.as_str()?.to_string()));
            if out.len() == batch {
                each(std::mem::replace(&mut out, Vec::with_capacity(batch)))?;
            }
        }
        if !out.is_empty() {
            each(out)?;
        }
        Ok(())
    }

    /// The ledger no longer speaks for these objects, as if something had written them.
    pub fn mark_dirty(&mut self, objects: &[ObjectId]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut mark = tx.prepare_cached("INSERT OR IGNORE INTO dirty (object) VALUES (?1)")?;
            for id in in_key_order(objects, |id| id) {
                mark.execute([id.0])?;
            }
        }
        tx.commit()?;
        Ok(())
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

    /// What the ledger says it stands on, if a full sync has written it since it last could not.
    pub fn ledger_anchor(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT v FROM ledger_anchor WHERE k = 1", [], |r| r.get(0))
            .optional()?)
    }

    /// The ledger, as stored: incr.rs reads the entries.
    /// The ledger speaks for nothing until a full sync writes it again.
    pub fn forget_ledger(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "BEGIN; DELETE FROM ledger_anchor; DELETE FROM ledger; DELETE FROM dirty; COMMIT;",
            )
            .context("forget the ledger")?;
        Ok(())
    }

    pub fn ledger_entries(&self) -> Result<Vec<(ObjectId, Vec<u8>)>> {
        let mut stmt = self.conn.prepare("SELECT object, entry FROM ledger")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, entry) = row?;
            out.push((object_id(id)?, entry));
        }
        Ok(out)
    }

    /// How many entries the ledger has.
    pub fn ledger_len(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM ledger", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// The ledger, entry by entry, in the order of the objects, to `each`.
    pub fn each_ledger_entry(&self, each: &mut dyn FnMut(ObjectId, &[u8])) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT object, entry FROM ledger ORDER BY object")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id = object_id(row.get::<_, Vec<u8>>(0)?)?;
            let entry = row.get_ref(1)?.as_blob()?;
            each(id, entry);
        }
        Ok(())
    }

    /// Every object written since the ledger last spoke for it.
    pub fn dirty(&self) -> Result<BTreeSet<ObjectId>> {
        let mut stmt = self.conn.prepare("SELECT object FROM dirty")?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        let mut out = BTreeSet::new();
        for row in rows {
            out.insert(object_id(row?)?);
        }
        Ok(out)
    }

    /// The base nodes of these objects, those it has.
    pub fn nodes_of(&self, objects: &[ObjectId]) -> Result<BTreeMap<ObjectId, Node>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT node FROM base WHERE object = ?1")?;
        let mut out = BTreeMap::new();
        for id in objects {
            if let Some(text) = stmt
                .query_row([id.0], |r| r.get::<_, String>(0))
                .optional()?
            {
                out.insert(*id, serde_json::from_str::<Node>(&text)?);
            }
        }
        Ok(out)
    }

    /// The rows of these objects, those that have one.
    pub fn rows_of(&self, objects: &[ObjectId]) -> Result<BTreeMap<ObjectId, Row>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT row FROM rows WHERE object = ?1")?;
        let mut out = BTreeMap::new();
        for id in objects {
            if let Some(text) = stmt
                .query_row([id.0], |r| r.get::<_, String>(0))
                .optional()?
            {
                out.insert(*id, serde_json::from_str::<Row>(&text)?);
            }
        }
        Ok(out)
    }

    /// Drops these manifests, which nothing holds any more.
    pub fn drop_manifests(&mut self, hashes: &[Hash]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut del = tx.prepare_cached("DELETE FROM manifests WHERE hash = ?1")?;
            for h in in_key_order(hashes, |h| h) {
                del.execute([h.0])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The hashes of every manifest held.
    pub fn manifest_hashes(&self) -> Result<BTreeSet<Hash>> {
        let mut stmt = self.conn.prepare("SELECT hash FROM manifests")?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        let mut out = BTreeSet::new();
        for row in rows {
            let h: [u8; 32] = row?
                .try_into()
                .map_err(|_| anyhow::anyhow!("a stored hash is not 32 bytes"))?;
            out.insert(Hash(h));
        }
        Ok(out)
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
        self.store_base_with(old, new, cursor, cursor_op, rows, gone, None)
    }

    /// The same, and the ledger with it: what it says is only ever about the base and rows beside it.
    #[allow(clippy::too_many_arguments)]
    pub fn store_base_with(
        &mut self,
        old: &Tree,
        new: &Tree,
        cursor: u64,
        cursor_op: Option<OpId>,
        rows: &[(ObjectId, &Row)],
        gone: &[ObjectId],
        ledger: Option<&LedgerWrite<'_>>,
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
        if let Some(l) = ledger {
            write_ledger(&tx, l)?;
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
            for (h, b) in in_key_order(manifests, |(h, _)| h) {
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
            for key in in_key_order(marks, |k| k.as_str()) {
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
            for (key, value) in in_key_order(marks, |(k, _)| k.as_str()) {
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
            for key in in_key_order(marks, |k| k.as_str()) {
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

/// After the base and rows, whose triggers have marked what they wrote: the entries, then the marks.
fn write_ledger(tx: &rusqlite::Transaction<'_>, l: &LedgerWrite<'_>) -> Result<()> {
    if l.whole {
        tx.execute_batch("DELETE FROM ledger; DELETE FROM dirty;")?;
    }
    {
        let mut del = tx.prepare_cached("DELETE FROM ledger WHERE object = ?1")?;
        for id in &l.drop {
            del.execute([id.0])?;
        }
        let mut put = tx.prepare_cached(
            "INSERT INTO ledger (object, entry) VALUES (?1, ?2) ON CONFLICT (object) DO UPDATE SET entry = excluded.entry",
        )?;
        if let Some(rest) = &l.rest {
            rest(&mut |id, entry| {
                put.execute(params![id.0, entry])?;
                Ok(())
            })?;
        }
        for (id, entry) in &l.put {
            put.execute(params![id.0, entry])?;
        }
        let mut clean = tx.prepare_cached("DELETE FROM dirty WHERE object = ?1")?;
        for id in &l.clean {
            clean.execute([id.0])?;
        }
        let mut mark = tx.prepare_cached("INSERT OR IGNORE INTO dirty (object) VALUES (?1)")?;
        for id in &l.dirty {
            mark.execute([id.0])?;
        }
    }
    if let Some(anchor) = &l.anchor {
        tx.execute("DELETE FROM ledger_anchor", [])?;
        tx.execute(
            "INSERT INTO ledger_anchor (k, v) VALUES (1, ?1)",
            [anchor.as_str()],
        )?;
    }
    Ok(())
}

fn object_id(bytes: Vec<u8>) -> Result<ObjectId> {
    let id: [u8; 16] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("a stored ID is not 16 bytes"))?;
    Ok(ObjectId(id))
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
        for (id, row) in in_key_order(changed, |(id, _)| id) {
            put.execute(params![id.0, serde_json::to_string(row)?])?;
        }
    }
    let mut del = tx.prepare_cached("DELETE FROM rows WHERE object = ?1")?;
    for id in in_key_order(gone, |id| id) {
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

    /// Every table's rows, and the schema with its triggers, in one order: what a compaction keeps.
    fn everything(state: &State) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
        let tables: Vec<String> = {
            let mut stmt = state
                .conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .unwrap();
            let names = stmt.query_map([], |r| r.get(0)).unwrap();
            names.map(Result::unwrap).collect()
        };
        let mut all = Vec::new();
        for table in ["sqlite_master".to_string()].into_iter().chain(tables) {
            // Where each table starts is the one thing in the schema a rewrite moves.
            let sql = match table.as_str() {
                "sqlite_master" => {
                    "SELECT type, name, tbl_name, sql FROM sqlite_master".to_string()
                }
                t => format!("SELECT * FROM \"{t}\""),
            };
            let mut stmt = state.conn.prepare(&sql).unwrap();
            let n = stmt.column_count();
            let mut rows: Vec<Vec<rusqlite::types::Value>> = stmt
                .query_map([], |r| (0..n).map(|i| r.get(i)).collect())
                .unwrap()
                .map(Result::unwrap)
                .collect();
            rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            all.push((table, rows));
        }
        all
    }

    #[track_caller]
    fn same_everything(
        got: &[(String, Vec<Vec<rusqlite::types::Value>>)],
        want: &[(String, Vec<Vec<rusqlite::types::Value>>)],
    ) {
        let names = |all: &[(String, Vec<Vec<rusqlite::types::Value>>)]| -> Vec<String> {
            all.iter()
                .map(|(t, rows)| format!("{t}: {}", rows.len()))
                .collect()
        };
        let differ: Vec<&String> = got
            .iter()
            .zip(want)
            .filter(|(g, w)| g != w)
            .map(|(g, _)| &g.0)
            .collect();
        assert!(
            got.len() == want.len() && differ.is_empty(),
            "differ: {differ:?}; got {:?}, wanted {:?}",
            names(got),
            names(want)
        );
    }

    #[test]
    fn a_state_is_due_a_compaction_past_half_free_and_the_floor_alone() {
        let pages = |count, free| Pages {
            size: 4096,
            count,
            free,
        };
        let floor = COMPACT_FLOOR;
        let at_floor = floor / 4096;
        assert!(pages(2 * at_floor - 1, at_floor).due(floor));
        assert!(
            !pages(2 * at_floor, at_floor).due(floor),
            "half is not over half"
        );
        assert!(!pages(at_floor, at_floor - 1).due(floor), "under the floor");
        assert!(!pages(0, 0).due(floor), "empty");
        assert!(
            pages(570_789, 302_952).due(floor),
            "a state rewritten by a join"
        );
        assert!(!pages(570_789, 30_000).due(floor), "a state in use");
    }

    /// The rows stay, every one, the schema and its triggers with them, and the generation, so the next
    /// sync with nothing to do still knows it has nothing to do; the free pages go, and the WAL is empty.
    #[test]
    fn a_compaction_takes_back_the_free_pages_and_keeps_every_row() {
        let (dir, mut state) = fresh();
        let path = dir.join("state.sqlite");
        let rows: Vec<(ObjectId, Row)> = (0..2000).map(|i| (ObjectId::random(), row(i))).collect();
        let refs: Vec<(ObjectId, &Row)> = rows.iter().map(|(id, r)| (*id, r)).collect();
        state.store_rows(&refs, &[]).unwrap();
        let manifests: Vec<(Hash, Vec<u8>)> = (0..48u8)
            .map(|i| (Hash::of(&[i]), vec![i; 200_000]))
            .collect();
        state.put_manifests(&manifests).unwrap();
        let keep: BTreeSet<Hash> = manifests.iter().take(3).map(|(h, _)| *h).collect();
        state.retain_manifests(&keep).unwrap();
        let gone: Vec<ObjectId> = rows.iter().skip(500).map(|(id, _)| *id).collect();
        state.store_rows(&[], &gone).unwrap();

        let before = state.pages().unwrap();
        assert!(state.compaction_due(), "{before:?}");
        let (kept, generation) = (everything(&state), state.generation().unwrap());
        let (size, vacuum): (i64, i64) = state
            .conn
            .query_row(
                "SELECT * FROM pragma_page_size, pragma_auto_vacuum",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        let done = state.compact(&path).unwrap();
        let Compaction::Done { after, .. } = done else {
            panic!("{done:?}")
        };
        let now = state.pages().unwrap();
        assert_eq!(now.free, 0, "{now:?}");
        assert_eq!(after, std::fs::metadata(&path).unwrap().len());
        assert!(after <= before.live(), "{after} of {before:?}");
        assert_eq!(
            std::fs::metadata(dir.join("state.sqlite-wal"))
                .unwrap()
                .len(),
            0
        );
        same_everything(&everything(&state), &kept);
        assert_eq!(state.generation().unwrap(), generation);
        let same: (i64, i64) = state
            .conn
            .query_row(
                "SELECT * FROM pragma_page_size, pragma_auto_vacuum",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(same, (size, vacuum));
        let temp: i64 = state
            .conn
            .pragma_query_value(None, "temp_store", |r| r.get(0))
            .unwrap();
        assert_eq!(temp, 2, "back to memory for what a sync does");
        assert!(!state.compaction_due());
        assert!(matches!(
            state.compact(&path).unwrap(),
            Compaction::NotDue(_)
        ));

        // And as the next open finds it.
        drop(state);
        let state = State::open(&path).unwrap();
        same_everything(&everything(&state), &kept);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_state_mostly_in_use_is_left_as_it_is() {
        let (dir, state) = fresh();
        let path = dir.join("state.sqlite");
        state.set("big", &"x".repeat(12 << 20)).unwrap();
        state.set("other", &"x".repeat(12 << 20)).unwrap();
        state.set("other", &"y".repeat(4 << 20)).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        let pages = state.pages().unwrap();
        assert!(pages.free > 0 && pages.free * 2 < pages.count, "{pages:?}");
        assert!(matches!(
            state.compact(&path).unwrap(),
            Compaction::NotDue(_)
        ));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The warm-up reads the file whole, and a WAL a run left beside it, and nothing where there is none.
    #[test]
    fn warm_reads_the_state_and_a_wal_left_beside_it_whole() {
        let (dir, state) = fresh();
        state.set("big", &"x".repeat(3 << 20)).unwrap();
        drop(state);
        let path = dir.join("state.sqlite");
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len > 3 << 20, "{len}");
        assert_eq!(warm(&path), len);
        std::fs::write(dir.join("state.sqlite-wal"), vec![7u8; 5000]).unwrap();
        assert_eq!(warm(&path), len + 5000);
        assert_eq!(warm(&dir.join("none.sqlite")), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The ledger speaks for an object only until its base node or row is written: any write marks it,
    /// whatever makes it, and only a write that changes something.
    #[test]
    fn every_write_to_a_base_node_or_row_marks_its_object_and_nothing_else_does() {
        let (dir, mut state) = fresh();
        let (id, other) = (ObjectId::random(), ObjectId::random());
        let clean = |state: &mut State| {
            let all: Vec<ObjectId> = state.dirty().unwrap().into_iter().collect();
            let tx = state.conn.transaction().unwrap();
            write_ledger(
                &tx,
                &LedgerWrite {
                    clean: all,
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        };
        let marked = |state: &State, what: &str, expect: &[ObjectId]| {
            let now: Vec<ObjectId> = state.dirty().unwrap().into_iter().collect();
            let mut want = expect.to_vec();
            want.sort();
            assert_eq!(now, want, "{what}");
        };
        state.store_rows(&[(id, &row(1))], &[]).unwrap();
        marked(&state, "a new row", &[id]);
        clean(&mut state);
        state.store_rows(&[(id, &row(1))], &[]).unwrap();
        marked(&state, "the same row again", &[]);
        state.store_rows(&[(id, &row(2))], &[]).unwrap();
        marked(&state, "a changed row", &[id]);
        clean(&mut state);
        state.store_rows(&[], &[id]).unwrap();
        marked(&state, "a row gone", &[id]);
        clean(&mut state);
        state
            .put_manifests(&[(Hash::of(b"m"), b"m".to_vec())])
            .unwrap();
        state.put_intent("x/1", &1).unwrap();
        state.set("cursor", "7").unwrap();
        state.set_steady(Some("{}")).unwrap();
        marked(&state, "manifests, intents, the cursor and the record", &[]);
        let old = Tree::new();
        let mut new = Tree::new();
        let node = |n: u8| Node {
            rev: cairn_protocol::RevisionId([n; 16]),
            body: RevisionBody {
                content: Content::Dir,
                metadata: Metadata::default(),
            },
            links: BTreeSet::new(),
        };
        new.nodes.insert(id, node(1));
        new.nodes.insert(other, node(1));
        state.store_base(&old, &new, 7, None, &[], &[]).unwrap();
        marked(&state, "two base nodes", &[id, other]);
        clean(&mut state);
        state.store_base(&new, &new, 7, None, &[], &[]).unwrap();
        marked(&state, "the same base", &[]);
        let mut newer = new.clone();
        newer.nodes.insert(other, node(2));
        state.store_base(&new, &newer, 8, None, &[], &[]).unwrap();
        marked(&state, "one node changed", &[other]);
        clean(&mut state);
        // Written by anything else than these methods, as an older build would: still marked.
        state
            .conn
            .execute("UPDATE rows SET row = row || ' '", [])
            .unwrap();
        state
            .conn
            .execute("DELETE FROM base WHERE object = ?1", [id.0])
            .unwrap();
        marked(&state, "plain SQL", &[id]);
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_mark_trigger_is_made_again_and_the_ledger_dropped() {
        let (dir, mut state) = fresh();
        let id = ObjectId::random();
        {
            let tx = state.conn.transaction().unwrap();
            write_ledger(
                &tx,
                &LedgerWrite {
                    whole: true,
                    put: vec![(id, vec![1, 2, 3])],
                    anchor: Some("{}".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }
        drop(state);
        let state = State::open(&dir.join("state.sqlite")).unwrap();
        assert_eq!(
            state.ledger_anchor().unwrap().as_deref(),
            Some("{}"),
            "opened again"
        );
        assert_eq!(state.ledger_entries().unwrap().len(), 1);
        state
            .conn
            .execute_batch("DROP TRIGGER dirty_base_update;")
            .unwrap();
        drop(state);
        let state = State::open(&dir.join("state.sqlite")).unwrap();
        assert_eq!(state.ledger_anchor().unwrap(), None);
        assert!(state.ledger_entries().unwrap().is_empty());
        let triggers: i64 = state
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name GLOB 'dirty_*'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(triggers as usize, DIRTY_TRIGGERS.len());
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
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

    /// What this thread has written, through any file: SQLite writes from the thread that calls it, so
    /// tests running beside this one do not count.
    #[cfg(target_os = "linux")]
    fn written_by_this_thread() -> u64 {
        let io = std::fs::read_to_string("/proc/thread-self/io").unwrap();
        io.lines()
            .find_map(|l| l.strip_prefix("wchar: "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap()
    }

    /// Every row of a big tree written in one transaction, then every one of them again, as a join does
    /// in several of its passes: about the state's own size each time, and not dozens of times that.
    /// With the statement journals that every statement on a table with triggers keeps in temporary
    /// files, and rows in the order given into a page cache of 2 MiB, which sent a page to the WAL again
    /// each time it was dirtied again, this wrote 29 times what it left (3.6 GB for 125 MB).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_transaction_over_every_row_writes_about_the_state_once() {
        let (dir, mut state) = fresh();
        let ids: Vec<ObjectId> = (0..50_000).map(|_| ObjectId::random()).collect();
        let rows = |pass: u64| -> Vec<(ObjectId, Row)> {
            ids.iter()
                .enumerate()
                .map(|(i, id)| {
                    let mut r = row(pass * 1_000_000 + i as u64);
                    r.content = Content::File {
                        manifest: Hash(rand::random()),
                        size: i as u64,
                    };
                    // About 800 bytes of JSON a row, as the rows of a tree of small files take.
                    let pad = "x".repeat(345 + (i * 7) % 250);
                    r.scan
                        .unknown
                        .insert("pad".into(), serde_json::Value::String(pad));
                    (*id, r)
                })
                .collect()
        };
        let before = written_by_this_thread();
        for pass in 0..2 {
            let rows = rows(pass);
            let list: Vec<(ObjectId, &Row)> = rows.iter().map(|(id, r)| (*id, r)).collect();
            state.store_rows(&list, &[]).unwrap();
        }
        let written = written_by_this_thread() - before;
        let size = |name: &str| std::fs::metadata(dir.join(name)).map_or(0, |m| m.len());
        let left = size("state.sqlite") + size("state.sqlite-wal");
        assert!(
            written < 4 * left,
            "wrote {written} bytes for a state of {left}: {:.1} times",
            written as f64 / left as f64
        );
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
                "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name GLOB 'generation_*'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(triggers as usize, GENERATION_TRIGGERS.len());
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }
}
