//! `fsck`: every chunk hash, every manifest reference, and the tables against a replay of the journal.
//! It only reads: each chunk store is opened to be read alone, so a check of a backup's database
//! against a live store's chunks, through links to them, changes nothing of the live store.
//!
//! A revision retention gave up may have lost its content to a collection: that is counted, not
//! damage. One given up that is an object's head with an entry, or a mark naming no revision, is.

use crate::chunks::ChunkStore;
use crate::meta::TreeState;
use crate::repo::Repo;
use anyhow::Result;
use cairn_protocol::{Content, Hash, Manifest, ObjectId, Op, OpReject, RevisionId, Tree};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Debug, Default)]
pub struct FsckReport {
    pub namespaces: usize,
    pub commits: u64,
    pub revisions: usize,
    pub chunks: usize,
    /// Stored chunks no revision names: an interrupted upload, not damage.
    pub unreferenced: usize,
    /// Revisions retention gave up, and of them those whose content is gone, wholly or in part.
    pub pruned: usize,
    pub pruned_gone: usize,
    /// Stored chunks only revisions given up name: what a collection takes once their delay is past.
    pub awaiting: usize,
    pub errors: Vec<String>,
}

impl FsckReport {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

pub fn fsck(repo: &Repo) -> Result<FsckReport> {
    let mut report = FsckReport::default();
    for ns in repo.namespaces()? {
        report.namespaces += 1;
        check_namespace(repo, &ns, &mut report)?;
    }
    Ok(report)
}

/// Who names a manifest: each revision, with the size it says when it is the revision's file.
#[derive(Default)]
struct Naming {
    kept: Vec<(RevisionId, ObjectId, Option<u64>)>,
    pruned: Vec<(RevisionId, Option<u64>)>,
}

fn check_namespace(repo: &Repo, ns: &str, report: &mut FsckReport) -> Result<()> {
    let store = repo.chunks_read_only(ns)?;
    let stored = store.list()?;
    let revisions = repo.with_meta(|m| m.revisions(ns))?;
    let pruned: HashMap<RevisionId, i64> =
        repo.with_meta(|m| m.all_pruned(ns))?.into_iter().collect();
    report.revisions += revisions.len();
    report.pruned += pruned.len();

    let mut naming: BTreeMap<Hash, Naming> = BTreeMap::new();
    let mut known: HashSet<RevisionId> = HashSet::new();
    for r in &revisions {
        known.insert(r.revision);
        let is_pruned = pruned.contains_key(&r.revision);
        let file = match &r.body.content {
            Content::File { manifest, size } => Some((*manifest, Some(*size))),
            _ => None,
        };
        let streams = r.body.metadata.streams.iter().map(|s| (s.manifest, None));
        for (manifest, size) in file.into_iter().chain(streams) {
            let n = naming.entry(manifest).or_default();
            if is_pruned {
                n.pruned.push((r.revision, size));
            } else {
                n.kept.push((r.revision, r.object, size));
            }
        }
    }

    // What revisions kept name must all be here; what only revisions given up name may be gone.
    let mut referenced: BTreeSet<Hash> = BTreeSet::new();
    let mut given_up: BTreeSet<Hash> = BTreeSet::new();
    let mut gone: HashSet<RevisionId> = HashSet::new();
    for (manifest, n) in &naming {
        let kept = !n.kept.is_empty();
        let into = if kept { &mut referenced } else { &mut given_up };
        into.insert(*manifest);
        let m = match store.get(manifest) {
            Ok(Some(bytes)) => match Manifest::decode(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    report
                        .errors
                        .push(format!("{ns}: manifest {manifest}: {e}"));
                    continue;
                }
            },
            Ok(None) if !kept => {
                gone.extend(n.pruned.iter().map(|(r, _)| *r));
                continue;
            }
            Ok(None) => {
                let (rev, object, _) = n.kept[0];
                report.errors.push(format!(
                    "{ns}: revision {rev} of object {object} names missing manifest {manifest}"
                ));
                continue;
            }
            Err(e) => {
                report
                    .errors
                    .push(format!("{ns}: manifest {manifest}: {e}"));
                continue;
            }
        };
        let sizes = n
            .kept
            .iter()
            .map(|(r, _, s)| (r, s))
            .chain(n.pruned.iter().map(|(r, s)| (r, s)));
        for (rev, size) in sizes {
            if let Some(size) = size.filter(|s| *s != m.size) {
                report.errors.push(format!(
                    "{ns}: revision {rev} says {size} bytes, manifest {manifest} says {}",
                    m.size
                ));
            }
        }
        for c in m.chunks() {
            into.insert(c.hash);
            match store.len_of(&c.hash)? {
                None if !kept => gone.extend(n.pruned.iter().map(|(r, _)| *r)),
                None => report.errors.push(format!(
                    "{ns}: manifest {manifest} names missing chunk {}",
                    c.hash
                )),
                Some(len) if len != u64::from(c.len) => report.errors.push(format!(
                    "{ns}: chunk {} is {len} bytes, manifest {manifest} says {}",
                    c.hash, c.len
                )),
                Some(_) => {}
            }
        }
    }
    report.pruned_gone += gone.len();
    for rev in pruned.keys() {
        if !known.contains(rev) {
            report.errors.push(format!(
                "{ns}: revision {rev} is marked as given up by retention, and there is no such revision"
            ));
        }
    }
    let state = repo.with_meta(|m| m.tree_state(ns))?;
    let linked: HashSet<ObjectId> = state.entries.iter().map(|(_, _, o)| *o).collect();
    for (object, head) in &state.heads {
        if pruned.contains_key(head) && linked.contains(object) {
            report.errors.push(format!(
                "{ns}: object {object} has an entry, and its head {head} is marked as given up by retention"
            ));
        }
    }
    for hash in &stored {
        report.chunks += 1;
        if let Err(e) = store.check_passing(hash) {
            report.errors.push(format!("{ns}: {e}"));
        }
        if !referenced.contains(hash) {
            if given_up.contains(hash) {
                report.awaiting += 1;
            } else {
                report.unreferenced += 1;
            }
        }
    }

    let mut named = HashSet::new();
    let replayed = match replay(repo, ns, report, &mut named) {
        Ok(tree) => {
            compare_tables(ns, &tree, &repo.with_meta(|m| m.tree_state(ns))?, report);
            true
        }
        Err(e) => {
            report
                .errors
                .push(format!("{ns}: journal does not replay: {e:#}"));
            false
        }
    };
    // Each body is kept once, in `revisions`, and named by the commit that made it: one no commit
    // names is not in the history, save the root's first, made with the namespace.
    if replayed {
        for r in &revisions {
            let root = r.revision == RevisionId::ZERO && r.object == ObjectId::ROOT && r.seq == 0;
            if !root && !named.contains(&r.revision) {
                report.errors.push(format!(
                    "{ns}: revision {} of object {} at commit {} is named by no commit",
                    r.revision, r.object, r.seq
                ));
            }
        }
    }
    Ok(())
}

fn replay(
    repo: &Repo,
    ns: &str,
    report: &mut FsckReport,
    named: &mut HashSet<RevisionId>,
) -> Result<Tree> {
    let mut tree = Tree::new();
    let mut after = 0;
    loop {
        let batch = repo.with_meta(|m| m.changes(ns, after, 1000, 64 << 20))?;
        let Some(last) = batch.last() else { break };
        after = last.seq;
        for change in &batch {
            report.commits += 1;
            for op in &change.ops {
                if let Op::Create { revision, .. } | Op::Update { revision, .. } = op {
                    named.insert(*revision);
                }
                tree.apply(op)
                    .map_err(|e: OpReject| anyhow::anyhow!("seq {}: {e}", change.seq))?;
            }
        }
    }
    Ok(tree)
}

fn compare_tables(ns: &str, tree: &Tree, state: &TreeState, report: &mut FsckReport) {
    let mut replayed: Vec<(ObjectId, Vec<u8>, ObjectId)> = tree
        .entries()
        .map(|(l, o)| (l.parent, l.name.as_bytes().to_vec(), o))
        .collect();
    replayed.sort();
    let mut tables = state.entries.clone();
    tables.sort();
    if replayed != tables {
        report.errors.push(format!(
            "{ns}: entries table ({} rows) differs from the journal's replay ({} rows)",
            tables.len(),
            replayed.len()
        ));
    }
    let heads: Vec<_> = tree.nodes.iter().map(|(o, n)| (*o, n.rev)).collect();
    let mut table_heads = state.heads.clone();
    table_heads.sort();
    if heads != table_heads {
        report.errors.push(format!(
            "{ns}: object heads differ from the journal's replay"
        ));
    }
}
