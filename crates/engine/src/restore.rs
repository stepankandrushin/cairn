//! Restoring past state on the server: always a new commit with new revisions, so that a restore can
//! be undone like anything else (invariant 7).

use crate::client::Client;
use crate::export::tree_at;
use crate::merge::{conflict_name, in_git_dir};
use crate::root::Root;
use crate::scan::display_path;
use crate::schedule::{schedule, Topo, TopoOp};
use anyhow::{anyhow, bail, Result};
use cairn_protocol::wire::{Change, CommitRequest, CommitResponse};
use cairn_protocol::{Content, Link, Name, ObjectId, Op, OpId, RevisionId, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// One revision of an object, as the journal tells it.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub seq: u64,
    pub time: i64,
    pub device: String,
    pub revision: RevisionId,
    pub size: Option<u64>,
    pub mode: u32,
    pub restored_from: Option<RevisionId>,
}

/// Every change in the journal.
pub fn journal(client: &Client) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    let mut after = 0;
    loop {
        let resp = client.changes(after)?;
        let Some(last) = resp.changes.last() else {
            break;
        };
        after = last.seq;
        out.extend(resp.changes);
        if after >= resp.head {
            break;
        }
    }
    Ok(out)
}

/// The revisions of the object at `path` as of `at`.
pub fn history(root: &Root, path: &[Name], at: Option<u64>) -> Result<Vec<HistoryEntry>> {
    let client = Client::new(&root.config)?;
    let (tree, seq) = tree_at(&client, at)?;
    let object = tree
        .resolve(path)
        .ok_or_else(|| anyhow!("nothing at {} as of sequence {seq}", display_path(path)))?;
    let mut out = Vec::new();
    for change in journal(&client)? {
        for op in &change.ops {
            let (o, revision, body, restored_from) = match op {
                Op::Create {
                    object,
                    revision,
                    body,
                    ..
                } => (*object, *revision, body, None),
                Op::Update {
                    object,
                    revision,
                    body,
                    restored_from,
                    ..
                } => (*object, *revision, body, *restored_from),
                _ => continue,
            };
            if o != object {
                continue;
            }
            out.push(HistoryEntry {
                seq: change.seq,
                time: change.time,
                device: change.device_name.clone(),
                revision,
                size: match &body.content {
                    Content::File { size, .. } => Some(*size),
                    _ => None,
                },
                mode: body.metadata.mode,
                restored_from,
            });
        }
    }
    Ok(out)
}

fn topo(tree: &Tree) -> Topo {
    tree.live()
        .filter(|(id, _)| *id != ObjectId::ROOT)
        .map(|(id, n)| (id, (n.kind(), n.links.clone())))
        .collect()
}

/// A free name for something restored over a name now in use.
fn free_name(desired: &BTreeMap<Link, ObjectId>, link: &Link, seq: u64, in_git: bool) -> Link {
    let mut attempt = 0;
    loop {
        let name = conflict_name(
            &link.name,
            "restored",
            &format!("seq {seq}"),
            attempt,
            in_git,
        );
        let candidate = Link {
            parent: link.parent,
            name,
        };
        if !desired.contains_key(&candidate) {
            return candidate;
        }
        attempt += 1;
    }
}

/// The operations that bring the subtree at `path` back to how it was at `at`.
fn restore_ops(now: &Tree, then: &Tree, top: ObjectId, seq: u64) -> Result<Vec<Op>> {
    let restored: BTreeSet<ObjectId> = then.subtree(top);
    let mut to = topo(now);
    // What is under this path now and was not then goes away; the journal keeps it.
    if now.is_live(top) {
        let inside = now.subtree(top);
        for o in &inside {
            if *o == ObjectId::ROOT || restored.contains(o) {
                continue;
            }
            // A hard link outside this path is not part of what is being restored.
            let Some(entry) = to.get_mut(o) else { continue };
            entry.1.retain(|l| !inside.contains(&l.parent));
            if entry.1.is_empty() {
                to.remove(o);
            }
        }
    }
    // A deleted parent comes back with its child.
    let mut wanted: BTreeSet<ObjectId> = restored.clone();
    let mut at = top;
    while at != ObjectId::ROOT {
        let Some(up) = then.nodes.get(&at).and_then(|n| n.links.iter().next()) else {
            break;
        };
        at = up.parent;
        if at != ObjectId::ROOT && !now.is_live(at) {
            wanted.insert(at);
        }
    }
    let mut desired: BTreeMap<Link, ObjectId> = BTreeMap::new();
    for (id, (_, links)) in &to {
        if !wanted.contains(id) {
            for l in links {
                desired.insert(l.clone(), *id);
            }
        }
    }
    for id in &wanted {
        if *id == ObjectId::ROOT {
            continue;
        }
        let n = then
            .nodes
            .get(id)
            .ok_or_else(|| anyhow!("object {id} is not in that state"))?;
        let mut links = BTreeSet::new();
        for l in &n.links {
            // The name is in use by something outside this restore: both stay.
            let placed = if desired.contains_key(l) {
                let in_git = then
                    .path_of_link(l)
                    .is_some_and(|p| in_git_dir(&p[..p.len() - 1]));
                free_name(&desired, l, seq, in_git)
            } else {
                l.clone()
            };
            desired.insert(placed.clone(), *id);
            links.insert(placed);
        }
        to.insert(*id, (n.kind(), links));
    }

    let mut updates = Vec::new();
    for id in &wanted {
        let Some(old) = then.nodes.get(id) else {
            continue;
        };
        let was = now
            .nodes
            .get(id)
            .ok_or_else(|| anyhow!("object {id} is unknown to the server"))?;
        let relinked = to.get(id).is_some_and(|t| t.1 != was.links);
        if was.body != old.body || relinked {
            updates.push(Op::Update {
                object: *id,
                base: was.rev,
                revision: RevisionId::random(),
                body: old.body.clone(),
                restored_from: Some(old.rev),
            });
        }
    }
    let mut ops = Vec::new();
    for step in schedule(&topo(now), &to)? {
        ops.push(match step {
            // Every object here has existed before, so it comes back as itself.
            TopoOp::Create { object, link } | TopoOp::Link { object, link } => {
                Op::Link { object, link }
            }
            TopoOp::Unlink { object, link } => Op::Unlink {
                object,
                link,
                base: now.nodes.get(&object).map(|n| n.rev),
            },
            TopoOp::Move { object, from, to } => Op::Move { object, from, to },
        });
    }
    // After structure: what is coming back is linked by now, and may be updated.
    ops.extend(updates);
    Ok(ops)
}

#[derive(Debug)]
pub struct RestoreOutcome {
    pub seq: Option<u64>,
    pub ops: usize,
}

fn commit(client: &Client, root: &Root, ops: Vec<Op>) -> Result<RestoreOutcome> {
    if ops.is_empty() {
        return Ok(RestoreOutcome { seq: None, ops: 0 });
    }
    let count = ops.len();
    let req = CommitRequest {
        op_id: OpId::random(),
        device: root.config.device,
        device_name: root.config.device_name.clone(),
        ops,
    };
    match client.commit(&req)? {
        CommitResponse::Committed { seq } => Ok(RestoreOutcome {
            seq: Some(seq),
            ops: count,
        }),
        CommitResponse::Rejected { reject } => {
            bail!("the server refused the restore, probably because something just changed; try again: {reject:?}")
        }
    }
}

/// Bring `path` back to its state as of sequence `at`, as a new commit. `cairn sync` then applies it.
pub fn restore_at(root: &Root, path: &[Name], at: u64) -> Result<RestoreOutcome> {
    let client = Client::new(&root.config)?;
    let (then, seq) = tree_at(&client, Some(at))?;
    let (now, _) = tree_at(&client, None)?;
    let top = then
        .resolve(path)
        .ok_or_else(|| anyhow!("nothing at {} as of sequence {seq}", display_path(path)))?;
    commit(&client, root, restore_ops(&now, &then, top, seq)?)
}

/// Bring the object at `path` back to one revision, as a new revision. Only its own state changes.
pub fn restore_revision(
    root: &Root,
    path: &[Name],
    revision: RevisionId,
) -> Result<RestoreOutcome> {
    let client = Client::new(&root.config)?;
    let (now, _) = tree_at(&client, None)?;
    let object = now.resolve(path).ok_or_else(|| {
        anyhow!(
            "nothing at {} now; to bring back something deleted, use --at SEQ",
            display_path(path)
        )
    })?;
    let mut body = None;
    for change in journal(&client)? {
        for op in &change.ops {
            match op {
                Op::Create {
                    object: o,
                    revision: r,
                    body: b,
                    ..
                }
                | Op::Update {
                    object: o,
                    revision: r,
                    body: b,
                    ..
                } if *o == object && *r == revision => {
                    body = Some(b.clone());
                }
                _ => {}
            }
        }
    }
    let body = body.ok_or_else(|| anyhow!("{} has no revision {revision}", display_path(path)))?;
    commit(
        &client,
        root,
        vec![Op::Update {
            object,
            base: now.nodes[&object].rev,
            revision: RevisionId::random(),
            body,
            restored_from: Some(revision),
        }],
    )
}
