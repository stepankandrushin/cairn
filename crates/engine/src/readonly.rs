//! A folder this device's user may only read: everything the server has comes down, and nothing made
//! here goes up, nor is lost. The merge never learns of it, as it never learns of what is excluded: it
//! is given a local tree in which what cannot go up does not show as a change.
//!
//! - Something new here is held back as a name the server has in another form is (twins.rs): it stays
//!   on disk, out of the merge, listed by every sync.
//! - Something the server has, changed here (content, metadata, names) while the server's stayed as it
//!   was, is carried: it stands for the server's state, as what a scan could not read does, so that
//!   nothing is pushed for it and the applier, with no fingerprint of it, can touch none of it; its row
//!   keeps its baseline, so the next sync sees the change again.
//! - Something deleted here that the server still has as it was is given up, as a device gives up
//!   what the server deletes: the merge then brings it back as to a device that never had it.
//! - Where both sides changed something, the merge does what it always does, and keeps both: the
//!   server's at the name, this device's beside it, which the next round holds back as new.
//!
//! So a sync completes and records its agreement as any other does, and costs no more; what was
//! changed here is listed until it is moved out of the root, or until the folder can be written to,
//! when it goes up like any other change.

use crate::identify::Identified;
use crate::merge::{meta_changed, LNode, Shared};
use crate::scan::display_path;
use crate::state::Row;
use cairn_protocol::{Link, ObjectId, RevisionBody, Skip, SkipKind, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// The merge's common ancestor of an object: the state this device knows it shares with the server,
/// or else the base. As `merge::ancestor`.
fn ancestor(
    base: &Tree,
    shared: &BTreeMap<ObjectId, Shared>,
    id: ObjectId,
) -> Option<(RevisionBody, BTreeSet<Link>)> {
    let b = base.nodes.get(&id).filter(|_| base.is_live(id));
    let s = shared.get(&id);
    let body = s
        .and_then(|s| s.body.clone())
        .or_else(|| b.map(|n| n.body.clone()))?;
    let links = s
        .and_then(|s| s.links.clone())
        .or_else(|| b.map(|n| n.links.clone()))
        .unwrap_or_default();
    Some((body, links))
}

/// Whether a side's state differs from the ancestor's, as the merge tells a change.
fn differs(
    body: &RevisionBody,
    links: &BTreeSet<Link>,
    (a_body, a_links): &(RevisionBody, BTreeSet<Link>),
) -> bool {
    body.content != a_body.content
        || links != a_links
        || meta_changed(body.content.kind(), &a_body.metadata, &body.metadata)
}

/// What a person reads for an object: its path here as this scan saw it, or as the server has it.
fn path_of(identified: &Identified, remote: &Tree, base: &Tree, id: ObjectId) -> String {
    if let Some(p) = identified.scanned.get(&id).and_then(|s| s.paths.first()) {
        return display_path(p);
    }
    let path = |tree: &Tree| {
        tree.get_live(id)
            .and_then(|n| n.links.iter().next())
            .and_then(|l| tree.path_of_link(l))
    };
    match path(remote).or_else(|| path(base)) {
        Some(p) => display_path(&p),
        None => id.to_string(),
    }
}

fn skip(path: String, detail: &str) -> Skip {
    Skip {
        path,
        kind: SkipKind::ReadOnly,
        detail: Some(detail.to_string()),
    }
}

/// Before the merge, after the check for a mass delete has passed: of what is gone from here, what the
/// server still has as it was is given up, so that it comes back. Taken out of `identified.shared`
/// too, so that this round's merge brings it down rather than the next one's. Returns the rows
/// changed, which the caller stores, and a line for each topmost thing coming back.
pub fn give_up_deleted(
    identified: &mut Identified,
    base: &Tree,
    remote: &Tree,
    rows: &mut BTreeMap<ObjectId, Row>,
) -> (Vec<ObjectId>, Vec<String>) {
    let mut gone: BTreeSet<ObjectId> = BTreeSet::new();
    for (id, node) in base.live() {
        if id == ObjectId::ROOT
            || identified.local.contains_key(&id)
            || identified.refused.contains(&id)
        {
            continue;
        }
        let (Some(a), Some(r)) = (ancestor(base, &identified.shared, id), remote.get_live(id))
        else {
            continue;
        };
        // Changed on the server since: the merge brings that back by itself, and says so.
        if differs(&r.body, &r.links, &a) {
            continue;
        }
        let Some(row) = rows.get_mut(&id) else {
            continue;
        };
        let held = row.held.get_or_insert_with(|| node.links.clone());
        let before = held.len();
        for link in &node.links {
            held.remove(link);
        }
        if held.len() != before {
            gone.insert(id);
        }
    }
    let lines = gone
        .iter()
        .filter(|id| {
            base.nodes[id]
                .links
                .iter()
                .all(|l| !gone.contains(&l.parent))
        })
        .map(|id| {
            format!(
                "{}: deleted here in a folder that is read-only for you, so the server's comes back",
                path_of(identified, remote, base, *id)
            )
        })
        .collect();
    for id in &gone {
        identified.shared.remove(id);
    }
    (gone.into_iter().collect(), lines)
}

/// Before the merge, after the twins are held back: take out of what the merge sees every change here
/// that could only go up. Returns a skip for each, topmost only where a whole new folder waits, and a
/// line for each change of metadata here put back as the server has it.
pub fn hold_back(
    identified: &mut Identified,
    base: &Tree,
    remote: &Tree,
) -> (Vec<Skip>, Vec<String>) {
    let mut new_here: BTreeSet<ObjectId> = BTreeSet::new();
    let mut carry: Vec<(ObjectId, Option<&'static str>)> = Vec::new();
    let mut server_meta: Vec<(ObjectId, bool)> = Vec::new();
    let mut skips = Vec::new();
    for (id, l) in &identified.local {
        if l.carried {
            continue;
        }
        let here = RevisionBody {
            content: l.content.clone(),
            metadata: l.stored.clone(),
        };
        let r = remote.get_live(*id);
        let Some(a) = ancestor(base, &identified.shared, *id) else {
            // New here. Where the server has something at its path the merge takes it in, or keeps
            // both, and holds nothing back: what it keeps beside the server's is new the round after.
            let at_server = identified
                .scanned
                .get(id)
                .is_some_and(|s| s.paths.iter().any(|p| remote.resolve(p).is_some()));
            if r.is_none() && !at_server {
                new_here.insert(*id);
            }
            continue;
        };
        // Exactly, a folder's time too: the merge would send that up with anything else.
        if here == a.0 && l.links == a.1 {
            continue;
        }
        // What a person would call a change, which a folder's time alone is not.
        let noticed = differs(&here, &l.links, &a);
        let Some(r) = r else {
            // Changed here, deleted on the server: it stays here as something new.
            if noticed {
                skips.push(skip(
                    path_of(identified, remote, base, *id),
                    "changed here, and deleted on the server",
                ));
                new_here.insert(*id);
            }
            continue;
        };
        if r.body == a.0 && r.links == a.1 {
            let what = if !noticed {
                None
            } else if l.links != a.1 {
                Some("moved or renamed here")
            } else if l.content != a.0.content {
                Some("changed here")
            } else {
                Some("its metadata changed here")
            };
            carry.push((*id, what));
        } else if l.links == a.1 && l.content == a.0.content {
            // Its metadata alone changed here, and the server changed it too: the server's stands.
            server_meta.push((*id, noticed));
        }
        // Otherwise both changed what it holds or where it is: the merge keeps both, as ever.
    }
    // Only what is topmost of a new folder is said; what is inside waits with it.
    let topmost: Vec<ObjectId> = new_here
        .iter()
        .filter(|id| {
            identified.local[id]
                .links
                .iter()
                .all(|l| !new_here.contains(&l.parent))
        })
        .copied()
        .collect();
    let added: Vec<Skip> = topmost
        .iter()
        .filter(|id| ancestor(base, &identified.shared, **id).is_none())
        .map(|id| skip(path_of(identified, remote, base, *id), "added here"))
        .collect();
    skips.extend(added);
    for (id, what) in carry {
        if let Some(what) = what {
            skips.push(skip(path_of(identified, remote, base, id), what));
        }
        let r = remote
            .get_live(id)
            .expect("carried only where the server has it");
        identified.local.insert(
            id,
            LNode {
                links: r.links.clone(),
                content: r.body.content.clone(),
                stored: r.body.metadata.clone(),
                apply_from: r.body.metadata.clone(),
                carried: true,
                conflict_of: None,
            },
        );
    }
    let mut lines = Vec::new();
    for (id, noticed) in server_meta {
        let r = remote.get_live(id).expect("only where the server has it");
        if noticed {
            lines.push(format!(
                "{}: its metadata changed here and on the server, in a folder that is read-only for you, so the server's is put back",
                path_of(identified, remote, base, id)
            ));
        }
        if let Some(l) = identified.local.get_mut(&id) {
            l.stored = r.body.metadata.clone();
        }
    }
    crate::twins::hold_back_objects(identified, new_here, remote);
    skips.sort_by(|a, b| a.path.cmp(&b.path));
    (skips, lines)
}
