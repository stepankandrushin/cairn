//! Three-way merge per object: base, local, remote go in; the tree both sides should end at comes
//! out, with what must be reported. Pure, so that every row of PLAN.md's merge table is a unit test.

use cairn_protocol::{
    ConflictOf, Content, Link, Metadata, Name, ObjectId, ObjectKind, RevisionBody, RevisionId, Tree,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// An object as this client sees it locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LNode {
    pub links: BTreeSet<Link>,
    pub content: Content,
    /// The metadata in stored terms: the stored record plus only what changed on this filesystem.
    pub stored: Metadata,
    /// What local metadata is compared with to decide what to apply.
    pub apply_from: Metadata,
    /// Not seen by this scan (unreadable, mount, changing): it stands for the base and cannot be
    /// touched.
    pub carried: bool,
    pub conflict_of: Option<ConflictOf>,
}

impl LNode {
    pub fn kind(&self) -> ObjectKind {
        self.content.kind()
    }
}

/// A newer common ancestor than the base tree: the state this client knows it already shares with
/// the server, for an object whose journal entry the base has not reached yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shared {
    pub body: Option<RevisionBody>,
    pub links: Option<BTreeSet<Link>>,
}

/// What the server holds that this device's view of it leaves out (selection.rs). The merge sees only
/// the view, and these are the three ways the real tree can still refuse what it decides.
#[derive(Debug, Default)]
pub struct Hidden {
    /// Directories that are not empty on the server, so that deleting them would be refused.
    pub pinned: BTreeSet<ObjectId>,
    /// Entries in use on the server, by something that is not in the view.
    pub reserved: BTreeSet<Link>,
    /// Objects alive on the server, out of view, so that they cannot be created or brought back.
    pub moved_away: BTreeSet<ObjectId>,
}

pub struct MergeInput<'a> {
    pub base: &'a Tree,
    pub local: &'a BTreeMap<ObjectId, LNode>,
    pub remote: &'a Tree,
    pub shared: &'a BTreeMap<ObjectId, Shared>,
    pub hidden: &'a Hidden,
    pub host: &'a str,
    pub date: &'a str,
    pub fresh_id: &'a mut dyn FnMut() -> ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TNode {
    pub links: BTreeSet<Link>,
    pub body: RevisionBody,
    pub conflict_of: Option<ConflictOf>,
}

/// Local content that must move aside to a conflict copy before the remote content arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictCopy {
    pub object: ObjectId,
    pub copy: ObjectId,
    /// Each local entry and the name it moves to.
    pub moves: Vec<(Link, Link)>,
    pub competing_revision: RevisionId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReportKind {
    ConflictCopy,
    EditSurvivedDelete,
    SameNameKeptBoth,
    MetadataConflict,
    MoveConflict,
    CycleRefused,
    KeptForChild,
    /// A separate copy here of a file the server holds under several names, linked to it.
    Linked,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Report {
    pub kind: ReportKind,
    pub object: ObjectId,
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// The tree to reach, live objects only.
    pub target: BTreeMap<ObjectId, TNode>,
    pub copies: Vec<ConflictCopy>,
    /// New local objects recognised as remote ones: same name and content.
    pub adopted: BTreeMap<ObjectId, ObjectId>,
    /// The local tree with adoptions applied, which everything downstream works from.
    pub local: BTreeMap<ObjectId, LNode>,
    pub reports: Vec<Report>,
    /// Activity-log entries for the loser of a same-field metadata conflict.
    pub notes: Vec<(ObjectId, String)>,
}

/// A directory's mtime moves with every entry in it; it says nothing about intent.
fn meta_changed(kind: ObjectKind, a: &Metadata, b: &Metadata) -> bool {
    if kind == ObjectKind::Dir {
        let mut b = b.clone();
        b.mtime = a.mtime;
        b.time_precision_ns = a.time_precision_ns;
        *a != b
    } else {
        a != b
    }
}

fn merge_field<T: Clone + PartialEq>(b: &T, l: &T, r: &T) -> (T, bool) {
    if l == r || l == b {
        (r.clone(), false)
    } else if r == b {
        (l.clone(), false)
    } else {
        // Both changed it, differently: server order wins.
        (r.clone(), true)
    }
}

/// Field-by-field merge. Same-field conflicts go to the server's value and return a note.
pub fn merge_metadata(b: &Metadata, l: &Metadata, r: &Metadata) -> (Metadata, Vec<String>) {
    let mut notes = Vec::new();
    let mut out = r.clone();

    let (mode, clash) = merge_field(&b.mode, &l.mode, &r.mode);
    out.mode = mode;
    if clash {
        notes.push(format!(
            "mode: kept {:o} from the server; this device had {:o}",
            r.mode, l.mode
        ));
    }
    let (owner, clash) = merge_field(&b.owner, &l.owner, &r.owner);
    out.owner = owner;
    if clash {
        notes.push(format!(
            "owner: kept {:?} from the server; this device had {:?}",
            r.owner, l.owner
        ));
    }
    let (times, _) = merge_field(
        &(b.mtime, b.time_precision_ns),
        &(l.mtime, l.time_precision_ns),
        &(r.mtime, r.time_precision_ns),
    );
    out.mtime = times.0;
    out.time_precision_ns = times.1;
    out.btime = merge_field(&b.btime, &l.btime, &r.btime).0;
    out.streams = merge_field(&b.streams, &l.streams, &r.streams).0;

    let keys: BTreeSet<_> = b
        .records
        .keys()
        .chain(l.records.keys())
        .chain(r.records.keys())
        .collect();
    for key in keys {
        let (v, clash) = merge_field(
            &b.records.get(key),
            &l.records.get(key),
            &r.records.get(key),
        );
        match v {
            Some(v) => {
                out.records.insert(key.clone(), v.clone());
            }
            None => {
                out.records.remove(key);
            }
        }
        if clash {
            notes.push(format!(
                "record {} {}: kept the server's value",
                key.ns,
                cairn_protocol::name::display_bytes(&key.name.0)
            ));
        }
    }
    (out, notes)
}

/// Whether a folder is a git directory: `.git`, or a bare repository's `name.git`.
pub fn is_git_dir(name: &Name) -> bool {
    name.as_bytes().ends_with(b".git")
}

/// Whether something in this folder, given by its names from the root, is inside a git directory.
pub fn in_git_dir(folder: &[Name]) -> bool {
    folder.iter().any(is_git_dir)
}

/// `name (conflict host 2026-10-31).ext`, within the 255 bytes filesystems allow. Inside a git
/// directory it is `name.conflict-host-2026-10-31`, a name git takes: a copy of a branch is a branch,
/// and of its reflog that branch's reflog, so `git log --all` shows both commits and `git merge`
/// joins them. A space in a name in `refs/` makes git fail on the whole repository.
pub fn conflict_name(name: &Name, host: &str, date: &str, attempt: u32, in_git: bool) -> Name {
    if in_git {
        return git_conflict_name(name, host, date, attempt);
    }
    let bytes = name.as_bytes();
    let split = bytes
        .iter()
        .rposition(|&b| b == b'.')
        .filter(|&i| i > 0)
        .unwrap_or(bytes.len());
    let (stem, ext) = bytes.split_at(split);
    let mut tag = format!(" (conflict {host} {date}").into_bytes();
    if attempt > 0 {
        tag.extend_from_slice(format!(" {}", attempt + 1).as_bytes());
    }
    tag.push(b')');
    // An extension too long to keep beside the tag is just part of the name.
    let (stem, ext) = if tag.len() + ext.len() + 1 > 255 {
        (bytes, &bytes[0..0])
    } else {
        (stem, ext)
    };
    let room = 255usize.saturating_sub(tag.len() + ext.len());
    let mut out = stem[..stem.len().min(room)].to_vec();
    out.extend_from_slice(&tag);
    out.extend_from_slice(ext);
    Name::new(
        out.into_iter()
            .map(|b| if b == b'/' { b'_' } else { b })
            .collect::<Vec<u8>>(),
    )
    .expect("a conflict name is built from a valid name")
}

/// The suffix keeps to what any ref name may hold, and is appended whole: an `.idx` or `.pack` that
/// kept its extension would be taken for part of a pack.
fn git_conflict_name(name: &Name, host: &str, date: &str, attempt: u32) -> Name {
    let mut tag = format!("conflict-{host}-{date}");
    if attempt > 0 {
        tag.push_str(&format!("-{}", attempt + 1));
    }
    let mut tag: Vec<u8> = tag
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                b
            } else {
                b'-'
            }
        })
        .collect();
    tag.insert(0, b'.');
    let bytes = name.as_bytes();
    let room = 255usize.saturating_sub(tag.len());
    let mut out = bytes[..bytes.len().min(room)].to_vec();
    out.extend_from_slice(&tag);
    Name::new(out).expect("a conflict name is built from a valid name")
}

/// Whether a folder, by its object, is inside a git directory, going up by its first entry.
fn folder_in_git(
    mut at: ObjectId,
    local: &BTreeMap<ObjectId, LNode>,
    remote: &cairn_protocol::Tree,
) -> bool {
    for _ in 0..4096 {
        if at == ObjectId::ROOT {
            return false;
        }
        let up = local
            .get(&at)
            .and_then(|n| n.links.iter().next())
            .or_else(|| remote.nodes.get(&at).and_then(|n| n.links.iter().next()));
        let Some(up) = up else {
            return false;
        };
        if is_git_dir(&up.name) {
            return true;
        }
        at = up.parent;
    }
    false
}

/// A copy with no history and the server's record of the same thing, together. Nothing says who
/// changed what, so it is the merge field by field with each side's fields its own: a record (an xattr)
/// only one side has stays, and where both have a value and they differ the server's stands and the
/// other goes in the activity log, value and all, as the loser of a same-field conflict does. Times and
/// owners differ between any two copies and say nothing of anyone's intent: the server's stand, unlogged.
pub fn join_metadata(server: &Metadata, here: &Metadata, whose: &str) -> (Metadata, Vec<String>) {
    let mut out = server.clone();
    let mut notes = Vec::new();
    if here.mode != server.mode {
        notes.push(format!(
            "mode: kept {:o} from the server; {whose} had {:o}",
            server.mode, here.mode
        ));
    }
    for (key, value) in &here.records {
        match server.records.get(key) {
            None => {
                out.records.insert(key.clone(), value.clone());
            }
            Some(theirs) if theirs != value => notes.push(format!(
                "record {} {}: kept the server's value; {whose} had {}",
                key.ns,
                cairn_protocol::name::display_bytes(&key.name.0),
                cairn_protocol::name::display_bytes(&value.0)
            )),
            Some(_) => {}
        }
    }
    (out, notes)
}

/// New local objects recognised as the server's: same name and content, or two directories.
struct Adoption {
    /// The local tree with adoptions applied.
    local: BTreeMap<ObjectId, LNode>,
    adopted: BTreeMap<ObjectId, ObjectId>,
    /// Separate copies here of a file the server holds under several names, each at one of them: they
    /// become links of it here.
    linked: BTreeMap<ObjectId, ObjectId>,
    /// What this device's copies lose to the server's record.
    notes: Vec<(ObjectId, String)>,
}

fn adopt(input: &MergeInput<'_>) -> Adoption {
    let mut by_parent: HashMap<ObjectId, Vec<(Name, ObjectId)>> = HashMap::new();
    for (id, n) in input.local {
        for l in &n.links {
            by_parent
                .entry(l.parent)
                .or_default()
                .push((l.name.clone(), *id));
        }
    }
    let mut adopted: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
    let mut taken: BTreeSet<ObjectId> = BTreeSet::new();
    // Files once every folder is seen: one the server holds under several names can have a copy at
    // each here, and which of them adopts it is a choice.
    let mut files: BTreeMap<ObjectId, Vec<(Name, ObjectId)>> = BTreeMap::new();
    // Local directory, its counterpart on the server.
    let mut queue = vec![(ObjectId::ROOT, ObjectId::ROOT)];
    while let Some((ldir, rdir)) = queue.pop() {
        let Some(children) = by_parent.get(&ldir) else {
            continue;
        };
        for (name, x) in children {
            let node = &input.local[x];
            let known = input.base.nodes.contains_key(x)
                || input.shared.contains_key(x)
                || input.remote.nodes.contains_key(x);
            if known {
                if node.kind() == ObjectKind::Dir && input.remote.is_live(*x) {
                    queue.push((*x, *x));
                }
                continue;
            }
            let Some(y) = input.remote.child(rdir, name) else {
                continue;
            };
            let r = &input.remote.nodes[&y];
            let free = !input.local.contains_key(&y)
                && !input.base.nodes.contains_key(&y)
                && !taken.contains(&y);
            let same = r.kind() == node.kind()
                && (node.kind() == ObjectKind::Dir || r.body.content == node.content);
            if !(free && same) {
                continue;
            }
            if node.kind() == ObjectKind::Dir {
                taken.insert(y);
                adopted.insert(*x, y);
                queue.push((*x, y));
            } else {
                files.entry(y).or_default().push((name.clone(), *x));
            }
        }
    }
    // The copy with most of its own adopts the file, since what it has stays on its inode. Another
    // folds into it, as a link, only if it has nothing that neither that inode nor the server's record
    // holds: it is removed before anything is written to the inode that stays, so that would be lost
    // to a kill in between. Such a copy stays itself, new, and so a conflict copy.
    let mut linked: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
    for (y, mut copies) in files {
        copies.retain(|(_, x)| !adopted.contains_key(x) && !linked.contains_key(x));
        copies.sort_by(|a, b| {
            let own = |x: &ObjectId| input.local[x].stored.records.len();
            own(&b.1).cmp(&own(&a.1)).then_with(|| a.cmp(b))
        });
        let mut once = BTreeSet::new();
        copies.retain(|(_, x)| once.insert(*x));
        let Some((_, first)) = copies.first().cloned() else {
            continue;
        };
        adopted.insert(first, y);
        let stays = &input.local[&first].stored.records;
        let server = &input.remote.nodes[&y].body.metadata.records;
        for (_, x) in &copies[1..] {
            let n = &input.local[x];
            let brings = n
                .stored
                .records
                .iter()
                .any(|(k, v)| server.get(k) != Some(v) && stays.get(k) != Some(v));
            if n.links.len() == 1 && !brings {
                linked.insert(*x, y);
            }
        }
    }
    let map = |o: ObjectId| adopted.get(&o).copied().unwrap_or(o);
    let mut notes = Vec::new();
    let local: BTreeMap<ObjectId, LNode> = input
        .local
        .iter()
        .map(|(id, n)| {
            let mut n = n.clone();
            n.links = n
                .links
                .iter()
                .map(|l| Link {
                    parent: map(l.parent),
                    name: l.name.clone(),
                })
                .collect();
            if let Some(y) = adopted.get(id) {
                // Same content, and this copy's metadata has no history: it joins the server's
                // record, and what that holds and this copy lacks is applied here.
                let (joined, lost) = join_metadata(
                    &input.remote.nodes[y].body.metadata,
                    &n.stored,
                    "this device",
                );
                notes.extend(lost.into_iter().map(|t| (*y, t)));
                n.apply_from = n.stored.clone();
                n.stored = joined;
            }
            (map(*id), n)
        })
        .collect();
    for (x, y) in &linked {
        let n = &input.local[x];
        let at = n.links.iter().next().map(|l| l.name.to_string());
        let whose = format!("this device's copy at {}", at.unwrap_or_default());
        let (_, lost) = join_metadata(&local[y].stored, &n.stored, &whose);
        notes.extend(lost.into_iter().map(|t| (*y, t)));
    }
    Adoption {
        local,
        adopted,
        linked,
        notes,
    }
}

/// The common ancestor of an object, if it had one: the newer shared state where known.
fn ancestor(input: &MergeInput<'_>, id: ObjectId) -> (Option<RevisionBody>, BTreeSet<Link>, bool) {
    let b = input.base.nodes.get(&id).filter(|_| input.base.is_live(id));
    let shared = input.shared.get(&id);
    let body = shared
        .and_then(|s| s.body.clone())
        .or_else(|| b.map(|n| n.body.clone()));
    let links = shared
        .and_then(|s| s.links.clone())
        .or_else(|| b.map(|n| n.links.clone()))
        .unwrap_or_default();
    let existed = body.is_some();
    (body, links, existed)
}

fn merge_links(
    id: ObjectId,
    kind: ObjectKind,
    b: &BTreeSet<Link>,
    l: &BTreeSet<Link>,
    r: &BTreeSet<Link>,
    reports: &mut Vec<Report>,
) -> BTreeSet<Link> {
    let l_gone: BTreeSet<_> = b.difference(l).cloned().collect();
    let r_gone: BTreeSet<_> = b.difference(r).cloned().collect();
    let l_new: BTreeSet<_> = l.difference(b).cloned().collect();
    let r_new: BTreeSet<_> = r.difference(b).cloned().collect();
    // Both moved the same entry to different places: server order wins.
    if !l_new.is_empty()
        && !r_new.is_empty()
        && l_new != r_new
        && l_gone.intersection(&r_gone).next().is_some()
    {
        reports.push(Report {
            kind: ReportKind::MoveConflict,
            object: id,
            detail: "moved on both sides; the server's move stands".into(),
        });
        return r.clone();
    }
    let mut out: BTreeSet<Link> = b.difference(&l_gone).cloned().collect();
    out = out.difference(&r_gone).cloned().collect();
    out.extend(l_new);
    out.extend(r_new);
    if kind == ObjectKind::Dir && out.len() > 1 {
        return r.clone();
    }
    out
}

/// Merge the three trees into a plan.
pub fn merge(mut input: MergeInput<'_>) -> Plan {
    let Adoption {
        local,
        adopted,
        linked,
        notes,
    } = adopt(&input);
    let mut plan = Plan {
        adopted,
        ..Plan::default()
    };
    for (y, note) in notes {
        plan.reports.push(Report {
            kind: ReportKind::MetadataConflict,
            object: y,
            detail: note.clone(),
        });
        plan.notes.push((y, note));
    }
    for (x, y) in &linked {
        let name = local[x].links.iter().next().map(|l| l.name.to_string());
        plan.reports.push(Report {
            kind: ReportKind::Linked,
            object: *y,
            detail: format!(
                "`{}` here was a separate copy with the same bytes, and the server holds both names as one file: now it is a link of it here too",
                name.unwrap_or_default()
            ),
        });
    }

    let ids: BTreeSet<ObjectId> = local
        .keys()
        .copied()
        .chain(input.remote.nodes.keys().copied())
        .chain(input.base.nodes.keys().copied())
        .chain(input.shared.keys().copied())
        .collect();

    // Names in use on this filesystem, for choosing conflict names that are free.
    let occupied: BTreeSet<Link> = local
        .values()
        .flat_map(|n| n.links.iter().cloned())
        .collect();

    for id in ids {
        // Goes, its name taken by a link of the file it is a copy of.
        if linked.contains_key(&id) {
            continue;
        }
        let l = local.get(&id);
        let r = input.remote.get_live(id);
        // What this scan could not see says nothing, so it can never push: the server's state
        // stands. Applying that here fails, with a report, while it stays unobservable.
        if l.is_some_and(|l| l.carried) {
            if let Some(r) = r {
                plan.target.insert(
                    id,
                    TNode {
                        links: r.links.clone(),
                        body: r.body.clone(),
                        conflict_of: None,
                    },
                );
            }
            continue;
        }
        let (b_body, b_links, existed) = ancestor(&input, id);
        match (existed, l, r) {
            (_, None, None) => {}
            (false, Some(l), None) => {
                plan.target.insert(
                    id,
                    TNode {
                        links: l.links.clone(),
                        body: RevisionBody {
                            content: l.content.clone(),
                            metadata: l.stored.clone(),
                        },
                        conflict_of: l.conflict_of.clone(),
                    },
                );
            }
            (false, None, Some(r)) => {
                plan.target.insert(
                    id,
                    TNode {
                        links: r.links.clone(),
                        body: r.body.clone(),
                        conflict_of: None,
                    },
                );
            }
            (true, None, Some(r)) => {
                let b = b_body.as_ref().expect("existed");
                let changed = r.body.content != b.content
                    || r.links != b_links
                    || meta_changed(r.kind(), &b.metadata, &r.body.metadata);
                let pinned = input.hidden.pinned.contains(&id);
                if changed || pinned {
                    plan.reports.push(if changed {
                        Report {
                            kind: ReportKind::EditSurvivedDelete,
                            object: id,
                            detail:
                                "deleted here but changed on another device; the change is back here"
                                    .into(),
                        }
                    } else {
                        Report {
                            kind: ReportKind::KeptForChild,
                            object: id,
                            detail: "deleted here, but the server holds something inside it that this device does not sync; the folder stays".into(),
                        }
                    });
                    plan.target.insert(
                        id,
                        TNode {
                            links: r.links.clone(),
                            body: r.body.clone(),
                            conflict_of: None,
                        },
                    );
                }
            }
            (true, Some(l), None) => {
                let b = b_body.as_ref().expect("existed");
                let changed = l.content != b.content
                    || l.links != b_links
                    || meta_changed(l.kind(), &b.metadata, &l.stored);
                if changed {
                    plan.reports.push(Report {
                        kind: ReportKind::EditSurvivedDelete,
                        object: id,
                        detail: "changed here but deleted on another device; the change stays"
                            .into(),
                    });
                    plan.target.insert(
                        id,
                        TNode {
                            links: l.links.clone(),
                            body: RevisionBody {
                                content: l.content.clone(),
                                metadata: l.stored.clone(),
                            },
                            conflict_of: None,
                        },
                    );
                }
            }
            (_, Some(l), Some(r)) => {
                merge_both(
                    &mut input,
                    id,
                    l,
                    r,
                    b_body.as_ref(),
                    &b_links,
                    existed,
                    &occupied,
                    &mut plan,
                );
            }
        }
    }

    plan.local = local;
    repair_orphans(&input, &mut plan);
    detach_moved_away(&mut input, &mut plan);
    repair_cycles(&input, &mut plan);
    resolve_collisions(&input, &mut plan);
    one_entry_per_directory(&input, &mut plan);
    plan
}

#[allow(clippy::too_many_arguments)]
fn merge_both(
    input: &mut MergeInput<'_>,
    id: ObjectId,
    l: &LNode,
    r: &cairn_protocol::Node,
    b: Option<&RevisionBody>,
    b_links: &BTreeSet<Link>,
    existed: bool,
    occupied: &BTreeSet<Link>,
    plan: &mut Plan,
) {
    let base_meta = b.map_or(&r.body.metadata, |b| &b.metadata);
    let links = if id == ObjectId::ROOT {
        BTreeSet::new()
    } else if existed {
        merge_links(id, r.kind(), b_links, &l.links, &r.links, &mut plan.reports)
    } else if r.kind() == ObjectKind::Dir {
        // A directory has one entry, and with no shared history the server's stands.
        r.links.clone()
    } else {
        // Known to both sides with no shared history: the server's entries, and any this side added.
        l.links.union(&r.links).cloned().collect()
    };

    // Each side removed a different hard link and together they removed all of them: a delete no one
    // device made. It stands only if nobody changed the object in the meantime.
    let mut links = links;
    if links.is_empty() && id != ObjectId::ROOT {
        let edited = b.is_none_or(|b| {
            l.content != b.content
                || r.body.content != b.content
                || meta_changed(r.kind(), &b.metadata, &l.stored)
                || meta_changed(r.kind(), &b.metadata, &r.body.metadata)
        });
        if !edited {
            return;
        }
        plan.reports.push(Report {
            kind: ReportKind::EditSurvivedDelete,
            object: id,
            detail:
                "every name was removed across devices, but it was also changed; the change stays"
                    .into(),
        });
        links = r.links.clone();
    }

    let content_conflict = l.content != r.body.content
        && b.is_none_or(|b| l.content != b.content && r.body.content != b.content);
    if content_conflict {
        // Keep both. The object takes the remote content; the local one moves to a new object.
        let copy = (input.fresh_id)();
        let moves: Vec<(Link, Link)> = l
            .links
            .iter()
            .map(|old| {
                let in_git = folder_in_git(old.parent, input.local, input.remote);
                let free = (0u32..)
                    .map(|attempt| Link {
                        parent: old.parent,
                        name: conflict_name(&old.name, input.host, input.date, attempt, in_git),
                    })
                    .find(|c| !occupied.contains(c) && input.remote.entry_at(c).is_none())
                    .expect("the attempts are unbounded");
                (old.clone(), free)
            })
            .collect();
        plan.target.insert(
            copy,
            TNode {
                links: moves.iter().map(|m| m.1.clone()).collect(),
                // A new object: it holds this file as it is here.
                body: RevisionBody {
                    content: l.content.clone(),
                    metadata: l.apply_from.clone(),
                },
                conflict_of: Some(ConflictOf {
                    object: id,
                    competing_revision: r.rev,
                }),
            },
        );
        plan.copies.push(ConflictCopy {
            object: id,
            copy,
            moves,
            competing_revision: r.rev,
        });
        plan.reports.push(Report {
            kind: ReportKind::ConflictCopy,
            object: copy,
            detail: "edited on two devices; both versions kept".into(),
        });
        plan.target.insert(
            id,
            TNode {
                links,
                body: r.body.clone(),
                conflict_of: None,
            },
        );
        return;
    }

    let content = if b.is_some_and(|b| l.content != b.content) {
        l.content.clone()
    } else {
        r.body.content.clone()
    };
    let (metadata, notes) = merge_metadata(base_meta, &l.stored, &r.body.metadata);
    for note in notes {
        plan.reports.push(Report {
            kind: ReportKind::MetadataConflict,
            object: id,
            detail: note.clone(),
        });
        plan.notes.push((id, note));
    }
    plan.target.insert(
        id,
        TNode {
            links,
            body: RevisionBody { content, metadata },
            conflict_of: None,
        },
    );
}

/// The server refuses a directory with two entries, for ever: whatever led here, one must stand.
fn one_entry_per_directory(input: &MergeInput<'_>, plan: &mut Plan) {
    for (id, t) in plan.target.iter_mut() {
        if t.body.content == Content::Dir && t.links.len() > 1 {
            let keep = input
                .remote
                .get_live(*id)
                .and_then(|r| r.links.iter().next().cloned())
                .or_else(|| t.links.iter().next().cloned());
            t.links = keep.into_iter().collect();
        }
    }
}

/// A surviving object inside a directory that was deleted brings the directory back.
fn repair_orphans(input: &MergeInput<'_>, plan: &mut Plan) {
    loop {
        let missing: BTreeSet<ObjectId> = plan
            .target
            .values()
            .flat_map(|t| t.links.iter().map(|l| l.parent))
            .filter(|p| *p != ObjectId::ROOT && !plan.target.contains_key(p))
            .collect();
        if missing.is_empty() {
            return;
        }
        for p in missing {
            let node = plan
                .local
                .get(&p)
                .filter(|l| !l.carried)
                .map(|l| TNode {
                    links: l.links.clone(),
                    body: RevisionBody {
                        content: l.content.clone(),
                        metadata: l.stored.clone(),
                    },
                    conflict_of: None,
                })
                .or_else(|| {
                    let b = input.base.nodes.get(&p).filter(|n| !n.links.is_empty())?;
                    let last = input.remote.nodes.get(&p).unwrap_or(b);
                    Some(TNode {
                        links: b.links.clone(),
                        body: last.body.clone(),
                        conflict_of: None,
                    })
                })
                .or_else(|| {
                    // Nothing says where it lived, which a valid history cannot produce: keep the
                    // data reachable at the root.
                    let last = input.remote.nodes.get(&p)?;
                    let name = Name::new(format!("recovered-{p}").into_bytes())
                        .expect("a hex name is valid");
                    Some(TNode {
                        links: [Link {
                            parent: ObjectId::ROOT,
                            name,
                        }]
                        .into(),
                        body: last.body.clone(),
                        conflict_of: None,
                    })
                });
            let Some(node) = node else {
                // No record of the parent at all: re-root its children.
                reroot_children(plan, p);
                continue;
            };
            plan.reports.push(Report {
                kind: ReportKind::KeptForChild,
                object: p,
                detail: "deleted on one side, but something inside it survives".into(),
            });
            plan.target.insert(p, node);
        }
    }
}

/// Something to keep here, under an object that the server holds out of this device's view: another
/// device moved it out of view while this one changed it, or a file was moved here from an excluded
/// path. The server's object stays where it is, and what is here goes on as a new object, reported
/// like any other edit that outlived its object.
fn detach_moved_away(input: &mut MergeInput<'_>, plan: &mut Plan) {
    let caught: Vec<ObjectId> = plan
        .target
        .keys()
        .filter(|id| input.hidden.moved_away.contains(id))
        .copied()
        .collect();
    if caught.is_empty() {
        return;
    }
    let mut fresh: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
    for id in caught {
        let new = (input.fresh_id)();
        fresh.insert(id, new);
        plan.adopted.insert(id, new);
        plan.reports.push(Report {
            kind: ReportKind::EditSurvivedDelete,
            object: new,
            detail: "on the server this has moved somewhere this device does not sync; what is here stays, as a new entry".into(),
        });
    }
    let renew = |id: ObjectId| fresh.get(&id).copied().unwrap_or(id);
    for report in &mut plan.reports {
        report.object = renew(report.object);
    }
    let renew_links = |links: &BTreeSet<Link>| -> BTreeSet<Link> {
        links
            .iter()
            .map(|l| Link {
                parent: renew(l.parent),
                name: l.name.clone(),
            })
            .collect()
    };
    plan.target = std::mem::take(&mut plan.target)
        .into_iter()
        .map(|(id, mut t)| {
            t.links = renew_links(&t.links);
            (renew(id), t)
        })
        .collect();
    plan.local = std::mem::take(&mut plan.local)
        .into_iter()
        .map(|(id, mut l)| {
            l.links = renew_links(&l.links);
            (renew(id), l)
        })
        .collect();
}

fn reroot_children(plan: &mut Plan, parent: ObjectId) {
    for (id, t) in plan.target.iter_mut() {
        let moved: BTreeSet<Link> = t
            .links
            .iter()
            .map(|l| {
                if l.parent == parent {
                    let name = Name::new(format!("recovered-{id}").into_bytes())
                        .expect("a hex name is valid");
                    Link {
                        parent: ObjectId::ROOT,
                        name,
                    }
                } else {
                    l.clone()
                }
            })
            .collect();
        t.links = moved;
    }
}

/// Concurrent moves that would form a cycle: the local mover yields to the server's tree.
fn repair_cycles(input: &MergeInput<'_>, plan: &mut Plan) {
    for _ in 0..=plan.target.len() {
        let Some(cycle) = find_cycle(&plan.target) else {
            return;
        };
        let mut changed = false;
        for id in cycle {
            let fallback = input
                .remote
                .get_live(id)
                .map(|r| r.links.clone())
                .or_else(|| input.base.nodes.get(&id).map(|b| b.links.clone()));
            if let (Some(links), Some(t)) = (fallback, plan.target.get_mut(&id)) {
                if t.links != links && !links.is_empty() {
                    t.links = links;
                    changed = true;
                    plan.reports.push(Report {
                        kind: ReportKind::CycleRefused,
                        object: id,
                        detail: "this move would have formed a cycle with another device's; it was undone".into(),
                    });
                }
            }
        }
        if !changed {
            return;
        }
    }
}

fn find_cycle(target: &BTreeMap<ObjectId, TNode>) -> Option<Vec<ObjectId>> {
    let mut clear: BTreeSet<ObjectId> = BTreeSet::new();
    for (start, node) in target {
        if node.body.content != Content::Dir {
            continue;
        }
        let mut seen = Vec::new();
        let mut at = *start;
        loop {
            if at == ObjectId::ROOT || clear.contains(&at) {
                break;
            }
            if let Some(i) = seen.iter().position(|s| *s == at) {
                return Some(seen[i..].to_vec());
            }
            seen.push(at);
            let Some(up) = target.get(&at).and_then(|t| t.links.iter().next()) else {
                break;
            };
            at = up.parent;
        }
        clear.extend(seen);
    }
    None
}

/// Two objects at one name: both stay, the one that got there first keeps it.
fn resolve_collisions(input: &MergeInput<'_>, plan: &mut Plan) {
    let mut by_link: BTreeMap<Link, Vec<ObjectId>> = BTreeMap::new();
    for (id, t) in &plan.target {
        for l in &t.links {
            by_link.entry(l.clone()).or_default().push(*id);
        }
    }
    let mut used: BTreeSet<Link> = by_link.keys().cloned().collect();
    used.extend(input.remote.entries().map(|(l, _)| l.clone()));
    used.extend(input.hidden.reserved.iter().cloned());
    for (link, owners) in by_link {
        // Taken on the server by something out of view: nobody here can have that name.
        let taken = input.hidden.reserved.contains(&link);
        if owners.len() < 2 && !taken {
            continue;
        }
        let rank = |id: &ObjectId| {
            if input.remote.entry_at(&link) == Some(*id) {
                0
            } else if input.base.entry_at(&link) == Some(*id) {
                1
            } else {
                2
            }
        };
        let winner = *owners
            .iter()
            .min_by_key(|id| (rank(id), **id))
            .expect("at least two owners");
        let in_git = folder_in_git(link.parent, input.local, input.remote);
        for loser in owners.into_iter().filter(|o| taken || *o != winner) {
            let mut attempt = 0;
            let renamed = loop {
                let candidate = Link {
                    parent: link.parent,
                    name: conflict_name(&link.name, input.host, input.date, attempt, in_git),
                };
                if used.insert(candidate.clone()) {
                    break candidate;
                }
                attempt += 1;
            };
            let t = plan
                .target
                .get_mut(&loser)
                .expect("an owner is in the target");
            t.links.remove(&link);
            t.links.insert(renamed.clone());
            // New beside what the server has at that name, and other than it: a conflict copy like
            // any other, in the inbox until a person deals with it.
            if t.conflict_of.is_none() && input.remote.entry_at(&link) == Some(winner) {
                t.conflict_of = Some(ConflictOf {
                    object: winner,
                    competing_revision: input.remote.nodes[&winner].rev,
                });
            }
            for copy in plan.copies.iter_mut().filter(|c| c.copy == loser) {
                for m in copy.moves.iter_mut().filter(|m| m.1 == link) {
                    m.1 = renamed.clone();
                }
            }
            plan.reports.push(Report {
                kind: ReportKind::SameNameKeptBoth,
                object: loser,
                detail: format!(
                    "the name {} was taken on another device; both kept",
                    link.name
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::{Bytes, Hash, Node, Op, RecordKey, Timestamp, NS_LINUX_XATTR};

    fn name(s: &str) -> Name {
        Name::new(s.as_bytes().to_vec()).unwrap()
    }
    fn at(s: &str) -> Link {
        Link {
            parent: ObjectId::ROOT,
            name: name(s),
        }
    }
    fn file(s: &str) -> Content {
        let h = Hash::of(s.as_bytes());
        Content::File {
            manifest: h,
            size: s.len() as u64,
        }
    }
    fn meta(mode: u32) -> Metadata {
        Metadata {
            mode,
            ..Default::default()
        }
    }

    struct T {
        base: Tree,
        remote: Tree,
        local: BTreeMap<ObjectId, LNode>,
    }

    impl T {
        fn new() -> Self {
            let mut local = BTreeMap::new();
            local.insert(
                ObjectId::ROOT,
                LNode {
                    links: BTreeSet::new(),
                    content: Content::Dir,
                    stored: Metadata::default(),
                    apply_from: Metadata::default(),
                    carried: false,
                    conflict_of: None,
                },
            );
            Self {
                base: Tree::new(),
                remote: Tree::new(),
                local,
            }
        }

        /// The same object on all three sides.
        fn shared(&mut self, link: Link, content: Content, metadata: Metadata) -> ObjectId {
            let id = ObjectId::random();
            let body = RevisionBody {
                content: content.clone(),
                metadata: metadata.clone(),
            };
            let node = Node {
                rev: RevisionId::random(),
                body,
                links: [link.clone()].into(),
            };
            self.base = add(&self.base, id, node.clone());
            self.remote = add(&self.remote, id, node);
            self.local.insert(id, lnode(link, content, metadata));
            id
        }

        fn run(&self) -> Plan {
            merge(MergeInput {
                base: &self.base,
                local: &self.local,
                remote: &self.remote,
                shared: &BTreeMap::new(),
                hidden: &Hidden::default(),
                host: "host",
                date: "2026-10-31",
                fresh_id: &mut ObjectId::random,
            })
        }

        fn remote_set_body(&mut self, id: ObjectId, content: Content, metadata: Metadata) {
            let base = self.remote.nodes[&id].rev;
            self.remote
                .apply(&Op::Update {
                    object: id,
                    base,
                    revision: RevisionId::random(),
                    body: RevisionBody { content, metadata },
                    restored_from: None,
                })
                .unwrap();
        }

        fn remote_delete(&mut self, id: ObjectId) {
            for link in self.remote.nodes[&id].links.clone() {
                self.remote
                    .apply(&Op::Unlink {
                        object: id,
                        link,
                        base: None,
                    })
                    .unwrap();
            }
        }

        fn remote_add(&mut self, link: Link, content: Content, metadata: Metadata) -> ObjectId {
            let id = ObjectId::random();
            self.remote
                .apply(&Op::Create {
                    object: id,
                    revision: RevisionId::random(),
                    body: RevisionBody { content, metadata },
                    link,
                    conflict_of: None,
                })
                .unwrap();
            id
        }
    }

    fn lnode(link: Link, content: Content, metadata: Metadata) -> LNode {
        LNode {
            links: [link].into(),
            content,
            stored: metadata.clone(),
            apply_from: metadata,
            carried: false,
            conflict_of: None,
        }
    }

    fn add(tree: &Tree, id: ObjectId, node: Node) -> Tree {
        let mut nodes = tree.nodes.clone();
        nodes.insert(id, node);
        Tree::from_nodes(nodes)
    }

    fn only(plan: &Plan, kind: ReportKind) -> usize {
        plan.reports.iter().filter(|r| r.kind == kind).count()
    }

    #[test]
    fn one_side_only_applies_it() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("old"), meta(0o644));
        let b = t.shared(at("b"), file("old"), meta(0o644));
        t.local.get_mut(&a).unwrap().content = file("local edit");
        t.remote_set_body(b, file("remote edit"), meta(0o644));
        let p = t.run();
        assert_eq!(p.target[&a].body.content, file("local edit"));
        assert_eq!(p.target[&b].body.content, file("remote edit"));
        assert!(p.copies.is_empty());
    }

    #[test]
    fn metadata_on_both_different_fields_merges_field_by_field() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("x"), meta(0o644));
        t.local.get_mut(&a).unwrap().stored.mode = 0o600;
        let mut remote = meta(0o644);
        remote.records.insert(
            RecordKey {
                ns: NS_LINUX_XATTR.into(),
                name: Bytes(b"user.x".to_vec()),
            },
            Bytes(b"1".to_vec()),
        );
        t.remote_set_body(a, file("x"), remote);
        let p = t.run();
        let m = &p.target[&a].body.metadata;
        assert_eq!(m.mode, 0o600);
        assert_eq!(m.records.len(), 1);
        assert!(p.notes.is_empty());
    }

    #[test]
    fn metadata_on_both_same_field_server_wins_and_the_loser_is_logged() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("x"), meta(0o644));
        t.local.get_mut(&a).unwrap().stored.mode = 0o600;
        t.remote_set_body(a, file("x"), meta(0o640));
        let p = t.run();
        assert_eq!(p.target[&a].body.metadata.mode, 0o640);
        assert_eq!(p.notes.len(), 1, "the loser goes in the activity log");
        assert!(p.notes[0].1.contains("600"));
    }

    #[test]
    fn content_on_one_and_metadata_on_the_other_applies_both() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("old"), meta(0o644));
        t.local.get_mut(&a).unwrap().content = file("new");
        t.remote_set_body(a, file("old"), meta(0o755));
        let p = t.run();
        assert_eq!(p.target[&a].body.content, file("new"));
        assert_eq!(p.target[&a].body.metadata.mode, 0o755);
        assert!(p.copies.is_empty());
    }

    #[test]
    fn content_on_one_and_rename_on_the_other_applies_both() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("old"), meta(0o644));
        t.local.get_mut(&a).unwrap().content = file("new");
        let from = at("a");
        t.remote
            .apply(&Op::Move {
                object: a,
                from,
                to: at("renamed"),
            })
            .unwrap();
        let p = t.run();
        assert_eq!(p.target[&a].body.content, file("new"));
        assert_eq!(p.target[&a].links, [at("renamed")].into());
    }

    #[test]
    fn content_on_both_keeps_both() {
        let mut t = T::new();
        let a = t.shared(at("a.txt"), file("old"), meta(0o644));
        t.local.get_mut(&a).unwrap().content = file("local");
        t.remote_set_body(a, file("remote"), meta(0o644));
        let p = t.run();
        assert_eq!(p.target[&a].body.content, file("remote"));
        let copy = &p.copies[0];
        assert_eq!(p.target[&copy.copy].body.content, file("local"));
        assert_eq!(
            p.target[&copy.copy].links,
            [at("a (conflict host 2026-10-31).txt")].into()
        );
        assert_eq!(p.target[&copy.copy].conflict_of.as_ref().unwrap().object, a);
        assert_eq!(only(&p, ReportKind::ConflictCopy), 1);
    }

    #[test]
    fn a_second_conflict_on_one_day_finds_its_own_name() {
        let mut t = T::new();
        let a = t.shared(at("a.txt"), file("old"), meta(0o644));
        t.shared(
            at("a (conflict host 2026-10-31).txt"),
            file("the first conflict"),
            meta(0o644),
        );
        t.local.get_mut(&a).unwrap().content = file("local");
        t.remote_set_body(a, file("remote"), meta(0o644));
        let p = t.run();
        let copy = &p.copies[0];
        let name = at("a (conflict host 2026-10-31 2).txt");
        assert_eq!(p.target[&copy.copy].links, [name.clone()].into());
        assert_eq!(
            copy.moves[0].1, name,
            "the rename goes where the target says"
        );
    }

    #[test]
    fn the_same_edit_on_both_sides_is_no_conflict() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("old"), meta(0o644));
        t.local.get_mut(&a).unwrap().content = file("same");
        t.remote_set_body(a, file("same"), meta(0o644));
        let p = t.run();
        assert!(p.copies.is_empty());
        assert_eq!(p.target[&a].body.content, file("same"));
    }

    #[test]
    fn a_local_edit_against_a_remote_delete_survives() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("old"), meta(0o644));
        t.local.get_mut(&a).unwrap().content = file("edited");
        t.remote_delete(a);
        let p = t.run();
        assert_eq!(p.target[&a].body.content, file("edited"));
        assert_eq!(only(&p, ReportKind::EditSurvivedDelete), 1);
    }

    #[test]
    fn a_remote_edit_against_a_local_delete_survives() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("old"), meta(0o644));
        t.local.remove(&a);
        t.remote_set_body(a, file("edited"), meta(0o644));
        let p = t.run();
        assert_eq!(p.target[&a].body.content, file("edited"));
        assert_eq!(only(&p, ReportKind::EditSurvivedDelete), 1);
    }

    #[test]
    fn an_unchanged_delete_applies_on_either_side() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("x"), meta(0o644));
        let b = t.shared(at("b"), file("x"), meta(0o644));
        t.local.remove(&a);
        t.remote_delete(b);
        let p = t.run();
        assert!(!p.target.contains_key(&a));
        assert!(!p.target.contains_key(&b));
        assert!(p.reports.is_empty());
    }

    #[test]
    fn the_same_new_name_with_different_content_keeps_both() {
        let mut t = T::new();
        let x = ObjectId::random();
        t.local
            .insert(x, lnode(at("new"), file("mine"), meta(0o644)));
        let y = t.remote_add(at("new"), file("theirs"), meta(0o644));
        let p = t.run();
        assert_eq!(
            p.target[&y].links,
            [at("new")].into(),
            "the server's stays where it is"
        );
        assert_eq!(
            p.target[&x].links,
            [at("new (conflict host 2026-10-31)")].into()
        );
        assert_eq!(only(&p, ReportKind::SameNameKeptBoth), 1);
    }

    #[test]
    fn the_same_new_file_on_both_sides_becomes_one_object() {
        let mut t = T::new();
        let x = ObjectId::random();
        t.local
            .insert(x, lnode(at("new"), file("same"), meta(0o644)));
        let y = t.remote_add(at("new"), file("same"), meta(0o644));
        let p = t.run();
        assert_eq!(p.adopted[&x], y);
        assert!(!p.target.contains_key(&x));
        assert_eq!(p.target[&y].links, [at("new")].into());
        assert!(p.reports.is_empty());
    }

    #[test]
    fn an_adopted_copy_with_other_metadata_leaves_a_log_entry() {
        let mut t = T::new();
        let x = ObjectId::random();
        t.local
            .insert(x, lnode(at("new"), file("same"), meta(0o750)));
        let y = t.remote_add(at("new"), file("same"), meta(0o644));
        let p = t.run();
        assert_eq!(p.adopted[&x], y);
        assert_eq!(p.target[&y].body.metadata.mode, 0o644);
        assert_eq!(p.notes.len(), 1);
        assert!(p.notes[0].1.contains("750"), "{:?}", p.notes);
    }

    fn with(mode: u32, records: &[(&str, &str)]) -> Metadata {
        let mut m = meta(mode);
        for (k, v) in records {
            m.records.insert(
                RecordKey {
                    ns: NS_LINUX_XATTR.into(),
                    name: Bytes(k.as_bytes().to_vec()),
                },
                Bytes(v.as_bytes().to_vec()),
            );
        }
        m
    }

    fn record<'a>(m: &'a Metadata, k: &str) -> Option<&'a [u8]> {
        m.records
            .get(&RecordKey {
                ns: NS_LINUX_XATTR.into(),
                name: Bytes(k.as_bytes().to_vec()),
            })
            .map(|v| v.0.as_slice())
    }

    /// A joining device's copy: what only it has stays, and goes to the server; where both have a
    /// value the server's stands, and this device's is in the log.
    #[test]
    fn an_adopted_copy_keeps_what_the_server_lacks_and_logs_what_it_loses() {
        let mut t = T::new();
        let x = ObjectId::random();
        let here = with(0o644, &[("user.mine", "2"), ("user.both", "mine")]);
        t.local
            .insert(x, lnode(at("f"), file("same"), here.clone()));
        let y = t.remote_add(
            at("f"),
            file("same"),
            with(0o664, &[("user.server", "1"), ("user.both", "server's")]),
        );
        let p = t.run();
        assert_eq!(p.adopted[&x], y);
        let m = &p.target[&y].body.metadata;
        assert_eq!(m.mode, 0o664);
        assert_eq!(record(m, "user.mine"), Some(b"2".as_slice()));
        assert_eq!(record(m, "user.server"), Some(b"1".as_slice()));
        assert_eq!(record(m, "user.both"), Some(b"server's".as_slice()));
        assert_eq!(p.local[&y].apply_from, here, "against what is on disk");
        let notes: Vec<&str> = p.notes.iter().map(|n| n.1.as_str()).collect();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("this device had 644")));
        assert!(notes.iter().any(|n| n.contains("this device had mine")));
    }

    fn linked_on_the_server(t: &mut T, names: &[&str], content: Content) -> ObjectId {
        let y = t.remote_add(at(names[0]), content, meta(0o644));
        for n in &names[1..] {
            t.remote
                .apply(&Op::Link {
                    object: y,
                    link: at(n),
                })
                .unwrap();
        }
        y
    }

    #[test]
    fn separate_equal_copies_of_a_file_with_several_names_become_its_links() {
        let mut t = T::new();
        let y = linked_on_the_server(&mut t, &["one", "two"], file("shared"));
        let x1 = ObjectId::random();
        let x2 = ObjectId::random();
        t.local
            .insert(x1, lnode(at("one"), file("shared"), meta(0o644)));
        t.local
            .insert(x2, lnode(at("two"), file("shared"), meta(0o600)));
        let p = t.run();
        assert_eq!(p.adopted.values().collect::<Vec<_>>(), vec![&y]);
        assert_eq!(p.target[&y].links, [at("one"), at("two")].into());
        let objects = |p: &Plan| p.target.keys().filter(|id| **id != ObjectId::ROOT).count();
        assert_eq!(objects(&p), 1, "no copy of its own: {:?}", p.target);
        assert_eq!(only(&p, ReportKind::SameNameKeptBoth), 0);
        assert_eq!(only(&p, ReportKind::Linked), 1);
        assert!(
            p.notes
                .iter()
                .any(|n| n.1.contains("this device's copy at two had 600")),
            "{:?}",
            p.notes
        );
    }

    /// The copy that stays is the one whose inode holds what the others have.
    #[test]
    fn the_copy_with_records_of_its_own_is_the_one_that_stays() {
        let mut t = T::new();
        let y = linked_on_the_server(&mut t, &["one", "two"], file("shared"));
        let plain = ObjectId::random();
        let tagged = ObjectId::random();
        t.local
            .insert(plain, lnode(at("one"), file("shared"), meta(0o644)));
        t.local.insert(
            tagged,
            lnode(
                at("two"),
                file("shared"),
                with(0o644, &[("user.tag", "red")]),
            ),
        );
        let p = t.run();
        assert_eq!(p.adopted.get(&tagged), Some(&y));
        assert!(!p.target.contains_key(&plain));
        assert_eq!(
            record(&p.target[&y].body.metadata, "user.tag"),
            Some(b"red".as_slice())
        );
    }

    /// Folding it away would take a name only it has.
    #[test]
    fn a_copy_with_a_name_the_file_lacks_is_not_folded_away() {
        let mut t = T::new();
        linked_on_the_server(&mut t, &["one", "two"], file("shared"));
        let first = ObjectId::random();
        let other = ObjectId::random();
        t.local
            .insert(first, lnode(at("one"), file("shared"), meta(0o644)));
        let mut n = lnode(at("two"), file("shared"), meta(0o644));
        n.links.insert(at("mine"));
        t.local.insert(other, n);
        let p = t.run();
        let (kept, adopter) = if p.adopted.contains_key(&first) {
            (other, first)
        } else {
            (first, other)
        };
        assert!(p.target.contains_key(&kept), "{:?}", p.target);
        assert!(!p.target.contains_key(&adopter));
    }

    /// Folding it in would remove what it alone holds before the file here holds it too.
    #[test]
    fn a_copy_with_something_the_file_here_would_not_hold_stays_a_listed_conflict_copy() {
        let mut t = T::new();
        let y = linked_on_the_server(&mut t, &["one", "two"], file("shared"));
        let a = ObjectId::random();
        let b = ObjectId::random();
        t.local.insert(
            a,
            lnode(at("one"), file("shared"), with(0o644, &[("user.a", "1")])),
        );
        t.local.insert(
            b,
            lnode(at("two"), file("shared"), with(0o644, &[("user.b", "2")])),
        );
        let p = t.run();
        let (kept, lost_name) = if p.adopted.contains_key(&a) {
            (b, "two")
        } else {
            (a, "one")
        };
        let copy = &p.target[&kept];
        assert_ne!(copy.links, [at(lost_name)].into());
        assert_eq!(
            copy.conflict_of,
            Some(ConflictOf {
                object: y,
                competing_revision: t.remote.nodes[&y].rev,
            })
        );
    }

    /// Found by review: the same new name with other bytes was kept beside, but in no inbox.
    #[test]
    fn a_new_name_the_server_has_with_other_bytes_is_a_listed_conflict() {
        let mut t = T::new();
        let x = ObjectId::random();
        t.local
            .insert(x, lnode(at("new"), file("mine"), meta(0o644)));
        let y = t.remote_add(at("new"), file("theirs"), meta(0o644));
        let p = t.run();
        assert_eq!(
            p.target[&x].conflict_of,
            Some(ConflictOf {
                object: y,
                competing_revision: t.remote.nodes[&y].rev,
            })
        );
        assert!(p.target[&y].conflict_of.is_none());
    }

    /// Found by review: a file unobservable after this device pushed it stood for the older base,
    /// and read as a local revert of the push.
    #[test]
    fn what_could_not_be_observed_never_pushes() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("old"), meta(0o644));
        t.remote_set_body(a, file("newer"), meta(0o644));
        t.local.get_mut(&a).unwrap().carried = true;
        let shared: BTreeMap<_, _> = [(
            a,
            Shared {
                body: Some(RevisionBody {
                    content: file("newer"),
                    metadata: meta(0o644),
                }),
                links: None,
            },
        )]
        .into();
        let p = merge(MergeInput {
            base: &t.base,
            local: &t.local,
            remote: &t.remote,
            shared: &shared,
            hidden: &Hidden::default(),
            host: "host",
            date: "2026-10-31",
            fresh_id: &mut ObjectId::random,
        });
        assert_eq!(
            p.target[&a].body.content,
            file("newer"),
            "the server stands"
        );
        assert!(p.copies.is_empty());
    }

    #[test]
    fn the_same_new_directory_on_both_sides_merges_its_contents() {
        let mut t = T::new();
        let x = ObjectId::random();
        let child = ObjectId::random();
        t.local.insert(x, lnode(at("d"), Content::Dir, meta(0o755)));
        t.local.insert(
            child,
            lnode(
                Link {
                    parent: x,
                    name: name("mine"),
                },
                file("1"),
                meta(0o644),
            ),
        );
        let y = t.remote_add(at("d"), Content::Dir, meta(0o755));
        t.remote_add(
            Link {
                parent: y,
                name: name("theirs"),
            },
            file("2"),
            meta(0o644),
        );
        let p = t.run();
        assert_eq!(p.adopted[&x], y);
        assert_eq!(
            p.target[&child].links,
            [Link {
                parent: y,
                name: name("mine")
            }]
            .into()
        );
        assert!(p.reports.is_empty());
    }

    #[test]
    fn a_new_name_of_another_kind_is_kept_under_a_conflict_name() {
        let mut t = T::new();
        let x = ObjectId::random();
        t.local.insert(x, lnode(at("n"), Content::Dir, meta(0o755)));
        let y = t.remote_add(at("n"), file("data"), meta(0o644));
        let p = t.run();
        assert_eq!(p.target[&y].links, [at("n")].into());
        assert_ne!(p.target[&x].links, [at("n")].into());
        assert!(p.adopted.is_empty());
    }

    #[test]
    fn concurrent_moves_that_would_cycle_are_refused() {
        let mut t = T::new();
        let a = t.shared(at("a"), Content::Dir, meta(0o755));
        let b = t.shared(at("b"), Content::Dir, meta(0o755));
        // Locally a moves under b; remotely b moves under a.
        t.local.get_mut(&a).unwrap().links = [Link {
            parent: b,
            name: name("a"),
        }]
        .into();
        t.remote
            .apply(&Op::Move {
                object: b,
                from: at("b"),
                to: Link {
                    parent: a,
                    name: name("b"),
                },
            })
            .unwrap();
        let p = t.run();
        assert!(find_cycle(&p.target).is_none());
        assert_eq!(only(&p, ReportKind::CycleRefused), 1);
        // The remote move stands.
        assert_eq!(
            p.target[&b].links,
            [Link {
                parent: a,
                name: name("b")
            }]
            .into()
        );
        assert_eq!(p.target[&a].links, [at("a")].into());
    }

    #[test]
    fn something_new_inside_a_deleted_directory_brings_it_back() {
        let mut t = T::new();
        let d = t.shared(at("d"), Content::Dir, meta(0o755));
        let child = ObjectId::random();
        // Local: a new file in d. Remote: d deleted.
        t.local.insert(
            child,
            lnode(
                Link {
                    parent: d,
                    name: name("new"),
                },
                file("x"),
                meta(0o644),
            ),
        );
        t.remote_delete(d);
        let p = t.run();
        assert!(p.target.contains_key(&child));
        assert!(p.target.contains_key(&d), "the parent comes back");
        assert_eq!(only(&p, ReportKind::KeptForChild), 1);
    }

    #[test]
    fn a_local_delete_of_a_directory_with_something_new_remotely_keeps_it() {
        let mut t = T::new();
        let d = t.shared(at("d"), Content::Dir, meta(0o755));
        t.local.remove(&d);
        let child = t.remote_add(
            Link {
                parent: d,
                name: name("new"),
            },
            file("x"),
            meta(0o644),
        );
        let p = t.run();
        assert!(p.target.contains_key(&child));
        assert!(p.target.contains_key(&d));
    }

    #[test]
    fn moved_on_both_sides_the_server_wins_and_it_is_reported() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("x"), meta(0o644));
        t.local.get_mut(&a).unwrap().links = [at("mine")].into();
        t.remote
            .apply(&Op::Move {
                object: a,
                from: at("a"),
                to: at("theirs"),
            })
            .unwrap();
        let p = t.run();
        assert_eq!(p.target[&a].links, [at("theirs")].into());
        assert_eq!(only(&p, ReportKind::MoveConflict), 1);
    }

    #[test]
    fn a_rename_on_one_side_and_chmod_on_the_other_applies_both() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("x"), meta(0o644));
        t.local.get_mut(&a).unwrap().links = [at("renamed")].into();
        t.remote_set_body(a, file("x"), meta(0o755));
        let p = t.run();
        assert_eq!(p.target[&a].links, [at("renamed")].into());
        assert_eq!(p.target[&a].body.metadata.mode, 0o755);
    }

    /// Found by the randomized harness: one device edits a hard-linked file while two others remove
    /// one name each. No device deleted the file, and the edit must not vanish.
    #[test]
    fn links_removed_on_each_side_do_not_delete_an_edit() {
        let mut t = T::new();
        let a = t.shared(at("one"), file("old"), meta(0o644));
        for tree in [&mut t.base, &mut t.remote] {
            tree.apply(&Op::Link {
                object: a,
                link: at("two"),
            })
            .unwrap();
        }
        t.local.get_mut(&a).unwrap().links = [at("two")].into();
        // The server: "two" removed, and the content changed.
        t.remote
            .apply(&Op::Unlink {
                object: a,
                link: at("two"),
                base: None,
            })
            .unwrap();
        t.remote_set_body(a, file("edited"), meta(0o644));
        let p = t.run();
        assert_eq!(p.target[&a].body.content, file("edited"));
        assert_eq!(p.target[&a].links, [at("one")].into());
        assert_eq!(only(&p, ReportKind::EditSurvivedDelete), 1);
    }

    #[test]
    fn links_removed_on_each_side_with_no_edit_is_a_delete() {
        let mut t = T::new();
        let a = t.shared(at("one"), file("old"), meta(0o644));
        for tree in [&mut t.base, &mut t.remote] {
            tree.apply(&Op::Link {
                object: a,
                link: at("two"),
            })
            .unwrap();
        }
        t.local.get_mut(&a).unwrap().links = [at("two")].into();
        t.remote
            .apply(&Op::Unlink {
                object: a,
                link: at("two"),
                base: None,
            })
            .unwrap();
        let p = t.run();
        assert!(
            !p.target.contains_key(&a),
            "no one edited it, so it is gone"
        );
    }

    #[test]
    fn hard_links_added_on_each_side_all_stay() {
        let mut t = T::new();
        let a = t.shared(at("one"), file("x"), meta(0o644));
        t.local.get_mut(&a).unwrap().links.insert(at("mine"));
        t.remote
            .apply(&Op::Link {
                object: a,
                link: at("theirs"),
            })
            .unwrap();
        let p = t.run();
        assert_eq!(
            p.target[&a].links,
            [at("one"), at("mine"), at("theirs")].into()
        );
    }

    #[test]
    fn a_shared_ancestor_stops_our_own_landed_commit_looking_like_a_conflict() {
        let mut t = T::new();
        let a = t.shared(at("a"), file("old"), meta(0o644));
        // We pushed "pushed", the base has not caught up, and we edited again.
        t.remote_set_body(a, file("pushed"), meta(0o644));
        t.local.get_mut(&a).unwrap().content = file("edited again");
        let shared: BTreeMap<_, _> = [(
            a,
            Shared {
                body: Some(RevisionBody {
                    content: file("pushed"),
                    metadata: meta(0o644),
                }),
                links: None,
            },
        )]
        .into();
        let p = merge(MergeInput {
            base: &t.base,
            local: &t.local,
            remote: &t.remote,
            shared: &shared,
            hidden: &Hidden::default(),
            host: "host",
            date: "2026-10-31",
            fresh_id: &mut ObjectId::random,
        });
        assert!(p.copies.is_empty());
        assert_eq!(p.target[&a].body.content, file("edited again"));
    }

    #[test]
    fn dir_mtime_alone_does_not_save_a_deleted_directory() {
        let mut t = T::new();
        let d = t.shared(at("d"), Content::Dir, meta(0o755));
        t.local.get_mut(&d).unwrap().stored.mtime = Timestamp { sec: 5, nsec: 0 };
        t.remote_delete(d);
        let p = t.run();
        assert!(!p.target.contains_key(&d));
    }

    #[test]
    fn conflict_names_keep_the_extension_and_the_limit() {
        assert_eq!(
            conflict_name(&name("a.tar.gz"), "h", "d", 0, false).to_string(),
            "a.tar (conflict h d).gz"
        );
        assert_eq!(
            conflict_name(&name(".bashrc"), "h", "d", 0, false).to_string(),
            ".bashrc (conflict h d)"
        );
        assert_eq!(
            conflict_name(&name("x"), "h", "d", 1, false).to_string(),
            "x (conflict h d 2)"
        );
        let long = "n".repeat(255);
        assert!(
            conflict_name(&name(&long), "h", "d", 0, false)
                .as_bytes()
                .len()
                <= 255
        );
        let long_ext = format!("x.{}", "e".repeat(253));
        let named = conflict_name(&name(&long_ext), "h", "d", 0, false);
        assert!(named.as_bytes().len() <= 255, "{}", named.as_bytes().len());
        assert!(named.to_string().contains("(conflict h d)"));
    }

    /// git's check-ref-format, for what a conflict name can contain.
    fn valid_ref_component(c: &str) -> bool {
        !c.is_empty()
            && !c.starts_with('.')
            && !c.ends_with('.')
            && !c.ends_with(".lock")
            && !c.contains("..")
            && !c.contains("@{")
            && !c
                .bytes()
                .any(|b| b < 0x20 || b == 0x7f || b" ~^:?*[\\".contains(&b))
    }

    #[test]
    fn conflict_names_in_a_git_directory_are_names_git_takes() {
        assert_eq!(
            conflict_name(&name("main"), "me", "2026-11-03", 0, true).to_string(),
            "main.conflict-me-2026-11-03"
        );
        assert_eq!(
            conflict_name(&name("main"), "me", "2026-11-03", 2, true).to_string(),
            "main.conflict-me-2026-11-03-3"
        );
        // Kept whole, so git does not take it for one half of a pack.
        assert_eq!(
            conflict_name(&name("pack-1.idx"), "me", "d", 0, true).to_string(),
            "pack-1.idx.conflict-me-d"
        );
        for (host, date) in [
            ("my laptop", "seq 12"),
            ("a~b^c:d", "x..y"),
            ("h@{1}", "*[?]"),
        ] {
            for stem in ["main", "main.lock", "feature.v2"] {
                let named = conflict_name(&name(stem), host, date, 0, true).to_string();
                assert!(valid_ref_component(&named), "{named}");
            }
        }
        let long = "n".repeat(255);
        assert!(
            conflict_name(&name(&long), "h", "d", 0, true)
                .as_bytes()
                .len()
                <= 255
        );
        assert!(in_git_dir(&[name("repo"), name(".git"), name("refs")]));
        assert!(in_git_dir(&[name("srv"), name("repo.git")]));
        assert!(!in_git_dir(&[name("repo"), name("src")]));
    }
}
