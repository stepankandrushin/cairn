//! The namespace as a tree in memory: what a journal replays to. Used by clients for their base and
//! remote state, by `export`, and by `fsck` to check the server's tables against its journal.

use crate::ids::{ObjectId, RevisionId};
use crate::metadata::Metadata;
use crate::name::Name;
use crate::ops::{
    check_and_apply, Content, Link, ObjectKind, Op, OpReject, RevisionBody, TreeStore,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Node {
    pub rev: RevisionId,
    pub body: RevisionBody,
    /// Empty for a deleted object, whose last state stays for history and resurrection.
    pub links: BTreeSet<Link>,
}

impl Node {
    pub fn kind(&self) -> ObjectKind {
        self.body.content.kind()
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Tree {
    pub nodes: BTreeMap<ObjectId, Node>,
    entries: BTreeMap<Link, ObjectId>,
}

/// More directory levels than this is a corrupt tree, not a deep one.
const MAX_DEPTH: usize = 1 << 20;

impl Tree {
    /// An empty namespace: the root directory, with no metadata yet.
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ObjectId::ROOT,
            Node {
                rev: RevisionId::ZERO,
                body: RevisionBody {
                    content: Content::Dir,
                    metadata: Metadata::default(),
                },
                links: BTreeSet::new(),
            },
        );
        Self {
            nodes,
            entries: BTreeMap::new(),
        }
    }

    pub fn from_nodes(nodes: BTreeMap<ObjectId, Node>) -> Self {
        let mut t = Self {
            nodes,
            entries: BTreeMap::new(),
        };
        t.nodes
            .entry(ObjectId::ROOT)
            .or_insert_with(|| Tree::new().nodes[&ObjectId::ROOT].clone());
        let all: Vec<(ObjectId, Link)> = t
            .nodes
            .iter()
            .flat_map(|(id, n)| n.links.iter().map(move |l| (*id, l.clone())))
            .collect();
        for (id, l) in all {
            t.entries.insert(l, id);
        }
        t
    }

    pub fn is_live(&self, o: ObjectId) -> bool {
        o == ObjectId::ROOT || self.nodes.get(&o).is_some_and(|n| !n.links.is_empty())
    }

    pub fn get_live(&self, o: ObjectId) -> Option<&Node> {
        self.nodes.get(&o).filter(|_| self.is_live(o))
    }

    pub fn live(&self) -> impl Iterator<Item = (ObjectId, &Node)> {
        self.nodes
            .iter()
            .filter(|(id, _)| self.is_live(**id))
            .map(|(id, n)| (*id, n))
    }

    pub fn child(&self, parent: ObjectId, name: &Name) -> Option<ObjectId> {
        self.entries
            .get(&Link {
                parent,
                name: name.clone(),
            })
            .copied()
    }

    pub fn entry_at(&self, link: &Link) -> Option<ObjectId> {
        self.entries.get(link).copied()
    }

    pub fn children(&self, parent: ObjectId) -> impl Iterator<Item = (&Name, ObjectId)> {
        self.entries
            .range(
                Link {
                    parent,
                    name: Name::lowest(),
                }..,
            )
            .take_while(move |(l, _)| l.parent == parent)
            .map(|(l, o)| (&l.name, *o))
    }

    /// All entries, in a stable order.
    pub fn entries(&self) -> impl Iterator<Item = (&Link, ObjectId)> {
        self.entries.iter().map(|(l, o)| (l, *o))
    }

    /// The names from the root down to `link`.
    pub fn path_of_link(&self, link: &Link) -> Option<Vec<Name>> {
        let mut path = vec![link.name.clone()];
        let mut at = link.parent;
        let mut depth = 0;
        while at != ObjectId::ROOT {
            let up = self.nodes.get(&at)?.links.iter().next()?;
            path.push(up.name.clone());
            at = up.parent;
            depth += 1;
            if depth > MAX_DEPTH {
                return None;
            }
        }
        path.reverse();
        Some(path)
    }

    /// The object at a path of names, following entries only.
    pub fn resolve(&self, path: &[Name]) -> Option<ObjectId> {
        let mut at = ObjectId::ROOT;
        for name in path {
            at = self.child(at, name)?;
        }
        Some(at)
    }

    /// Every live object in the subtree under `o`, `o` included.
    pub fn subtree(&self, o: ObjectId) -> BTreeSet<ObjectId> {
        let mut out = BTreeSet::new();
        let mut stack = vec![o];
        while let Some(at) = stack.pop() {
            if out.insert(at) {
                stack.extend(self.children(at).map(|(_, c)| c));
            }
        }
        out
    }

    /// Replay one operation. It was validated when committed, so a failure here means the journal
    /// or this tree is corrupt.
    pub fn apply(&mut self, op: &Op) -> Result<(), OpReject> {
        check_and_apply(self, op)
    }
}

impl TreeStore for Tree {
    type Err = OpReject;

    fn object(&mut self, o: ObjectId) -> Result<Option<(ObjectKind, RevisionId)>, OpReject> {
        Ok(self.nodes.get(&o).map(|n| (n.kind(), n.rev)))
    }
    fn link_count(&mut self, o: ObjectId) -> Result<u64, OpReject> {
        Ok(self.nodes.get(&o).map_or(0, |n| n.links.len() as u64))
    }
    fn entry(&mut self, link: &Link) -> Result<Option<ObjectId>, OpReject> {
        Ok(self.entries.get(link).copied())
    }
    fn has_children(&mut self, dir: ObjectId) -> Result<bool, OpReject> {
        Ok(self.children(dir).next().is_some())
    }
    fn dir_parent(&mut self, dir: ObjectId) -> Result<Option<ObjectId>, OpReject> {
        Ok(self
            .nodes
            .get(&dir)
            .and_then(|n| n.links.iter().next())
            .map(|l| l.parent))
    }
    fn insert_object(
        &mut self,
        o: ObjectId,
        rev: RevisionId,
        body: &RevisionBody,
    ) -> Result<(), OpReject> {
        self.nodes.insert(
            o,
            Node {
                rev,
                body: body.clone(),
                links: BTreeSet::new(),
            },
        );
        Ok(())
    }
    fn set_head(
        &mut self,
        o: ObjectId,
        _base: RevisionId,
        rev: RevisionId,
        body: &RevisionBody,
    ) -> Result<(), OpReject> {
        let n = self.nodes.get_mut(&o).ok_or(OpReject::NoSuchObject(o))?;
        n.rev = rev;
        n.body = body.clone();
        Ok(())
    }
    fn add_link(&mut self, o: ObjectId, link: &Link) -> Result<(), OpReject> {
        self.nodes
            .get_mut(&o)
            .ok_or(OpReject::NoSuchObject(o))?
            .links
            .insert(link.clone());
        self.entries.insert(link.clone(), o);
        Ok(())
    }
    fn remove_link(&mut self, o: ObjectId, link: &Link) -> Result<(), OpReject> {
        self.nodes
            .get_mut(&o)
            .ok_or(OpReject::NoSuchObject(o))?
            .links
            .remove(link);
        self.entries.remove(link);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::Bytes;

    fn name(s: &str) -> Name {
        Name::new(s.as_bytes().to_vec()).unwrap()
    }
    fn dir() -> RevisionBody {
        RevisionBody {
            content: Content::Dir,
            metadata: Metadata::default(),
        }
    }
    fn link_body() -> RevisionBody {
        RevisionBody {
            content: Content::Symlink {
                target: Bytes(b"t".to_vec()),
            },
            metadata: Metadata::default(),
        }
    }
    fn create(
        t: &mut Tree,
        parent: ObjectId,
        n: &str,
        body: RevisionBody,
    ) -> Result<ObjectId, OpReject> {
        let object = ObjectId::random();
        t.apply(&Op::Create {
            object,
            revision: RevisionId::random(),
            body,
            link: Link {
                parent,
                name: name(n),
            },
            conflict_of: None,
        })?;
        Ok(object)
    }

    #[test]
    fn a_move_into_your_own_subtree_is_a_cycle() {
        let mut t = Tree::new();
        let a = create(&mut t, ObjectId::ROOT, "a", dir()).unwrap();
        let b = create(&mut t, a, "b", dir()).unwrap();
        let r = t.apply(&Op::Move {
            object: a,
            from: Link {
                parent: ObjectId::ROOT,
                name: name("a"),
            },
            to: Link {
                parent: b,
                name: name("a"),
            },
        });
        assert_eq!(r, Err(OpReject::Cycle(a)));
    }

    #[test]
    fn the_rules_refuse_what_would_corrupt_a_tree() {
        let mut t = Tree::new();
        let a = create(&mut t, ObjectId::ROOT, "a", dir()).unwrap();
        let l = create(&mut t, a, "l", link_body()).unwrap();
        // A taken name, a file as a parent, the reserved state directory.
        assert!(matches!(
            create(&mut t, a, "l", dir()),
            Err(OpReject::NameTaken(_))
        ));
        assert!(matches!(
            create(&mut t, l, "x", dir()),
            Err(OpReject::BadParent(_))
        ));
        assert!(matches!(
            create(&mut t, ObjectId::ROOT, ".cairn", dir()),
            Err(OpReject::ReservedName(_))
        ));
        assert!(
            create(&mut t, a, ".cairn", dir()).is_ok(),
            "only the root's .cairn is reserved"
        );
        // A directory with children, or a second entry for a directory.
        let unlink = Op::Unlink {
            base: None,
            object: a,
            link: Link {
                parent: ObjectId::ROOT,
                name: name("a"),
            },
        };
        assert_eq!(t.apply(&unlink), Err(OpReject::NotEmpty(a)));
        let second = Op::Link {
            object: a,
            link: Link {
                parent: ObjectId::ROOT,
                name: name("a2"),
            },
        };
        assert_eq!(t.apply(&second), Err(OpReject::DirMultiLink(a)));
    }

    #[test]
    fn an_update_against_an_old_base_is_refused() {
        let mut t = Tree::new();
        let l = create(&mut t, ObjectId::ROOT, "l", link_body()).unwrap();
        let head = t.nodes[&l].rev;
        let update = |base| Op::Update {
            object: l,
            base,
            revision: RevisionId::random(),
            body: link_body(),
            restored_from: None,
        };
        t.apply(&update(head)).unwrap();
        assert_eq!(t.apply(&update(head)), Err(OpReject::StaleBase(l)));
    }

    #[test]
    fn an_edit_that_has_not_seen_a_delete_is_refused() {
        let mut t = Tree::new();
        let l = create(&mut t, ObjectId::ROOT, "l", link_body()).unwrap();
        let head = t.nodes[&l].rev;
        let at = Link {
            parent: ObjectId::ROOT,
            name: name("l"),
        };
        t.apply(&Op::Unlink {
            object: l,
            link: at.clone(),
            base: None,
        })
        .unwrap();
        let update = Op::Update {
            object: l,
            base: head,
            revision: RevisionId::random(),
            body: link_body(),
            restored_from: None,
        };
        assert_eq!(t.apply(&update), Err(OpReject::Deleted(l)));
        // Brought back first, it may change.
        t.apply(&Op::Link {
            object: l,
            link: at,
        })
        .unwrap();
        t.apply(&update).unwrap();
    }

    #[test]
    fn a_delete_that_has_not_seen_the_latest_edit_is_refused() {
        let mut t = Tree::new();
        let l = create(&mut t, ObjectId::ROOT, "l", link_body()).unwrap();
        let seen = t.nodes[&l].rev;
        t.apply(&Op::Update {
            object: l,
            base: seen,
            revision: RevisionId::random(),
            body: link_body(),
            restored_from: None,
        })
        .unwrap();
        let at = Link {
            parent: ObjectId::ROOT,
            name: name("l"),
        };
        let stale = Op::Unlink {
            object: l,
            link: at.clone(),
            base: Some(seen),
        };
        assert_eq!(t.apply(&stale), Err(OpReject::StaleBase(l)));
        let head = t.nodes[&l].rev;
        t.apply(&Op::Unlink {
            object: l,
            link: at,
            base: Some(head),
        })
        .unwrap();
    }

    #[test]
    fn a_deleted_object_keeps_its_state_and_can_come_back() {
        let mut t = Tree::new();
        let l = create(&mut t, ObjectId::ROOT, "l", link_body()).unwrap();
        let at = Link {
            parent: ObjectId::ROOT,
            name: name("l"),
        };
        t.apply(&Op::Unlink {
            base: None,
            object: l,
            link: at.clone(),
        })
        .unwrap();
        assert!(!t.is_live(l));
        assert!(t.nodes.contains_key(&l));
        t.apply(&Op::Link {
            object: l,
            link: at,
        })
        .unwrap();
        assert!(t.is_live(l));
    }

    #[test]
    fn paths_resolve_through_entries() {
        let mut t = Tree::new();
        let a = create(&mut t, ObjectId::ROOT, "a", dir()).unwrap();
        let b = create(&mut t, a, "b", dir()).unwrap();
        assert_eq!(t.resolve(&[name("a"), name("b")]), Some(b));
        assert_eq!(
            t.path_of_link(t.nodes[&b].links.iter().next().unwrap()),
            Some(vec![name("a"), name("b")])
        );
        assert_eq!(t.subtree(a), [a, b].into_iter().collect());
    }
}
