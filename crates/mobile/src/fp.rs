//! What the Files app is shown of a folder, through the File Provider extension: each object an item
//! by its ID, listed when the system opens its folder, and the change feed read for what changed
//! since, so that the system's copy of what it has seen follows the server's.
//!
//! What Files cannot show is left out with its reason (invariant 8): a symbolic link, and a name that
//! is not UTF-8. An object of more than one name is shown at one of them.

use crate::folder::{Entry, EntryKind, Folder};
use crate::{object_of, Result};
use cairn_protocol::wire::Access;
use cairn_protocol::{Content, Link, ObjectId, Op, RevisionBody, RevisionId};
use std::collections::{BTreeMap, HashSet};

/// The most objects one call looks up again: the rest wait for the next.
const MAX_LOOKUPS: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FpItem {
    /// The object's hex, or `root` for the folder's root.
    pub identifier: String,
    pub parent: String,
    pub filename: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub mode: u32,
    /// The revision: new bytes, a new version.
    pub content_version: Vec<u8>,
    /// The revision, where it is and its mode: a move or a rename is a new one too.
    pub metadata_version: Vec<u8>,
    /// A folder something may be added to.
    pub writable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FpSkipped {
    pub object: String,
    pub display: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FpPage {
    pub items: Vec<FpItem>,
    pub skipped: Vec<FpSkipped>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FpChanges {
    pub updated: Vec<FpItem>,
    /// Identifiers of items gone: deleted, or now something Files is not shown.
    pub deleted: Vec<String>,
    pub skipped: Vec<FpSkipped>,
    /// The sequence read to: the next call's `after`.
    pub next: u64,
    /// More changes wait after `next`.
    pub more: bool,
}

fn id_of(o: ObjectId) -> String {
    if o == ObjectId::ROOT {
        "root".into()
    } else {
        o.to_string()
    }
}

fn metadata_version(revision: &RevisionId, parent: ObjectId, name: &[u8], mode: u32) -> Vec<u8> {
    let mut h = blake3::Hasher::new();
    h.update(&revision.0);
    h.update(&parent.0);
    h.update(&(name.len() as u64).to_le_bytes());
    h.update(name);
    h.update(&mode.to_le_bytes());
    h.finalize().as_bytes()[..16].to_vec()
}

/// An item, or why it is not one.
pub(crate) struct Shown {
    kind: EntryKind,
    object: ObjectId,
    revision: RevisionId,
    parent: ObjectId,
    name: Vec<u8>,
    size: u64,
    mtime: (i64, u32),
    mode: u32,
}

impl Shown {
    fn of_entry(e: &Entry, parent: ObjectId) -> Option<Self> {
        Some(Self {
            kind: e.kind,
            object: object_of(&e.object).ok()?,
            revision: e.revision.parse().ok()?,
            parent,
            name: e.name.clone(),
            size: e.size.unwrap_or(0),
            mtime: (e.mtime_sec, e.mtime_nsec),
            mode: e.mode,
        })
    }

    fn of_body(object: ObjectId, revision: RevisionId, body: &RevisionBody, link: &Link) -> Self {
        let (kind, size) = match &body.content {
            Content::Dir => (EntryKind::Dir, 0),
            Content::Symlink { .. } => (EntryKind::Symlink, 0),
            Content::File { size, .. } => (EntryKind::File, *size),
        };
        Self {
            kind,
            object,
            revision,
            parent: link.parent,
            name: link.name.as_bytes().to_vec(),
            size,
            mtime: (body.metadata.mtime.sec, body.metadata.mtime.nsec),
            mode: body.metadata.mode,
        }
    }

    fn display(&self) -> String {
        cairn_protocol::name::display_bytes(&self.name)
    }

    /// The item, or why Files is not shown it.
    fn item(&self, writable: bool) -> std::result::Result<FpItem, FpSkipped> {
        let skip = |reason: &str| FpSkipped {
            object: self.object.to_string(),
            display: self.display(),
            reason: reason.into(),
        };
        if self.kind == EntryKind::Symlink {
            return Err(skip(
                "a symbolic link, which the Files app cannot show: it is on the server as it was",
            ));
        }
        let Ok(filename) = std::str::from_utf8(&self.name) else {
            return Err(skip(
                "its name is not UTF-8, which the Files app cannot show: it is on the server as it was",
            ));
        };
        Ok(FpItem {
            identifier: id_of(self.object),
            parent: id_of(self.parent),
            filename: filename.to_string(),
            is_dir: self.kind == EntryKind::Dir,
            size: self.size,
            mtime_sec: self.mtime.0,
            mtime_nsec: self.mtime.1,
            mode: self.mode,
            content_version: self.revision.0.to_vec(),
            metadata_version: metadata_version(&self.revision, self.parent, &self.name, self.mode),
            writable: writable && self.kind == EntryKind::Dir,
        })
    }
}

/// What a batch of changes left an object as.
enum State {
    /// Known whole from the operations: made in this batch.
    Known(Shown),
    /// Gone: its entry removed.
    Gone,
    /// Changed in a way that needs its item read again: an edit, a move, a new name.
    Lookup,
}

impl Folder {
    fn writable(&self) -> bool {
        self.client.access().is_none_or(|a| a >= Access::Write)
    }

    fn page(&self, entries: &[Entry], parent: ObjectId) -> FpPage {
        let writable = self.writable();
        let mut items = Vec::new();
        let mut skipped = Vec::new();
        let mut seen = HashSet::new();
        for e in entries {
            let Some(s) = Shown::of_entry(e, parent) else {
                continue;
            };
            if !seen.insert(s.object) {
                skipped.push(FpSkipped {
                    object: s.object.to_string(),
                    display: s.display(),
                    reason: "another name of a file shown in this folder already".into(),
                });
                continue;
            }
            match s.item(writable) {
                Ok(i) => items.push(i),
                Err(k) => skipped.push(k),
            }
        }
        FpPage { items, skipped }
    }
}

#[uniffi::export]
impl Folder {
    /// A folder's items for the Files app; `dir` is an identifier, `root` for the folder's root.
    pub fn fp_list(&self, dir: String) -> Result<FpPage> {
        let l = self.list(dir.clone())?;
        Ok(self.page(&l.entries, object_of(&dir)?))
    }

    /// One item as it is now: `None` once it is gone or is something Files is not shown.
    pub fn fp_item(&self, identifier: String) -> Result<Option<FpItem>> {
        let object = object_of(&identifier)?;
        if object == ObjectId::ROOT {
            let l = self.list("root".into())?;
            return Ok(Some(FpItem {
                identifier: "root".into(),
                parent: "root".into(),
                filename: l.dir.display.clone(),
                is_dir: true,
                size: 0,
                mtime_sec: l.dir.mtime_sec,
                mtime_nsec: l.dir.mtime_nsec,
                mode: l.dir.mode,
                content_version: l.dir.revision.as_bytes().to_vec(),
                metadata_version: l.dir.revision.as_bytes().to_vec(),
                writable: self.writable(),
            }));
        }
        let Some(v) = self.client.item(object)? else {
            return Ok(None);
        };
        let parent = parent_of(&v.path);
        Ok(Shown::of_entry(&Entry::of(&v.item), parent).and_then(|s| s.item(self.writable()).ok()))
    }

    /// What changed after `after`, for the working set: items made or changed, and items gone. At most
    /// about `limit` objects are looked up again; `more` says that changes wait after `next`.
    pub fn fp_changes(&self, after: u64, limit: u32) -> Result<FpChanges> {
        let limit = (limit as usize).clamp(1, MAX_LOOKUPS);
        let resp = self.client.changes(after)?;
        let mut states: BTreeMap<ObjectId, State> = BTreeMap::new();
        let mut next = after;
        let mut stopped = false;
        for change in &resp.changes {
            let lookups = states
                .values()
                .filter(|s| matches!(s, State::Lookup))
                .count();
            let adds = change
                .ops
                .iter()
                .filter(|op| {
                    matches!(op, Op::Update { .. } | Op::Move { .. } | Op::Link { .. })
                        && !matches!(
                            states.get(&op_object(op)),
                            Some(State::Known(_) | State::Lookup)
                        )
                })
                .count();
            if next > after && lookups + adds > limit {
                stopped = true;
                break;
            }
            for op in &change.ops {
                apply(&mut states, op);
            }
            next = change.seq;
        }
        let writable = self.writable();
        let mut out = FpChanges {
            updated: Vec::new(),
            deleted: Vec::new(),
            skipped: Vec::new(),
            next,
            more: stopped || next < resp.head,
        };
        for (object, state) in states {
            let shown = match state {
                State::Known(s) => Some(s),
                State::Gone => None,
                State::Lookup => self.client.item(object)?.and_then(|v| {
                    let parent = parent_of(&v.path);
                    Shown::of_entry(&Entry::of(&v.item), parent)
                }),
            };
            match shown.map(|s| s.item(writable)) {
                Some(Ok(i)) => out.updated.push(i),
                Some(Err(k)) => {
                    out.deleted.push(id_of(object));
                    out.skipped.push(k);
                }
                None => out.deleted.push(id_of(object)),
            }
        }
        Ok(out)
    }
}

fn parent_of(path: &[cairn_protocol::web::Step]) -> ObjectId {
    match path.len() {
        0 | 1 => ObjectId::ROOT,
        n => path[n - 2].object,
    }
}

fn op_object(op: &Op) -> ObjectId {
    match op {
        Op::Create { object, .. }
        | Op::Link { object, .. }
        | Op::Unlink { object, .. }
        | Op::Move { object, .. }
        | Op::Update { object, .. }
        | Op::Note { object, .. } => *object,
    }
}

fn apply(states: &mut BTreeMap<ObjectId, State>, op: &Op) {
    match op {
        Op::Create {
            object,
            revision,
            body,
            link,
            ..
        } => {
            states.insert(
                *object,
                State::Known(Shown::of_body(*object, *revision, body, link)),
            );
        }
        Op::Update {
            object,
            revision,
            body,
            ..
        } => {
            let next = match states.remove(object) {
                Some(State::Known(s)) => {
                    let link = Link {
                        parent: s.parent,
                        name: cairn_protocol::Name::new(s.name.clone())
                            .expect("a name it was made with"),
                    };
                    State::Known(Shown::of_body(*object, *revision, body, &link))
                }
                _ => State::Lookup,
            };
            states.insert(*object, next);
        }
        Op::Move { object, to, .. } => {
            let next = match states.remove(object) {
                Some(State::Known(mut s)) => {
                    s.parent = to.parent;
                    s.name = to.name.as_bytes().to_vec();
                    State::Known(s)
                }
                _ => State::Lookup,
            };
            states.insert(*object, next);
        }
        Op::Link { object, .. } => {
            if !matches!(states.get(object), Some(State::Known(_))) {
                states.insert(*object, State::Lookup);
            }
        }
        // An object of one name, as nearly all are, is deleted with it.
        Op::Unlink { object, .. } => {
            states.insert(*object, State::Gone);
        }
        Op::Note { .. } => {}
    }
}
