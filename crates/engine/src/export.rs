//! Writing a point in history into a separate folder, for inspection or an exact restore. Never
//! touches the sync root or the server.

use crate::client::{Client, Retries};
use crate::fsops::{self, AsFd, Dir, OwnedFd};
use crate::metadata::{self, Accounts, ApplyPolicy};
use crate::root::Config;
use crate::scan::display_path;
use crate::sync::fetch_changes;
use anyhow::{anyhow, bail, Context, Result};
use cairn_protocol::wire::MAX_CHUNK_BATCH_BYTES;
use cairn_protocol::{
    Content, Hash, Manifest, Name, ObjectId, ObjectKind, RevisionId, Skip, SkipKind, Tree,
};
use std::collections::BTreeMap;
use std::path::Path;

/// The tree as of a sequence number, by replaying the journal.
pub fn tree_at(client: &Client, at: Option<u64>) -> Result<(Tree, u64)> {
    let mut tree = Tree::new();
    let mut seq = 0;
    match at {
        None => fetch_changes(client, &mut tree, &mut seq, &mut Default::default())?,
        Some(limit) => loop {
            let resp = client.changes(seq)?;
            if limit > resp.head {
                bail!(
                    "sequence {limit} is in the future: the server is at {}",
                    resp.head
                );
            }
            let batch_empty = resp.changes.is_empty();
            for change in &resp.changes {
                if change.seq > limit {
                    return Ok((tree, seq));
                }
                for op in &change.ops {
                    tree.apply(op).map_err(|e| {
                        anyhow!("the journal does not replay at seq {}: {e}", change.seq)
                    })?;
                }
                seq = change.seq;
            }
            if batch_empty || seq >= limit {
                break;
            }
        },
    }
    Ok((tree, seq))
}

fn write_file(
    client: &Client,
    dir: &Dir,
    tmp: &[u8],
    manifest: &Hash,
    size: u64,
) -> Result<OwnedFd> {
    let got = client.download(&[*manifest])?;
    let (_, bytes) = got
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no manifest {manifest}"))?;
    let m = Manifest::decode(&bytes)?;
    let fd = dir.create_new(tmp, 0o600)?;
    // The size alone makes the holes: only data segments are ever written.
    fsops::set_len(fd.as_fd(), size, crate::chunker::has_holes(&m))?;
    let placed: Vec<(u64, &cairn_protocol::ChunkRef)> = m.placed_chunks().collect();
    let mut i = 0;
    while i < placed.len() {
        let mut batch = Vec::new();
        let mut bytes = 0usize;
        while i + batch.len() < placed.len()
            && (batch.is_empty()
                || bytes + placed[i + batch.len()].1.len as usize <= MAX_CHUNK_BATCH_BYTES)
        {
            let (_, c) = placed[i + batch.len()];
            batch.push(c.hash);
            bytes += c.len as usize;
        }
        let frames = client.download(&batch)?;
        for (k, (_, data)) in frames.iter().enumerate() {
            fsops::write_all_at(fd.as_fd(), data, placed[i + k].0)?;
        }
        i += batch.len();
    }
    crate::chunker::restore_holes(fd.as_fd(), &m);
    fsops::sync_fd(fd.as_fd())?;
    Ok(fd)
}

/// What an export wrote, and what it did not.
#[derive(Debug, Default)]
pub struct Exported {
    /// The sequence it is the tree as of.
    pub seq: u64,
    /// What was written and could not be applied in full.
    pub warnings: Vec<Skip>,
    /// What the folder's retention gave up, left out by `skip_pruned`: each path, and when.
    pub left_out: Vec<(Vec<Name>, i64)>,
}

/// Those of `objects` whose revision in `tree` the folder's retention gave up, and when.
pub(crate) fn given_up(
    client: &Client,
    tree: &Tree,
    objects: impl IntoIterator<Item = ObjectId>,
) -> Result<BTreeMap<ObjectId, i64>> {
    let mut by_rev = BTreeMap::new();
    for id in objects {
        if let Some(n) = tree.nodes.get(&id) {
            if crate::export::names_content(&n.body) {
                by_rev.insert(n.rev, id);
            }
        }
    }
    if by_rev.is_empty() {
        return Ok(BTreeMap::new());
    }
    let revs: Vec<RevisionId> = by_rev.keys().copied().collect();
    Ok(client
        .pruned(&revs)?
        .into_iter()
        .filter_map(|p| by_rev.get(&p.revision).map(|o| (*o, p.at)))
        .collect())
}

/// Whether a revision names content a collection could take: a file's, or a stream's.
pub(crate) fn names_content(body: &cairn_protocol::RevisionBody) -> bool {
    matches!(body.content, Content::File { .. }) || !body.metadata.streams.is_empty()
}

/// The refusal of a restore or export that would need what retention gave up, naming the first few.
pub(crate) fn refuse_given_up(what: &str, gone: &[(Vec<Name>, i64)]) -> anyhow::Error {
    let mut names: Vec<String> = gone
        .iter()
        .take(10)
        .map(|(p, at)| format!("{} (on {})", display_path(p), crate::sync::utc_date(*at)))
        .collect();
    if gone.len() > 10 {
        names.push(format!("and {} more", gone.len() - 10));
    }
    anyhow!(
        "the folder's retention removed {} file(s) this {what} would need: {}. --skip-pruned does the rest and lists them",
        gone.len(),
        names.join(", ")
    )
}

/// Write the subtree at `path` as of `at` into `dest`, which must be missing or empty. A file whose
/// revision then the folder's retention gave up refuses it, unless `skip_pruned`, which leaves it out
/// and says so.
pub fn export(
    config: &Config,
    at: Option<u64>,
    path: &[Name],
    dest: &Path,
    policy: &ApplyPolicy,
    skip_pruned: bool,
) -> Result<Exported> {
    let client = Client::new(config, Retries::Short)?;
    let (tree, seq) = tree_at(&client, at)?;
    let top = tree
        .resolve(path)
        .ok_or_else(|| anyhow!("nothing at {} as of sequence {seq}", display_path(path)))?;
    if tree.nodes[&top].kind() != ObjectKind::Dir {
        bail!(
            "{} is not a directory; export its parent",
            display_path(path)
        );
    }
    // What retention gave up is known before anything is written.
    let mut inside = Vec::new();
    let mut stack = vec![(top, Vec::new())];
    while let Some((d, p)) = stack.pop() {
        for (name, child) in tree.children(d) {
            let mut q: Vec<Name> = p.clone();
            q.push(name.clone());
            if tree.nodes[&child].kind() == ObjectKind::Dir {
                stack.push((child, q.clone()));
            }
            inside.push((child, q));
        }
    }
    let gone = given_up(&client, &tree, inside.iter().map(|(o, _)| *o))?;
    let mut left_out: Vec<(Vec<Name>, i64)> = inside
        .iter()
        .filter_map(|(o, p)| gone.get(o).map(|at| (p.clone(), *at)))
        .collect();
    left_out.sort();
    if !left_out.is_empty() && !skip_pruned {
        return Err(refuse_given_up("export", &left_out));
    }
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let dest_dir = Dir::open_root(dest)?;
    if !dest_dir.list()?.is_empty() {
        bail!(
            "{} is not empty; export writes only into an empty folder",
            dest.display()
        );
    }
    let accounts = Accounts::load();
    let mut warnings = Vec::new();
    let mut warn = |path: &[Name], problems: Vec<String>| {
        for p in problems {
            warnings.push(Skip {
                path: display_path(path),
                kind: SkipKind::CannotApply,
                detail: Some(p),
            });
        }
    };

    // Directories top down, then everything else; final directory metadata comes last.
    let mut dirs: Vec<(ObjectId, Vec<Name>)> = Vec::new();
    let mut files: Vec<(ObjectId, Vec<Name>)> = Vec::new();
    let mut stack: Vec<(ObjectId, Vec<Name>)> = vec![(top, Vec::new())];
    while let Some((at_dir, at_path)) = stack.pop() {
        if !at_path.is_empty() {
            dest_dir
                .open_path(at_path[..at_path.len() - 1].iter())?
                .mkdir(at_path[at_path.len() - 1].as_bytes(), 0o700)?;
        }
        for (name, child) in tree.children(at_dir) {
            let mut p = at_path.clone();
            p.push(name.clone());
            if tree.nodes[&child].kind() == ObjectKind::Dir {
                stack.push((child, p));
            } else {
                files.push((child, p));
            }
        }
        dirs.push((at_dir, at_path));
    }

    let mut first_path: BTreeMap<ObjectId, Vec<Name>> = BTreeMap::new();
    for (id, p) in &files {
        if gone.contains_key(id) {
            continue;
        }
        let (name, parents) = p.split_last().expect("a file has a name");
        let dir = dest_dir.open_path(parents.iter())?;
        if let Some(first) = first_path.get(id) {
            let (fname, fparents) = first.split_last().expect("a file has a name");
            dest_dir.open_path(fparents.iter())?.hard_link(
                fname.as_bytes(),
                &dir,
                name.as_bytes(),
            )?;
            continue;
        }
        let node = &tree.nodes[id];
        let tmp = format!(".cairn-export-{}", hex::encode(rand::random::<[u8; 8]>())).into_bytes();
        let fd = match &node.body.content {
            Content::File { manifest, size } => {
                Some(write_file(&client, &dir, &tmp, manifest, *size)?)
            }
            Content::Symlink { target } => {
                dir.symlink(&target.0, &tmp)?;
                None
            }
            Content::Dir => unreachable!("directories were sorted out above"),
        };
        warn(
            p,
            metadata::apply(
                &dir,
                &tmp,
                node.kind(),
                &node.body.metadata,
                None,
                policy,
                &accounts,
            ),
        );
        if let Some(fd) = fd {
            fsops::sync_fd(fd.as_fd())?;
        }
        dir.rename_new(&tmp, &dir, name.as_bytes())?;
        first_path.insert(*id, p.clone());
    }

    dirs.sort_by_key(|d| std::cmp::Reverse(d.1.len()));
    for (id, p) in &dirs {
        let node = &tree.nodes[id];
        let (dir, name): (Dir, Vec<u8>) = match p.split_last() {
            Some((last, parents)) => (
                dest_dir.open_path(parents.iter())?,
                last.as_bytes().to_vec(),
            ),
            None => (dest_dir.dup()?, b".".to_vec()),
        };
        if node.rev == cairn_protocol::RevisionId::ZERO {
            continue;
        }
        warn(
            p,
            metadata::apply(
                &dir,
                &name,
                ObjectKind::Dir,
                &node.body.metadata,
                None,
                policy,
                &accounts,
            ),
        );
    }
    dest_dir.sync()?;
    Ok(Exported {
        seq,
        warnings,
        left_out,
    })
}
