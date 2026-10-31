//! One sync of a root: scan, merge against the server, apply locally, push, until both sides agree.
//!
//! The merge is state-based: each round works from what is on disk and what the journal says. A
//! crash anywhere only means the next run sees some of the work already done on one side or both,
//! which merges as agreement, so recovery has no special path beyond undoing half-made moves.

use crate::apply::{
    self, cur_from_identified, read_for_upload, recover, Applier, CurNode, INTENT_COPY,
};
use crate::client::Client;
use crate::identify::{identify, CopyIntent};
use crate::merge::{merge, MergeInput, Plan, ReportKind, TNode};
use crate::metadata::Accounts;
use crate::root::{Paused, Root};
use crate::scan::{scan, ScanOptions, ScanResult};
use crate::schedule::{schedule, Topo, TopoOp};
use crate::state::{Row, Synced};
use anyhow::{anyhow, bail, Context, Result};
use cairn_failpoint::{check, hit};
use cairn_protocol::wire::{self, CommitRequest, CommitResponse, MAX_CHUNK_BATCH_BYTES};
use cairn_protocol::{
    ChunkRef, Content, Hash, Manifest, ObjectId, Op, OpId, RevisionId, Skip, Tree,
};
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
}

impl SyncReport {
    fn new() -> Self {
        Self {
            outcome: SyncOutcome::Partial,
            skips: Vec::new(),
            warnings: Vec::new(),
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
        }
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

fn scan_stable(
    root: &Root,
    rows: &BTreeMap<ObjectId, Row>,
    accounts: &Accounts,
    remove_temps: bool,
) -> Result<ScanResult> {
    let by_inode: HashMap<(u64, u64), &Row> =
        rows.values().map(|r| ((r.fp.dev, r.fp.ino), r)).collect();
    let prefix = root.temp_prefix();
    let opts = ScanOptions {
        temp_prefix: prefix.as_bytes(),
        accounts,
        rows: &by_inode,
        remove_own_temps: remove_temps,
    };
    let mut result = scan(&root.dir, &opts).context("scanning the root")?;
    if !result.stale_temps.is_empty() {
        for (path, name) in &result.stale_temps {
            apply::remove_stale_temp(root, path, name)
                .context("removing a leftover temporary file")?;
        }
        result = scan(&root.dir, &opts).context("scanning the root")?;
    }
    for _ in 1..SCAN_ATTEMPTS {
        if !result.unstable {
            break;
        }
        result = scan(&root.dir, &opts).context("scanning the root")?;
    }
    Ok(result)
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
        if frames.len() + data.len() + 36 > MAX_CHUNK_BATCH_BYTES {
            flush(&mut frames)?;
        }
        wire::write_frame(&mut frames, hash, &data);
        chunks += 1;
        bytes += data.len() as u64;
    }
    flush(&mut frames)?;
    Ok(Upload::Done { chunks, bytes })
}

/// Before anything else: commits queued by an earlier run go out with their original IDs, so a
/// response lost to a crash or the network is harmless.
fn replay_outbox(root: &Root, client: &Client, report: &mut SyncReport) -> Result<()> {
    for req in root.state.outbox()? {
        hit("sync.outbox.replay");
        let resp = client.commit(&req)?;
        if matches!(resp, CommitResponse::Committed { .. }) {
            report.commits += 1;
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
) -> Result<()> {
    let mut changed = Vec::new();
    for (id, s) in &identified.scanned {
        let new = match rows.get(id) {
            Some(old) => Row {
                fp: s.fp.clone(),
                racy: s.racy,
                content: s.content.clone(),
                scan: s.metadata.clone(),
                applied: old.applied.clone(),
                synced: old.synced.clone(),
                incomplete: s.metadata_incomplete,
            },
            None => Row {
                fp: s.fp.clone(),
                racy: s.racy,
                content: s.content.clone(),
                scan: s.metadata.clone(),
                applied: None,
                synced: None,
                incomplete: s.metadata_incomplete,
            },
        };
        if rows.get(id) != Some(&new) {
            rows.insert(*id, new);
            changed.push(*id);
        }
    }
    let list: Vec<(ObjectId, &Row)> = changed.iter().map(|id| (*id, &rows[id])).collect();
    root.state.store_rows(&list, &[])
}

/// A new local object recognised as a remote one takes the remote identity.
fn adopt_rows(
    root: &mut Root,
    rows: &mut BTreeMap<ObjectId, Row>,
    adopted: &BTreeMap<ObjectId, ObjectId>,
) -> Result<()> {
    if adopted.is_empty() {
        return Ok(());
    }
    let mut changed = Vec::new();
    let mut gone = Vec::new();
    for (x, y) in adopted {
        if let Some(row) = rows.remove(x) {
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
            // What could not be read fully is no baseline.
            Some(_) if row.incomplete => {
                if row.synced.take().is_some() {
                    changed.push(*id);
                }
            }
            Some(_) => {
                if row.applied.as_ref() != Some(&row.scan) || row.synced.is_some() {
                    row.applied = Some(row.scan.clone());
                    row.synced = None;
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
        replay_outbox(&root, &client, &mut report)?;
    }

    let mut base = root.state.load_base()?;
    let mut rows = root.state.load_rows()?;
    let mut remote = base.clone();
    let mut seq = root.state.cursor()?;
    let date = utc_date(now_secs());
    let mut seen_events: BTreeSet<String> = report.events.iter().cloned().collect();
    let mut retryable = false;

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

        let scanned = scan_stable(&root, &rows, &accounts, !opts.dry_run)?;
        hit("sync.scanned");
        root.state.put_manifests(&scanned.new_manifests)?;
        let unstable = scanned.unstable;
        report.skips = scanned.skips.clone();
        retryable = report.skips.iter().any(|s| s.kind.retryable()) || unstable;

        let intents: Vec<CopyIntent> = root
            .state
            .intents::<CopyIntent>(INTENT_COPY)?
            .into_iter()
            .map(|(_, i)| i)
            .collect();
        let identified = identify(&scanned, &base, &remote, &rows, &intents, &seen.commits);
        update_rows(&mut root, &mut rows, &identified)?;
        hit("sync.identified");

        if !opts.allow_mass_delete {
            if let Some(why) = check_vanished(&base, &remote, &rows, &identified.local) {
                return Ok(SyncReport {
                    outcome: SyncOutcome::Paused(why),
                    ..report
                });
            }
        }

        let host = root.config.device_name.clone();
        let plan = merge(MergeInput {
            base: &base,
            local: &identified.local,
            remote: &remote,
            shared: &identified.shared,
            host: &host,
            date: &date,
            fresh_id: &mut ObjectId::random,
        });
        for r in &plan.reports {
            if r.kind == ReportKind::ConflictCopy {
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
            for op in remote_ops(&remote, &plan)? {
                report.planned.push(format!("server: {}", describe_op(&op)));
            }
            break SyncOutcome::Synced;
        }

        adopt_rows(&mut root, &mut rows, &plan.adopted)?;
        let cur = cur_from_identified(&plan.local, &identified.scanned, &plan.adopted);
        let (outcome, cur) =
            Applier::new(&mut root, Some(&client), &accounts, cur, &mut rows, &remote)
                .run(&plan)?;
        hit("sync.applied");
        report.steps_applied += outcome.applied_steps;
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
                s.kind = cairn_protocol::SkipKind::CannotApply;
                s
            }));

        let mut ops = remote_ops(&remote, &plan)?;
        // The loser of a metadata conflict goes in the log even when nothing else is to be pushed.
        // Unless applying failed: the conflict stands, and would be logged on every run.
        if !failed || !ops.is_empty() {
            for (object, text) in &plan.notes {
                ops.push(Op::Note {
                    object: *object,
                    text: text.clone(),
                });
            }
        }
        if ops.is_empty() {
            if failed {
                break SyncOutcome::Partial;
            }
            if outcome.touched {
                // The filesystem was written: confirm it with a fresh scan before calling this agreed.
                continue;
            }
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
        // A huge change goes in several commits, the next round picking up where this one ended.
        let truncated = truncate_ops(&mut ops);

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
        let pushed = mark_synced(&mut rows, &remote, &cur, &req);
        let list: Vec<(ObjectId, &Row)> = pushed.iter().map(|id| (*id, &rows[id])).collect();
        root.state.enqueue(&req, &list)?;
        hit("sync.enqueued");
        let resp = client.commit(&req)?;
        hit("sync.committed");
        root.state.dequeue(&req.op_id)?;
        hit("sync.dequeued");
        match resp {
            CommitResponse::Committed { .. } => {
                report.commits += 1;
                report.ops_pushed += req.ops.len() as u64;
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
    for op in &req.ops {
        // It would be refused, and there is nothing to record.
        if after.apply(op).is_err() {
            return Vec::new();
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
            });
            out.push(id);
        }
    }
    out
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
    use cairn_protocol::Link;

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
