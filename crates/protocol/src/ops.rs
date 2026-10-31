//! Journal operations and the one set of rules that validates them.
//!
//! The server's store and the in-memory tree both implement `TreeStore`, so a commit is judged by
//! the same code that later replays it.

use crate::ids::{Hash, ObjectId, RevisionId};
use crate::metadata::{Bytes, Metadata};
use crate::name::Name;
use crate::STATE_DIR;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    File,
    Dir,
    Symlink,
}

/// What an object holds, apart from its metadata.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Dir,
    /// Target verbatim, broken ones included.
    Symlink {
        target: Bytes,
    },
    File {
        manifest: Hash,
        size: u64,
    },
}

impl Content {
    pub fn kind(&self) -> ObjectKind {
        match self {
            Content::Dir => ObjectKind::Dir,
            Content::Symlink { .. } => ObjectKind::Symlink,
            Content::File { .. } => ObjectKind::File,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RevisionBody {
    pub content: Content,
    pub metadata: Metadata,
}

/// A directory entry: `name` inside directory `parent`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct Link {
    pub parent: ObjectId,
    pub name: Name,
}

/// Why an object exists as a conflict copy, for the conflict inbox.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ConflictOf {
    pub object: ObjectId,
    pub competing_revision: RevisionId,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Create {
        object: ObjectId,
        revision: RevisionId,
        body: RevisionBody,
        link: Link,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conflict_of: Option<ConflictOf>,
    },
    /// Add an entry: a further hard link, or bringing a deleted object back.
    Link { object: ObjectId, link: Link },
    /// `base` is the revision this device saw when it decided to delete: a delete made without
    /// having seen the latest edit is refused, so that the edit survives.
    Unlink {
        object: ObjectId,
        link: Link,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<RevisionId>,
    },
    Move {
        object: ObjectId,
        from: Link,
        to: Link,
    },
    Update {
        object: ObjectId,
        base: RevisionId,
        revision: RevisionId,
        body: RevisionBody,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restored_from: Option<RevisionId>,
    },
    /// An entry in the activity log with no effect on the tree: the loser of a metadata conflict.
    Note { object: ObjectId, text: String },
}

impl Op {
    pub fn body(&self) -> Option<&RevisionBody> {
        match self {
            Op::Create { body, .. } | Op::Update { body, .. } => Some(body),
            _ => None,
        }
    }
}

/// A reason to refuse a commit. Nothing was applied; the client rebases and retries.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reject", content = "object")]
pub enum OpReject {
    #[error("object {0} already exists")]
    ObjectExists(ObjectId),
    #[error("object {0} does not exist")]
    NoSuchObject(ObjectId),
    #[error("object {0} was deleted by another device first")]
    Deleted(ObjectId),
    #[error("revision base of {0} is not the head: another device changed it first")]
    StaleBase(ObjectId),
    #[error("object {0} would change kind")]
    KindChange(ObjectId),
    #[error("parent of {0} is not a live directory")]
    BadParent(ObjectId),
    #[error("name already taken in the parent of {0}")]
    NameTaken(ObjectId),
    #[error("no such entry for {0}")]
    NoSuchEntry(ObjectId),
    #[error("directory {0} is not empty")]
    NotEmpty(ObjectId),
    #[error("directory {0} may have only one entry")]
    DirMultiLink(ObjectId),
    #[error("moving {0} would make a cycle")]
    Cycle(ObjectId),
    #[error("the root directory {0} cannot be linked, moved or removed")]
    RootEntry(ObjectId),
    #[error("name reserved for client state in the root, for {0}")]
    ReservedName(ObjectId),
}

/// The state a commit is validated against and applied to.
pub trait TreeStore {
    type Err: From<OpReject>;

    fn object(&mut self, o: ObjectId) -> Result<Option<(ObjectKind, RevisionId)>, Self::Err>;
    fn link_count(&mut self, o: ObjectId) -> Result<u64, Self::Err>;
    fn entry(&mut self, link: &Link) -> Result<Option<ObjectId>, Self::Err>;
    fn has_children(&mut self, dir: ObjectId) -> Result<bool, Self::Err>;
    /// The parent of a live directory, `None` for the root and for deleted ones.
    fn dir_parent(&mut self, dir: ObjectId) -> Result<Option<ObjectId>, Self::Err>;

    fn insert_object(
        &mut self,
        o: ObjectId,
        rev: RevisionId,
        body: &RevisionBody,
    ) -> Result<(), Self::Err>;
    fn set_head(
        &mut self,
        o: ObjectId,
        base: RevisionId,
        rev: RevisionId,
        body: &RevisionBody,
    ) -> Result<(), Self::Err>;
    fn add_link(&mut self, o: ObjectId, link: &Link) -> Result<(), Self::Err>;
    fn remove_link(&mut self, o: ObjectId, link: &Link) -> Result<(), Self::Err>;
}

fn check_target<S: TreeStore>(s: &mut S, o: ObjectId, link: &Link) -> Result<(), S::Err> {
    if link.parent == ObjectId::ROOT && link.name.as_bytes() == STATE_DIR {
        return Err(OpReject::ReservedName(o).into());
    }
    let parent_live = match s.object(link.parent)? {
        Some((ObjectKind::Dir, _)) => {
            link.parent == ObjectId::ROOT || s.link_count(link.parent)? > 0
        }
        _ => false,
    };
    if !parent_live {
        return Err(OpReject::BadParent(o).into());
    }
    if s.entry(link)?.is_some() {
        return Err(OpReject::NameTaken(o).into());
    }
    Ok(())
}

fn check_no_cycle<S: TreeStore>(
    s: &mut S,
    dir: ObjectId,
    new_parent: ObjectId,
) -> Result<(), S::Err> {
    let mut at = Some(new_parent);
    while let Some(here) = at {
        if here == dir {
            return Err(OpReject::Cycle(dir).into());
        }
        at = s.dir_parent(here)?;
    }
    Ok(())
}

/// Validate one operation against the store, then apply it.
pub fn check_and_apply<S: TreeStore>(s: &mut S, op: &Op) -> Result<(), S::Err> {
    match op {
        Op::Create {
            object,
            revision,
            body,
            link,
            ..
        } => {
            if *object == ObjectId::ROOT {
                return Err(OpReject::RootEntry(*object).into());
            }
            if s.object(*object)?.is_some() {
                return Err(OpReject::ObjectExists(*object).into());
            }
            check_target(s, *object, link)?;
            s.insert_object(*object, *revision, body)?;
            s.add_link(*object, link)
        }
        Op::Link { object, link } => {
            if *object == ObjectId::ROOT {
                return Err(OpReject::RootEntry(*object).into());
            }
            let Some((kind, _)) = s.object(*object)? else {
                return Err(OpReject::NoSuchObject(*object).into());
            };
            check_target(s, *object, link)?;
            if kind == ObjectKind::Dir {
                if s.link_count(*object)? > 0 {
                    return Err(OpReject::DirMultiLink(*object).into());
                }
                check_no_cycle(s, *object, link.parent)?;
            }
            s.add_link(*object, link)
        }
        Op::Unlink { object, link, base } => {
            if *object == ObjectId::ROOT {
                return Err(OpReject::RootEntry(*object).into());
            }
            if s.entry(link)? != Some(*object) {
                return Err(OpReject::NoSuchEntry(*object).into());
            }
            if let Some(base) = base {
                if s.object(*object)?.map(|(_, head)| head) != Some(*base) {
                    return Err(OpReject::StaleBase(*object).into());
                }
            }
            if s.object(*object)?.map(|(k, _)| k) == Some(ObjectKind::Dir)
                && s.has_children(*object)?
            {
                return Err(OpReject::NotEmpty(*object).into());
            }
            s.remove_link(*object, link)
        }
        Op::Move { object, from, to } => {
            if *object == ObjectId::ROOT {
                return Err(OpReject::RootEntry(*object).into());
            }
            if s.entry(from)? != Some(*object) {
                return Err(OpReject::NoSuchEntry(*object).into());
            }
            check_target(s, *object, to)?;
            if s.object(*object)?.map(|(k, _)| k) == Some(ObjectKind::Dir) {
                check_no_cycle(s, *object, to.parent)?;
            }
            s.remove_link(*object, from)?;
            s.add_link(*object, to)
        }
        Op::Update {
            object,
            base,
            revision,
            body,
            ..
        } => {
            let Some((kind, head)) = s.object(*object)? else {
                return Err(OpReject::NoSuchObject(*object).into());
            };
            if head != *base {
                return Err(OpReject::StaleBase(*object).into());
            }
            if body.content.kind() != kind {
                return Err(OpReject::KindChange(*object).into());
            }
            // An edit made without having seen the delete: that device must merge, so that its edit
            // survives. Bringing an object back links it first, in the same commit.
            if *object != ObjectId::ROOT && s.link_count(*object)? == 0 {
                return Err(OpReject::Deleted(*object).into());
            }
            s.set_head(*object, *base, *revision, body)
        }
        Op::Note { object, .. } => {
            if s.object(*object)?.is_none() {
                return Err(OpReject::NoSuchObject(*object).into());
            }
            Ok(())
        }
    }
}
