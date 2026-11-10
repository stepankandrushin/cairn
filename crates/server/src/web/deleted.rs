//! What was deleted lately and is still deleted, read from the journal back from the head, and bringing
//! it back as `cairn undelete` does: each object linked again to the entry it last had, as itself, with
//! nothing reverted or replaced.

use super::edit::commit;
use super::{conflict, display_name, View};
use crate::{blocking, now, ApiResult, App};
use anyhow::Result;
use axum::extract::{Extension, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use cairn_protocol::name::display_bytes;
use cairn_protocol::web::{self as w};
use cairn_protocol::wire::Access;
use cairn_protocol::{conflict_name, in_git_dir, Content, Link, Name, ObjectId, ObjectKind, Op};
use cairn_store::MetaStore;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// How much of the journal one read goes back through, in pages of operations as stored.
const PAGE_BYTES: usize = 8 * 1024 * 1024;
const PAGES: usize = 8;
/// What a device deleted in a folder this long before deleting the folder went with it.
const WITH_FOLDER_SECS: i64 = 600;

/// The delete that left an object with no entry.
#[derive(Clone)]
struct Gone {
    link: Link,
    seq: u64,
    time: i64,
    device_name: String,
    user: Option<String>,
}

struct Index {
    /// Every object deleted in what was read and still deleted, with its last delete.
    gone: HashMap<ObjectId, Gone>,
    /// Whether the read reached as far back as was asked.
    complete: bool,
}

/// Back from the head to `since`, or as far as the pages allow, the store taken a page at a time so
/// that no device's sync waits for all of it.
fn read_back(app: &App, ns: &str, since: i64) -> Result<Index> {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut candidates: HashMap<ObjectId, Gone> = HashMap::new();
    let mut before = u64::MAX;
    let mut complete = false;
    'pages: for _ in 0..PAGES {
        let page = app
            .repo
            .with_meta(|m| m.changes_before(ns, before, 1000, PAGE_BYTES))?;
        let Some(last) = page.last() else {
            complete = true;
            break;
        };
        before = last.seq;
        for change in page {
            if change.time < since {
                complete = true;
                break 'pages;
            }
            // Newest first: the first operation met on an object is its last.
            for op in change.ops.iter().rev() {
                let (object, unlinked) = match op {
                    Op::Unlink { object, link, .. } => (*object, Some(link)),
                    Op::Create { object, .. }
                    | Op::Link { object, .. }
                    | Op::Move { object, .. }
                    | Op::Update { object, .. } => (*object, None),
                    Op::Note { .. } => continue,
                };
                if !seen.insert(object) {
                    continue;
                }
                if let Some(link) = unlinked {
                    candidates.insert(
                        object,
                        Gone {
                            link: link.clone(),
                            seq: change.seq,
                            time: change.time,
                            device_name: change.device_name.clone(),
                            user: change.user.clone(),
                        },
                    );
                }
            }
        }
        if before <= 1 {
            complete = true;
            break;
        }
    }
    // Still deleted: no entry now.
    let gone = app.repo.with_meta(|m| {
        let mut gone = HashMap::new();
        for (object, g) in candidates {
            if m.links_of(ns, object)?.is_empty() && m.object_head(ns, object)?.is_some() {
                gone.insert(object, g);
            }
        }
        Ok(gone)
    })?;
    Ok(Index { gone, complete })
}

impl Index {
    /// Whether `o` went with the folder it was in: deleted by the same device, with it or in the 10
    /// minutes before.
    fn with_folder(&self, o: &ObjectId) -> Option<ObjectId> {
        let g = self.gone.get(o)?;
        let p = self.gone.get(&g.link.parent)?;
        (g.device_name == p.device_name && g.seq <= p.seq && p.time - g.time <= WITH_FOLDER_SECS)
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
    fn path(&self, m: &mut dyn MetaStore, ns: &str, dir: ObjectId) -> Result<Option<Vec<Name>>> {
        let mut out = Vec::new();
        let mut at = dir;
        for _ in 0..4096 {
            if at == ObjectId::ROOT {
                out.reverse();
                return Ok(Some(out));
            }
            let link = match m.links_of(ns, at)?.into_iter().next() {
                Some(l) => l,
                None => match self.gone.get(&at) {
                    Some(g) => g.link.clone(),
                    None => return Ok(None),
                },
            };
            out.push(link.name);
            at = link.parent;
        }
        Ok(None)
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
        let index = read_back(&app, &view.ns, now() - days * 86_400)?;
        app.repo.with_meta(|m| {
            let groups = index.groups();
            let mut items = Vec::new();
            for (object, g) in &index.gone {
                if index.with_folder(object).is_some() {
                    continue;
                }
                let Some(head) = m.object_head(&view.ns, *object)? else {
                    continue;
                };
                // A folder counts the files that went with it, all the way down.
                let (mut files, mut size) = (0u64, 0u64);
                if head.kind == ObjectKind::Dir {
                    let mut stack = vec![*object];
                    while let Some(d) = stack.pop() {
                        for c in groups.get(&d).into_iter().flatten() {
                            match m.object_head(&view.ns, *c)? {
                                Some(h) if h.kind == ObjectKind::Dir => stack.push(*c),
                                Some(h) => {
                                    files += 1;
                                    if let Content::File { size: s, .. } = h.body.content {
                                        size += s;
                                    }
                                }
                                None => {}
                            }
                        }
                    }
                } else if let Content::File { size: s, .. } = head.body.content {
                    size = s;
                }
                let path = index
                    .path(m, &view.ns, g.link.parent)?
                    .map(|p| joined(&view, &p))
                    .unwrap_or_else(|| "…".into());
                items.push(w::DeletedEntry {
                    object: *object,
                    kind: head.kind,
                    display: display_bytes(g.link.name.as_bytes()),
                    path,
                    seq: g.seq,
                    time: g.time,
                    user: g.user.clone(),
                    device_name: g.device_name.clone(),
                    size,
                    files,
                });
            }
            items.sort_by(|a, b| b.seq.cmp(&a.seq).then_with(|| a.display.cmp(&b.display)));
            Ok(w::DeletedList {
                items,
                complete: index.complete,
            })
        })
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
    index: &Index,
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
        let Some(g) = index.gone.get(id) else {
            return Ok(Err(format!(
                "{id} is not deleted now, or was deleted before what the server reads back: reload"
            )));
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
                    "the folder {} was in was deleted before what the server reads back: `cairn undelete` on a device reaches further",
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
            let folder = index.path(m, &view.ns, parent)?.unwrap_or_default();
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
        // As far back as the listing reads by default, and further if anything asked is older.
        let index = read_back(&app, &view.ns, i64::MIN)?;
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
