//! What was deleted lately and is still deleted, and bringing it back as `cairn-sync undelete` does: each
//! object linked again to the entry it last had, as itself, with nothing reverted or replaced.
//!
//! Both are answered from an index of each namespace's journal, replayed when the server starts and
//! kept up with every commit, so that a list reads nothing of the store but what is new.

use super::edit::commit;
use super::{conflict, display_name, View};
use crate::{blocking, now, ApiResult, App};
use anyhow::{anyhow, Result};
use axum::extract::{Extension, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use cairn_protocol::name::display_bytes;
use cairn_protocol::web::{self as w};
use cairn_protocol::wire::{Access, Change};
use cairn_protocol::{
    conflict_name, in_git_dir, Content, Link, Name, ObjectId, ObjectKind, Op, RevisionBody,
};
use cairn_store::MetaStore;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

/// How much of the journal one read of the store takes, so that no device's sync waits for all of it.
const PAGE_CHANGES: u32 = 1000;
const PAGE_BYTES: usize = 8 * 1024 * 1024;
/// What a device deleted in a folder this long before deleting the folder went with it.
const WITH_FOLDER_SECS: i64 = 600;
/// More folders above one than this is a corrupt index, not a deep tree.
const MAX_DEPTH: usize = 4096;

/// What the index keeps of each object: no body, and no entry but a folder's, in `Replayed::dirs`.
#[derive(Clone, Copy)]
struct Obj {
    kind: ObjectKind,
    /// A file's size, 0 for anything else.
    size: u64,
    /// Its entries now: none once deleted, more than one for a file with hard links.
    links: u32,
}

/// The delete that left an object with no entry.
#[derive(Clone)]
struct Gone {
    link: Link,
    seq: u64,
    time: i64,
    /// Into `Replayed::names`: the device that deleted it, and the user whose token it sent.
    device: u32,
    user: Option<u32>,
}

/// One namespace's journal, replayed: each object's kind, size and number of entries, where each live
/// folder is, and for each object with no entry the unlink that left it so. About 90 bytes an
/// object.
struct Replayed {
    /// The last sequence replayed.
    seq: u64,
    objects: HashMap<ObjectId, Obj>,
    dirs: HashMap<ObjectId, Link>,
    gone: HashMap<ObjectId, Gone>,
    /// Device and user names, each once.
    names: Vec<String>,
    name_ix: HashMap<String, u32>,
    /// Set when the journal did not replay: said to each request rather than a list that may be wrong,
    /// until the server starts again.
    broken: Option<String>,
}

/// Every namespace's index, each made at the server's start or at its first request, and kept.
#[derive(Default)]
pub(crate) struct Indexes(Mutex<HashMap<String, Arc<Mutex<Replayed>>>>);

impl Indexes {
    fn of(&self, ns: &str) -> Arc<Mutex<Replayed>> {
        self.0
            .lock()
            .expect("a panic aborts the process")
            .entry(ns.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Replayed::new())))
            .clone()
    }

    /// A namespace renamed: its index is kept by its name now.
    pub(crate) fn renamed(&self, from: &str, to: &str) {
        let mut all = self.0.lock().expect("a panic aborts the process");
        if let Some(index) = all.remove(from) {
            all.insert(to.to_string(), index);
        }
    }
}

fn lock(index: &Mutex<Replayed>) -> MutexGuard<'_, Replayed> {
    index.lock().expect("a panic aborts the process")
}

/// Every index brought up to its namespace's head: at the start each namespace's, replayed from its
/// first commit, and after a commit those already made.
pub(crate) fn keep_up(app: &App, every_namespace: bool) -> Result<()> {
    let names = app.repo.namespaces()?;
    if every_namespace {
        for ns in &names {
            app.deleted.of(ns);
        }
    }
    // One made by a name that a rename had just made former, by a request begun before it: the index
    // by the name now stands for the namespace.
    app.deleted
        .0
        .lock()
        .expect("a panic aborts the process")
        .retain(|ns, _| names.contains(ns));
    let all: Vec<(String, Arc<Mutex<Replayed>>)> = app
        .deleted
        .0
        .lock()
        .expect("a panic aborts the process")
        .iter()
        .map(|(ns, i)| (ns.clone(), i.clone()))
        .collect();
    let mut failed = None;
    for (ns, index) in all {
        let mut index = lock(&index);
        if index.broken.is_some() {
            continue;
        }
        if let Err(e) = index.catch_up(app, &ns) {
            failed = Some(e.context(format!("the index of what is deleted in {ns:?}")));
        }
    }
    failed.map_or(Ok(()), Err)
}

fn size_of(body: &RevisionBody) -> u64 {
    match body.content {
        Content::File { size, .. } => size,
        _ => 0,
    }
}

impl Replayed {
    fn new() -> Self {
        let mut objects = HashMap::new();
        // The root has no entry and is never deleted; its metadata changes as any folder's does.
        objects.insert(
            ObjectId::ROOT,
            Obj {
                kind: ObjectKind::Dir,
                size: 0,
                links: 0,
            },
        );
        Replayed {
            seq: 0,
            objects,
            dirs: HashMap::new(),
            gone: HashMap::new(),
            names: Vec::new(),
            name_ix: HashMap::new(),
            broken: None,
        }
    }

    /// What is new in the journal since the last replayed, a page at a time. A store that cannot be
    /// read leaves the index as it was, to go on from the next time; a journal that does not replay
    /// leaves it broken, and says so.
    fn catch_up(&mut self, app: &App, ns: &str) -> Result<()> {
        self.catch_up_by(ns, |after| {
            app.repo
                .with_meta(|m| m.changes(ns, after, PAGE_CHANGES, PAGE_BYTES))
        })
    }

    /// As `catch_up`, each page as `read` gives what the journal has after a sequence.
    fn catch_up_by(
        &mut self,
        ns: &str,
        mut read: impl FnMut(u64) -> Result<Vec<Change>>,
    ) -> Result<()> {
        if let Some(why) = &self.broken {
            return Err(anyhow!("{why}; it is read again when the server starts"));
        }
        loop {
            let page = read(self.seq)?;
            if page.is_empty() {
                return Ok(());
            }
            for change in &page {
                if let Err(e) = self.apply(change) {
                    let why = format!("{e:#}");
                    eprintln!("the index of what is deleted in {ns:?}: {why}");
                    *self = Replayed::new();
                    self.broken = Some(why.clone());
                    return Err(anyhow!(why));
                }
                self.seq = change.seq;
            }
        }
    }

    fn name(&mut self, s: &str) -> u32 {
        if let Some(i) = self.name_ix.get(s) {
            return *i;
        }
        let i = self.names.len() as u32;
        self.names.push(s.to_string());
        self.name_ix.insert(s.to_string(), i);
        i
    }

    /// One commit, as the store applied it (`check_and_apply`): anything it could not have applied is
    /// a journal that does not replay.
    fn apply(&mut self, change: &Change) -> Result<()> {
        let device = self.name(&change.device_name);
        let user = change.user.as_deref().map(|u| self.name(u));
        let corrupt = |o: &ObjectId| {
            anyhow!(
                "the journal does not replay at seq {}: object {o}",
                change.seq
            )
        };
        for op in &change.ops {
            match op {
                Op::Create {
                    object, body, link, ..
                } => {
                    let kind = body.content.kind();
                    let new = Obj {
                        kind,
                        size: size_of(body),
                        links: 1,
                    };
                    if self.objects.insert(*object, new).is_some() {
                        return Err(corrupt(object));
                    }
                    if kind == ObjectKind::Dir {
                        self.dirs.insert(*object, link.clone());
                    }
                }
                Op::Update { object, body, .. } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    if (o.links == 0 && *object != ObjectId::ROOT) || o.kind != body.content.kind()
                    {
                        return Err(corrupt(object));
                    }
                    o.size = size_of(body);
                }
                Op::Link { object, link } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    o.links += 1;
                    if o.links == 1 {
                        self.gone.remove(object);
                    }
                    if o.kind == ObjectKind::Dir
                        && (o.links > 1 || self.dirs.insert(*object, link.clone()).is_some())
                    {
                        return Err(corrupt(object));
                    }
                }
                Op::Unlink { object, link, .. } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    o.links = o.links.checked_sub(1).ok_or_else(|| corrupt(object))?;
                    if o.kind == ObjectKind::Dir && self.dirs.remove(object).as_ref() != Some(link)
                    {
                        return Err(corrupt(object));
                    }
                    if o.links == 0 {
                        self.gone.insert(
                            *object,
                            Gone {
                                link: link.clone(),
                                seq: change.seq,
                                time: change.time,
                                device,
                                user,
                            },
                        );
                    }
                }
                Op::Move { object, from, to } => {
                    let o = self.objects.get(object).ok_or_else(|| corrupt(object))?;
                    if o.links == 0 {
                        return Err(corrupt(object));
                    }
                    if o.kind == ObjectKind::Dir {
                        match self.dirs.get_mut(object) {
                            Some(at) if at == from => *at = to.clone(),
                            _ => return Err(corrupt(object)),
                        }
                    }
                }
                Op::Note { .. } => {}
            }
        }
        Ok(())
    }

    /// Whether `o` went with the folder it was in: deleted by the same device, with it or in the 10
    /// minutes before.
    fn with_folder(&self, o: &ObjectId) -> Option<ObjectId> {
        let g = self.gone.get(o)?;
        let p = self.gone.get(&g.link.parent)?;
        (g.device == p.device && g.seq <= p.seq && p.time - g.time <= WITH_FOLDER_SECS)
            .then_some(g.link.parent)
    }

    /// What went with each folder, by the folder.
    fn groups(&self) -> HashMap<ObjectId, Vec<ObjectId>> {
        let mut out: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        for o in self.gone.keys() {
            if let Some(p) = self.with_folder(o) {
                out.entry(p).or_default().push(*o);
            }
        }
        out
    }

    /// The names of the folders from the root to `dir`: through deleted ones by where they were.
    fn path(&self, dir: ObjectId) -> Option<Vec<Name>> {
        let mut out = Vec::new();
        let mut at = dir;
        while at != ObjectId::ROOT {
            if out.len() > MAX_DEPTH {
                return None;
            }
            let link = self
                .dirs
                .get(&at)
                .or_else(|| self.gone.get(&at).map(|g| &g.link))?;
            out.push(link.name.clone());
            at = link.parent;
        }
        out.reverse();
        Some(out)
    }

    /// Everything deleted since `since` and still deleted, newest first: a folder once, counting the
    /// files that went with it, all the way down.
    fn list(&self, view: &View, since: i64) -> w::DeletedList {
        let groups = self.groups();
        let mut items = Vec::new();
        for (object, g) in &self.gone {
            if g.time < since || self.with_folder(object).is_some() {
                continue;
            }
            let Some(o) = self.objects.get(object) else {
                continue;
            };
            let (mut files, mut size) = (0u64, 0u64);
            if o.kind == ObjectKind::Dir {
                let mut stack = vec![*object];
                // Each object at most once, whatever the index holds.
                let mut left = self.gone.len();
                while let Some(d) = stack.pop() {
                    for c in groups.get(&d).into_iter().flatten() {
                        left = left.saturating_sub(1);
                        match self.objects.get(c) {
                            Some(h) if h.kind == ObjectKind::Dir => {
                                if left > 0 {
                                    stack.push(*c);
                                }
                            }
                            Some(h) => {
                                files += 1;
                                size += h.size;
                            }
                            None => {}
                        }
                    }
                }
            } else {
                size = o.size;
            }
            let path = self
                .path(g.link.parent)
                .map(|p| joined(view, &p))
                .unwrap_or_else(|| "…".into());
            items.push(w::DeletedEntry {
                object: *object,
                kind: o.kind,
                display: display_bytes(g.link.name.as_bytes()),
                path,
                seq: g.seq,
                time: g.time,
                user: g.user.map(|u| self.names[u as usize].clone()),
                device_name: self.names[g.device as usize].clone(),
                size,
                files,
            });
        }
        items.sort_by(|a, b| b.seq.cmp(&a.seq).then_with(|| a.display.cmp(&b.display)));
        w::DeletedList {
            items,
            complete: true,
        }
    }
}

fn joined(view: &View, names: &[Name]) -> String {
    names
        .iter()
        .map(|n| display_name(view, Some(n)))
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Deserialize)]
pub(super) struct DeletedQuery {
    days: Option<u32>,
}

pub(super) async fn list(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Query(q): Query<DeletedQuery>,
) -> ApiResult<Response> {
    let days = i64::from(q.days.unwrap_or(30).min(36_500));
    let list = blocking(move || {
        let index = app.deleted.of(&view.ns);
        let mut index = lock(&index);
        index.catch_up(&app, &view.ns)?;
        Ok(index.list(&view, now() - days * 86_400))
    })
    .await?;
    Ok(Json(list).into_response())
}

/// The links that bring what was asked for back, and what to report of each.
type Planned = (Vec<Op>, Vec<w::UndeletedEntry>);

/// The links that bring each asked-for object back, its deleted folders first, and the items to report.
fn plan(
    m: &mut dyn MetaStore,
    view: &View,
    index: &Replayed,
    asked: &[ObjectId],
) -> Result<Result<Planned, String>> {
    let groups = index.groups();
    let mut placed: BTreeMap<ObjectId, Link> = BTreeMap::new();
    let mut taken: HashMap<ObjectId, HashSet<Name>> = HashMap::new();
    let mut ops = Vec::new();
    for id in asked {
        if placed.contains_key(id) {
            continue;
        }
        // The store is the word on whether it has an entry now: a commit may have come since the index.
        let (Some(g), true) = (index.gone.get(id), m.links_of(&view.ns, *id)?.is_empty()) else {
            return Ok(Err(format!("{id} is not deleted now: reload")));
        };
        // The deleted folders above it, which come back holding only what is asked for.
        let mut chain = Vec::new();
        let mut up = g.link.parent;
        while up != ObjectId::ROOT
            && !placed.contains_key(&up)
            && m.links_of(&view.ns, up)?.is_empty()
        {
            let Some(ug) = index.gone.get(&up) else {
                return Ok(Err(format!(
                    "the folder {} was in is not deleted now: reload",
                    display_bytes(g.link.name.as_bytes())
                )));
            };
            chain.push(up);
            up = ug.link.parent;
            if chain.len() > 4096 {
                return Ok(Err("the folders above it do not end".into()));
            }
        }
        let mut members: Vec<ObjectId> = chain.into_iter().rev().collect();
        // It, then what went with it, each folder before what was in it.
        let mut queue = std::collections::VecDeque::from([*id]);
        while let Some(o) = queue.pop_front() {
            members.push(o);
            let mut with: Vec<ObjectId> = groups.get(&o).cloned().unwrap_or_default();
            with.sort();
            queue.extend(with);
        }
        for o in members {
            if placed.contains_key(&o) {
                continue;
            }
            let Some(og) = index.gone.get(&o) else {
                continue;
            };
            let parent = og.link.parent;
            if let std::collections::hash_map::Entry::Vacant(e) = taken.entry(parent) {
                let names = if placed.contains_key(&parent) {
                    HashSet::new()
                } else {
                    m.children(&view.ns, parent)?
                        .into_iter()
                        .map(|c| c.name)
                        .collect()
                };
                e.insert(names);
            }
            let folder = index.path(parent).unwrap_or_default();
            let names = taken.get_mut(&parent).expect("just made");
            let mut name = og.link.name.clone();
            let mut attempt = 0;
            while names.contains(&name)
                || (parent == ObjectId::ROOT && name.as_bytes() == cairn_protocol::STATE_DIR)
            {
                name = conflict_name(
                    &og.link.name,
                    "restored",
                    &format!("seq {}", og.seq),
                    attempt,
                    in_git_dir(&folder),
                );
                attempt += 1;
            }
            names.insert(name.clone());
            let link = Link { parent, name };
            placed.insert(o, link.clone());
            ops.push(Op::Link { object: o, link });
        }
    }
    let mut items = Vec::new();
    for id in asked {
        let Some(link) = placed.get(id) else { continue };
        if items.iter().any(|i: &w::UndeletedEntry| i.object == *id) {
            continue;
        }
        // Where it is now: the folders that came back by their new names, the rest as they are.
        let mut names = vec![link.name.clone()];
        let mut renamed = index.gone.get(id).is_some_and(|g| g.link.name != link.name);
        let mut at = link.parent;
        while at != ObjectId::ROOT {
            let l = match placed.get(&at) {
                Some(l) => {
                    renamed |= index.gone.get(&at).is_some_and(|g| g.link.name != l.name);
                    l.clone()
                }
                None => match m.links_of(&view.ns, at)?.into_iter().next() {
                    Some(l) => l,
                    None => break,
                },
            };
            names.push(l.name);
            at = l.parent;
            if names.len() > 4096 {
                break;
            }
        }
        names.reverse();
        items.push(w::UndeletedEntry {
            object: *id,
            display: display_bytes(link.name.as_bytes()),
            path: joined(view, &names),
            renamed,
        });
    }
    Ok(Ok((ops, items)))
}

pub(super) async fn undelete(
    State(app): State<Arc<App>>,
    Extension(view): Extension<View>,
    Json(req): Json<w::Undelete>,
) -> ApiResult<Response> {
    if let Some(r) = view.refuses(Access::Write) {
        return Ok(r);
    }
    if req.objects.is_empty() {
        return Ok(super::bad_request("name something to bring back"));
    }
    let answer = blocking(move || {
        // Held to the commit, so that a second request plans with this one's in the index.
        let index = app.deleted.of(&view.ns);
        let mut index = lock(&index);
        index.catch_up(&app, &view.ns)?;
        let planned = app
            .repo
            .with_meta(|m| plan(m, &view, &index, &req.objects))?;
        let (ops, items) = match planned {
            Ok(p) => p,
            Err(why) => return Ok(conflict(why)),
        };
        if ops.is_empty() {
            return Ok(conflict("nothing of that is deleted now"));
        }
        Ok(match commit(&app, &view, ops)? {
            Ok(seq) => Json(w::UndeleteDone { seq, items }).into_response(),
            Err(why) => conflict(why),
        })
    })
    .await?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::wire::CommitRequest;
    use cairn_protocol::{DeviceId, Hash, Metadata, OpId, RevisionId};
    use cairn_store::{CommitOutcome, SqliteStore};

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n.max(1) as u64) as usize
        }
        /// An ID from the seed, so that a failing seed fails again.
        fn id(&mut self) -> [u8; 16] {
            let (a, b) = (self.below(usize::MAX) as u64, self.below(usize::MAX) as u64);
            let mut out = [0; 16];
            out[..8].copy_from_slice(&a.to_le_bytes());
            out[8..].copy_from_slice(&b.to_le_bytes());
            out
        }
    }

    fn body(content: Content) -> RevisionBody {
        RevisionBody {
            content,
            metadata: Metadata::default(),
        }
    }

    fn file(size: u64) -> RevisionBody {
        body(Content::File {
            manifest: Hash::of(&size.to_le_bytes()),
            size,
        })
    }

    /// The index against the store's own tables: every object's kind, size and entries, every live
    /// folder's place, and for each object with no entry the commit and entry that left it so.
    fn agrees(
        index: &Replayed,
        store: &mut SqliteStore,
        unlinked: &HashMap<ObjectId, (Link, u64)>,
    ) {
        let state = store.tree_state(cairn_store::FIRST_FOLDER).unwrap();
        let mut links: HashMap<ObjectId, Vec<Link>> = HashMap::new();
        for (parent, name, object) in &state.entries {
            links.entry(*object).or_default().push(Link {
                parent: *parent,
                name: Name::new(name.clone()).unwrap(),
            });
        }
        assert_eq!(index.objects.len(), state.heads.len());
        let mut dirs = 0;
        for (object, _) in &state.heads {
            let head = store
                .object_head(cairn_store::FIRST_FOLDER, *object)
                .unwrap()
                .unwrap();
            let o = index.objects[object];
            let here = links.get(object).map_or(&[][..], |l| &l[..]);
            assert_eq!(o.kind, head.kind, "{object}");
            assert_eq!(o.size, size_of(&head.body), "{object}");
            assert_eq!(o.links as usize, here.len(), "{object}");
            if head.kind == ObjectKind::Dir && !here.is_empty() {
                dirs += 1;
                assert_eq!(index.dirs.get(object), Some(&here[0]), "{object}");
            }
            if here.is_empty() && *object != ObjectId::ROOT {
                let g = &index.gone[object];
                assert_eq!((&g.link, g.seq), (&unlinked[object].0, unlinked[object].1));
            } else {
                assert!(!index.gone.contains_key(object), "{object}");
            }
        }
        assert_eq!(index.dirs.len(), dirs);
        assert_eq!(index.gone.len(), unlinked.len());
    }

    #[test]
    fn the_index_of_what_is_deleted_agrees_with_the_store_after_any_journal() {
        for seed in 1..=8u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut store = SqliteStore::in_memory().unwrap();
            let mut kept = Replayed::new();
            // What the store says has no entry after each commit, and the commit that left it so.
            let mut unlinked: HashMap<ObjectId, (Link, u64)> = HashMap::new();
            // What the journal came to hold: restores, hard links, folders moved, emptied and deleted.
            let (mut restored, mut hard, mut moved, mut dirs_gone) = (0, 0, 0, 0);
            for step in 0..700i64 {
                let state = store.tree_state(cairn_store::FIRST_FOLDER).unwrap();
                let entries = &state.entries;
                let mut dirs = vec![ObjectId::ROOT];
                let mut kind = HashMap::new();
                for (o, _) in &state.heads {
                    kind.insert(
                        *o,
                        store
                            .object_head(cairn_store::FIRST_FOLDER, *o)
                            .unwrap()
                            .unwrap()
                            .kind,
                    );
                }
                for (_, _, o) in entries {
                    if kind[o] == ObjectKind::Dir {
                        dirs.push(*o);
                    }
                }
                let name = Name::new(format!("n{}", rng.below(12)).into_bytes()).unwrap();
                let to = Link {
                    parent: dirs[rng.below(dirs.len())],
                    name,
                };
                let entry =
                    (!entries.is_empty()).then(|| entries[rng.below(entries.len())].clone());
                let link_of = |(p, n, _): &(ObjectId, Vec<u8>, ObjectId)| Link {
                    parent: *p,
                    name: Name::new(n.clone()).unwrap(),
                };
                let mut gone: Vec<ObjectId> = unlinked.keys().copied().collect();
                gone.sort();
                // A few operations to a commit, so that one commit holds a folder and what was in it.
                let mut ops = Vec::new();
                for _ in 0..1 + rng.below(3) {
                    let op = match rng.below(9) {
                        0 => Op::Create {
                            object: ObjectId(rng.id()),
                            revision: RevisionId(rng.id()),
                            body: body(Content::Dir),
                            link: to.clone(),
                            conflict_of: None,
                        },
                        1 | 2 => Op::Create {
                            object: ObjectId(rng.id()),
                            revision: RevisionId(rng.id()),
                            body: if rng.below(4) == 0 {
                                body(Content::Symlink {
                                    target: cairn_protocol::Bytes(b"t".to_vec()),
                                })
                            } else {
                                file(rng.below(1000) as u64)
                            },
                            link: to.clone(),
                            conflict_of: None,
                        },
                        3 | 4 => match &entry {
                            Some(e) => Op::Unlink {
                                object: e.2,
                                link: link_of(e),
                                base: None,
                            },
                            None => continue,
                        },
                        5 => match &entry {
                            Some(e) => Op::Move {
                                object: e.2,
                                from: link_of(e),
                                to: to.clone(),
                            },
                            None => continue,
                        },
                        // A further hard link, or a folder linked twice, which the store refuses.
                        6 => match &entry {
                            Some(e) => Op::Link {
                                object: e.2,
                                link: to.clone(),
                            },
                            None => continue,
                        },
                        // Brought back.
                        7 if !gone.is_empty() => Op::Link {
                            object: gone[rng.below(gone.len())],
                            link: to.clone(),
                        },
                        8 => match &entry {
                            Some(e) if kind[&e.2] == ObjectKind::File => Op::Update {
                                object: e.2,
                                base: state.heads.iter().find(|h| h.0 == e.2).unwrap().1,
                                revision: RevisionId(rng.id()),
                                body: file(rng.below(1000) as u64),
                                restored_from: None,
                            },
                            _ => continue,
                        },
                        _ => continue,
                    };
                    ops.push(op);
                }
                if ops.is_empty() {
                    continue;
                }
                let req = CommitRequest {
                    op_id: OpId::random(),
                    device: DeviceId::random(),
                    device_name: format!("d{}", rng.below(3)),
                    ops,
                };
                let user = ["admin", "u"][rng.below(2)];
                let outcome = store
                    .commit(
                        cairn_store::FIRST_FOLDER,
                        &req,
                        1_000 + step,
                        (step % 3 != 0).then_some(user),
                    )
                    .unwrap();
                if let CommitOutcome::Committed { seq, .. } = outcome {
                    for op in &req.ops {
                        match op {
                            Op::Link { object, .. } if unlinked.contains_key(object) => {
                                restored += 1
                            }
                            Op::Link { .. } => hard += 1,
                            Op::Move { object, .. }
                                if kind.get(object) == Some(&ObjectKind::Dir) =>
                            {
                                moved += 1
                            }
                            _ => {}
                        }
                        let o = match op {
                            Op::Create { object, .. }
                            | Op::Link { object, .. }
                            | Op::Unlink { object, .. }
                            | Op::Move { object, .. }
                            | Op::Update { object, .. }
                            | Op::Note { object, .. } => *object,
                        };
                        if store
                            .links_of(cairn_store::FIRST_FOLDER, o)
                            .unwrap()
                            .is_empty()
                        {
                            if let Op::Unlink { link, .. } = op {
                                unlinked.insert(o, (link.clone(), seq));
                                if kind.get(&o) == Some(&ObjectKind::Dir) {
                                    dirs_gone += 1;
                                }
                            }
                        } else {
                            unlinked.remove(&o);
                        }
                    }
                }
                // Kept up now and then, as after commits, in pages smaller than a commit can be.
                if rng.below(20) == 0 {
                    kept.catch_up_by(cairn_store::FIRST_FOLDER, |after| {
                        store.changes(cairn_store::FIRST_FOLDER, after, 3, 64)
                    })
                    .unwrap();
                    agrees(&kept, &mut store, &unlinked);
                }
            }
            kept.catch_up_by(cairn_store::FIRST_FOLDER, |after| {
                store.changes(cairn_store::FIRST_FOLDER, after, 3, 64)
            })
            .unwrap();
            agrees(&kept, &mut store, &unlinked);
            let mut fresh = Replayed::new();
            fresh
                .catch_up_by(cairn_store::FIRST_FOLDER, |after| {
                    store.changes(cairn_store::FIRST_FOLDER, after, PAGE_CHANGES, PAGE_BYTES)
                })
                .unwrap();
            agrees(&fresh, &mut store, &unlinked);
            assert!(
                restored > 0 && hard > 0 && moved > 0 && dirs_gone > 0 && !unlinked.is_empty(),
                "seed {seed}: {restored} restored, {hard} hard links, {moved} folders moved, {dirs_gone} folders deleted"
            );
        }
    }

    #[test]
    fn a_journal_that_does_not_replay_is_said_so_and_not_listed() {
        let mut index = Replayed::new();
        let change = Change {
            seq: 4,
            op_id: OpId::random(),
            device: DeviceId::random(),
            device_name: "d".into(),
            time: 0,
            ops: vec![Op::Unlink {
                object: ObjectId::random(),
                link: Link {
                    parent: ObjectId::ROOT,
                    name: Name::new(b"x".to_vec()).unwrap(),
                },
                base: None,
            }],
            user: None,
        };
        let mut pages = vec![vec![change]];
        let first = index.catch_up_by(cairn_store::FIRST_FOLDER, |_| {
            Ok(pages.pop().unwrap_or_default())
        });
        assert!(format!("{:#}", first.unwrap_err()).contains("does not replay at seq 4"));
        let again = index.catch_up_by(cairn_store::FIRST_FOLDER, |_| Ok(Vec::new()));
        assert!(format!("{:#}", again.unwrap_err()).contains("read again when the server starts"));
        assert!(index.gone.is_empty() && index.seq == 0);
    }
}
