//! `fsck`: every chunk hash, every manifest reference, and the tables against a replay of the journal.

use crate::chunks::ChunkStore;
use crate::meta::TreeState;
use crate::repo::Repo;
use anyhow::Result;
use cairn_protocol::{Content, Hash, Manifest, ObjectId, Op, OpReject, RevisionId, Tree};
use std::collections::{BTreeSet, HashSet};

#[derive(Debug, Default)]
pub struct FsckReport {
    pub namespaces: usize,
    pub commits: u64,
    pub revisions: usize,
    pub chunks: usize,
    /// Stored chunks no revision names: an interrupted upload, not damage.
    pub unreferenced: usize,
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

fn check_namespace(repo: &Repo, ns: &str, report: &mut FsckReport) -> Result<()> {
    let store = repo.chunks(ns)?;
    let stored = store.list()?;
    let revisions = repo.with_meta(|m| m.revisions(ns))?;
    report.revisions += revisions.len();

    let mut referenced: BTreeSet<Hash> = BTreeSet::new();
    let mut manifests: BTreeSet<Hash> = BTreeSet::new();
    for r in &revisions {
        if let Content::File { manifest, size } = &r.body.content {
            manifests.insert(*manifest);
            match store.get(manifest) {
                Ok(Some(bytes)) => match Manifest::decode(&bytes) {
                    Ok(m) if m.size == *size => {}
                    Ok(m) => report.errors.push(format!(
                        "{ns}: revision {} says {size} bytes, manifest {manifest} says {}",
                        r.revision, m.size
                    )),
                    Err(e) => report
                        .errors
                        .push(format!("{ns}: manifest {manifest}: {e}")),
                },
                Ok(None) => report.errors.push(format!(
                    "{ns}: revision {} of object {} names missing manifest {manifest}",
                    r.revision, r.object
                )),
                Err(e) => report
                    .errors
                    .push(format!("{ns}: manifest {manifest}: {e}")),
            }
        }
        manifests.extend(r.body.metadata.streams.iter().map(|s| s.manifest));
    }
    for hash in &manifests {
        referenced.insert(*hash);
        let Ok(Some(bytes)) = store.get(hash) else {
            continue;
        };
        let Ok(m) = Manifest::decode(&bytes) else {
            continue;
        };
        for c in m.chunks() {
            referenced.insert(c.hash);
            match store.len_of(&c.hash)? {
                None => report.errors.push(format!(
                    "{ns}: manifest {hash} names missing chunk {}",
                    c.hash
                )),
                Some(len) if len != u64::from(c.len) => report.errors.push(format!(
                    "{ns}: chunk {} is {len} bytes, manifest {hash} says {}",
                    c.hash, c.len
                )),
                Some(_) => {}
            }
        }
    }
    for hash in &stored {
        report.chunks += 1;
        if let Err(e) = store.check_passing(hash) {
            report.errors.push(format!("{ns}: {e}"));
        }
        if !referenced.contains(hash) {
            report.unreferenced += 1;
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
