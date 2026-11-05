//! Giving each scanned inode its object identity. Inodes are only hints: they get reused, and editors
//! save by replacing the file, so a name match fills in where an inode says nothing.

use crate::fsops::birth_mark;
use crate::merge::{LNode, Shared};
use crate::metadata::rebase_local_changes;
use crate::scan::{ScanResult, Scanned};
use crate::state::{Row, Synced};
use cairn_protocol::{
    ConflictOf, Content, Link, Metadata, Name, ObjectId, ObjectKind, OpId, RevisionBody, Timestamp,
    Tree,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A local file renamed aside as a conflict copy: which object it now is, whatever the inode's row
/// says, so that a crash right after the rename cannot confuse it with the original.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyIntent {
    pub copy: ObjectId,
    pub dev: u64,
    pub ino: u64,
    /// What proves it is still that inode: the mark lives until the next finished sync, and the copy
    /// can be gone by then, its number handed to something new. Absent in a mark an older version left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is: Option<(ObjectKind, Option<Timestamp>)>,
    pub conflict_of: ConflictOf,
    /// Each name the copy moves from and to, as paths, so that a kill partway through finishes the
    /// moves rather than leaving the copy at the original's names. Empty in a mark an older version left.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moves: Vec<(Vec<Name>, Vec<Name>)>,
}

impl CopyIntent {
    fn marks(&self, node: &Scanned) -> bool {
        self.is.as_ref().is_none_or(|(kind, btime)| {
            *kind == node.content.kind() && birth_mark(*btime) == birth_mark(node.fp.btime)
        })
    }
}

/// What the server held for an object when this device set about bringing it here, noted before the
/// first step. It claims nothing: the next scan compares it with the disk to see how far that got, so
/// that what arrived is not read as this device's own change, and a kill or a full disk costs nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arriving {
    pub object: ObjectId,
    pub body: RevisionBody,
    pub links: BTreeSet<Link>,
    /// Where the plan puts it here, which a rename made here can make different from the server's.
    #[serde(default)]
    pub names: BTreeSet<Link>,
    /// A folder something happens in, whose timestamp moves until the last pass puts it back.
    #[serde(default)]
    pub folder_touched: bool,
}

#[derive(Debug, Default)]
pub struct Identified {
    pub local: BTreeMap<ObjectId, LNode>,
    pub shared: BTreeMap<ObjectId, Shared>,
    /// The scanned inode behind each local object.
    pub scanned: BTreeMap<ObjectId, Scanned>,
}

/// Several rows can hold one inode number: a deleted file's row lives until the next finalize, and
/// filesystems such as ext4 hand its number straight to the next new file.
fn rows_by_inode(rows: &BTreeMap<ObjectId, Row>) -> HashMap<(u64, u64), Vec<ObjectId>> {
    let mut by_inode: HashMap<(u64, u64), Vec<ObjectId>> = HashMap::new();
    for (id, r) in rows {
        by_inode.entry((r.fp.dev, r.fp.ino)).or_default().push(*id);
    }
    by_inode
}

pub fn identify(
    scan: &ScanResult,
    base: &Tree,
    remote: &Tree,
    rows: &BTreeMap<ObjectId, Row>,
    intents: &[CopyIntent],
    arriving: &BTreeMap<ObjectId, Arriving>,
    landed: &BTreeSet<OpId>,
) -> Identified {
    let by_inode = rows_by_inode(rows);
    let copies: HashMap<(u64, u64), &CopyIntent> =
        intents.iter().map(|i| ((i.dev, i.ino), i)).collect();
    let synced_names: HashMap<&Link, ObjectId> = rows
        .iter()
        .filter_map(|(id, r)| r.synced.as_ref().map(|s| (id, s)))
        .flat_map(|(id, s)| s.links.iter().map(move |l| (l, *id)))
        .collect();
    let arriving_names: HashMap<&Link, &Arriving> = arriving
        .values()
        .flat_map(|a| a.links.iter().chain(&a.names).map(move |l| (l, a)))
        .collect();

    let mut claimed: BTreeSet<ObjectId> = BTreeSet::new();
    claimed.insert(ObjectId::ROOT);
    let mut ids: Vec<Option<ObjectId>> = vec![None; scan.nodes.len()];

    // First the intents, then inodes: both identify a file wherever it has moved to.
    for (i, node) in scan.nodes.iter().enumerate() {
        if let Some(intent) = copies.get(&(node.fp.dev, node.fp.ino)) {
            if intent.marks(node) && claimed.insert(intent.copy) {
                ids[i] = Some(intent.copy);
            }
        }
    }
    for (i, node) in scan.nodes.iter().enumerate() {
        if ids[i].is_some() {
            continue;
        }
        let Some(candidates) = by_inode.get(&(node.fp.dev, node.fp.ino)) else {
            continue;
        };
        // A birth time that differs proves the inode number was reused. Without birth times, the row
        // whose whole fingerprint still holds is the likeliest to be this file's.
        let found = candidates
            .iter()
            .filter(|id| {
                let row = &rows[*id];
                row.content.kind() == node.content.kind()
                    && birth_mark(row.fp.btime) == birth_mark(node.fp.btime)
                    && !claimed.contains(*id)
            })
            .max_by_key(|id| rows[*id].fp.current() == node.fp.current());
        if let Some(id) = found {
            claimed.insert(*id);
            ids[i] = Some(*id);
        }
    }

    // Then names, top down, so that each parent is known before its children.
    let mut order: Vec<usize> = (0..scan.nodes.len()).collect();
    order.sort_by_key(|&i| {
        let n = &scan.nodes[i];
        (
            n.content != Content::Dir,
            n.paths.iter().map(Vec::len).min().unwrap_or(0),
        )
    });
    let mut dir_ids: HashMap<Vec<Name>, ObjectId> = HashMap::new();
    dir_ids.insert(Vec::new(), ObjectId::ROOT);
    let mut assign = vec![];
    for i in order {
        let node = &scan.nodes[i];
        if ids[i].is_none() {
            ids[i] = node.paths.iter().find_map(|path| {
                let (name, parent_path) = path.split_last()?;
                let link = Link {
                    parent: *dir_ids.get(parent_path)?,
                    name: name.clone(),
                };
                // What was on its way to this name, as far as a look can tell: `settle_arriving` decides,
                // and what a person put there instead is new, and merges as new.
                let on_its_way = arriving_names
                    .get(&link)
                    .filter(|a| {
                        a.body.content.kind() == node.content.kind() && !claimed.contains(&a.object)
                    })
                    .map(|a| a.object);
                // Only what this device has a record of: anything else at that name is new here, and
                // merges as new.
                let by_base = base.entry_at(&link).filter(|c| {
                    base.nodes
                        .get(c)
                        .is_some_and(|n| n.kind() == node.content.kind())
                        && rows.contains_key(c)
                });
                let candidate = on_its_way.or(by_base).or_else(|| {
                    synced_names
                        .get(&link)
                        .copied()
                        .filter(|c| rows[c].content.kind() == node.content.kind())
                })?;
                claimed.insert(candidate).then_some(candidate)
            });
        }
        let id = *ids[i].get_or_insert_with(ObjectId::random);
        if node.content == Content::Dir {
            for path in &node.paths {
                dir_ids.insert(path.clone(), id);
            }
        }
        assign.push((i, id));
    }

    let mut out = Identified::default();
    let intent_of: HashMap<ObjectId, &CopyIntent> = intents.iter().map(|i| (i.copy, i)).collect();
    let mut found: BTreeSet<ObjectId> = assign.iter().map(|(_, id)| *id).collect();
    found.insert(ObjectId::ROOT);
    for (id, row) in rows {
        // What this device removed itself shares nothing with the server any more, unless it is here
        // after all because the removal did not happen.
        if !found.contains(id) && !holds_what_it_synced(row) {
            continue;
        }
        if let Some(s) = shared_state(row, *id, remote, landed) {
            out.shared.insert(*id, s);
        }
    }
    let add = |id: ObjectId, node: &Scanned, links: BTreeSet<Link>, out: &mut Identified| {
        // Something new that cannot be fully read is not pushed half-described; it waits, reported.
        let known = base.is_live(id) || remote.is_live(id);
        if node.metadata_incomplete && !known {
            return;
        }
        // A folder here only for the sake of what this device excludes is nothing to anyone else, or
        // deleting it on another device would bring it back for ever. If the server has one at that
        // path, the two are merged as usual.
        if node.only_excluded && !known && !node.paths.iter().any(|p| remote.resolve(p).is_some()) {
            return;
        }
        let (stored, apply_from) = stored_terms(id, node, base, remote, rows, &out.shared);
        out.local.insert(
            id,
            LNode {
                links,
                content: node.content.clone(),
                apply_from,
                stored,
                carried: false,
                conflict_of: intent_of.get(&id).map(|i| i.conflict_of.clone()),
            },
        );
        out.scanned.insert(id, node.clone());
    };
    add(ObjectId::ROOT, &scan.root, BTreeSet::new(), &mut out);
    for (i, id) in assign {
        let node = &scan.nodes[i];
        let links: BTreeSet<Link> = node
            .paths
            .iter()
            .filter_map(|path| {
                let (name, parent_path) = path.split_last()?;
                Some(Link {
                    parent: *dir_ids.get(parent_path)?,
                    name: name.clone(),
                })
            })
            .collect();
        if links.is_empty() {
            continue;
        }
        add(id, node, links, &mut out);
    }

    drop_orphans_of_skipped(&mut out, base, remote);
    carry_unobserved(scan, base, remote, rows, &dir_ids, &mut out);
    out
}

/// What this scan shows of what was on its way: for each object it can speak for, what reached the disk
/// goes into its row as shared with the server, and the rest stays to be applied. Returns the rows
/// changed and the objects answered; one that could not be observed is asked again next time.
pub fn settle_arriving(
    identified: &Identified,
    arriving: &BTreeMap<ObjectId, Arriving>,
    holds: &BTreeMap<ObjectId, Metadata>,
    base: &Tree,
    rows: &mut BTreeMap<ObjectId, Row>,
) -> (Vec<ObjectId>, Vec<ObjectId>) {
    let (mut changed, mut answered) = (Vec::new(), Vec::new());
    for (id, a) in arriving {
        let Some(l) = identified.local.get(id) else {
            answered.push(*id);
            continue;
        };
        let Some(node) = identified.scanned.get(id).filter(|_| !l.carried) else {
            continue;
        };
        answered.push(*id);
        let row = settled_row(
            *id,
            a,
            holds.get(id),
            l.links.clone(),
            node,
            identified,
            base,
            rows,
        );
        if let Some(row) = row {
            rows.insert(*id, row);
            changed.push(*id);
        }
    }
    (changed, answered)
}

#[allow(clippy::too_many_arguments)]
fn settled_row(
    id: ObjectId,
    a: &Arriving,
    holds: Option<&Metadata>,
    now: BTreeSet<Link>,
    node: &Scanned,
    identified: &Identified,
    base: &Tree,
    rows: &BTreeMap<ObjectId, Row>,
) -> Option<Row> {
    let old = rows.get(&id);
    let known = base.get_live(id);
    let shared = identified.shared.get(&id);
    let shared_body = shared.and_then(|s| s.body.clone());
    let baseline = match &shared_body {
        Some(_) => old
            .and_then(|o| o.synced.as_ref())
            .map(|s| s.applied.clone()),
        None => old.and_then(|o| o.applied.clone()),
    };
    // A folder still private from a killed run: what is on disk is no pair of its record, whatever
    // else arrived since, until the last pass has given it its metadata.
    let still_unapplied = shared_body.is_some()
        && old
            .and_then(|o| o.synced.as_ref())
            .is_some_and(|s| s.unapplied);
    let from_body = shared_body.or_else(|| known.map(|n| n.body.clone()));
    let is_held = |l: &Link| {
        now.contains(l)
            || old
                .and_then(|o| o.held.as_ref())
                .is_none_or(|held| held.contains(l))
    };
    // An entry this device gave up before moving or removing it is no ancestor of anything.
    let from_links: BTreeSet<Link> = shared
        .and_then(|s| s.links.clone())
        .or_else(|| known.map(|n| n.links.clone()))
        .unwrap_or_default()
        .into_iter()
        .filter(is_held)
        .collect();

    let whole = node.content == a.body.content;
    // New content comes with its metadata, whole, by one rename: this is what the filesystem then held,
    // and anything else on disk has been changed here.
    let held = || {
        let mut held = holds.unwrap_or(&node.metadata).clone();
        held.btime = node.metadata.btime;
        held
    };
    let (body, applied, unapplied) = match &from_body {
        Some(from) if from.content == a.body.content => match &baseline {
            Some(baseline) if !node.metadata_incomplete => {
                let (metadata, applied) = crate::metadata::arrived_fields(
                    &from.metadata,
                    baseline,
                    &a.body.metadata,
                    &node.metadata,
                );
                (
                    RevisionBody {
                        content: from.content.clone(),
                        metadata,
                    },
                    applied,
                    still_unapplied,
                )
            }
            Some(baseline) => (from.clone(), baseline.clone(), still_unapplied),
            // Known, with no record of having matched it: the server's record stands, against the disk.
            None => (from.clone(), node.metadata.clone(), true),
        },
        Some(_) if whole => (a.body.clone(), held(), false),
        Some(from) => (
            from.clone(),
            baseline.unwrap_or_else(|| node.metadata.clone()),
            false,
        ),
        // A folder is private until the last pass; a file was complete before its rename.
        None if whole && node.content == Content::Dir => {
            (a.body.clone(), node.metadata.clone(), true)
        }
        None if whole => (a.body.clone(), held(), false),
        // Something else stands at that name, and this device never had this object.
        None => return None,
    };

    // An entry that came shows the rest of the step was reached too. Otherwise nothing arrived: an
    // entry gone with none in its place is a person's.
    let came = a.links.difference(&from_links).any(|l| now.contains(l));
    let links: BTreeSet<Link> = if came {
        from_links
            .intersection(&a.links)
            .cloned()
            .chain(
                from_links
                    .symmetric_difference(&a.links)
                    .filter(|l| now.contains(*l))
                    .cloned(),
            )
            .collect()
    } else {
        from_links.clone()
    };

    // Moved by the run, from what the last scan saw, which is what it goes back to.
    let moved_from = old
        .map(|o| o.scan.mtime)
        .filter(|t| a.folder_touched && *t != node.metadata.mtime)
        .filter(|_| applied.mtime != node.metadata.mtime);
    let nothing = from_body.as_ref() == Some(&body) && !came && !unapplied && moved_from.is_none();
    if nothing {
        return None;
    }
    let folder_time = old
        .and_then(|o| o.synced.as_ref())
        .and_then(|s| s.folder_time)
        .or(moved_from);
    let held_links = match old.and_then(|o| o.held.clone()) {
        Some(held) => Some(held.into_iter().chain(now).collect()),
        None if old.is_some() => None,
        None => Some(now),
    };
    Some(Row {
        fp: node.fp.clone(),
        racy: node.racy,
        content: node.content.clone(),
        scan: node.metadata.clone(),
        applied: old.and_then(|o| o.applied.clone()),
        synced: Some(Synced {
            body,
            links,
            applied,
            commit: None,
            unapplied,
            folder_time,
        }),
        incomplete: node.metadata_incomplete,
        held: held_links,
    })
}

fn holds_what_it_synced(row: &Row) -> bool {
    match (&row.held, &row.synced) {
        (Some(held), Some(synced)) => !held.is_disjoint(&synced.links),
        _ => true,
    }
}

/// What the filesystem and the record were last known to agree on, while the server's head still
/// equals it.
fn shared_state(row: &Row, id: ObjectId, remote: &Tree, landed: &BTreeSet<OpId>) -> Option<Shared> {
    let synced = row.synced.as_ref()?;
    // Read from the journal, or carried by a commit seen in it: it was on the server, whatever has
    // happened to it since, moved out of this device's view included, so it is the ancestor. A commit
    // not seen there may have been refused.
    if synced.commit.is_none_or(|op| landed.contains(&op)) {
        return Some(Shared {
            body: Some(synced.body.clone()),
            links: Some(synced.links.clone()),
        });
    }
    let r = remote.nodes.get(&id)?;
    if !remote.is_live(id) {
        return None;
    }
    let body = (r.body == synced.body).then(|| synced.body.clone());
    let links = (r.links == synced.links).then(|| synced.links.clone());
    if body.is_none() && links.is_none() {
        return None;
    }
    Some(Shared { body, links })
}

/// The scanned metadata in stored terms (invariant 2), and what to compare with when applying.
fn stored_terms(
    id: ObjectId,
    node: &Scanned,
    base: &Tree,
    remote: &Tree,
    rows: &BTreeMap<ObjectId, Row>,
    shared: &BTreeMap<ObjectId, Shared>,
) -> (cairn_protocol::Metadata, cairn_protocol::Metadata) {
    let same = |m: cairn_protocol::Metadata| (m.clone(), m);
    let row = rows.get(&id);
    if let (Some(s), Some(synced)) = (
        shared.get(&id).filter(|s| s.body.is_some()),
        row.and_then(|r| r.synced.as_ref()),
    ) {
        let ancestor = s.body.as_ref().expect("filtered on body");
        if synced.unapplied && !node.metadata_incomplete {
            return (ancestor.metadata.clone(), node.metadata.clone());
        }
        if let Some(time) = synced.folder_time.filter(|_| !node.metadata_incomplete) {
            let mut seen = node.metadata.clone();
            seen.mtime = time;
            let stored = rebase_local_changes(&ancestor.metadata, &synced.applied, &seen);
            let mut on_disk = stored.clone();
            on_disk.mtime = node.metadata.mtime;
            return (stored, on_disk);
        }
        return if node.metadata_incomplete {
            same(ancestor.metadata.clone())
        } else {
            same(rebase_local_changes(
                &ancestor.metadata,
                &synced.applied,
                &node.metadata,
            ))
        };
    }
    // The root before any client has described it counts as not known at all.
    let known = base
        .get_live(id)
        .or_else(|| remote.get_live(id))
        .filter(|k| k.rev != cairn_protocol::RevisionId::ZERO);
    match (known, row.and_then(|r| r.applied.as_ref())) {
        (Some(k), Some(applied)) if !node.metadata_incomplete => same(rebase_local_changes(
            &k.body.metadata,
            applied,
            &node.metadata,
        )),
        (Some(k), _) if node.metadata_incomplete => same(k.body.metadata.clone()),
        // Known to the server, but this client has no record of having matched it, as after joining:
        // the two records join as a copy and the server's do at adoption, and what that gives is
        // applied here against what is really on disk.
        (Some(k), _) => (
            crate::merge::join_metadata(&k.body.metadata, &node.metadata, "this device").0,
            node.metadata.clone(),
        ),
        (None, _) => same(node.metadata.clone()),
    }
}

/// Whatever is inside a new directory that waits, because it cannot be fully read, waits with it.
fn drop_orphans_of_skipped(out: &mut Identified, base: &Tree, remote: &Tree) {
    loop {
        let parents: BTreeSet<ObjectId> = out.local.keys().copied().collect();
        let orphans: Vec<ObjectId> = out
            .local
            .iter()
            .filter(|(id, n)| {
                **id != ObjectId::ROOT
                    && n.links.iter().all(|l| {
                        !parents.contains(&l.parent)
                            && !base.is_live(l.parent)
                            && !remote.is_live(l.parent)
                    })
            })
            .map(|(id, _)| *id)
            .collect();
        if orphans.is_empty() {
            return;
        }
        for id in orphans {
            out.local.remove(&id);
            out.scanned.remove(&id);
        }
    }
}

/// What a scan could not see stands for what the base says is there.
fn carry_unobserved(
    scan: &ScanResult,
    base: &Tree,
    remote: &Tree,
    rows: &BTreeMap<ObjectId, Row>,
    dir_ids: &HashMap<Vec<Name>, ObjectId>,
    out: &mut Identified,
) {
    // Everything under an object this device knows, by the base or by what it has pushed.
    let known_subtree = |id: ObjectId| {
        let mut all = BTreeSet::new();
        let mut stack = vec![id];
        while let Some(at) = stack.pop() {
            if all.insert(at) {
                stack.extend(base.children(at).map(|(_, c)| c));
                stack.extend(
                    remote
                        .children(at)
                        .map(|(_, c)| c)
                        .filter(|c| rows.get(c).is_some_and(|r| r.synced.is_some())),
                );
            }
        }
        all
    };
    let carry = |id: ObjectId, out: &mut Identified| {
        for o in known_subtree(id) {
            if o == ObjectId::ROOT || out.local.contains_key(&o) {
                continue;
            }

            // It stands for what this filesystem was last known to hold, never for the server's
            // state: a difference from the server must show, or the base would move past the disk.
            let (body, links) = match rows.get(&o).and_then(|r| r.synced.as_ref()) {
                Some(s) => (&s.body, &s.links),
                None => match base.get_live(o).or_else(|| remote.get_live(o)) {
                    Some(n) => (&n.body, &n.links),
                    None => continue,
                },
            };
            out.local.insert(
                o,
                LNode {
                    links: links.clone(),
                    content: body.content.clone(),
                    stored: body.metadata.clone(),
                    apply_from: body.metadata.clone(),
                    carried: true,
                    conflict_of: None,
                },
            );
        }
    };
    for path in &scan.unobserved {
        let Some((name, parent_path)) = path.split_last() else {
            continue;
        };
        let Some(parent) = dir_ids.get(parent_path) else {
            continue;
        };
        if let Some(id) = base.child(*parent, name) {
            carry(id, out);
        }
    }
    // A file that moved and then became unreadable is known by its inode, not its name. Nothing says
    // which row is its own, so all of them wait: a stale row's delete only comes later.
    let by_inode = rows_by_inode(rows);
    for inode in &scan.unobserved_inodes {
        for id in by_inode.get(inode).into_iter().flatten() {
            carry(*id, out);
        }
    }
    for path in &scan.unlisted {
        let Some(dir) = dir_ids.get(path) else {
            continue;
        };
        for c in known_subtree(*dir) {
            if c != *dir {
                carry(c, out);
            }
        }
    }
    if scan.unstable || scan.unidentified {
        let pushed = remote
            .live()
            .map(|(id, _)| id)
            .filter(|id| rows.get(id).is_some_and(|r| r.synced.is_some()));
        let live: Vec<ObjectId> = base.live().map(|(id, _)| id).chain(pushed).collect();
        for id in live {
            carry(id, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Fingerprint, Synced};
    use cairn_protocol::{ConflictOf, Hash, RevisionBody, RevisionId};

    const STALE_BTIME: Timestamp = Timestamp { sec: 100, nsec: 0 };
    const LIVE_BTIME: Timestamp = Timestamp { sec: 200, nsec: 0 };

    fn fp(ino: u64, ctime_sec: i64, btime: Option<Timestamp>) -> Fingerprint {
        let stamp = Timestamp {
            sec: ctime_sec,
            nsec: 0,
        };
        Fingerprint {
            dev: 1,
            ino,
            size: 5,
            mtime: stamp,
            ctime: stamp,
            btime,
        }
    }

    fn file() -> Content {
        Content::File {
            manifest: Hash::of(b"body"),
            size: 5,
        }
    }

    fn row(fp: Fingerprint) -> Row {
        Row {
            fp,
            racy: false,
            content: file(),
            scan: Metadata::default(),
            applied: None,
            synced: None,
            incomplete: false,
            held: None,
        }
    }

    fn scanned(paths: Vec<Vec<Name>>, fp: Fingerprint, content: Content) -> Scanned {
        Scanned {
            paths,
            fp,
            racy: false,
            content,
            metadata: Metadata::default(),
            metadata_incomplete: false,
            holds_excluded: false,
            only_excluded: false,
        }
    }

    fn scan_of(nodes: Vec<Scanned>) -> ScanResult {
        ScanResult {
            root: scanned(Vec::new(), fp(1, 1, None), Content::Dir),
            nodes,
            skips: Vec::new(),
            unobserved: Vec::new(),
            unobserved_inodes: Vec::new(),
            unidentified: false,
            unlisted: Vec::new(),
            unstable: false,
            new_manifests: Vec::new(),
            stale_temps: Vec::new(),
            rescued_temps: Vec::new(),
            rescued: Vec::new(),
            excluded: Vec::new(),
        }
    }

    fn run(scan: &ScanResult, rows: &BTreeMap<ObjectId, Row>) -> Vec<ObjectId> {
        identify(
            scan,
            &Tree::default(),
            &Tree::default(),
            rows,
            &[],
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .local
        .keys()
        .copied()
        .filter(|id| *id != ObjectId::ROOT)
        .collect()
    }

    /// Both orders, since a map keyed by the inode alone kept whichever row came last.
    fn orderings() -> [(ObjectId, ObjectId); 2] {
        let (a, b) = (ObjectId::random(), ObjectId::random());
        [(a.min(b), a.max(b)), (a.max(b), a.min(b))]
    }

    fn found_under_its_own_row(stale: Fingerprint, live: Fingerprint) {
        let path = vec![vec![Name::new(b"f".to_vec()).unwrap()]];
        for (stale_id, live_id) in orderings() {
            let rows =
                BTreeMap::from([(stale_id, row(stale.clone())), (live_id, row(live.clone()))]);
            let scan = scan_of(vec![scanned(path.clone(), live.clone(), file())]);
            assert_eq!(run(&scan, &rows), vec![live_id]);
        }
    }

    /// A file that read as new was adopted back on every round with its record wiped, so that a sync
    /// of an idle tree never finished.
    #[test]
    fn a_deleted_files_row_does_not_hide_the_file_that_reused_its_inode() {
        found_under_its_own_row(fp(7, 10, Some(STALE_BTIME)), fp(7, 20, Some(LIVE_BTIME)));
    }

    #[test]
    fn without_birth_times_the_row_whose_fingerprint_holds_wins() {
        found_under_its_own_row(fp(7, 10, None), fp(7, 20, None));
    }

    /// Not on Windows, where a file ID is never reused, and so is identity enough.
    #[cfg(unix)]
    #[test]
    fn a_reused_inode_alone_is_no_identity() {
        let id = ObjectId::random();
        let rows = BTreeMap::from([(id, row(fp(7, 10, Some(STALE_BTIME))))]);
        let path = vec![vec![Name::new(b"new".to_vec()).unwrap()]];
        let scan = scan_of(vec![scanned(path, fp(7, 20, Some(LIVE_BTIME)), file())]);
        assert!(!run(&scan, &rows).contains(&id));
    }

    fn copy_found(is: Option<(ObjectKind, Option<Timestamp>)>, node: Scanned) -> bool {
        let copy = ObjectId::random();
        let intent = CopyIntent {
            copy,
            dev: 1,
            ino: 7,
            is,
            conflict_of: ConflictOf {
                object: ObjectId::random(),
                competing_revision: RevisionId::random(),
            },
            moves: Vec::new(),
        };
        identify(
            &scan_of(vec![node]),
            &Tree::default(),
            &Tree::default(),
            &BTreeMap::new(),
            &[intent],
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .local
        .contains_key(&copy)
    }

    /// The copy was deleted, and its number went to something else before the sync finished: pushed
    /// under the copy's identity, a symlink is refused by the server on every round.
    #[test]
    fn a_conflict_copys_inode_number_reused_is_no_identity() {
        let path = || vec![vec![Name::new(b"new".to_vec()).unwrap()]];
        let at = |btime| fp(7, 10, Some(btime));
        let mark = Some((ObjectKind::File, Some(STALE_BTIME)));
        assert!(copy_found(mark, scanned(path(), at(STALE_BTIME), file())));
        // On Windows a file ID is never reused, and a creation time moves between files.
        assert_eq!(
            copy_found(mark, scanned(path(), at(LIVE_BTIME), file())),
            cfg!(windows)
        );
        let link = Content::Symlink {
            target: cairn_protocol::Bytes(b"a".to_vec()),
        };
        assert!(!copy_found(mark, scanned(path(), at(STALE_BTIME), link)));
        // A mark an older version left says only the number.
        assert!(copy_found(None, scanned(path(), at(LIVE_BTIME), file())));
    }

    /// A swap of two names on NTFS swaps their creation times too (tunneling): read as a birth time,
    /// that made each file a new one where it was, and a swap two edits.
    #[cfg(windows)]
    #[test]
    fn a_file_whose_creation_time_ntfs_moved_is_still_found_by_its_id() {
        let id = ObjectId::random();
        let rows = BTreeMap::from([(id, row(fp(7, 10, Some(STALE_BTIME))))]);
        let path = vec![vec![Name::new(b"moved".to_vec()).unwrap()]];
        let scan = scan_of(vec![scanned(path, fp(7, 20, Some(LIVE_BTIME)), file())]);
        assert_eq!(run(&scan, &rows), vec![id]);
    }

    /// Missing from the carried set, the unreadable file would read as deleted.
    #[test]
    fn an_unreadable_inode_carries_every_row_that_holds_its_number() {
        for (one, other) in orderings() {
            let known = |ctime| Row {
                synced: Some(Synced {
                    body: RevisionBody {
                        content: file(),
                        metadata: Metadata::default(),
                    },
                    links: BTreeSet::new(),
                    applied: Metadata::default(),
                    commit: None,
                    unapplied: false,
                    folder_time: None,
                }),
                ..row(fp(7, ctime, None))
            };
            let rows = BTreeMap::from([(one, known(10)), (other, known(20))]);
            let mut scan = scan_of(Vec::new());
            scan.unobserved_inodes = vec![(1, 7)];
            let mut carried = run(&scan, &rows);
            carried.sort();
            let mut both = vec![one, other];
            both.sort();
            assert_eq!(carried, both);
        }
    }
}
