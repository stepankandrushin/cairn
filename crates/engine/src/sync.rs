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
use crate::client::Client;
use crate::identify::{identify, settle_arriving, Arriving, CopyIntent, Identified};
use crate::merge::{merge, MergeInput, Plan, ReportKind, TNode};
use crate::metadata::Accounts;
use crate::root::{Paused, Root};
use crate::scan::{scan, ScanOptions, ScanResult};
use crate::schedule::{schedule, Topo, TopoOp};
use crate::selection::Selection;
use crate::state::{Row, Synced};
use anyhow::{anyhow, bail, Context, Result};
use cairn_failpoint::{check, hit};
use cairn_protocol::local::{Activity, Change, Direction};
use cairn_protocol::wire::{self, CommitRequest, CommitResponse, MAX_CHUNK_BATCH_BYTES};
use cairn_protocol::{
    ChunkRef, ConflictOf, Content, Hash, Link, Manifest, Name, ObjectId, ObjectKind, Op, OpId,
    RevisionId, Skip, Tree,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

/// Rounds in one run before admitting the tree will not hold still.
const MAX_ROUNDS: u32 = 12;
/// Rounds in which something moving under an apply holds back pushing, too.
const ROUNDS_BEFORE_PUSHING: u32 = 3;
/// Walks of the tree in one round, while it changes under the walk.
const SCAN_ATTEMPTS: u32 = 3;
/// At least this many vanished entries, making up half the tree, pause sync.
const MASS_DELETE_MIN: usize = 10;

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    pub allow_mass_delete: bool,
    pub dry_run: bool,
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
    /// How long the scans took: the part of a sync that every one repeats, whatever there is to do.
    pub scan_time: std::time::Duration,
    /// What it sent and what it applied here, in order, for the daemon's list of recent changes.
    /// Not metadata applied here alone, which a filesystem that cannot hold all of it would repeat.
    pub activity: Vec<Activity>,
}

impl SyncReport {
    fn new() -> Self {
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
            activity: Vec::new(),
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
    loop {
        let resp = client.changes(*seq)?;
        for change in &resp.changes {
            for op in &change.ops {
                tree.apply(op).map_err(|e| {
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
    let resp = client.changes(cursor - 1)?;
    let same = resp
        .changes
        .first()
        .is_some_and(|c| c.seq == cursor && cursor_op.is_none_or(|op| op == c.op_id));
    Ok((!same).then(|| {
        format!(
            "the server no longer has the history this device last saw (sequence {cursor}; it is at {}): was it \
             restored from a backup? Sync is paused and nothing was changed. Files only here are safe. To \
             rejoin, remove this root's .cairn folder and run `cairn init` again: the next sync merges by \
             name and content, keeps every difference as a conflict copy, and sends the server what it lost.",
            resp.head
        )
    }))
}

#[allow(clippy::too_many_arguments)]
fn scan_stable(
    root: &mut Root,
    rows: &BTreeMap<ObjectId, Row>,
    accounts: &Accounts,
    remove_temps: bool,
    excluded: &dyn Fn(&[cairn_protocol::Name], &crate::fsops::Stat) -> bool,
    date: &str,
    events: &mut Vec<String>,
    trees: (&Tree, &Tree),
    kept_edits: &mut u64,
) -> Result<ScanResult> {
    let by_inode: HashMap<(u64, u64), &Row> =
        rows.values().map(|r| ((r.fp.dev, r.fp.ino), r)).collect();
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
        rows: &by_inode,
        remove_own_temps: remove_temps,
        arriving: &on_its_way,
        excluded,
    };
    let mut result = scan(&root.dir, &opts).context("scanning the root")?;
    let mut rescued = Vec::new();
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
    for _ in 1..SCAN_ATTEMPTS {
        if !result.unstable {
            break;
        }
        result = scan(&root.dir, &opts).context("scanning the root")?;
    }
    result.rescued = rescued;
    keep_what_only_moved_holes(root, &by_inode, &mut result);
    Ok(result)
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
    by_inode: &HashMap<(u64, u64), &Row>,
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
/// have made of that record. Without it, on a dry run or a disk too full to ask, what is there stands.
fn what_arrives_here(
    root: &Root,
    accounts: &Accounts,
    arriving: &BTreeMap<ObjectId, Arriving>,
    identified: &Identified,
    dry_run: bool,
) -> BTreeMap<ObjectId, cairn_protocol::Metadata> {
    if dry_run {
        return BTreeMap::new();
    }
    arriving
        .values()
        .filter_map(|a| {
            let node = identified.scanned.get(&a.object)?;
            let mut want = a.body.metadata.clone();
            want.btime = node.metadata.btime;
            if node.content != a.body.content || node.metadata == want {
                return None;
            }
            let held = apply::what_arrives(root, accounts, &node.content, &a.body.metadata)?;
            Some((a.object, held))
        })
        .collect()
}

/// An empty or largely vanished tree is not a mass delete until a person says so (invariant 5).
fn check_vanished(
    base: &Tree,
    remote: &Tree,
    rows: &BTreeMap<ObjectId, Row>,
    local: &BTreeMap<ObjectId, crate::merge::LNode>,
) -> Option<String> {
    let mut live = 0usize;
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

/// Keep as many operations as one commit should hold. Returns whether any were left out. Every
/// operation leaves a valid tree, so any cut is valid, except that a stage name must not outlive
/// its commit: the cut only falls where nothing is staged.
fn truncate_ops(ops: &mut Vec<Op>) -> bool {
    let mut bytes = 0usize;
    let mut staged = 0i64;
    let mut cut = 0usize;
    for (i, op) in ops.iter().enumerate() {
        bytes += serde_json::to_vec(op).map_or(0, |v| v.len());
        if i > 0 && (i >= MAX_OPS_PER_COMMIT || bytes > MAX_COMMIT_BYTES) {
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
    if cut == 0 || cut == ops.len() {
        // Everything staged at once, however large, has to go together.
        return false;
    }
    ops.truncate(cut);
    true
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
    let list: Vec<(ObjectId, &Row)> = changed.iter().map(|id| (*id, &rows[id])).collect();
    root.state.store_rows(&list, &[])
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
fn finalize(
    root: &mut Root,
    base: &mut Tree,
    remote: &Tree,
    seq: u64,
    last_op: Option<OpId>,
    rows: &mut BTreeMap<ObjectId, Row>,
    plan: &Plan,
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
    root.state
        .store_base(base, remote, seq, last_op, &list, &gone)?;
    hit("sync.finalize.base");
    *base = remote.clone();

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
    root.state.retain_manifests(&keep)?;
    let cache = root.state_dir.open_dir(b"chunks")?;
    for name in cache.list()? {
        let _ = cache.unlink(&name);
    }
    Ok(())
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

/// Sync one root once.
pub fn sync(path: &Path, opts: &SyncOptions) -> Result<SyncReport> {
    let mut report = SyncReport::new();
    let mut root = match Root::open(path) {
        Ok(r) => r,
        Err(e) => {
            report.outcome = SyncOutcome::Paused(e.downcast::<Paused>()?.0);
            return Ok(report);
        }
    };
    let client = Client::new(&root.config)?;
    let accounts = Accounts::load();
    // Read once, before anything is touched: a rule that cannot be read must stop the sync.
    let mut selection = Selection::load(&root)?;
    // Purely local, and first: anything else may send a person to act on this folder.
    if !opts.dry_run {
        report.events.extend(recover(&mut root)?);
    }
    let cursor_op = root.state.cursor_op()?;
    if let Some(why) = check_history(&client, root.state.cursor()?, cursor_op)? {
        report.outcome = SyncOutcome::Paused(why);
        return Ok(report);
    }
    if !opts.dry_run {
        replay_outbox(&mut root, &client, &mut report)?;
    }

    let mut base = root.state.load_base()?;
    let mut rows = root.state.load_rows()?;
    let mut remote = base.clone();
    let mut seq = root.state.cursor()?;
    let date = utc_date(now_secs());
    let mut seen_events: BTreeSet<String> = report.events.iter().cloned().collect();
    let mut retryable = false;
    // Losers of metadata conflicts not yet in a commit the server took. Nothing else says them again:
    // the next round finds the conflict settled.
    let mut notes: Vec<(ObjectId, String)> = Vec::new();
    let mut noted: HashSet<(ObjectId, String)> = HashSet::new();

    let mut seen = Seen {
        last: cursor_op,
        ..Seen::default()
    };
    let finish = loop {
        report.rounds += 1;
        if report.rounds > MAX_ROUNDS {
            report.events.push(format!(
                "the tree kept changing for {MAX_ROUNDS} rounds; sync again once it settles"
            ));
            break SyncOutcome::Partial;
        }
        hit("sync.round");
        fetch_changes(&client, &mut remote, &mut seq, &mut seen)?;
        report.head = seq;

        if selection.recheck_unheld(&root.dir, &remote) && !opts.dry_run {
            selection.store(&root)?;
        }
        // From here to the push, everything sees only what this device takes part in. `base` and
        // `remote` stay the real record, for the cursor and for the next round.
        let mut view = selection.view(&base, &remote, &mut rows);
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
        let scanned = scan_stable(
            &mut root,
            &rows,
            &accounts,
            !opts.dry_run,
            &|path, st| selection.leaves_out(path, st, &view, &rows, &mut moved.borrow_mut()),
            &date,
            &mut report.events,
            (&base, &remote),
            &mut report.conflicts,
        )?;
        report.scan_time += scan_started.elapsed();
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
        report.excluded_here = scanned
            .excluded
            .iter()
            .map(|path| crate::scan::display_path(path))
            .collect();
        root.state.put_manifests(&scanned.new_manifests)?;
        let unstable = scanned.unstable;
        report.skips = scanned.skips.clone();
        report
            .skips
            .extend(selection.unheld.iter().map(|u| unheld_skip(&remote, u)));
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
        let mut identified = identify(
            &scanned,
            &view.base,
            &view.remote,
            &rows,
            &intents,
            &arriving,
            &seen.commits,
        );
        // In a tree still moving, what was seen was seen, and only what was not is asked again.
        if !arriving.is_empty() {
            let holds = what_arrives_here(&root, &accounts, &arriving, &identified, opts.dry_run);
            let (changed, answered) =
                settle_arriving(&identified, &arriving, &holds, &view.base, &mut rows);
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
                &rows,
                &intents,
                &arriving,
                &seen.commits,
            );
        }
        update_rows(&mut root, &mut rows, &identified, &view.base)?;
        // Found here means held, however that came about. Not found, and given up, is no delete.
        view.settle(&rows);
        hit("sync.identified");

        if !opts.allow_mass_delete {
            if let Some(why) = check_vanished(&view.base, &view.remote, &rows, &identified.local) {
                return Ok(SyncReport {
                    outcome: SyncOutcome::Paused(why),
                    ..report
                });
            }
        }

        // A name the server has in another form goes up as a second name, unless it waits here.
        report.skips.extend(crate::twins::hold_back_twins(
            &root,
            &mut identified,
            &base,
            &remote,
        ));

        let host = root.config.device_name.clone();
        let plan = merge(MergeInput {
            base: &view.base,
            local: &identified.local,
            remote: &view.remote,
            shared: &identified.shared,
            hidden: &view.hidden,
            host: &host,
            date: &date,
            fresh_id: &mut ObjectId::random,
        });
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

        adopt_rows(
            &mut root,
            &mut rows,
            &plan.adopted,
            &view.base,
            &view.remote,
        )?;
        let cur = cur_from_identified(&plan.local, &identified.scanned, &plan.adopted);
        let arriving = down_activity(&cur, &plan);
        let (outcome, cur) = Applier::new(
            &mut root,
            Some(&client),
            &accounts,
            cur,
            &mut rows,
            &view.remote,
        )
        .run(&plan)?;
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
                report.skips.push(unheld_skip(&remote, unheld));
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

        let mut ops = remote_ops(&view.remote, &plan)?;
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
        let first_note = ops.len();
        ops.extend(notes.iter().map(|(object, text)| Op::Note {
            object: *object,
            text: text.clone(),
        }));
        if ops.is_empty() {
            if failed {
                break SyncOutcome::Partial;
            }
            if outcome.touched {
                // The filesystem was written: confirm it with a fresh scan before calling this agreed.
                continue;
            }
            drop(view);
            finalize(
                &mut root, &mut base, &remote, seq, seen.last, &mut rows, &plan,
            )?;
            hit("sync.finalized");
            break if unstable {
                SyncOutcome::Partial
            } else {
                SyncOutcome::Synced
            };
        }
        // A huge change goes in several commits, the next round picking up where this one ended, and
        // the notes left out with it.
        let truncated = truncate_ops(&mut ops);
        let notes_sent = ops.len().saturating_sub(first_note);

        match upload(&client, &root, &cur, &ops)? {
            Upload::FileChanged => {
                retryable = true;
                continue;
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
        let pushed = mark_synced(&mut rows, &view.remote, &cur, &req);
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
            let changed = take_back_refused(&req, &mut rows);
            let list: Vec<(ObjectId, &Row)> = changed.iter().map(|id| (*id, &rows[id])).collect();
            root.state.store_rows(&list, &[])?;
        }
        root.state.dequeue(&req.op_id)?;
        hit("sync.dequeued");
        match resp {
            CommitResponse::Committed { .. } => {
                notes.drain(..notes_sent);
                report.commits += 1;
                report.ops_pushed += req.ops.len() as u64;
                report
                    .activity
                    .extend(up_activity(&view.remote, &plan, &req.ops));
                // A huge change is not a tree failing to settle, as long as it is getting through.
                if truncated {
                    report.rounds -= 1;
                }
            }
            CommitResponse::Rejected { reject } => {
                // Someone else got in first, or a chunk has gone: the next round sees why.
                let line =
                    format!("the server refused a commit and this client rebased: {reject:?}");
                if seen_events.insert(line.clone()) {
                    report.events.push(line);
                }
            }
        }
        if failed {
            break SyncOutcome::Partial;
        }
    };

    report.outcome = match finish {
        SyncOutcome::Synced if retryable => SyncOutcome::Partial,
        other => other,
    };
    Ok(report)
}

/// What this commit will leave on the server, as far as this filesystem holds it too, paired with
/// what is on disk, so that the next round can tell a later local edit from the commit itself. It
/// means nothing until the commit is seen in the journal: `shared_state` checks that.
fn mark_synced(
    rows: &mut BTreeMap<ObjectId, Row>,
    remote: &Tree,
    cur: &BTreeMap<ObjectId, CurNode>,
    req: &CommitRequest,
) -> Vec<ObjectId> {
    let mut after = remote.clone();
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
            | Op::Update { object, .. }
            | Op::Note { object, .. } => *object,
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
        let on_disk = cur.get(&id).is_some_and(|c| {
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
    let client = Client::new(&root.config)?;
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
