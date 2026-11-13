//! A sync with nothing to do, told for what it is without the whole tree in memory.
//!
//! A sync is a function of what it reads: the state, the server's journal, the tree, the rules, the
//! configuration and the build. When one ran in full, found nothing to do and wrote nothing, it leaves
//! a record of what it read: the state by its generation, which every write moves (state.rs); the
//! journal by the entry at the cursor and the head; the rest by hashes; and the tree by a digest of its
//! scan. A later sync that finds all of those the same would decide what that one did, which was
//! nothing, so it stops after its scan, before the base is loaded, anything is merged or the tree is
//! scheduled. Anything else, and the full sync runs: this only ever skips work whose result is known.
//!
//! The scan here is the full sync's own, with one difference: where a full scan would read a file's
//! bytes, or remove a leftover of a killed run, it stops, since a sync that does either has something
//! to do. A file it does not read stands for its row, as in any scan; every other entry is read as the
//! full scan reads it, and goes into the digest whole.

use crate::client::Client;
use crate::metadata::Accounts;
use crate::root::Root;
use crate::scan::{scan, KnownRows, ScanOptions, ScanResult, Scanned};
use crate::selection::Selection;
use crate::state::{Fingerprint, Known};
use crate::sync::{SyncOptions, SyncOutcome, SyncReport};
use anyhow::{bail, Result};
use cairn_protocol::{Content, Hash, Name, OpId, Skip, Timestamp};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

/// Moved whenever what a record means changes, so that none written before is read as one of these.
/// 3: the scan's digest a sum of each node's, at its place in the walk.
const FORMAT: u32 = 3;

/// What a sync that had nothing to do read, and what it said.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steady {
    pub format: u32,
    pub generation: u64,
    /// The build, the configuration and the rules.
    pub inputs: Hash,
    pub cursor: u64,
    pub cursor_op: Option<OpId>,
    pub scan: Hash,
    pub report: Recorded,
}

impl Steady {
    pub fn new(
        generation: u64,
        inputs: Hash,
        cursor: u64,
        cursor_op: Option<OpId>,
        scan: Hash,
        report: Recorded,
    ) -> Self {
        Self {
            format: FORMAT,
            generation,
            inputs,
            cursor,
            cursor_op,
            scan,
            report,
        }
    }
}

/// The parts of a report a sync with nothing to do can have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recorded {
    pub partial: bool,
    pub skips: Vec<Skip>,
    pub warnings: Vec<Skip>,
    pub meta_applied: Vec<String>,
    pub events: Vec<String>,
    pub conflicts: u64,
    pub excluded_here: Vec<String>,
    pub not_synced_here: Vec<String>,
}

impl Recorded {
    pub fn of(report: &SyncReport) -> Self {
        Self {
            partial: report.outcome != SyncOutcome::Synced,
            skips: report.skips.clone(),
            warnings: report.warnings.clone(),
            meta_applied: report.meta_applied.clone(),
            events: report.events.clone(),
            conflicts: report.conflicts,
            excluded_here: report.excluded_here.clone(),
            not_synced_here: report.not_synced_here.clone(),
        }
    }
}

/// The build that is running, as it was when it started: another build may decide otherwise on the
/// same tree, so a record made by one says nothing to another. Unknown, it is this process's alone.
pub(crate) fn build() -> &'static str {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| {
        // Linux names the file that is running even after it has been replaced.
        #[cfg(target_os = "linux")]
        let exe = Some(std::path::PathBuf::from("/proc/self/exe"));
        #[cfg(not(target_os = "linux"))]
        let exe = std::env::current_exe().ok();
        let seen = exe.and_then(|p| std::fs::metadata(p).ok()).map(|m| {
            #[cfg(unix)]
            let more = {
                use std::os::unix::fs::MetadataExt;
                format!("{} {} {} {}", m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
            };
            #[cfg(not(unix))]
            let more = format!("{:?}", m.created().ok());
            format!("{} {:?} {more}", m.len(), m.modified().ok())
        });
        let seen = seen.unwrap_or_else(|| format!("unknown {:032x}", rand::random::<u128>()));
        format!("{} {seen}", env!("CARGO_PKG_VERSION"))
    })
}

/// Everything besides the state, the journal and the tree that a sync's decisions follow from.
pub fn inputs(root: &Root, selection: &Selection, opts: &SyncOptions) -> Hash {
    let mut d = Digest::new(b"cairn steady inputs");
    d.u64(u64::from(FORMAT));
    d.flag(opts.allow_mass_delete);
    d.bytes(build().as_bytes());
    d.bytes(&serde_json::to_vec(&root.config).unwrap_or_default());
    match selection.rules.text_hash() {
        Some(h) => d.bytes(&h.0),
        None => d.u64(0),
    }
    d.finish()
}

struct Digest(blake3::Hasher);

impl Digest {
    fn new(tag: &[u8]) -> Self {
        let mut d = Self(blake3::Hasher::new());
        d.bytes(tag);
        d
    }

    fn u64(&mut self, v: u64) {
        self.0.update(&v.to_le_bytes());
    }

    fn flag(&mut self, b: bool) {
        self.u64(u64::from(b));
    }

    fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.update(b);
    }

    fn json<T: Serialize>(&mut self, v: &T) {
        // Serializing these cannot fail: maps with string keys, and plain values.
        self.bytes(&serde_json::to_vec(v).unwrap_or_default());
    }

    fn path(&mut self, path: &[Name]) {
        self.u64(path.len() as u64);
        for name in path {
            self.bytes(name.as_bytes());
        }
    }

    fn paths(&mut self, paths: &[Vec<Name>]) {
        self.u64(paths.len() as u64);
        for p in paths {
            self.path(p);
        }
    }

    fn time(&mut self, t: Timestamp) {
        self.u64(t.sec as u64);
        self.u64(u64::from(t.nsec));
    }

    fn fp(&mut self, fp: &Fingerprint) {
        self.u64(fp.dev);
        self.u64(fp.ino);
        self.u64(fp.size);
        self.time(fp.mtime);
        self.time(fp.ctime);
        match fp.btime {
            Some(t) => {
                self.u64(1);
                self.time(t);
            }
            None => self.u64(0),
        }
    }

    fn node(&mut self, n: &Scanned, rows: &dyn KnownRows) {
        self.paths(&n.paths);
        self.fp(&n.fp);
        self.flag(n.racy);
        self.flag(n.metadata_incomplete);
        self.flag(n.holds_excluded);
        self.flag(n.only_excluded);
        // A file its row speaks for is that row's content and metadata, which the generation fixes.
        let vouched = matches!(n.content, Content::File { .. })
            && rows
                .known(&(n.fp.dev, n.fp.ino))
                .is_some_and(|r| r.vouches_for(&n.fp) && r.content == n.content);
        self.flag(vouched);
        if !vouched {
            self.json(&n.content);
            self.json(&n.metadata);
        }
    }

    fn finish(self) -> Hash {
        Hash(*self.0.finalize().as_bytes())
    }
}

/// All a scan saw, with what a row spoke for counted as that row: each node's own digest with its place in
/// the walk, summed, so that the nodes can be counted in any order and on any thread, as the walk lets
/// each go; then the root and the rest of what the scan says.
pub struct Stream {
    sum: [u64; 4],
    nodes: u64,
}

/// One node's part of a scan's digest, at its place in the walk.
pub fn node_term(seq: u32, n: &Scanned, rows: &dyn KnownRows) -> [u64; 4] {
    let mut d = Digest::new(b"cairn steady node");
    d.u64(u64::from(seq));
    d.node(n, rows);
    let h = d.finish().0;
    let lane = |i: usize| u64::from_le_bytes(h[i * 8..i * 8 + 8].try_into().unwrap_or_default());
    [lane(0), lane(1), lane(2), lane(3)]
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    pub fn new() -> Self {
        Stream {
            sum: [0; 4],
            nodes: 0,
        }
    }

    pub fn node(&mut self, seq: u32, n: &Scanned, rows: &dyn KnownRows) {
        self.add(node_term(seq, n, rows));
    }

    /// A node's part worked out elsewhere, by `node_term`.
    pub fn add(&mut self, term: [u64; 4]) {
        for (s, t) in self.sum.iter_mut().zip(term) {
            *s = s.wrapping_add(t);
        }
        self.nodes += 1;
    }

    pub fn finish(self, result: &ScanResult, rows: &dyn KnownRows) -> Hash {
        let mut d = Digest::new(b"cairn steady scan");
        d.u64(self.nodes);
        for lane in self.sum {
            d.u64(lane);
        }
        d.node(&result.root, rows);
        d.u64(result.skips.len() as u64);
        for s in &result.skips {
            d.json(s);
        }
        d.paths(&result.unobserved);
        d.paths(&result.refused);
        d.u64(result.unobserved_inodes.len() as u64);
        for (dev, ino) in &result.unobserved_inodes {
            d.u64(*dev);
            d.u64(*ino);
        }
        d.flag(result.unidentified);
        d.paths(&result.unlisted);
        d.flag(result.unstable);
        d.u64(result.new_manifests.len() as u64);
        for (h, _) in &result.new_manifests {
            d.bytes(&h.0);
        }
        d.u64(result.stale_temps.len() as u64);
        for (path, name) in &result.stale_temps {
            d.path(path);
            d.bytes(name);
        }
        d.u64(result.rescued_temps.len() as u64);
        for (path, name, fp) in &result.rescued_temps {
            d.path(path);
            d.bytes(name);
            d.fp(fp);
        }
        d.u64(result.rescued.len() as u64);
        for id in &result.rescued {
            d.bytes(&id.0);
        }
        d.paths(&result.excluded);
        // What a filter decided by, for each entry it left out. A folder's own times already move when an
        // entry in it is replaced, but not on a filesystem whose times are coarser than that replacement.
        d.u64(result.excluded_ids.len() as u64);
        for (dev, ino, btime, is_dir) in &result.excluded_ids {
            d.u64(*dev);
            d.u64(*ino);
            match btime {
                Some(t) => {
                    d.u64(1);
                    d.time(*t);
                }
                None => d.u64(0),
            }
            d.flag(*is_dir);
        }
        d.flag(result.stopped);
        d.finish()
    }
}

/// The digest a scan asked for one worked out.
pub fn digest(result: &ScanResult) -> Result<Hash> {
    match result.digest {
        Some(d) => Ok(d),
        None => bail!("a scan whose digest was asked for came back without one"),
    }
}

pub fn load(root: &Root) -> Option<Steady> {
    let text = root.state.steady().ok()??;
    serde_json::from_str::<Steady>(&text)
        .ok()
        .filter(|s| s.format == FORMAT)
}

pub fn store(root: &Root, steady: &Steady) -> Result<()> {
    root.state.set_steady(Some(&serde_json::to_string(steady)?))
}

/// What the fast path found: nothing to do, and how it knows.
pub struct Fast {
    pub report: SyncReport,
    pub steady: Steady,
    pub digest: Hash,
}

/// Nothing to do, if this sync reads what the last one with nothing to do read. `None` sends it the
/// full way, which decides for itself.
pub fn try_fast(
    root: &mut Root,
    client: &Client,
    accounts: &Accounts,
    selection: &Selection,
    opts: &SyncOptions,
    generation: Option<u64>,
) -> Result<Option<Fast>> {
    let started = Instant::now();
    let Some(steady) = load(root) else {
        return Ok(None);
    };
    let cursor = root.state.cursor()?;
    let cursor_op = root.state.cursor_op()?;
    if generation != Some(steady.generation)
        || steady.inputs != inputs(root, selection, opts)
        || (cursor, cursor_op) != (steady.cursor, steady.cursor_op)
    {
        return Ok(None);
    }
    // Whatever a run left marked or queued is the full sync's to settle, however it was before.
    if root.state.pending()? {
        return Ok(None);
    }
    // What a folder that was read-only held back here goes up once it is not: what it may do is the
    // server's to say, which no record of the tree knows.
    let held_back = steady
        .report
        .skips
        .iter()
        .any(|s| s.kind == cairn_protocol::SkipKind::ReadOnly);
    if held_back && !client.read_only() {
        return Ok(None);
    }
    // One entry, without its operations: the history is the same, and nothing came after it.
    let at = client.change_at(cursor)?;
    if crate::sync::history_gone(&at, cursor, cursor_op).is_some() || at.head != cursor {
        return Ok(None);
    }
    // A name this filesystem cannot hold stays out until its twin here goes: asked as the full sync
    // asks, of the base, which is the server's tree while nothing has come after the cursor.
    if !selection.unheld.is_empty() {
        let tree = root
            .state
            .base_subset(selection.unheld.iter().map(|u| u.object))?;
        let mut again = Selection {
            unheld: selection.unheld.clone(),
            ..Selection::default()
        };
        if again.recheck_unheld(&root.dir, &tree) {
            return Ok(None);
        }
    }

    // What the scan is given of each inode: the ledger and what was written since, as an incremental
    // round has it, where the ledger speaks for this sync; otherwise a glance at every row.
    let session = match crate::incr::enabled() {
        true => crate::incr::Session::open(root, selection, false)?.ok(),
        false => None,
    };
    let mut glance = Vec::new();
    let mut by_inode: HashMap<(u64, u64), Known> = HashMap::new();
    let (mut dirty, mut dirty_nodes, mut dirty_rows) = Default::default();
    if session.is_some() {
        dirty = root.state.dirty()?;
        let list: Vec<cairn_protocol::ObjectId> = dirty.iter().copied().collect();
        dirty_nodes = root.state.nodes_of(&list)?;
        dirty_rows = root.state.rows_of(&list)?;
    } else {
        glance = root.state.glance_rows()?;
        by_inode = glance
            .iter()
            .map(|(_, g)| ((g.fp.dev, g.fp.ino), g.known()))
            .collect();
    }
    let unscanned = std::collections::BTreeSet::new();
    let none = std::collections::BTreeSet::new();
    let journal = crate::incr::Journal::default();
    let before = session.as_ref().map(|s| crate::incr::Before {
        ledger: &s.ledger,
        dirty: &dirty,
        dirty_nodes: &dirty_nodes,
        dirty_rows: &dirty_rows,
        journal: &journal,
        glanced: false,
        unscanned: &unscanned,
    });
    let sorting = before.as_ref().map(|b| crate::incr::Sorting::new(b, &none));
    let rows: &dyn KnownRows = match &sorting {
        Some(s) => s,
        None => &by_inode,
    };
    let prefix = root.temp_prefix();
    // What the full scan leaves out but without the trees: the digest holds each entry it was asked
    // about, with what it decides by, so where the two would differ this one sees another tree.
    let excluded = |path: &[Name], st: &crate::fsops::Stat| selection.leaves_out_roughly(path, st);
    let opts = ScanOptions {
        temp_prefix: prefix.as_bytes(),
        accounts,
        rows,
        remove_own_temps: false,
        arriving: &[],
        excluded: &excluded,
        probe: true,
        scope: None,
        started: None,
        sieve: None,
        keep_all: false,
        digest: true,
        discard: true,
    };
    let scan_started = Instant::now();
    let result = scan(&root.dir, &opts)?;
    let scan_time = scan_started.elapsed();
    if result.stopped || result.unstable {
        return Ok(None);
    }
    let digest = digest(&result)?;
    drop(sorting);
    drop(by_inode);
    drop(glance);
    if digest != steady.scan {
        return Ok(None);
    }
    // What a finished sync does to the cache of chunks on their way, which only a killed one leaves.
    let cache = root.state_dir.open_dir(b"chunks")?;
    for name in cache.list()? {
        let _ = cache.unlink(&name);
    }

    let r = &steady.report;
    let mut report = SyncReport::new();
    report.outcome = if r.partial {
        SyncOutcome::Partial
    } else {
        SyncOutcome::Synced
    };
    report.skips.clone_from(&r.skips);
    report.warnings.clone_from(&r.warnings);
    report.meta_applied.clone_from(&r.meta_applied);
    report.events.clone_from(&r.events);
    report.conflicts = r.conflicts;
    report.excluded_here.clone_from(&r.excluded_here);
    report.not_synced_here.clone_from(&r.not_synced_here);
    report.rounds = 1;
    report.head = cursor;
    report.scan_time = scan_time;
    report.repeated = started.elapsed();
    report.agreed_since = Some(result.started);
    Ok(Some(Fast {
        report,
        steady,
        digest,
    }))
}

/// In a harness build, every sync the fast path takes is run the full way as well, and the two must
/// agree; `CAIRN_FP_FAST_ORACLE=0` turns that off, to see the fast path as a release build runs it.
pub fn oracle() -> bool {
    cfg!(feature = "failpoints") && std::env::var("CAIRN_FP_FAST_ORACLE").map_or(true, |v| v != "0")
}

/// What the full sync saw in its first round, for the oracle.
#[derive(Default)]
pub struct Trace {
    /// The digest of its first scan, and the head after its first look at the journal.
    pub first: Option<(Hash, u64)>,
    /// The record it left, if it had nothing to do and wrote nothing.
    pub steady: Option<Steady>,
}

fn same_report(a: &SyncReport, b: &SyncReport) -> Option<String> {
    let project = |r: &SyncReport| {
        format!(
            "outcome {:?}\nskips {:?}\nwarnings {:?}\nmeta_applied {:?}\nevents {:?}\nconflicts {}\n\
             commits {} ops {} chunks {} bytes {} steps {} rounds {} head {}\nplanned {:?}\n\
             excluded_here {:?}\nnot_synced_here {:?}\nactivity {}",
            r.outcome,
            r.skips,
            r.warnings,
            r.meta_applied,
            r.events,
            r.conflicts,
            r.commits,
            r.ops_pushed,
            r.chunks_uploaded,
            r.bytes_uploaded,
            r.steps_applied,
            r.rounds,
            r.head,
            r.planned,
            r.excluded_here,
            r.not_synced_here,
            r.activity.len(),
        )
    };
    let (pa, pb) = (project(a), project(b));
    (pa != pb).then(|| format!("fast:\n{pa}\nfull:\n{pb}"))
}

/// The fast path's answer against the full sync's, run after it on the same tree. Where the full sync
/// saw the same tree and journal in its first round, it must have done nothing, written nothing and
/// said the same; otherwise the tree moved in between, and there is nothing to compare.
pub fn compare(fast: Fast, full: (SyncReport, Trace)) -> Result<SyncReport> {
    let (report, trace) = full;
    if trace.first != Some((fast.digest, fast.steady.cursor)) {
        cairn_failpoint::hit("sync.fast.moved");
        return Ok(report);
    }
    let Some(steady) = &trace.steady else {
        bail!(
            "the fast path took this sync for one with nothing to do, and the full sync, on the same tree \
             and journal, did something or wrote to the state: {}",
            same_report(&fast.report, &report).unwrap_or_default()
        );
    };
    if steady.report != fast.steady.report || steady.generation != fast.steady.generation {
        bail!(
            "the fast path and the full sync have nothing to do, but differ in what they say: {:?} against {:?}",
            fast.steady,
            steady
        );
    }
    if let Some(diff) = same_report(&fast.report, &report) {
        bail!("the fast path and the full sync differ:\n{diff}");
    }
    cairn_failpoint::hit("sync.fast.confirmed");
    Ok(fast.report)
}
