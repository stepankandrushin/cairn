//! One sync of a root: scan, merge against the server, apply locally, push, until both sides agree.
//!
//! The merge is state-based: each round works from what is on disk and what the journal says. A
//! crash anywhere only means the next run sees some of the work already done on one side or both.
//! What was on its way here is noted first, and the next scan says how far it got, so recovery has no
//! special path beyond undoing half-made moves.

use crate::apply::{
    self, cur_from_identified, read_for_upload, recover, Applier, CurNode, NoteIntent,
    INTENT_ARRIVING, INTENT_COPY, INTENT_NOTE,
};
use crate::client::{Client, Retries};
use crate::identify::{identify, settle_arriving, Arriving, CopyIntent, Identified};
use crate::merge::{merge, MergeInput, Plan, ReportKind, TNode};
use crate::metadata::Accounts;
use crate::root::{Paused, Root};
use crate::scan::{scan, ScanOptions, ScanResult};
use crate::schedule::{schedule, Topo, TopoOp};
use crate::selection::Selection;
use crate::state::{Known, Row, Synced};
use anyhow::{anyhow, bail, Context, Result};
use cairn_failpoint::{check, hit};
use cairn_protocol::local::{Activity, Change, Direction};
use cairn_protocol::wire::{self, CommitRequest, CommitResponse, MAX_CHUNK_BATCH_BYTES};
use cairn_protocol::{
    ChunkRef, ConflictOf, Content, Hash, Link, Manifest, Name, ObjectId, ObjectKind, Op, OpId,
    RevisionId, Skip, Tree,
};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

/// Rounds in one run before admitting the tree will not hold still.
const MAX_ROUNDS: u32 = 12;
/// Rounds in which something moving under an apply holds back pushing, too.
const ROUNDS_BEFORE_PUSHING: u32 = 3;
/// Walks of the tree in one round, while it changes under the walk.
const SCAN_ATTEMPTS: u32 = 3;

/// What a round takes of the server's journal before it scans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Journal {
    /// Everything up to the head: the first round, and one after a commit, which must see it.
    Head,
    /// Nothing more: the round confirms what the one before it applied, against the same sequence, so
    /// that a sync beside a device that commits more often than a round takes still comes to agree, as
    /// of that sequence. What came after it is the next sync's.
    Pinned,
}
/// At least this many vanished entries, making up half the tree, pause sync.
const MASS_DELETE_MIN: usize = 10;

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    pub allow_mass_delete: bool,
    pub dry_run: bool,
    /// Something changed under the root, by the watcher's word: no use looking for nothing to do.
    pub changed_here: bool,
    /// The daemon's check on the incremental path: a sync this long after the last whose every decision
    /// was the full sync's sets the ledger it keeps against what the state says, object by object, and
    /// takes up, saying so loudly, whatever the ledger vouched for that the state does not.
    pub full_every: Option<std::time::Duration>,
    /// The daemon's watcher, where it can say where things changed since the last agreement: each round
    /// on the incremental path then lists those folders and what the round itself must see, and takes
    /// the rest from the ledger (scope.rs).
    pub scope: Option<crate::scope::Source>,
}

/// Times a round's scope is widened, to what its decisions reach, before it lists the whole tree.
const SCOPE_WIDENINGS: u32 = 4;

/// Whether this sync is to check the ledger against the state.
fn full_check_due(root: &Root, opts: &SyncOptions) -> bool {
    let Some(every) = opts.full_every else {
        return false;
    };
    if opts.dry_run {
        return false;
    }
    root.full_checked()
        .is_none_or(|t| now_secs().saturating_sub(t) >= every.as_secs() as i64)
}

/// What the daemon's check says of an object the ledger vouched for and the state does not.
fn defect(path: &str, what: &str) -> String {
    format!(
        "a defect: the incremental bookkeeping vouched for {path}, which the state says is not settled: {what}; this sync takes it up as the full sync would; please report it, with this log"
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Both sides agree. Anything excluded is listed in the report.
    Synced,
    /// Agreement was not fully reached; another run may do it. Nothing was lost.
    Partial,
    /// Sync stopped itself and is waiting for a person. Nothing was changed.
    Paused(String),
}

#[derive(Debug)]
pub struct SyncReport {
    pub outcome: SyncOutcome,
    pub skips: Vec<Skip>,
    /// Metadata this filesystem could not take. The server keeps it.
    pub warnings: Vec<Skip>,
    /// Paths whose metadata was applied here in full, which clears an earlier warning about them.
    pub meta_applied: Vec<String>,
    /// Conflicts and other events a person should know about.
    pub events: Vec<String>,
    pub conflicts: u64,
    pub commits: u64,
    pub ops_pushed: u64,
    pub chunks_uploaded: u64,
    pub bytes_uploaded: u64,
    pub steps_applied: u64,
    pub rounds: u32,
    pub head: u64,
    /// What a dry run would have done.
    pub planned: Vec<String>,
    /// The topmost entries here left alone because this device excludes them.
    pub excluded_here: Vec<String>,
    /// The topmost entries on the server that this device does not sync.
    pub not_synced_here: Vec<String>,
    /// How long the scans took.
    pub scan_time: std::time::Duration,
    /// What the next sync will repeat, whatever there is to do: loading the state, and the last
    /// round's scan, merge, scheduling and record, without the data it moved or any wait.
    pub repeated: std::time::Duration,
    /// When the scan began that this sync's result rests on, if it ended agreeing with the server:
    /// whatever happened in the tree before that, it has seen.
    pub agreed_since: Option<std::time::Instant>,
    /// What it sent and what it applied here, in order, for the daemon's list of recent changes.
    /// Not metadata applied here alone, which a filesystem that cannot hold all of it would repeat.
    pub activity: Vec<Activity>,
    /// Why a round took the whole tree, if one did, when it could have taken what changed alone.
    pub whole_because: Option<String>,
    /// Why the ledger was rebuilt from the state, if it was, where it could not be read.
    pub rebuilt_because: Option<String>,
    /// For a sync whose rounds took what changed alone: the most objects one round decided about,
    /// the most it loaded with their neighbours, and how many the ledger holds.
    pub part: Option<(u64, u64, u64)>,
    /// For a sync that checked the ledger against the state: whether the two agreed.
    pub checked: Option<bool>,
    /// The folders the last round listed, where its scan had a scope, by their printable paths; `None`
    /// where it listed the whole tree. What was skipped elsewhere it did not see again.
    pub looked_in: Option<Vec<String>>,
    /// How many times a request the network failed was tried again.
    pub retries: u64,
}

impl SyncReport {
    pub(crate) fn new() -> Self {
        Self {
            outcome: SyncOutcome::Partial,
            skips: Vec::new(),
            warnings: Vec::new(),
            meta_applied: Vec::new(),
            events: Vec::new(),
            conflicts: 0,
            commits: 0,
            ops_pushed: 0,
            chunks_uploaded: 0,
            bytes_uploaded: 0,
            steps_applied: 0,
            rounds: 0,
            head: 0,
            planned: Vec::new(),
            excluded_here: Vec::new(),
            not_synced_here: Vec::new(),
            scan_time: std::time::Duration::ZERO,
            repeated: std::time::Duration::ZERO,
            agreed_since: None,
            activity: Vec::new(),
            whole_because: None,
            rebuilt_because: None,
            part: None,
            checked: None,
            looked_in: None,
            retries: 0,
        }
    }
}

fn unheld_skip(remote: &Tree, u: &crate::selection::Unheld) -> Skip {
    Skip {
        path: remote.path_of_link(&u.link).map_or_else(
            || u.link.name.to_string(),
            |p| crate::scan::display_path(&p),
        ),
        kind: cairn_protocol::SkipKind::CannotHoldName,
        detail: Some(u.why.clone()),
    }
}

/// Today's UTC date as `YYYY-MM-DD`, for conflict names.
pub fn utc_date(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    // Civil-from-days, Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// What replaying the journal has shown.
#[derive(Default)]
pub struct Seen {
    /// The operation at the end of what was replayed.
    pub last: Option<OpId>,
    /// Every commit replayed, since the base: these are known to have reached the server.
    pub commits: BTreeSet<OpId>,
}

/// Replay the journal onto `tree` up to the server's head.
pub fn fetch_changes(
    client: &Client,
    tree: &mut Tree,
    seq: &mut u64,
    seen: &mut Seen,
) -> Result<()> {
    let mut cow = Cow::Owned(std::mem::replace(tree, Tree::new()));
    let fetched = fetch_into(client, &mut cow, seq, seen);
    *tree = cow.into_owned();
    fetched
}

/// The same, copying the tree only once there is something to replay onto it.
fn fetch_into(
    client: &Client,
    tree: &mut Cow<'_, Tree>,
    seq: &mut u64,
    seen: &mut Seen,
) -> Result<()> {
    fetch_ops(client, seq, seen, &mut |op, _| {
        tree.to_mut().apply(op).map_err(anyhow::Error::from)
    })
}

/// Every operation after `seq` up to the server's head, in order, to `each`.
fn fetch_ops(
    client: &Client,
    seq: &mut u64,
    seen: &mut Seen,
    each: &mut dyn FnMut(&Op, OpId) -> Result<()>,
) -> Result<()> {
    loop {
        let resp = client.changes(*seq)?;
        for change in &resp.changes {
            for op in &change.ops {
                each(op, change.op_id).map_err(|e| {
                    anyhow!(
                        "the server's journal does not replay at seq {}: {e}",
                        change.seq
                    )
                })?;
            }
            *seq = change.seq;
            seen.last = Some(change.op_id);
            seen.commits.insert(change.op_id);
        }
        if resp.changes.is_empty() || *seq >= resp.head {
            return Ok(());
        }
    }
}

/// The server must still hold the history this device has seen. After a restore from backup it may
/// be behind, or have gone on to a different entry at the same number, and merging against that
/// would be wrong in ways nothing else would notice.
fn check_history(client: &Client, cursor: u64, cursor_op: Option<OpId>) -> Result<Option<String>> {
    if cursor == 0 {
        return Ok(None);
    }
    // The entry alone: its commit can be thousands of operations.
    let resp = client.change_at(cursor)?;
    Ok(history_gone(&resp, cursor, cursor_op))
}

/// Why sync must pause, if the server's journal at the cursor is not what this device saw there.
pub(crate) fn history_gone(
    resp: &wire::ChangesResponse,
    cursor: u64,
    cursor_op: Option<OpId>,
) -> Option<String> {
    if cursor == 0 {
        return None;
    }
    let same = resp
        .changes
        .first()
        .is_some_and(|c| c.seq == cursor && cursor_op.is_none_or(|op| op == c.op_id));
    (!same).then(|| {
        format!(
            "the server no longer has the history this device last saw (sequence {cursor}; it is at {}): was it \
             restored from a backup? Sync is paused and nothing was changed. Files only here are safe. To \
             rejoin, remove this root's .cairn folder and run `cairn init` again: the next sync merges by \
             name and content, keeps every difference as a conflict copy, and sends the server what it lost.",
            resp.head
        )
    })
}

#[allow(clippy::too_many_arguments)]
/// The rows whole, and the base and the server's tree.
type WholeRows<'a> = (&'a BTreeMap<ObjectId, Row>, (&'a Tree, &'a Tree));

/// `whole` is the rows and trees whole, which leftovers of a killed run need; without them, a scan that
/// finds any is `None`, for the full sync to take.
#[allow(clippy::too_many_arguments)]
fn scan_stable(
    root: &mut Root,
    by_inode: &HashMap<(u64, u64), Known>,
    whole: Option<WholeRows<'_>>,
    accounts: &Accounts,
    remove_temps: bool,
    excluded: &dyn Fn(&[cairn_protocol::Name], &crate::fsops::Stat) -> bool,
    date: &str,
    events: &mut Vec<String>,
    kept_edits: &mut u64,
    scope: Option<(&crate::scan::Scope<'_>, std::time::Instant)>,
) -> Result<Option<ScanResult>> {
    let prefix = root.temp_prefix();
    let on_its_way: Vec<(Content, Vec<Name>)> = root
        .state
        .intents::<Arriving>(INTENT_ARRIVING)?
        .into_iter()
        .map(|(_, a)| {
            let names = a.links.iter().chain(&a.names).map(|l| l.name.clone());
            (a.body.content, names.collect())
        })
        .collect();
    let opts = ScanOptions {
        temp_prefix: prefix.as_bytes(),
        accounts,
        rows: by_inode,
        remove_own_temps: remove_temps,
        arriving: &on_its_way,
        excluded,
        probe: false,
        scope: scope.map(|(s, _)| s),
        started: scope.map(|(_, t)| t),
    };
    let scan = |dir: &crate::fsops::Dir, opts: &ScanOptions<'_>| match opts.scope {
        Some(_) => crate::scan::scan_scoped(dir, opts),
        None => scan(dir, opts),
    };
    let mut result = scan(&root.dir, &opts).context("scanning the root")?;
    let mut rescued = Vec::new();
    let Some((rows, trees)) = whole else {
        // Leftovers, and what was on its way into a folder left out here, are the whole tree's to see.
        if !result.stale_temps.is_empty()
            || !result.rescued_temps.is_empty()
            || (remove_temps && !on_its_way.is_empty() && !result.excluded.is_empty())
        {
            return Ok(None);
        }
        for _ in 1..cairn_failpoint::tuned("sync.scan_attempts", SCAN_ATTEMPTS.into()) {
            if !result.unstable {
                break;
            }
            result = scan(&root.dir, &opts).context("scanning the root")?;
        }
        keep_what_only_moved_holes(root, by_inode, &mut result);
        return Ok(Some(result));
    };
    // A rule made since a kill can hide the folder its leftovers are in from the scan: looked for in the
    // folders of what was on its way, so that an edit caught there is kept under a name and said, not left
    // under a temporary one until the rule goes.
    let hidden = if remove_temps {
        leftovers_out_of_view(root, trees, &result.excluded)?
    } else {
        Vec::new()
    };
    for (path, name) in hidden {
        result.stale_temps.push((path, name));
    }
    if !result.stale_temps.is_empty() || !result.rescued_temps.is_empty() {
        // Looked at again now that the scan has removed what it could: a name that was another's when
        // the scan got there may be the last.
        for (path, name) in std::mem::take(&mut result.stale_temps) {
            let dir = root.dir.open_path(path.iter())?;
            match crate::scan::aside_holds_edit(&dir, &name, &opts) {
                Some(fp) => result.rescued_temps.push((path, name, fp)),
                None => apply::remove_stale_temp(root, &path, &name)
                    .context("removing a leftover temporary file")?,
            }
        }
        for (path, name, fp) in &result.rescued_temps {
            let found = rows.iter().find(|(_, r)| {
                (r.fp.dev, r.fp.ino, crate::fsops::birth_mark(r.fp.btime))
                    == (fp.dev, fp.ino, fp.btime)
            });
            let original = name
                .strip_prefix(prefix.as_bytes())
                .and_then(crate::root::aside_origin)
                .and_then(|origin| Name::new(origin.to_vec()).ok())
                .unwrap_or_else(|| Name::new(b"recovered".to_vec()).expect("a valid name"));
            // That inode becomes a new file under its new name, no longer the object it was, and the
            // record says so first: a kill between the two leaves the file to be found again. It goes up
            // as a conflict copy of what it was caught by, so the conflict inbox lists it on every device
            // however the run that kept it ends.
            let mark = rescue_mark(root, path, name, fp, found.map(|(id, _)| *id), trees)?;
            let row = found.map(|(id, row)| {
                let mut row = row.clone();
                row.fp.dev = 0;
                row.fp.ino = 0;
                (*id, row)
            });
            let rows_noted: Vec<(ObjectId, &Row)> = row.iter().map(|(id, r)| (*id, r)).collect();
            match &mark {
                Some((key, intent)) => root.state.put_intent_with_rows(key, intent, &rows_noted)?,
                None => root.state.store_rows(&rows_noted, &[])?,
            }
            if let Some((id, _)) = row {
                rescued.push(id);
            }
            if mark.is_some() || !rows_noted.is_empty() {
                hit("clean.rescue.noted");
            }
            *kept_edits += 1;
            let mut kept = apply::keep_rescued_temp(root, path, name, &original, date)
                .context("keeping an edit from a leftover temporary file")?;
            if result.excluded.iter().any(|e| path.starts_with(e)) {
                kept.push_str(
                    ", in a folder this device leaves out, so it is not synced while it does",
                );
            }
            events.push(kept);
        }
        result = scan(&root.dir, &opts).context("scanning the root")?;
    }
    for _ in 1..cairn_failpoint::tuned("sync.scan_attempts", SCAN_ATTEMPTS.into()) {
        if !result.unstable {
            break;
        }
        result = scan(&root.dir, &opts).context("scanning the root")?;
    }
    result.rescued = rescued;
    keep_what_only_moved_holes(root, by_inode, &mut result);
    Ok(Some(result))
}

/// This device's leftovers in folders the scan left out, where what was on its way was going.
fn leftovers_out_of_view(
    root: &Root,
    (base, remote): (&Tree, &Tree),
    excluded: &[Vec<Name>],
) -> Result<Vec<(Vec<Name>, Vec<u8>)>> {
    if excluded.is_empty() {
        return Ok(Vec::new());
    }
    let mut folders: BTreeSet<Vec<Name>> = BTreeSet::new();
    for (_, a) in root.state.intents::<Arriving>(INTENT_ARRIVING)? {
        for link in a.links.iter().chain(&a.names) {
            let Some(mut path) = remote
                .path_of_link(link)
                .or_else(|| base.path_of_link(link))
            else {
                continue;
            };
            path.pop();
            if excluded.iter().any(|e| path.starts_with(e)) {
                folders.insert(path);
            }
        }
    }
    let prefix = root.temp_prefix();
    let mut out = Vec::new();
    for path in folders {
        let Ok(dir) = root.dir.open_path(path.iter()) else {
            continue;
        };
        let Ok(names) = dir.list() else {
            continue;
        };
        for name in names {
            let Some(rest) = name.strip_prefix(prefix.as_bytes()) else {
                continue;
            };
            // What a scan would remove or keep: a folder only if it was moved aside.
            let folder = dir
                .stat(&name)
                .is_ok_and(|st| st.file_type == crate::fsops::FileType::Directory);
            if !folder || crate::root::aside_origin(rest).is_some() {
                out.push((path.clone(), name));
            }
        }
    }
    Ok(out)
}

/// The conflict copy a kept edit becomes: of the object it was, by its record or else by the name it was
/// moved aside from, against the server's revision of that, which is what was arriving or being removed.
/// The same mark again if a kill came before the edit got its name. None if no object can be named.
fn rescue_mark(
    root: &Root,
    path: &[Name],
    temp: &[u8],
    fp: &crate::state::Fingerprint,
    known: Option<ObjectId>,
    (base, remote): (&Tree, &Tree),
) -> Result<Option<(String, CopyIntent)>> {
    let kind = match root.dir.open_path(path.iter())?.stat(temp)?.file_type {
        crate::fsops::FileType::Directory => ObjectKind::Dir,
        crate::fsops::FileType::Symlink => ObjectKind::Symlink,
        _ => ObjectKind::File,
    };
    let is = Some((kind, fp.btime));
    if let Some((key, intent)) = root
        .state
        .intents::<CopyIntent>(INTENT_COPY)?
        .into_iter()
        .find(|(_, i)| (i.dev, i.ino) == (fp.dev, fp.ino) && i.is == is)
    {
        return Ok(Some((key, intent)));
    }
    let origin = temp
        .strip_prefix(root.temp_prefix().as_bytes())
        .and_then(crate::root::aside_origin)
        .unwrap_or_default();
    // A long name was cut to fit: the one name in the folder it begins.
    let by_name = || {
        let parent = base.resolve(path)?;
        let mut found = base
            .children(parent)
            .filter(|(n, _)| n.as_bytes().starts_with(origin) && !origin.is_empty());
        let (name, id) = found.next()?;
        (name.as_bytes() == origin || found.next().is_none()).then_some(id)
    };
    let Some(object) = known.or_else(by_name) else {
        return Ok(None);
    };
    let Some(node) = remote.nodes.get(&object) else {
        return Ok(None);
    };
    let copy = ObjectId::random();
    let intent = CopyIntent {
        copy,
        dev: fp.dev,
        ino: fp.ino,
        is,
        conflict_of: ConflictOf {
            object,
            competing_revision: node.rev,
        },
        moves: Vec::new(),
    };
    Ok(Some((format!("{INTENT_COPY}{copy}"), intent)))
}

/// What arrived and was removed here before it was recorded, said once for each topmost path.
fn gone_lines(gone: &[String]) -> Vec<String> {
    let mut paths: Vec<&String> = gone.iter().collect();
    paths.sort();
    paths.dedup();
    let mut top: Vec<(&String, usize)> = Vec::new();
    for p in paths {
        match top.last_mut() {
            Some((t, n))
                if p.starts_with(t.as_str()) && p.as_bytes().get(t.len()) == Some(&b'/') =>
            {
                *n += 1
            }
            _ => top.push((p, 0)),
        }
    }
    top.into_iter()
        .map(|(path, under)| {
            let with = match under {
                0 => String::new(),
                1 => " and 1 entry in it".into(),
                n => format!(" and {n} entries in it"),
            };
            format!(
                "{path}{with}: removed here while it was arriving, before this device had recorded it, \
                 so nothing proves the delete and it comes down again, with whatever was still on its \
                 way. Delete it again if that was meant"
            )
        })
        .collect()
}

fn layout(m: &Manifest) -> Vec<(u64, u64)> {
    m.segments.iter().map(|s| (s.offset, s.len())).collect()
}

/// A file read again whose manifest differs from its record only in where its holes are holds the
/// same bytes, and keeps the record's: filesystems move holes by themselves. APFS fills a small one when
/// it flushes and lays data runs out on its own boundaries, so a sparse file that arrived there would
/// otherwise go back to everyone as a new revision with the same bytes, and inflated. Two manifests
/// with the same layout and different hashes are different bytes, so only a layout that differs is
/// read again.
fn keep_what_only_moved_holes(
    root: &Root,
    by_inode: &HashMap<(u64, u64), Known>,
    result: &mut ScanResult,
) {
    let mut new: Option<HashMap<Hash, &[u8]>> = None;
    for node in &mut result.nodes {
        let Content::File {
            manifest: now,
            size,
        } = node.content
        else {
            continue;
        };
        let Some(Content::File {
            manifest: was,
            size: was_size,
        }) = by_inode
            .get(&(node.fp.dev, node.fp.ino))
            .map(|r| &r.content)
        else {
            continue;
        };
        if now == *was || size != *was_size {
            continue;
        }
        let new = new.get_or_insert_with(|| {
            result
                .new_manifests
                .iter()
                .map(|(h, b)| (*h, b.as_slice()))
                .collect()
        });
        let decode = |b: &[u8]| Manifest::decode(b).ok();
        let Some(read) = new.get(&now).and_then(|b| decode(b)) else {
            continue;
        };
        let Some(recorded) = root
            .state
            .manifest(was)
            .ok()
            .flatten()
            .and_then(|b| decode(&b))
        else {
            continue;
        };
        if layout(&read) != layout(&recorded) && still_holds(root, node, &recorded) {
            node.content = Content::File {
                manifest: *was,
                size,
            };
        }
    }
}

/// Whether the file a scan read still is what it read, and holds the bytes of `m`.
fn still_holds(root: &Root, node: &crate::scan::Scanned, m: &Manifest) -> bool {
    let Some((name, parent)) = node.paths.first().and_then(|p| p.split_last()) else {
        return false;
    };
    let Ok(dir) = root.dir.open_path(parent.iter()) else {
        return false;
    };
    let unchanged = || {
        dir.stat(name.as_bytes())
            .is_ok_and(|st| crate::scan::fingerprint(&st) == node.fp)
    };
    let Ok(fd) = dir.open_read(name.as_bytes()) else {
        return false;
    };
    unchanged()
        && crate::fsops::fstat(crate::fsops::AsFd::as_fd(&fd)).is_ok_and(|st| st.ino == node.fp.ino)
        && crate::chunker::same_bytes(crate::fsops::AsFd::as_fd(&fd), m).unwrap_or(false)
        && unchanged()
}

/// For what has arrived whole with something else than its record on disk: what this filesystem would
/// have made of that record, beside it and under the policy the run applied by. An error if that cannot be
/// asked: what is there would otherwise stand for what arrived, and a change made here would never go up.
fn what_arrives_here(
    root: &Root,
    accounts: &Accounts,
    arriving: &BTreeMap<ObjectId, Arriving>,
    identified: &Identified,
    dry_run: bool,
) -> Result<BTreeMap<ObjectId, cairn_protocol::Metadata>> {
    let mut out = BTreeMap::new();
    for a in arriving.values() {
        let Some(node) = identified.scanned.get(&a.object) else {
            continue;
        };
        let mut want = a.body.metadata.clone();
        want.btime = node.metadata.btime;
        if node.content != a.body.content || node.metadata == want {
            continue;
        }
        let policy = a.policy.as_ref().unwrap_or(&root.config.policy);
        let folder = node
            .paths
            .first()
            .and_then(|p| p.split_last())
            .map(|(_, parent)| parent)
            .filter(|_| !dry_run);
        let held = apply::what_arrives(root, accounts, &node.content, &a.body.metadata, policy, folder)
            .with_context(|| {
                format!(
                    "{}: asking this filesystem what it makes of what arrived before an interrupted sync",
                    crate::scan::display_path(&node.paths[0])
                )
            })?;
        if let Some(held) = held {
            out.insert(a.object, held);
        }
    }
    Ok(out)
}

/// An empty or largely vanished tree is not a mass delete until a person says so (invariant 5).
/// `outside` counts what a round that takes part of the tree found where it was and left out: live on
/// both sides, and here.
fn check_vanished(
    base: &Tree,
    remote: &Tree,
    rows: &BTreeMap<ObjectId, Row>,
    local: &BTreeMap<ObjectId, crate::merge::LNode>,
    outside: usize,
) -> Option<String> {
    let mut live = outside;
    let mut gone = 0usize;
    for (id, _) in remote.live() {
        // Known here: in the base, or pushed from here and not in the base yet.
        let known = base.is_live(id) || rows.get(&id).is_some_and(|r| r.synced.is_some());
        if id == ObjectId::ROOT || !known {
            continue;
        }
        live += 1;
        if !local.contains_key(&id) {
            gone += 1;
        }
    }
    let everything = live > 0 && gone == live;
    let most = gone >= MASS_DELETE_MIN && gone * 2 >= live;
    (everything || most).then(|| {
        format!(
            "{gone} of {live} synced entries have vanished from this folder. If a disk is not mounted, mount it. \
             If you did delete them, run `cairn sync --allow-mass-delete`. Sync is paused and nothing was changed."
        )
    })
}

/// The operations that turn the server's tree into the target.
pub fn remote_ops(remote: &Tree, plan: &Plan) -> Result<Vec<Op>> {
    let from: Topo = remote
        .live()
        .filter(|(id, _)| *id != ObjectId::ROOT)
        .map(|(id, n)| (id, (n.kind(), n.links.clone())))
        .collect();
    let to: Topo = plan
        .target
        .iter()
        .filter(|(id, _)| **id != ObjectId::ROOT)
        .map(|(id, t)| (*id, (t.body.content.kind(), t.links.clone())))
        .collect();
    let mut ops = Vec::new();
    for step in schedule(&from, &to).context("ordering the push")? {
        ops.push(match step {
            TopoOp::Create { object, link } => {
                if remote.nodes.contains_key(&object) {
                    // Deleted there, surviving here: the same object comes back.
                    Op::Link { object, link }
                } else {
                    let t: &TNode = &plan.target[&object];
                    Op::Create {
                        object,
                        revision: RevisionId::random(),
                        body: t.body.clone(),
                        link,
                        conflict_of: t.conflict_of.clone(),
                    }
                }
            }
            TopoOp::Link { object, link } => Op::Link { object, link },
            TopoOp::Unlink { object, link } => Op::Unlink {
                object,
                link,
                base: remote.nodes.get(&object).map(|n| n.rev),
            },
            TopoOp::Move { object, from, to } => Op::Move { object, from, to },
        });
    }
    // After structure: an object being brought back is linked by now, and may be updated.
    for (id, t) in &plan.target {
        if let Some(r) = remote.nodes.get(id) {
            if r.body != t.body {
                ops.push(Op::Update {
                    object: *id,
                    base: r.rev,
                    revision: RevisionId::random(),
                    body: t.body.clone(),
                    restored_from: None,
                });
            }
        }
    }
    Ok(ops)
}

/// Limits of one commit, well inside what the server accepts.
const MAX_OPS_PER_COMMIT: usize = 20_000;
const MAX_COMMIT_BYTES: usize = 48 * 1024 * 1024;

/// How many of these operations the next commit holds. Every operation leaves a valid tree, so any
/// cut is valid, except that a stage name must not outlive its commit: the cut only falls where
/// nothing is staged.
fn commit_cut(ops: &[Op]) -> usize {
    let max_ops = cairn_failpoint::tuned("sync.ops_a_commit", MAX_OPS_PER_COMMIT as u64) as usize;
    let mut bytes = 0usize;
    let mut staged = 0i64;
    let mut cut = 0usize;
    for (i, op) in ops.iter().enumerate() {
        bytes += serde_json::to_vec(op).map_or(0, |v| v.len());
        if i > 0 && (i >= max_ops || bytes > MAX_COMMIT_BYTES) {
            break;
        }
        if let Op::Move { from, to, .. } = op {
            staged += i64::from(crate::schedule::is_stage(to));
            staged -= i64::from(crate::schedule::is_stage(from));
        }
        if staged == 0 {
            cut = i + 1;
        }
    }
    if cut == 0 {
        // Everything staged at once, however large, has to go together.
        return ops.len();
    }
    cut
}

/// The operations cut into commits, in order.
fn commits(mut ops: Vec<Op>) -> Vec<Vec<Op>> {
    let mut cuts = Vec::new();
    let mut at = 0;
    while at < ops.len() {
        at += commit_cut(&ops[at..]);
        cuts.push(at);
    }
    cuts.pop();
    // Split from the end, so that each operation moves once.
    let mut out: Vec<Vec<Op>> = cuts.iter().rev().map(|&cut| ops.split_off(cut)).collect();
    out.push(ops);
    out.reverse();
    out
}

#[cfg(test)]
fn truncate_ops(ops: &mut Vec<Op>) -> bool {
    let cut = commit_cut(ops);
    let truncated = cut < ops.len();
    ops.truncate(cut);
    truncated
}

enum Upload {
    Done {
        chunks: u64,
        bytes: u64,
    },
    /// A file no longer holds what was scanned.
    FileChanged,
}

/// Send what the server is missing. Every chunk is durable there before the commit that names it.
/// The server syncs every chunk to disk before it answers, so small files take far longer than their
/// bytes suggest: this keeps an answer within the client's timeout on a slow disk.
const MAX_CHUNKS_A_REQUEST: usize = 2_000;

fn upload(
    client: &Client,
    root: &Root,
    cur: &BTreeMap<ObjectId, CurNode>,
    ops: &[Op],
) -> Result<Upload> {
    let mut manifests: BTreeMap<Hash, Vec<u8>> = BTreeMap::new();
    let mut parsed: Vec<(Hash, Manifest)> = Vec::new();
    for op in ops {
        let Some(body) = op.body() else { continue };
        let Content::File { manifest, .. } = &body.content else {
            continue;
        };
        if manifests.contains_key(manifest) {
            continue;
        }
        // A manifest this client never held came from the server, which therefore has it.
        if let Some(bytes) = root.state.manifest(manifest)? {
            parsed.push((*manifest, Manifest::decode(&bytes)?));
            manifests.insert(*manifest, bytes);
        }
    }
    let mut wanted: Vec<Hash> = manifests.keys().copied().collect();
    let mut seen: HashSet<Hash> = wanted.iter().copied().collect();
    for (_, m) in &parsed {
        for c in m.chunks() {
            if seen.insert(c.hash) {
                wanted.push(c.hash);
            }
        }
    }
    let missing: HashSet<Hash> = client.missing(&wanted)?.into_iter().collect();
    if missing.is_empty() {
        return Ok(Upload::Done {
            chunks: 0,
            bytes: 0,
        });
    }

    // Where each missing chunk can be read from.
    let by_manifest: HashMap<Hash, ObjectId> = cur
        .iter()
        .filter_map(|(id, n)| match &n.content {
            Content::File { manifest, .. } if n.fp.is_some() => Some((*manifest, *id)),
            _ => None,
        })
        .collect();
    let mut sources: HashMap<Hash, (ObjectId, u64, ChunkRef)> = HashMap::new();
    for (hash, m) in &parsed {
        let Some(id) = by_manifest.get(hash) else {
            continue;
        };
        for (offset, c) in m.placed_chunks() {
            if missing.contains(&c.hash) {
                sources.entry(c.hash).or_insert((*id, offset, c.clone()));
            }
        }
    }

    let (mut chunks, mut bytes) = (0u64, 0u64);
    let mut frames = Vec::new();
    let mut in_frames = 0;
    let flush = |frames: &mut Vec<u8>| -> Result<()> {
        if !frames.is_empty() {
            check("sync.upload.send")?;
            client.upload(frames)?;
            hit("sync.upload.sent");
            frames.clear();
        }
        Ok(())
    };
    for hash in &wanted {
        if !missing.contains(hash) {
            continue;
        }
        let data = if let Some(bytes) = manifests.get(hash) {
            bytes.clone()
        } else if let Some((id, offset, chunk)) = sources.get(hash) {
            match read_for_upload(root, cur, *id, *offset, chunk)? {
                Some(d) => d,
                None => return Ok(Upload::FileChanged),
            }
        } else {
            return Ok(Upload::FileChanged);
        };
        if frames.len() + data.len() + 36 > MAX_CHUNK_BATCH_BYTES
            || in_frames >= MAX_CHUNKS_A_REQUEST
        {
            flush(&mut frames)?;
            in_frames = 0;
        }
        wire::write_frame(&mut frames, hash, &data);
        in_frames += 1;
        chunks += 1;
        bytes += data.len() as u64;
    }
    flush(&mut frames)?;
    Ok(Upload::Done { chunks, bytes })
}

/// Before anything else: commits queued by an earlier run go out with their original IDs, so a
/// response lost to a crash or the network is harmless.
fn replay_outbox(root: &mut Root, client: &Client, report: &mut SyncReport) -> Result<()> {
    for req in root.state.outbox()? {
        hit("sync.outbox.replay");
        let resp = client.commit(&req)?;
        if matches!(resp, CommitResponse::Committed { .. }) {
            report.commits += 1;
        } else {
            notes_back(root, &req)?;
            let mut rows = root.state.load_rows()?;
            let changed = take_back_refused(&req, &mut rows);
            let list: Vec<(ObjectId, &Row)> = changed.iter().map(|id| (*id, &rows[id])).collect();
            // Before the commit leaves the queue, so that dying here costs nothing.
            root.state.store_rows(&list, &[])?;
        }
        root.state.dequeue(&req.op_id)?;
        hit("sync.outbox.replayed");
    }
    Ok(())
}

fn update_rows(
    root: &mut Root,
    rows: &mut BTreeMap<ObjectId, Row>,
    identified: &crate::identify::Identified,
    base: &Tree,
) -> Result<()> {
    let changed = rows_after_scan(rows, identified, base);
    let list: Vec<(ObjectId, &Row)> = changed.iter().map(|id| (*id, &rows[id])).collect();
    root.state.store_rows(&list, &[])
}

/// What a scan says of each row it found: its fingerprint, content and metadata, and that what it was
/// found at is held. Returns the rows that changed.
fn rows_after_scan(
    rows: &mut BTreeMap<ObjectId, Row>,
    identified: &crate::identify::Identified,
    base: &Tree,
) -> Vec<ObjectId> {
    let mut changed = Vec::new();
    for (id, s) in &identified.scanned {
        let found = identified.local.get(id).map(|n| &n.links);
        let old = rows.get(id);
        // Never narrowed by a scan: an entry held and not found is what a delete here looks like.
        let mut held = match old {
            Some(o) => o
                .held
                .clone()
                .or_else(|| base.nodes.get(id).map(|n| n.links.clone())),
            None => None,
        }
        .unwrap_or_default();
        held.extend(found.into_iter().flatten().cloned());
        let new = Row {
            fp: s.fp.clone(),
            racy: s.racy,
            content: s.content.clone(),
            scan: s.metadata.clone(),
            applied: old.and_then(|o| o.applied.clone()),
            synced: old.and_then(|o| o.synced.clone()),
            incomplete: s.metadata_incomplete,
            held: Some(held),
        };
        if rows.get(id) != Some(&new) {
            rows.insert(*id, new);
            changed.push(*id);
        }
    }
    changed
}

/// A new local object recognised as a remote one takes the remote identity. One the server does not
/// have is starting over, and shares nothing with it yet.
fn adopt_rows(
    root: &mut Root,
    rows: &mut BTreeMap<ObjectId, Row>,
    adopted: &BTreeMap<ObjectId, ObjectId>,
    base: &Tree,
    remote: &Tree,
) -> Result<()> {
    if adopted.is_empty() {
        return Ok(());
    }
    let mut changed = Vec::new();
    let mut gone = Vec::new();
    for (x, y) in adopted {
        if let Some(mut row) = rows.remove(x) {
            if !remote.nodes.contains_key(y) {
                row.synced = None;
                // What is in it has not moved, whatever name its folder goes by now, or a kill before
                // what is left has gone would read as a move here, and bring all of it back.
                for (_, child) in base.children(*x) {
                    let (Some(known), Some(row), false) = (
                        base.nodes.get(&child),
                        rows.get_mut(&child),
                        adopted.contains_key(&child),
                    ) else {
                        continue;
                    };
                    let applied = row.applied.clone().unwrap_or_else(|| row.scan.clone());
                    let synced = row.synced.get_or_insert_with(|| Synced {
                        body: known.body.clone(),
                        links: known.links.clone(),
                        applied,
                        commit: None,
                        unapplied: false,
                        folder_time: None,
                    });
                    synced.links = synced
                        .links
                        .iter()
                        .map(|l| Link {
                            parent: if l.parent == *x { *y } else { l.parent },
                            name: l.name.clone(),
                        })
                        .collect();
                    changed.push(child);
                }
            }
            rows.insert(*y, row);
            changed.push(*y);
            gone.push(*x);
        }
    }
    let list: Vec<(ObjectId, &Row)> = changed.iter().map(|id| (*id, &rows[id])).collect();
    root.state.store_rows(&list, &gone)
}

/// Both sides agree: the server's tree becomes the base, and what is on disk the baseline that
/// future local changes are measured from.
/// What a finalize needs for the ledger: what the round's scan saw, what was out of view, and, for an
/// incremental round, the part of the tree it took.
struct Ledgering<'a> {
    seen: &'a HashMap<ObjectId, Option<Hash>>,
    hidden: &'a BTreeSet<ObjectId>,
    anchor: crate::incr::Anchor,
    part: Option<(
        &'a crate::incr::Session,
        &'a BTreeSet<ObjectId>,
        &'a BTreeSet<ObjectId>,
    )>,
}

#[allow(clippy::too_many_arguments)]
fn finalize(
    root: &mut Root,
    base: &Tree,
    remote: &Tree,
    seq: u64,
    last_op: Option<OpId>,
    rows: &mut BTreeMap<ObjectId, Row>,
    plan: &Plan,
    ledgering: Ledgering<'_>,
) -> Result<()> {
    hit("sync.finalize.begin");
    let mut changed = Vec::new();
    let mut gone = Vec::new();
    for (id, row) in rows.iter_mut() {
        match plan.local.get(id) {
            Some(n) if n.carried => {}
            Some(n) => {
                let mut dirty = row.held.as_ref() != Some(&n.links);
                row.held = Some(n.links.clone());
                // What could not be read fully is no baseline.
                if row.incomplete {
                    dirty |= row.synced.take().is_some();
                } else if row.applied.as_ref() != Some(&row.scan) || row.synced.is_some() {
                    row.applied = Some(row.scan.clone());
                    row.synced = None;
                    dirty = true;
                }
                if dirty {
                    changed.push(*id);
                }
            }
            None => gone.push(*id),
        }
    }
    for id in &gone {
        rows.remove(id);
    }
    let list: Vec<(ObjectId, &Row)> = changed.iter().map(|id| (*id, &rows[id])).collect();
    let agreement = crate::incr::Agreement {
        remote,
        hidden: ledgering.hidden,
        rows,
        local: &plan.local,
        seen: ledgering.seen,
    };
    let ledger = crate::incr::ledger_write(&agreement, ledgering.part, &ledgering.anchor)?;
    root.state
        .store_base_with(base, remote, seq, last_op, &list, &gone, Some(&ledger))?;
    hit("sync.finalize.base");

    for (key, _) in root.state.intents::<CopyIntent>(INTENT_COPY)? {
        root.state.drop_intent(&key)?;
    }
    // Only notes on what the server never had are left by now.
    for (key, _) in root.state.intents::<NoteIntent>(INTENT_NOTE)? {
        root.state.drop_intent(&key)?;
    }
    let mut keep: BTreeSet<Hash> = BTreeSet::new();
    for r in rows.values() {
        if let Content::File { manifest, .. } = &r.content {
            keep.insert(*manifest);
        }
    }
    match ledgering.part {
        None => root.state.retain_manifests(&keep)?,
        // Of what this sync's rows held, what no row holds now: the rows it did not take are their
        // ledger entries, which they have not changed from.
        Some((session, all, _)) => {
            let mut unheld: BTreeSet<Hash> = session.released.difference(&keep).copied().collect();
            if !unheld.is_empty() {
                for (id, seen) in session.ledger.iter_seen() {
                    if let Content::File { manifest, .. } = &seen.content {
                        if !all.contains(&id) {
                            unheld.remove(manifest);
                        }
                    }
                }
                // Written since and not taken here, which nothing should leave, but a row's own
                // manifest is never what goes.
                let others: Vec<ObjectId> = root
                    .state
                    .dirty()?
                    .into_iter()
                    .filter(|id| !all.contains(id))
                    .collect();
                for r in root.state.rows_of(&others)?.values() {
                    if let Content::File { manifest, .. } = &r.content {
                        unheld.remove(manifest);
                    }
                }
                let unheld: Vec<Hash> = unheld.into_iter().collect();
                root.state.drop_manifests(&unheld)?;
            }
        }
    }
    let cache = root.state_dir.open_dir(b"chunks")?;
    for name in cache.list()? {
        let _ = cache.unlink(&name);
    }
    Ok(())
}

/// Of the folders moving, those something excluded here is in: all that `keep_out_what_moves` reads.
fn moving_with_excluded(
    plan: &Plan,
    identified: &Identified,
    excluded: &[Vec<Name>],
) -> BTreeMap<Vec<Name>, Vec<Name>> {
    let parents: BTreeSet<&[Name]> = excluded
        .iter()
        .filter_map(|p| p.split_last().map(|(_, parent)| parent))
        .collect();
    folders_moving(plan, identified)
        .into_iter()
        .filter(|(from, _)| parents.contains(from.as_slice()))
        .collect()
}

/// Where the folders here will be after this plan, for those that will not be where they are.
fn folders_moving(plan: &Plan, identified: &Identified) -> BTreeMap<Vec<Name>, Vec<Name>> {
    let after = |id: ObjectId| {
        let mut parts = Vec::new();
        let mut at = plan.adopted.get(&id).copied().unwrap_or(id);
        while at != ObjectId::ROOT {
            let link = plan.target.get(&at)?.links.iter().next()?;
            parts.push(link.name.clone());
            at = link.parent;
        }
        parts.reverse();
        Some(parts)
    };
    identified
        .scanned
        .iter()
        .filter(|(_, node)| node.content == Content::Dir)
        .filter_map(|(id, node)| {
            let to = after(*id)?;
            let from = node.paths.first()?;
            (*from != to).then(|| (from.clone(), to))
        })
        .collect()
}

fn event_path(plan: &Plan, id: ObjectId) -> String {
    let mut parts = Vec::new();
    let mut at = id;
    while at != ObjectId::ROOT {
        let Some(link) = plan.target.get(&at).and_then(|t| t.links.iter().next()) else {
            break;
        };
        parts.push(link.name.to_string());
        at = link.parent;
        if parts.len() > 4096 {
            break;
        }
    }
    parts.reverse();
    parts.join("/")
}

/// A path on disk as the scan found it, by its first entry.
fn cur_path(cur: &BTreeMap<ObjectId, CurNode>, id: ObjectId) -> String {
    let mut parts = Vec::new();
    let mut at = id;
    while at != ObjectId::ROOT {
        let Some(link) = cur.get(&at).and_then(|c| c.links.iter().next()) else {
            break;
        };
        parts.push(link.name.to_string());
        at = link.parent;
        if parts.len() > 4096 {
            break;
        }
    }
    parts.reverse();
    parts.join("/")
}

/// What applying this plan does to the disk here: from the tree as scanned to the target. A folder
/// removed is one entry, not one for each thing in it.
fn down_activity(cur: &BTreeMap<ObjectId, CurNode>, plan: &Plan) -> Vec<Activity> {
    let time = now_secs();
    let entry = |change, path: String, to: Option<String>| Activity {
        time,
        direction: Direction::Down,
        change,
        path,
        to,
    };
    let copies: BTreeSet<ObjectId> = plan.copies.iter().map(|c| c.copy).collect();
    let mut out = Vec::new();
    for c in &plan.copies {
        out.push(entry(Change::Conflict, event_path(plan, c.copy), None));
    }
    for (id, t) in &plan.target {
        if *id == ObjectId::ROOT || copies.contains(id) {
            continue;
        }
        match cur.get(id) {
            None => out.push(entry(Change::Added, event_path(plan, *id), None)),
            Some(c) => {
                if c.links != t.links && !c.links.is_empty() {
                    out.push(entry(
                        Change::Moved,
                        cur_path(cur, *id),
                        Some(event_path(plan, *id)),
                    ));
                }
                // What the applier sets: the same test. A folder's own, as for one sent, is not listed.
                if c.content != t.body.content {
                    out.push(entry(Change::Changed, event_path(plan, *id), None));
                } else if c.apply_from != t.body.metadata
                    && c.kind != cairn_protocol::ObjectKind::Dir
                {
                    out.push(entry(Change::Metadata, event_path(plan, *id), None));
                }
            }
        }
    }
    let removed = |id: &ObjectId| *id != ObjectId::ROOT && !plan.target.contains_key(id);
    for (id, c) in cur {
        let topmost = c.links.iter().all(|l| !removed(&l.parent));
        if removed(id) && topmost {
            out.push(entry(Change::Removed, cur_path(cur, *id), None));
        }
    }
    out
}

/// What a commit sent, in its order. A folder deleted is one entry, not one for each thing in it.
fn up_activity(remote: &Tree, plan: &Plan, ops: &[Op]) -> Vec<Activity> {
    let time = now_secs();
    let entry = |change, path: String, to: Option<String>| Activity {
        time,
        direction: Direction::Up,
        change,
        path,
        to,
    };
    let was = |link: &Link| {
        remote
            .path_of_link(link)
            .map_or_else(|| link.name.to_string(), |p| crate::scan::display_path(&p))
    };
    let unlinked: BTreeSet<ObjectId> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Unlink { object, .. } => Some(*object),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for op in ops {
        match op {
            Op::Create {
                object,
                conflict_of,
                ..
            } => out.push(entry(
                if conflict_of.is_some() {
                    Change::Conflict
                } else {
                    Change::Added
                },
                event_path(plan, *object),
                None,
            )),
            Op::Link { link, .. } => {
                let parent = event_path(plan, link.parent);
                let path = if parent.is_empty() {
                    link.name.to_string()
                } else {
                    format!("{parent}/{}", link.name)
                };
                out.push(entry(Change::Added, path, None));
            }
            Op::Unlink { link, .. } if !unlinked.contains(&link.parent) => {
                out.push(entry(Change::Removed, was(link), None));
            }
            Op::Unlink { .. } => {}
            Op::Move { object, from, .. } => out.push(entry(
                Change::Moved,
                was(from),
                Some(event_path(plan, *object)),
            )),
            Op::Update { object, body, .. } => {
                let before = remote.get_live(*object);
                if before.is_some_and(|n| n.body.content != body.content) {
                    out.push(entry(Change::Changed, event_path(plan, *object), None));
                } else if body.content.kind() != cairn_protocol::ObjectKind::Dir {
                    out.push(entry(Change::Metadata, event_path(plan, *object), None));
                }
            }
            Op::Note { .. } => {}
        }
    }
    out
}

fn describe_op(op: &Op) -> String {
    match op {
        Op::Create { link, .. } => format!("create {}", link.name),
        Op::Link { link, .. } => format!("link {}", link.name),
        Op::Unlink { link, .. } => format!("delete {}", link.name),
        Op::Move { from, to, .. } => format!("move {} -> {}", from.name, to.name),
        Op::Update { object, .. } => format!("update {object:?}"),
        Op::Note { text, .. } => format!("note: {text}"),
    }
}

/// Sync one root once. A full disk is said as such, wherever it shows.
pub fn sync(path: &Path, opts: &SyncOptions) -> Result<SyncReport> {
    sync_once(path, opts).map_err(|e| {
        if crate::is_disk_full(&e) && !format!("{e:#}").contains("this disk is full") {
            e.context("this disk is full: free space and sync again")
        } else {
            e
        }
    })
}

fn sync_once(path: &Path, opts: &SyncOptions) -> Result<SyncReport> {
    let mut report = SyncReport::new();
    let mut root = match Root::open(path) {
        Ok(r) => r,
        Err(e) => {
            report.outcome = SyncOutcome::Paused(e.downcast::<Paused>()?.0);
            return Ok(report);
        }
    };
    let client = Client::new(&root.config, Retries::Sync)?;
    let retried = client.retried();
    let counted = |mut report: SyncReport| {
        report.retries = retried.load(std::sync::atomic::Ordering::SeqCst);
        report
    };
    let accounts = Accounts::load();
    // Read once, before anything is touched: a rule that cannot be read must stop the sync.
    let selection = Selection::load(&root)?;
    // Purely local, and first: anything else may send a person to act on this folder.
    if !opts.dry_run {
        report.events.extend(recover(&mut root)?);
    }
    let generation = root.state.generation()?;
    // The check is of what the incremental path decides, which a sync with nothing to do never asks.
    let check = full_check_due(&root, opts);
    // A sync that sees what the last one with nothing to do saw has nothing to do either.
    let fast = match !opts.dry_run && !opts.changed_here && !check && report.events.is_empty() {
        true => {
            crate::steady::try_fast(&mut root, &client, &accounts, &selection, opts, generation)
                .unwrap_or(None)
        }
        false => None,
    };
    if let Some(fast) = fast {
        hit("sync.fast");
        if !crate::steady::oracle() {
            return Ok(counted(fast.report));
        }
        // Every failpoint stays live in it, so that a test stopping or killing a sync at a step the
        // fast path passes over still finds it.
        let unscoped = SyncOptions {
            scope: None,
            ..opts.clone()
        };
        let full = sync_full(
            root,
            client,
            accounts,
            selection,
            (&unscoped, false),
            SyncReport::new(),
            generation,
        )?;
        return crate::steady::compare(fast, full).map(counted);
    }
    let (report, _) = sync_full(
        root,
        client,
        accounts,
        selection,
        (opts, check),
        report,
        generation,
    )?;
    Ok(counted(report))
}

/// The whole sync: every object scanned, identified, merged, applied and pushed, until both sides agree.
fn sync_full(
    mut root: Root,
    client: Client,
    accounts: Accounts,
    mut selection: Selection,
    (opts, check): (&SyncOptions, bool),
    mut report: SyncReport,
    generation: Option<u64>,
) -> Result<(SyncReport, crate::steady::Trace)> {
    let mut trace = crate::steady::Trace::default();
    // What a record of a sync with nothing to do needs: that nothing came before the merge either.
    let recovered = !report.events.is_empty();
    let inputs = crate::steady::inputs(&root, &selection, opts);
    let cursor_op = root.state.cursor_op()?;
    if let Some(why) = check_history(&client, root.state.cursor()?, cursor_op)? {
        report.outcome = SyncOutcome::Paused(why);
        return Ok((report, trace));
    }
    if !opts.dry_run {
        replay_outbox(&mut root, &client, &mut report)?;
    }

    let loading = std::time::Instant::now();
    // What changed alone, while the ledger vouches for the rest; otherwise the whole tree, and why.
    let mut session: Option<crate::incr::Session> = None;
    if crate::incr::enabled() {
        match crate::incr::Session::open(&root, &selection, opts.dry_run)? {
            Ok(s) => session = Some(s),
            Err(closed) if closed.rebuild => {
                report.rebuilt_because = Some(closed.why.clone());
                session = Some(crate::incr::Session::rebuilding(
                    &root, &selection, closed.why,
                )?);
            }
            Err(closed) => report.whole_because = Some(closed.why),
        }
    }
    let mut whole: Option<Whole> = match session {
        Some(_) => None,
        None => Some(Whole::load(&root, &[])?),
    };
    // What every sync repeats, whatever it has to do: loading the state, then in each round the scan,
    // the merge, the scheduling and the record, but not the data it moves or the waits between.
    let loaded = loading.elapsed();
    let mut seq = root.state.cursor()?;
    let date = utc_date(now_secs());
    let mut seen_events: BTreeSet<String> = report.events.iter().cloned().collect();
    let mut retryable = false;
    // Losers of metadata conflicts not yet in a commit the server took. Nothing else says them again:
    // the next round finds the conflict settled.
    let mut notes: Vec<(ObjectId, String)> = Vec::new();
    let mut noted: HashSet<(ObjectId, String)> = HashSet::new();
    // This round had nothing to do: the record can be made once the outcome is known.
    let mut steady_round = false;
    let mut round_work;

    let mut seen = Seen {
        last: cursor_op,
        ..Seen::default()
    };
    let mut journal = Journal::Head;
    // What rounds' decisions reached past what their scopes listed, listed from then on.
    let mut widened = crate::scope::Folders::new();
    let mut scope_widenings = 0;
    let mut scope = opts.scope.clone();
    let finish = loop {
        report.rounds += 1;
        round_work = std::time::Duration::ZERO;
        if report.rounds > MAX_ROUNDS {
            report.events.push(format!(
                "the tree kept changing for {MAX_ROUNDS} rounds; sync again once it settles"
            ));
            break SyncOutcome::Partial;
        }
        hit("sync.round");
        match (journal, &mut session, &mut whole) {
            (Journal::Pinned, _, _) => hit("sync.round.pinned"),
            (Journal::Head, Some(s), _) => {
                fetch_ops(&client, &mut seq, &mut seen, &mut |op, commit| {
                    s.journal.add_of(op, commit);
                    Ok(())
                })?
            }
            (Journal::Head, None, Some(w)) => {
                fetch_ops(&client, &mut seq, &mut seen, &mut |op, _| w.apply(op))?
            }
            (Journal::Head, None, None) => {
                bail!("a sync with neither the ledger nor the whole tree")
            }
        }
        report.head = seq;
        if let Some(s) = &session {
            if s.journal.len() > s.limit() {
                let why = format!(
                    "the server's journal brings {} operations, more than taking the whole tree costs",
                    s.journal.len()
                );
                decline(&root, &mut session, &mut whole, &mut report, why)?;
            }
        }

        // An incremental round decides everything up to the merge on the part of the tree it takes.
        let mut part: Option<Ready> = None;
        let mut again = false;
        while let Some(s) = session.as_mut() {
            let first = report.rounds == 1
                && (crate::steady::oracle() || root.state.generation()? == generation);
            let why = match incremental_head(
                &mut root,
                s,
                &mut selection,
                &accounts,
                &date,
                &seen.commits,
                seq,
                &mut report,
                &mut trace,
                &mut round_work,
                first,
                (check, again),
                (scope.as_ref(), &widened),
            )? {
                Ok(r) => {
                    part = Some(r);
                    break;
                }
                Err(Declined::Again) if !again => {
                    again = true;
                    continue;
                }
                Err(Declined::Widen(more)) => {
                    if scope_widenings < SCOPE_WIDENINGS {
                        scope_widenings += 1;
                        for (path, whole) in more {
                            *widened.entry(path).or_insert(false) |= whole;
                        }
                    } else {
                        // Past this, the whole tree costs less than more rounds of widening.
                        scope = None;
                    }
                    continue;
                }
                Err(Declined::Rebuild(why)) if s.rebuilt.is_none() => {
                    hit("sync.incr.rebuilding");
                    s.rebuild(&root, &selection, why.clone())?;
                    report.rebuilt_because = Some(why);
                    continue;
                }
                Err(Declined::Again) => "the check kept finding defects".to_string(),
                Err(Declined::Whole(why) | Declined::Rebuild(why)) => why,
            };
            decline(&root, &mut session, &mut whole, &mut report, why)?;
        }
        let p_base: Tree;
        let p_remote: Tree;
        let mut p_rows: BTreeMap<ObjectId, Row>;
        let p_all: BTreeSet<ObjectId>;
        let p_dirty: BTreeSet<ObjectId>;
        let mut p_oracle: Option<crate::incr::FullRound> = None;
        let base: &Tree;
        let remote: &Tree;
        let rows: &mut BTreeMap<ObjectId, Row>;
        let mut view: crate::selection::View<'_>;
        let mut scanned: ScanResult;
        let mut identified: Identified;
        let plan: Plan;
        let unstable: bool;
        let seen_now: HashMap<ObjectId, Option<Hash>>;
        let mut held: Option<(&BTreeSet<ObjectId>, &BTreeSet<ObjectId>)> = None;
        let thinking;
        if let Some(r) = part {
            if !opts.allow_mass_delete {
                if let Some(why) = r.vanished {
                    let report = SyncReport {
                        outcome: SyncOutcome::Paused(why),
                        ..report
                    };
                    return Ok((report, trace));
                }
            }
            p_base = r.base;
            p_remote = r.remote;
            p_rows = r.rows;
            p_all = r.all;
            p_dirty = r.dirty;
            p_oracle = r.oracle;
            base = &p_base;
            remote = &p_remote;
            rows = &mut p_rows;
            held = Some((&p_all, &p_dirty));
            view = r.view;
            scanned = r.scanned;
            identified = r.identified;
            plan = r.plan;
            unstable = r.unstable;
            seen_now = r.seen;
            retryable = r.retryable;
            report.looked_in = scanned
                .listed
                .as_ref()
                .map(|l| l.iter().map(|p| crate::scan::display_path(p)).collect());
            thinking = std::time::Instant::now();
        } else {
            report.looked_in = None;
            let Some(Whole {
                base: w_base,
                rows: w_rows,
                remote: w_remote,
            }) = whole.as_mut()
            else {
                bail!("a round with neither the ledger nor the whole tree");
            };
            base = &*w_base;
            remote = w_remote.as_ref().unwrap_or(&*w_base);
            rows = w_rows;
            if selection.recheck_unheld(&root.dir, remote) && !opts.dry_run {
                selection.store(&root)?;
            }
            // From here to the push, everything sees only what this device takes part in. `base` and
            // `remote` stay the real record, for the cursor and for the next round.
            view = selection.view(base, remote, rows);
            {
                let list: Vec<(ObjectId, &Row)> =
                    view.changed.iter().map(|id| (*id, &rows[id])).collect();
                if !opts.dry_run && !(list.is_empty() && view.purge.is_empty()) {
                    root.state.store_rows(&list, &view.purge)?;
                }
            }
            report.not_synced_here = view
                .hidden
                .reserved
                .iter()
                .filter_map(|link| remote.path_of_link(link))
                .map(|path| crate::scan::display_path(&path))
                .collect();

            let moved = RefCell::new(Vec::new());
            let scan_started = std::time::Instant::now();
            scanned = {
                let by_inode: HashMap<(u64, u64), Known> = rows
                    .values()
                    .map(|r| ((r.fp.dev, r.fp.ino), r.known()))
                    .collect();
                scan_stable(
                    &mut root,
                    &by_inode,
                    Some((&*rows, (base, remote))),
                    &accounts,
                    !opts.dry_run,
                    &|path, st| {
                        selection.leaves_out(path, st, &view, rows, &mut moved.borrow_mut())
                    },
                    &date,
                    &mut report.events,
                    &mut report.conflicts,
                    None,
                )?
                .ok_or_else(|| anyhow!("a scan given the whole tree came back without one"))?
            };
            report.scan_time += scan_started.elapsed();
            round_work += scan_started.elapsed();
            // What a later sync with nothing to do would compare itself with, if this one turns out to be:
            // worked out beside the identification, which reads the same scan and rows, unless what this
            // scan rescued is about to change a row.
            let digest = report.rounds == 1
                && !opts.dry_run
                && (crate::steady::oracle() || root.state.generation()? == generation);
            let digest_beside = digest && scanned.rescued.is_empty();
            if digest && !digest_beside {
                trace.first = Some((crate::steady::digest_of(&scanned, rows), seq));
            }
            for id in &scanned.rescued {
                if let Some(row) = rows.get_mut(id) {
                    row.fp.dev = 0;
                    row.fp.ino = 0;
                }
            }
            if selection.follow_copies(&moved.into_inner()) && !opts.dry_run {
                selection.store(&root)?;
            }
            hit("sync.scanned");
            thinking = std::time::Instant::now();
            report.excluded_here = scanned
                .excluded
                .iter()
                .map(|path| crate::scan::display_path(path))
                .collect();
            root.state.put_manifests(&scanned.new_manifests)?;
            unstable = scanned.unstable;
            report.skips = scanned.skips.clone();
            report
                .skips
                .extend(selection.unheld.iter().map(|u| unheld_skip(remote, u)));
            retryable = report.skips.iter().any(|s| s.kind.retryable()) || unstable;

            let intents: Vec<CopyIntent> = root
                .state
                .intents::<CopyIntent>(INTENT_COPY)?
                .into_iter()
                .map(|(_, i)| i)
                .collect();
            let mut arriving: BTreeMap<ObjectId, Arriving> = root
                .state
                .intents::<Arriving>(INTENT_ARRIVING)?
                .into_iter()
                .map(|(_, a)| (a.object, a))
                .collect();
            let ids = crate::incr::Ids::for_round();
            identified = std::thread::scope(|s| {
                let digest =
                    digest_beside.then(|| s.spawn(|| crate::steady::digest_of(&scanned, rows)));
                let identified = identify(
                    &scanned,
                    &view.base,
                    &view.remote,
                    rows,
                    &intents,
                    &arriving,
                    &seen.commits,
                    &|n| ids.of_scanned(n),
                );
                if let Some(d) = digest {
                    let d = d.join().unwrap_or_else(|e| std::panic::resume_unwind(e));
                    trace.first = Some((d, seq));
                }
                identified
            });
            // In a tree still moving, what was seen was seen, and only what was not is asked again.
            if !arriving.is_empty() {
                let holds =
                    what_arrives_here(&root, &accounts, &arriving, &identified, opts.dry_run)?;
                let (changed, answered) =
                    settle_arriving(&identified, &arriving, &holds, &view.base, rows);
                if !opts.dry_run {
                    let list: Vec<(ObjectId, &Row)> =
                        changed.iter().map(|id| (*id, &rows[id])).collect();
                    let marks: Vec<String> = answered
                        .iter()
                        .map(|id| format!("{INTENT_ARRIVING}{id}"))
                        .collect();
                    root.state.store_rows_dropping(&list, &marks)?;
                }
                hit("sync.arrivals_settled");
                for id in &answered {
                    arriving.remove(id);
                }
                identified = identify(
                    &scanned,
                    &view.base,
                    &view.remote,
                    rows,
                    &intents,
                    &arriving,
                    &seen.commits,
                    &|n| ids.of_scanned(n),
                );
            }
            // Every node is in `identified` now, and the scan's own copy of the tree can go.
            scanned.nodes = Vec::new();
            scanned.new_manifests = Vec::new();
            update_rows(&mut root, rows, &identified, &view.base)?;
            // Found here means held, however that came about. Not found, and given up, is no delete.
            view.settle(rows);
            hit("sync.identified");

            if !opts.allow_mass_delete {
                if let Some(why) =
                    check_vanished(&view.base, &view.remote, rows, &identified.local, 0)
                {
                    let report = SyncReport {
                        outcome: SyncOutcome::Paused(why),
                        ..report
                    };
                    return Ok((report, trace));
                }
            }

            // A name the server has in another form goes up as a second name, unless it waits here.
            report.skips.extend(crate::twins::hold_back_twins(
                &root,
                &mut identified,
                base,
                remote,
            ));
            seen_now = crate::incr::seen_of(&identified.scanned);

            let host = root.config.device_name.clone();
            plan = merge(MergeInput {
                base: &view.base,
                local: &identified.local,
                remote: &view.remote,
                shared: &identified.shared,
                hidden: &view.hidden,
                host: &host,
                date: &date,
                fresh_id: &mut ids.counter(),
            });
        }
        round_work += thinking.elapsed();
        for r in &plan.reports {
            // A new object kept beside the server's at a name both took is a conflict copy too.
            let beside = r.kind == ReportKind::SameNameKeptBoth
                && plan
                    .target
                    .get(&r.object)
                    .is_some_and(|t| t.conflict_of.is_some());
            if r.kind == ReportKind::ConflictCopy || beside {
                report.conflicts += 1;
            }
            let line = format!("{}: {}", event_path(&plan, r.object), r.detail);
            if seen_events.insert(line.clone()) {
                report.events.push(line);
            }
        }

        if opts.dry_run {
            let cur = cur_from_identified(&plan.local, &identified.scanned, &plan.adopted);
            let from: Topo = cur
                .iter()
                .filter(|(i, _)| **i != ObjectId::ROOT)
                .map(|(i, c)| (*i, (c.kind, c.links.clone())))
                .collect();
            let to: Topo = plan
                .target
                .iter()
                .filter(|(i, _)| **i != ObjectId::ROOT)
                .map(|(i, t)| (*i, (t.body.content.kind(), t.links.clone())))
                .collect();
            for step in schedule(&from, &to)? {
                report.planned.push(format!("local: {step:?}"));
            }
            for (id, t) in &plan.target {
                if cur.get(id).is_some_and(|c| c.content != t.body.content) {
                    report
                        .planned
                        .push(format!("local: new content for {}", event_path(&plan, *id)));
                }
            }
            for op in remote_ops(&view.remote, &plan)? {
                report.planned.push(format!("server: {}", describe_op(&op)));
            }
            break SyncOutcome::Synced;
        }

        // Before anything moves, so that dying in between cannot let it go up.
        let kept_out = selection.keep_out_what_moves(
            &root,
            &scanned.excluded,
            &folders_moving(&plan, &identified),
            rows,
        );
        if !kept_out.is_empty() {
            selection.store(&root)?;
            for path in &kept_out {
                report.events.push(format!(
                    "{}: its folder is moving, and the rule that leaves it out would not match there; it stays out, and `cairn include --merge` brings it in",
                    crate::scan::display_path(path)
                ));
            }
        }

        adopt_rows(&mut root, rows, &plan.adopted, &view.base, &view.remote)?;
        let cur = cur_from_identified(&plan.local, &identified.scanned, &plan.adopted);
        // What the merge was given has served: from here on the plan and the disk say it all.
        drop(identified);
        let arriving = down_activity(&cur, &plan);
        let applying = std::time::Instant::now();
        let (outcome, cur) =
            Applier::new(&mut root, Some(&client), &accounts, cur, rows, &view.remote)
                .run(&plan)?;
        if !outcome.touched && outcome.applied_steps == 0 {
            round_work += applying.elapsed();
        }
        hit("sync.applied");
        report.steps_applied += outcome.applied_steps;
        // What did not get there is no change made, and comes again in a later round.
        if !outcome.halted {
            let missed: BTreeSet<&str> = outcome
                .failed
                .iter()
                .chain(&outcome.changed)
                .map(|s| s.path.as_str())
                .collect();
            report.activity.extend(
                arriving
                    .into_iter()
                    .filter(|a| !missed.contains(a.path.as_str())),
            );
        }
        for line in gone_lines(&outcome.gone) {
            if seen_events.insert(line.clone()) {
                report.events.push(line);
            }
        }
        if !outcome.unholdable.is_empty() {
            for (unheld, _) in &outcome.unholdable {
                report.skips.push(unheld_skip(remote, unheld));
            }
            selection
                .unheld
                .extend(outcome.unholdable.into_iter().map(|(u, _)| u));
            // Out of view from the next round on, and for every later sync.
            selection.store(&root)?;
        }
        report.meta_applied.extend(outcome.meta_applied);
        for w in outcome.warnings {
            if !report.warnings.contains(&w) {
                report.warnings.push(w);
            }
        }
        if outcome.halted || (!outcome.changed.is_empty() && report.rounds < ROUNDS_BEFORE_PUSHING)
        {
            report.skips.extend(outcome.failed);
            report.skips.extend(outcome.changed);
            retryable = true;
            journal = Journal::Pinned;
            continue;
        }
        let failed = !outcome.failed.is_empty() || !outcome.changed.is_empty();
        report.skips.extend(outcome.failed);
        // Still in the way after several tries: not a change settling but something standing here, a
        // FIFO at a name that arrived, say. The rest of the tree must not wait for it.
        report
            .skips
            .extend(outcome.changed.into_iter().map(|mut s| {
                s.kind = if report.excluded_here.contains(&s.path) {
                    cairn_protocol::SkipKind::ExcludedInTheWay
                } else {
                    cairn_protocol::SkipKind::CannotApply
                };
                s
            }));

        let ordering = std::time::Instant::now();
        let mut ops = remote_ops(&view.remote, &plan)?;
        round_work += ordering.elapsed();
        // The loser of a metadata conflict goes in the log even when nothing else is to be pushed.
        // Unless applying failed: the conflict stands, and would be logged on every run. A run killed
        // after applying the winner left its notes noted, and they go now.
        if !failed || !ops.is_empty() {
            for note in &plan.notes {
                if noted.insert(note.clone()) {
                    notes.push(note.clone());
                }
            }
            for (_, n) in root.state.intents::<NoteIntent>(INTENT_NOTE)? {
                let pair = (n.object, n.text);
                if remote.nodes.contains_key(&pair.0) && noted.insert(pair.clone()) {
                    notes.push(pair);
                }
            }
        }
        ops.extend(notes.iter().map(|(object, text)| Op::Note {
            object: *object,
            text: text.clone(),
        }));
        if ops.is_empty() {
            if failed {
                break SyncOutcome::Partial;
            }
            if outcome.touched {
                // The filesystem was written: confirm it with a fresh scan before calling this agreed,
                // against the same sequence, as a busy device elsewhere may commit within every round.
                journal = Journal::Pinned;
                continue;
            }
            // Out of view here: what the ledger keeps with no scan of it.
            let hidden: BTreeSet<ObjectId> = if std::ptr::eq(view.remote.as_ref(), remote) {
                BTreeSet::new()
            } else {
                remote
                    .live()
                    .map(|(id, _)| id)
                    .filter(|id| !view.remote.is_live(*id))
                    .collect()
            };
            let anchor = crate::incr::Anchor {
                format: crate::incr::FORMAT,
                inputs: crate::incr::inputs(&root, &selection),
                reserved: view.hidden.reserved.clone(),
            };
            drop(view);
            let finalizing = std::time::Instant::now();
            let part = match held {
                Some((all, dirty)) => Some((
                    session
                        .as_ref()
                        .ok_or_else(|| anyhow!("an incremental round without its session"))?,
                    all,
                    dirty,
                )),
                None => None,
            };
            finalize(
                &mut root,
                base,
                remote,
                seq,
                seen.last,
                rows,
                &plan,
                Ledgering {
                    seen: &seen_now,
                    hidden: &hidden,
                    anchor,
                    part,
                },
            )?;
            round_work += finalizing.elapsed();
            hit("sync.finalized");
            if let Some(o) = &p_oracle {
                crate::incr::check_finalized(&root, o)?;
                hit("sync.incr.finalize_confirmed");
            }
            report.agreed_since = Some(scanned.started);
            steady_round = report.rounds == 1 && !unstable && !recovered;
            break if unstable {
                SyncOutcome::Partial
            } else {
                SyncOutcome::Synced
            };
        }
        // A huge change goes in several commits, one after another from this one plan: each leaves a
        // valid tree, and the server checks every operation against what is there by then, so one
        // that something else has overtaken is refused, and the next round merges again. What the
        // filesystem does meanwhile is for that round too, which compares the disk with this scan.
        // The notes go last, so each commit's are the first of those still to go.
        let mut landed: Tree = view.remote.clone().into_owned();
        // Nothing went up: the next round looks again against the same sequence.
        journal = Journal::Pinned;
        for ops in commits(ops) {
            match upload(&client, &root, &cur, &ops)? {
                Upload::FileChanged => {
                    retryable = true;
                    break;
                }
                Upload::Done { chunks, bytes } => {
                    report.chunks_uploaded += chunks;
                    report.bytes_uploaded += bytes;
                }
            }
            hit("sync.uploaded");

            let req = CommitRequest {
                op_id: OpId::random(),
                device: root.config.device,
                device_name: root.config.device_name.clone(),
                ops,
            };
            let pushed = mark_synced(rows, &mut landed, &cur, &req);
            let list: Vec<(ObjectId, &Row)> = pushed.iter().map(|id| (*id, &rows[id])).collect();
            // The queue delivers the notes in it from here on.
            let delivered: Vec<String> = req
                .ops
                .iter()
                .filter_map(|op| match op {
                    Op::Note { object, text } => Some(
                        NoteIntent {
                            object: *object,
                            text: text.clone(),
                        }
                        .key(),
                    ),
                    _ => None,
                })
                .collect();
            root.state.enqueue(&req, &list, &delivered)?;
            hit("sync.enqueued");
            let resp = client.commit(&req)?;
            hit("sync.committed");
            // Before the commit leaves the queue, so that dying here costs nothing.
            if matches!(resp, CommitResponse::Rejected { .. }) {
                notes_back(&mut root, &req)?;
                let changed = take_back_refused(&req, rows);
                let list: Vec<(ObjectId, &Row)> =
                    changed.iter().map(|id| (*id, &rows[id])).collect();
                root.state.store_rows(&list, &[])?;
            }
            root.state.dequeue(&req.op_id)?;
            hit("sync.dequeued");
            match resp {
                CommitResponse::Committed { .. } => {
                    // The next round reads the journal again, to see this commit there.
                    journal = Journal::Head;
                    notes.drain(..delivered.len());
                    report.commits += 1;
                    report.ops_pushed += req.ops.len() as u64;
                    report
                        .activity
                        .extend(up_activity(&view.remote, &plan, &req.ops));
                }
                CommitResponse::Rejected { reject } => {
                    // Someone else got in first, or a chunk has gone: the next round sees why.
                    let line =
                        format!("the server refused a commit and this client rebased: {reject:?}");
                    if seen_events.insert(line.clone()) {
                        report.events.push(line);
                    }
                    // Someone got there first: what they committed is the next round's to see.
                    journal = Journal::Head;
                    break;
                }
            }
        }
        hit("sync.pushed");
        if failed {
            break SyncOutcome::Partial;
        }
    };

    report.outcome = match finish {
        SyncOutcome::Synced if retryable => SyncOutcome::Partial,
        other => other,
    };
    // Every decision the full sync's: the last round took the whole tree, which covers whatever the
    // rounds before it decided, or a ledger rebuilt from the state, or this was the check, which took up
    // whatever it found the ledger had missed. Not on every sync the harness's oracle confirms, as the
    // write would cost the next sync with nothing to do its shortcut.
    if report.outcome == SyncOutcome::Synced
        && !opts.dry_run
        && (session
            .as_ref()
            .is_none_or(|s| s.rebuilt.is_some() || check))
    {
        root.set_full_checked(now_secs())?;
    }
    report.repeated = loaded + round_work;
    // Nothing to do, and nothing written: the next sync that sees the same may say so at once.
    if let (true, Some(g), Some((scan, _))) = (steady_round, generation, trace.first) {
        if root.state.generation()? == Some(g) {
            let record = crate::steady::Steady::new(
                g,
                inputs,
                seq,
                seen.last,
                scan,
                crate::steady::Recorded::of(&report),
            );
            crate::steady::store(&root, &record)?;
            hit("sync.steady.recorded");
            trace.steady = Some(record);
        }
    }
    Ok((report, trace))
}
/// The base and rows whole, for rounds that take the whole tree, and the server's tree once the journal
/// has brought something: until then, the base.
struct Whole {
    base: Tree,
    rows: BTreeMap<ObjectId, Row>,
    remote: Option<Tree>,
}

impl Whole {
    fn load(root: &Root, journal: &[Op]) -> Result<Whole> {
        let mut w = Whole {
            base: root.state.load_base()?,
            rows: root.state.load_rows()?,
            remote: None,
        };
        for op in journal {
            w.apply(op)?;
        }
        Ok(w)
    }

    fn apply(&mut self, op: &Op) -> Result<()> {
        let Whole { base, remote, .. } = self;
        remote
            .get_or_insert_with(|| base.clone())
            .apply(op)
            .map_err(anyhow::Error::from)
    }
}

/// From this round on the whole tree, and the report says why.
fn decline(
    root: &Root,
    session: &mut Option<crate::incr::Session>,
    whole: &mut Option<Whole>,
    report: &mut SyncReport,
    why: String,
) -> Result<()> {
    hit("sync.incr.declined");
    let journal = session.take().map(|s| s.journal.ops).unwrap_or_default();
    *whole = Some(Whole::load(root, &journal)?);
    report.whole_because = Some(why);
    Ok(())
}

/// What an incremental round hands on to the rest of the round: the part of the tree it took, decided.
struct Ready {
    base: Tree,
    remote: Tree,
    rows: BTreeMap<ObjectId, Row>,
    view: crate::selection::View<'static>,
    scanned: ScanResult,
    identified: Identified,
    plan: Plan,
    unstable: bool,
    retryable: bool,
    vanished: Option<String>,
    seen: HashMap<ObjectId, Option<Hash>>,
    all: BTreeSet<ObjectId>,
    dirty: BTreeSet<ObjectId>,
    oracle: Option<crate::incr::FullRound>,
}

fn owned(v: crate::selection::View<'_>) -> crate::selection::View<'static> {
    crate::selection::View {
        base: Cow::Owned(v.base.into_owned()),
        remote: Cow::Owned(v.remote.into_owned()),
        hidden: v.hidden,
        purge: v.purge,
        changed: v.changed,
    }
}

/// The steps that bring what is here to the plan, as the apply orders them.
fn local_steps(plan: &Plan, identified: &Identified) -> Result<Vec<TopoOp>> {
    let cur = cur_from_identified(&plan.local, &identified.scanned, &plan.adopted);
    let from: Topo = cur
        .iter()
        .filter(|(i, _)| **i != ObjectId::ROOT)
        .map(|(i, c)| (*i, (c.kind, c.links.clone())))
        .collect();
    let to: Topo = plan
        .target
        .iter()
        .filter(|(i, _)| **i != ObjectId::ROOT)
        .map(|(i, t)| (*i, (t.body.content.kind(), t.links.clone())))
        .collect();
    schedule(&from, &to).context("ordering local changes")
}

/// The rows the arrivals settled, and the marks they answered.
type Settled = (Vec<(ObjectId, Row)>, Vec<String>);

/// Rounds of widening before a round gives up on what changed alone.
const WIDENINGS: usize = 4;

/// Every row's view for the scan, from a glance at each, but those the selection's view takes: on two
/// rows at one inode, the later object's.
fn glanced<'g>(
    g: &'g [(ObjectId, crate::state::Glance)],
    purged: &BTreeSet<ObjectId>,
) -> HashMap<(u64, u64), Known<'g>> {
    g.iter()
        .filter(|(id, _)| !purged.contains(id))
        .map(|(_, g)| ((g.fp.dev, g.fp.ino), g.known()))
        .collect()
}

/// Why a round on what changed alone cannot go on.
enum Declined {
    /// The whole tree is the round's to take, and why.
    Whole(String),
    /// The tree moved under the scan, or identification went otherwise than the ledger: the ledger
    /// rebuilt from the state, with a new scan, may still take what changed alone.
    Rebuild(String),
    /// The check found what the ledger vouched for and should not have, which is now marked as
    /// written since: the round again, with a new scan that reads it as the state says.
    Again,
    /// The round's decisions reach folders its scope did not list: the round again, listing these too.
    Widen(crate::scope::Folders),
}

/// A round on what changed alone, up to the merge: the scan, then the part of the tree the ledger cannot
/// vouch for and its neighbours, identified and merged by the full sync's own code; the first round of a
/// session that rebuilds its ledger rebuilds it from the state and this scan. `Err` says why the round
/// cannot go on; nothing is written before the round knows it can, but for the marks a check leaves.
#[allow(clippy::too_many_arguments)]
fn incremental_head(
    root: &mut Root,
    session: &mut crate::incr::Session,
    selection: &mut Selection,
    accounts: &Accounts,
    date: &str,
    landed: &BTreeSet<OpId>,
    seq: u64,
    report: &mut SyncReport,
    trace: &mut crate::steady::Trace,
    round_work: &mut std::time::Duration,
    first: bool,
    (check, again): (bool, bool),
    (scope, widened): (Option<&crate::scope::Source>, &crate::scope::Folders),
) -> Result<std::result::Result<Ready, Declined>> {
    use crate::incr::{self, Before};
    // The state as the full sync would read it, for the oracle, before this round writes anything.
    let snapshot = match incr::oracle() {
        true => Some((
            root.state.load_base()?,
            root.state.load_rows()?,
            selection.clone(),
        )),
        false => None,
    };
    let rebuilding = session.to_rebuild;
    // What the ledger does not speak for: what was written since it was, and what a rebuild of it found
    // unsettled. A rebuild starts from the state itself, and finds both again.
    let (mut dirty, mut dirty_nodes, mut dirty_rows) = match rebuilding {
        true => (BTreeSet::new(), BTreeMap::new(), BTreeMap::new()),
        false => {
            let mut dirty = root.state.dirty()?;
            dirty.extend(session.unsettled.iter().copied());
            let dirty_list: Vec<ObjectId> = dirty.iter().copied().collect();
            let nodes = root.state.nodes_of(&dirty_list)?;
            let rows = root.state.rows_of(&dirty_list)?;
            (dirty, nodes, rows)
        }
    };
    let mut released: BTreeSet<Hash> = BTreeSet::new();
    let hold = |rows: &BTreeMap<ObjectId, Row>, released: &mut BTreeSet<Hash>| {
        for r in rows.values() {
            if let Content::File { manifest, .. } = &r.content {
                released.insert(*manifest);
            }
        }
    };
    // Every row as a scan needs it, for a rebuild: the scan is given what the state says, not the ledger.
    let mut glance = match rebuilding {
        true => Some(root.state.glance_rows()?),
        false => None,
    };
    // For a rebuild, the base as a skeleton, what of it is in view here, and the rows the selection's
    // view takes, which the scan is not given, as the full sync's is not.
    let mut purged: BTreeSet<ObjectId> = BTreeSet::new();
    let mut skeleton = None;
    if let Some(g) = &glance {
        let sk = crate::rebuild::skeleton(root)?;
        let vis = crate::rebuild::visible(&sk, selection);
        match crate::rebuild::purged(&sk, selection, g, session.journal.moves_any()) {
            Some(p) => purged = p,
            None => {
                return Ok(Err(Declined::Whole(
                    "records here go out of view while the server moves things, which may bring them back".into(),
                )))
            }
        }
        skeleton = Some((sk, vis));
    }

    // What this sync's own earlier rounds marked: read as the full round reads them, and decided here.
    let intents: Vec<CopyIntent> = root
        .state
        .intents::<CopyIntent>(INTENT_COPY)?
        .into_iter()
        .map(|(_, i)| i)
        .collect();
    let arriving_marks: BTreeMap<ObjectId, Arriving> = root
        .state
        .intents::<Arriving>(INTENT_ARRIVING)?
        .into_iter()
        .map(|(_, a)| (a.object, a))
        .collect();
    let mut marked: BTreeSet<ObjectId> = BTreeSet::new();
    let mut marked_inodes: BTreeSet<(u64, u64)> = BTreeSet::new();
    for i in &intents {
        marked.insert(i.copy);
        marked.insert(i.conflict_of.object);
        marked_inodes.insert((i.dev, i.ino));
    }
    marked.extend(arriving_marks.keys().copied());
    for (_, n) in root.state.intents::<NoteIntent>(INTENT_NOTE)? {
        marked.insert(n.object);
    }
    let checking = check && session.rebuilt.is_none();
    let scan_started = std::time::Instant::now();
    let scanned = {
        let before = Before {
            ledger: &session.ledger,
            dirty: &dirty,
            dirty_nodes: &dirty_nodes,
            dirty_rows: &dirty_rows,
            journal: &session.journal,
            glanced: rebuilding,
            unscanned: &purged,
        };
        let known = match &glance {
            Some(g) => glanced(g, &purged),
            None => before.scan_rows(),
        };
        // Where the watcher saw something, and what this round must see besides; the rest of the tree
        // as the ledger has it. Not for a rebuild, which reads every row, nor for the check, which is
        // there to find what the bookkeeping missed.
        let marks = match (scope, rebuilding || checking) {
            (Some(source), false) => source.take(),
            _ => None,
        };
        let named: BTreeSet<ObjectId> = selection
            .unheld
            .iter()
            .map(|u| u.object)
            .chain(marked.iter().copied())
            .chain(
                session
                    .anchor
                    .reserved
                    .iter()
                    .filter_map(|l| session.ledger.children_named(&l.parent, &l.name)),
            )
            .collect();
        let word = marks
            .as_ref()
            .map(|_| incr::Word::new(&before, &named, !selection.rules.is_empty()));
        let list: Option<crate::scope::Folders> = marks.map(|mut list| {
            let mut add = |path: &Vec<Name>, whole: bool| {
                *list.entry(path.clone()).or_insert(false) |= whole;
            };
            for (p, w) in word.iter().flat_map(|w| w.folders.iter()).chain(widened) {
                add(p, *w);
            }
            for l in &session.anchor.reserved {
                if let Some(path) = word.as_ref().and_then(|w| w.path(&l.parent)) {
                    add(&path, false);
                }
            }
            list
        });
        let scoped = match (&list, &word) {
            (Some(list), Some(word)) => Some(crate::scan::Scope { list, vouch: word }),
            _ => None,
        };
        if scoped.is_some() {
            hit("sync.scoped");
        }
        let scanned = scan_stable(
            root,
            &known,
            None,
            accounts,
            true,
            &|path, st| selection.leaves_out_roughly(path, st),
            date,
            &mut report.events,
            &mut report.conflicts,
            scoped.as_ref().map(|s| (s, scan_started)),
        )?;
        let Some(scanned) = scanned else {
            return Ok(Err(Declined::Whole(
                "leftovers of a run that did not finish are the full sync's to settle".into(),
            )));
        };
        report.scan_time += scan_started.elapsed();
        *round_work += scan_started.elapsed();
        // A scan that took part of the tree from the ledger is no record of the tree.
        if first && scanned.listed.is_none() {
            trace.first = Some((crate::steady::digest(&scanned, &known), seq));
        }
        scanned
    };
    hit("sync.scanned");
    if let Some(path) = incr::inode_twice(&scanned) {
        return Ok(Err(Declined::Rebuild(format!(
            "the scan found {path} at an inode it had found elsewhere, as when something moves while it walks"
        ))));
    }
    if rebuilding {
        let rebuild_started = std::time::Instant::now();
        let provisional = crate::rebuild::provisional(
            glance.as_deref().unwrap_or_default(),
            &purged,
            &scanned.nodes,
        );
        // The oracle compares the scan's rows with the state's; nothing else needs them now.
        if !incr::oracle() {
            glance = None;
        }
        let Some(skeleton) = skeleton.take() else {
            bail!("a rebuild with no skeleton of the base");
        };
        match crate::rebuild::rebuild(
            root,
            skeleton,
            &scanned.nodes,
            &provisional,
            session.limit(),
        )? {
            Err(why) => return Ok(Err(Declined::Whole(why))),
            Ok(r) => {
                hit("sync.incr.rebuilt");
                session.ledger = r.ledger;
                session.anchor.reserved = r.reserved;
                session.size = session.ledger.len() + r.unsettled.len();
                session.unsettled = r.unsettled.clone();
                session.to_rebuild = false;
                dirty = r.unsettled;
                dirty_nodes = r.nodes;
                dirty_rows = r.rows;
            }
        }
        *round_work += rebuild_started.elapsed();
    }
    hold(&dirty_rows, &mut released);
    let before = Before {
        ledger: &session.ledger,
        dirty: &dirty,
        dirty_nodes: &dirty_nodes,
        dirty_rows: &dirty_rows,
        journal: &session.journal,
        glanced: rebuilding,
        unscanned: &purged,
    };
    let thinking = std::time::Instant::now();
    report.excluded_here = scanned
        .excluded
        .iter()
        .map(|path| crate::scan::display_path(path))
        .collect();
    root.state.put_manifests(&scanned.new_manifests)?;
    released.extend(scanned.new_manifests.iter().map(|(h, _)| *h));
    let unstable = scanned.unstable;

    let mut c = incr::classify(&before, &scanned);
    for (i, n) in scanned.nodes.iter().enumerate() {
        if c.quiet[i] && marked_inodes.contains(&(n.fp.dev, n.fp.ino)) {
            c.unquiet(i);
        }
    }
    let rules = !selection.rules.is_empty();
    let limit = session.limit();
    let unheld: BTreeSet<ObjectId> = selection
        .unheld
        .iter()
        .map(|u| u.object)
        .chain(marked.iter().copied())
        .collect();
    // What was out of view at the last agreement stays in sight, with its folder, so that what the
    // round says is out of view here is all of it.
    let mut beside: BTreeSet<ObjectId> = BTreeSet::new();
    for l in &session.anchor.reserved {
        beside.insert(l.parent);
        if let Some(x) = session
            .ledger
            .children_named(&l.parent, &l.name)
            .filter(|x| !dirty.contains(x))
        {
            beside.insert(x);
        }
    }
    // Where what is excluded here is: a folder above it that moves may take it where its rule does
    // not match, which the round must see.
    for path in &scanned.excluded {
        if let Some(x) = c.folder_of(path) {
            beside.insert(x);
        }
    }
    let ids = incr::Ids::for_round();
    let host = root.config.device_name.clone();
    let mut promoted: BTreeSet<ObjectId> = BTreeSet::new();
    // The check of the ledger kept: the state's own word on each object it vouches for, set against it.
    // In a harness build on every round whose ledger was not rebuilt just now, where any difference is a
    // failure; in the daemon's check, a defect said in the log, and the round again with what the
    // ledger missed taken as written since. Not of what the scan took from the ledger itself.
    if checking || (incr::oracle() && !rebuilding) {
        let skip = |id: &ObjectId| {
            *id == ObjectId::ROOT
                || c.seeds.contains(id)
                || unheld.contains(id)
                || c.node_of.get(id).is_some_and(|&i| scanned.vouched(i))
        };
        let places = crate::rebuild::Places {
            nodes: &scanned.nodes,
            dir_at: &c.dir_at,
            provisional: &c.provisional,
            node_of: &c.node_of,
        };
        let defects = crate::rebuild::check(root, &session.ledger, selection, &places, &skip)?;
        if defects.is_empty() {
            if checking {
                report.checked.get_or_insert(true);
            }
        } else {
            let ids: Vec<ObjectId> = defects.iter().map(|d| d.object).collect();
            let paths = crate::rebuild::paths_of(root, &ids)?;
            let mut lines: Vec<String> = defects
                .iter()
                .take(20)
                .map(|d| defect(&paths[&d.object], &d.what))
                .collect();
            if defects.len() > lines.len() {
                lines.push(format!(
                    "and {} more such defects, taken up the same way",
                    defects.len() - lines.len()
                ));
            }
            if !checking {
                bail!(
                    "the ledger vouches for what the state says is not settled:\n{}",
                    lines.join("\n")
                );
            }
            report.events.extend(lines);
            report.checked = Some(false);
            hit("sync.incr.defect");
            if !again {
                root.state.mark_dirty(&ids)?;
                return Ok(Err(Declined::Again));
            }
            promoted.extend(ids);
        }
    }
    for _ in 0..WIDENINGS {
        let also: BTreeSet<ObjectId> = unheld.union(&promoted).copied().collect();
        let reach = incr::reach(&before, &c, rules, &also, &beside);
        if reach.all.len() > limit {
            return Ok(Err(Declined::Whole(format!(
                "{} objects changed or are next to a change, more than taking the whole tree costs",
                reach.all.len()
            ))));
        }
        // Nothing the round loads may be the ledger's word alone, and every folder whose every name it
        // looks at must have been listed: otherwise the round again, listing those too.
        if let Some(listed) = &scanned.listed {
            let mut demand = crate::scope::Folders::new();
            for &i in &reach.nodes {
                if scanned.vouched(i) {
                    for p in &scanned.nodes[i].paths {
                        if let Some((_, folder)) = p.split_last() {
                            demand.insert(folder.to_vec(), false);
                        }
                    }
                }
            }
            for o in &reach.opened {
                let path = match *o == ObjectId::ROOT {
                    true => Some(Vec::new()),
                    false => c
                        .node_of
                        .get(o)
                        .and_then(|&i| scanned.nodes[i].paths.first().cloned()),
                };
                if let Some(path) = path.filter(|p| !listed.contains(p)) {
                    demand.insert(path, false);
                }
            }
            if !demand.is_empty() {
                hit("sync.scope.widened");
                return Ok(Err(Declined::Widen(demand)));
            }
        }
        let world = match incr::world(root, &before, &scanned, &reach)? {
            Ok(w) => w,
            Err(why) => return Ok(Err(Declined::Whole(why))),
        };
        let incr::World {
            base,
            remote,
            mut rows,
            scan,
        } = world;
        let loaded = rows.clone();
        let mut sel = selection.clone();
        let unheld_changed = sel.recheck_unheld(&root.dir, &remote);
        let mut view = owned(sel.view(&base, &remote, &mut rows));
        let viewed: Vec<(ObjectId, Row)> = view
            .changed
            .iter()
            .map(|id| (*id, rows[id].clone()))
            .collect();
        let mut arriving = arriving_marks.clone();
        let mut identified = identify(
            &scan,
            &view.base,
            &view.remote,
            &rows,
            &intents,
            &arriving,
            landed,
            &|n| ids.of_scanned(n),
        );
        // In a tree still moving, what was seen was seen, and only what was not is asked again.
        let mut settled: Option<Settled> = None;
        if !arriving.is_empty() {
            let holds = what_arrives_here(root, accounts, &arriving, &identified, false)?;
            let (changed, answered) =
                settle_arriving(&identified, &arriving, &holds, &view.base, &mut rows);
            settled = Some((
                changed.iter().map(|id| (*id, rows[id].clone())).collect(),
                answered
                    .iter()
                    .map(|id| format!("{INTENT_ARRIVING}{id}"))
                    .collect(),
            ));
            for id in &answered {
                arriving.remove(id);
            }
            identified = identify(
                &scan,
                &view.base,
                &view.remote,
                &rows,
                &intents,
                &arriving,
                landed,
                &|n| ids.of_scanned(n),
            );
        }
        if let Some(why) = c.misidentified(&reach, &identified) {
            return Ok(Err(Declined::Rebuild(why)));
        }
        let changed = rows_after_scan(&mut rows, &identified, &view.base);
        view.settle(&rows);
        let vanished = check_vanished(
            &view.base,
            &view.remote,
            &rows,
            &identified.local,
            c.quiet_outside(&reach),
        );
        let before_twins = identified.local.clone();
        let twins = crate::twins::hold_back_twins(root, &mut identified, &base, &remote);
        let plan = merge(MergeInput {
            base: &view.base,
            local: &identified.local,
            remote: &view.remote,
            shared: &identified.shared,
            hidden: &view.hidden,
            host: &host,
            date,
            fresh_id: &mut ids.counter(),
        });
        let decided = incr::decided_outside(
            &reach,
            &view.remote,
            &before_twins,
            &identified,
            &plan,
            &loaded,
            &rows,
        );
        if !decided.is_empty() {
            hit("sync.incr.widened");
            promoted.extend(decided);
            continue;
        }

        let oracle = match &snapshot {
            Some((sbase, srows, ssel)) => {
                let mine = incr::Decisions {
                    local: identified.local.clone(),
                    shared: identified.shared.clone(),
                    rows: rows.clone(),
                    vanished: vanished.clone(),
                    twins: twins.clone(),
                    target: plan.target.clone(),
                    copies: plan.copies.clone(),
                    adopted: plan.adopted.clone(),
                    reports: plan.reports.clone(),
                    notes: plan.notes.clone(),
                    ops: remote_ops(&view.remote, &plan)?,
                    steps: local_steps(&plan, &identified)?,
                    moving: moving_with_excluded(&plan, &identified, &scanned.excluded),
                    reserved: view.hidden.reserved.clone(),
                };
                // The rows the scan was given, again, as the scan had them.
                let known = match &glance {
                    Some(g) => glanced(g, &purged),
                    None => before.scan_rows(),
                };
                let full = oracle_full(
                    root,
                    (sbase, srows, ssel),
                    &session.journal.ops,
                    (&scanned, &session.ledger),
                    &known,
                    (&intents, &arriving_marks, accounts),
                    landed,
                    ids,
                    &host,
                    date,
                )?;
                // What the round made new is its own as much as what it loaded.
                let held: BTreeSet<ObjectId> = reach
                    .all
                    .iter()
                    .chain(mine.local.keys())
                    .chain(mine.rows.keys())
                    .chain(mine.target.keys())
                    .copied()
                    .collect();
                if let Err(diff) =
                    incr::compare(&mine, &full.decisions, &held, srows, &full.visible)
                {
                    // What the round knew of each object the full one decided about and it did not take.
                    let outside: Vec<String> = full
                        .decisions
                        .target
                        .keys()
                        .chain(full.decisions.rows.keys())
                        .filter(|id| !held.contains(id))
                        .take(8)
                        .map(|id| c.explain(&before, id, &reach))
                        .collect();
                    bail!(
                        "the incremental round and the full one on the same scan, journal and state decide differently:\n{diff}\nwhat the round knew of what it left out:\n{}",
                        outside.join("\n")
                    );
                }
                hit("sync.incr.confirmed");
                Some(incr::FullRound {
                    decisions: full.decisions,
                    remote: full.remote,
                    local: full.local,
                    seen: full.seen,
                    hidden: full.hidden,
                })
            }
            None => None,
        };

        // Decided, and within what was loaded for it: written as the full sync writes it.
        if unheld_changed {
            *selection = sel;
            selection.store(root)?;
        }
        {
            let list: Vec<(ObjectId, &Row)> = viewed.iter().map(|(id, r)| (*id, r)).collect();
            if !(list.is_empty() && view.purge.is_empty()) {
                root.state.store_rows(&list, &view.purge)?;
            }
        }
        report.not_synced_here = view
            .hidden
            .reserved
            .iter()
            .filter_map(|link| remote.path_of_link(link))
            .map(|path| crate::scan::display_path(&path))
            .collect();
        report.skips = scanned.skips.clone();
        report
            .skips
            .extend(selection.unheld.iter().map(|u| unheld_skip(&remote, u)));
        let retryable = report.skips.iter().any(|s| s.kind.retryable()) || unstable;
        if let Some((list, marks)) = &settled {
            let list: Vec<(ObjectId, &Row)> = list.iter().map(|(id, r)| (*id, r)).collect();
            root.state.store_rows_dropping(&list, marks)?;
            hit("sync.arrivals_settled");
        }
        {
            let list: Vec<(ObjectId, &Row)> = changed.iter().map(|id| (*id, &rows[id])).collect();
            root.state.store_rows(&list, &[])?;
        }
        hit("sync.identified");
        report.skips.extend(twins);
        hold(&loaded, &mut released);
        let seen = incr::seen_of(&identified.scanned);
        *round_work += thinking.elapsed();
        // The root is always among them, and never in the ledger.
        let (core, all, of) = (
            reach.core.len().saturating_sub(1) as u64,
            reach.all.len().saturating_sub(1) as u64,
            session.size as u64,
        );
        report.part = Some(match report.part {
            Some((c0, a0, _)) => (c0.max(core), a0.max(all), of),
            None => (core, all, of),
        });
        hit("sync.incr.round");
        let (all, scanned_part) = (reach.all, scan);
        session.released.extend(released);
        return Ok(Ok(Ready {
            base,
            remote,
            rows,
            view,
            scanned: scanned_part,
            identified,
            plan,
            unstable,
            retryable,
            vanished,
            seen,
            all,
            dirty,
            oracle,
        }));
    }
    Ok(Err(Declined::Whole(
        "the round's decisions kept reaching past what was loaded for them".into(),
    )))
}

/// What the oracle's full round leaves for the comparison and the check after a finalize.
struct OracleFull {
    decisions: crate::incr::Decisions,
    remote: Tree,
    visible: Tree,
    local: BTreeMap<ObjectId, crate::merge::LNode>,
    seen: HashMap<ObjectId, Option<Hash>>,
    hidden: BTreeSet<ObjectId>,
}

/// The round the full way, on the state as it was before the round, the same journal and the same
/// scan, with every file the scan did not read given its row's metadata as a full scan gives it; the
/// rows the scan was given are checked to be what the state says.
#[allow(clippy::too_many_arguments)]
fn oracle_full(
    root: &Root,
    (base, rows0, sel0): (&Tree, &BTreeMap<ObjectId, Row>, &Selection),
    journal: &[Op],
    (scanned, ledger): (&ScanResult, &crate::incr::Ledger),
    known: &HashMap<(u64, u64), Known>,
    (intents, arriving0, accounts): (&[CopyIntent], &BTreeMap<ObjectId, Arriving>, &Accounts),
    landed: &BTreeSet<OpId>,
    ids: crate::incr::Ids,
    host: &str,
    date: &str,
) -> Result<OracleFull> {
    let mut remote = base.clone();
    for op in journal {
        remote.apply(op)?;
    }
    let mut sel = sel0.clone();
    sel.recheck_unheld(&root.dir, &remote);
    let mut rows = rows0.clone();
    let mut view = sel.view(base, &remote, &mut rows);
    let by_inode: HashMap<(u64, u64), Known> = rows
        .values()
        .map(|r| ((r.fp.dev, r.fp.ino), r.known()))
        .collect();
    let same = |a: Option<&Known>, b: Option<&Known>| match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.fp == b.fp
                && a.racy == b.racy
                && a.incomplete == b.incomplete
                && a.content == b.content
        }
        _ => false,
    };
    let mut nodes = scanned.nodes.clone();
    // What the scan took from the ledger, by inode: the state's metadata for it, as a scan that read it
    // would have found it, and the ledger's digest for it, which nothing read again.
    let mut vouched: HashSet<(u64, u64)> = HashSet::new();
    let rows_by_inode: HashMap<(u64, u64), &Row> = match scanned.vouched_from < nodes.len() {
        true => rows0.values().map(|r| ((r.fp.dev, r.fp.ino), r)).collect(),
        false => HashMap::new(),
    };
    for (i, n) in nodes.iter_mut().enumerate() {
        if !scanned.vouched(i) {
            continue;
        }
        let key = (n.fp.dev, n.fp.ino);
        vouched.insert(key);
        if let Some(row) = rows_by_inode.get(&key) {
            n.metadata = row.scan.clone();
        }
    }
    for n in nodes.iter_mut() {
        let key = (n.fp.dev, n.fp.ino);
        if !same(known.get(&key), by_inode.get(&key)) {
            bail!(
                "the scan was given another row for {} than the state holds: {:?} against {:?}",
                n.paths
                    .first()
                    .map(|p| crate::scan::display_path(p))
                    .unwrap_or_default(),
                known.get(&key),
                by_inode.get(&key)
            );
        }
        if let Some(k) = by_inode.get(&key) {
            if matches!(n.content, Content::File { .. }) && k.vouches_for(&n.fp) {
                n.metadata = k.scan.cloned().unwrap_or_default();
            }
        }
    }
    let scan = scanned.with_nodes(nodes);
    let mut arriving = arriving0.clone();
    let mut identified = identify(
        &scan,
        &view.base,
        &view.remote,
        &rows,
        intents,
        &arriving,
        landed,
        &|n| ids.of_scanned(n),
    );
    if !arriving.is_empty() {
        let holds = what_arrives_here(root, accounts, &arriving, &identified, false)?;
        let (_, answered) = settle_arriving(&identified, &arriving, &holds, &view.base, &mut rows);
        for id in &answered {
            arriving.remove(id);
        }
        identified = identify(
            &scan,
            &view.base,
            &view.remote,
            &rows,
            intents,
            &arriving,
            landed,
            &|n| ids.of_scanned(n),
        );
    }
    rows_after_scan(&mut rows, &identified, &view.base);
    view.settle(&rows);
    let vanished = check_vanished(&view.base, &view.remote, &rows, &identified.local, 0);
    let twins = crate::twins::hold_back_twins(root, &mut identified, base, &remote);
    let plan = merge(MergeInput {
        base: &view.base,
        local: &identified.local,
        remote: &view.remote,
        shared: &identified.shared,
        hidden: &view.hidden,
        host,
        date,
        fresh_id: &mut ids.counter(),
    });
    let hidden: BTreeSet<ObjectId> = remote
        .live()
        .map(|(id, _)| id)
        .filter(|id| !view.remote.is_live(*id))
        .collect();
    let decisions = crate::incr::Decisions {
        local: identified.local.clone(),
        shared: identified.shared.clone(),
        rows: rows.clone(),
        vanished,
        twins,
        target: plan.target.clone(),
        copies: plan.copies.clone(),
        adopted: plan.adopted.clone(),
        reports: plan.reports.clone(),
        notes: plan.notes.clone(),
        ops: remote_ops(&view.remote, &plan)?,
        steps: local_steps(&plan, &identified)?,
        moving: moving_with_excluded(&plan, &identified, &scanned.excluded),
        reserved: view.hidden.reserved.clone(),
    };
    let mut seen = crate::incr::seen_of(&identified.scanned);
    for (id, n) in &identified.scanned {
        if vouched.contains(&(n.fp.dev, n.fp.ino)) {
            if let (Some(d), Some(s)) = (seen.get_mut(id), ledger.seen_of(id)) {
                if d.is_some() {
                    *d = s.digest;
                }
            }
        }
    }
    let visible = view.remote.into_owned();
    Ok(OracleFull {
        decisions,
        remote,
        visible,
        local: plan.local,
        seen,
        hidden,
    })
}

/// What this commit will leave on the server, as far as this filesystem holds it too, paired with
/// what is on disk, so that the next round can tell a later local edit from the commit itself. It
/// means nothing until the commit is seen in the journal: `shared_state` checks that.
/// `after` is the server's tree as the commits before this one leave it, and becomes the tree as this
/// one leaves it.
fn mark_synced(
    rows: &mut BTreeMap<ObjectId, Row>,
    after: &mut Tree,
    cur: &BTreeMap<ObjectId, CurNode>,
    req: &CommitRequest,
) -> Vec<ObjectId> {
    let mut touched: BTreeSet<ObjectId> = BTreeSet::new();
    let mut gave_up: BTreeSet<ObjectId> = BTreeSet::new();
    for op in &req.ops {
        // It would be refused, and there is nothing to record.
        if after.apply(op).is_err() {
            return Vec::new();
        }
        // Said to the server once: from now on this device has no claim on it, so if the server has
        // it later, by a restore, say, this device's own old delete is not made again.
        if let Op::Unlink { object, link, .. } = op {
            if let Some(held) = rows.get_mut(object).and_then(|r| r.held.as_mut()) {
                if held.remove(link) {
                    gave_up.insert(*object);
                }
            }
        }
        touched.insert(match op {
            Op::Create { object, .. }
            | Op::Link { object, .. }
            | Op::Unlink { object, .. }
            | Op::Move { object, .. }
            | Op::Update { object, .. } => *object,
            // A note changes neither the body nor the entries: nothing this device shares is ahead of
            // anything for it, and its row stays as it is, which a join's notes would rewrite by the
            // hundred thousand, 20,000 to a commit.
            Op::Note { .. } => continue,
        });
    }
    let mut out = Vec::new();
    for id in touched {
        let Some(state) = after.get_live(id) else {
            continue;
        };
        let Some(row) = rows.get_mut(&id) else {
            continue;
        };
        // What could not be read fully is no pair of what went up.
        let on_disk = !row.incomplete
            && cur.get(&id).is_some_and(|c| {
                c.fp.is_some()
                    && c.content == state.body.content
                    && c.apply_from == state.body.metadata
                    && state.links.is_subset(&c.links)
            });
        if on_disk {
            row.synced = Some(Synced {
                body: state.body.clone(),
                links: state.links.clone(),
                applied: row.scan.clone(),
                commit: Some(req.op_id),
                unapplied: false,
                folder_time: None,
            });
            out.push(id);
        }
    }
    gave_up.extend(out);
    gave_up.into_iter().collect()
}

/// A refused commit said nothing: what its deletes gave up is still this device's to delete.
/// The notes of a refused commit, noted again: the queue no longer carries them.
fn notes_back(root: &mut Root, req: &CommitRequest) -> Result<()> {
    let notes: Vec<(String, NoteIntent)> = req
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::Note { object, text } => {
                let note = NoteIntent {
                    object: *object,
                    text: text.clone(),
                };
                Some((note.key(), note))
            }
            _ => None,
        })
        .collect();
    root.state.put_intents(&notes)
}

fn take_back_refused(req: &CommitRequest, rows: &mut BTreeMap<ObjectId, Row>) -> Vec<ObjectId> {
    let mut changed = BTreeSet::new();
    for op in &req.ops {
        if let Op::Unlink { object, link, .. } = op {
            if let Some(held) = rows.get_mut(object).and_then(|r| r.held.as_mut()) {
                if held.insert(link.clone()) {
                    changed.insert(*object);
                }
            }
        }
    }
    changed.into_iter().collect()
}

/// Fail plainly if the configured server is unreachable or refuses the token.
pub fn check_server(root: &Root) -> Result<u64> {
    let client = Client::new(&root.config, Retries::Short)?;
    Ok(client.info()?.head)
}

pub fn ensure_clean_name(name: &str) -> Result<()> {
    if name.len() > 64 {
        bail!("a device name is at most 64 bytes, since it becomes part of conflict file names");
    }
    if name.contains('/') {
        bail!("a device name cannot contain `/`, since it becomes part of conflict file names");
    }
    Ok(())
}

// Re-exported for export and restore.
pub use apply::CurNode as LocalNode;

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_move(n: usize, into: bool) -> Op {
        let object = ObjectId::random();
        let stage = crate::schedule::stage_link(object, n);
        let plain = Link {
            parent: ObjectId::ROOT,
            name: cairn_protocol::Name::new(format!("f{n}").into_bytes()).unwrap(),
        };
        let (from, to) = if into { (plain, stage) } else { (stage, plain) };
        Op::Move { object, from, to }
    }

    fn note() -> Op {
        Op::Note {
            object: ObjectId::ROOT,
            text: String::new(),
        }
    }

    #[test]
    fn small_changes_are_not_cut() {
        let mut ops = vec![note(), note()];
        assert!(!truncate_ops(&mut ops));
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn large_changes_are_cut_and_never_with_something_staged() {
        let mut ops: Vec<Op> = (0..MAX_OPS_PER_COMMIT + 500).map(|_| note()).collect();
        assert!(truncate_ops(&mut ops));
        assert_eq!(ops.len(), MAX_OPS_PER_COMMIT);

        // A stage entered just before the limit and left after it pulls the cut back.
        let mut ops: Vec<Op> = (0..MAX_OPS_PER_COMMIT - 5).map(|_| note()).collect();
        ops.push(stage_move(1, true));
        ops.extend((0..10).map(|_| note()));
        ops.push(stage_move(1, false));
        ops.extend((0..10).map(|_| note()));
        assert!(truncate_ops(&mut ops));
        assert_eq!(ops.len(), MAX_OPS_PER_COMMIT - 5);
    }

    #[test]
    fn dates_are_civil_utc() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(1_793_404_800), "2026-10-31");
        assert_eq!(utc_date(-86_400), "1969-12-31");
    }
}
