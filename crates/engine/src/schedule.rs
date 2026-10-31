//! Ordering a change of directory structure into steps that each leave a valid tree, for both the
//! server's sequentially validated batch and the local filesystem. Swaps and cycles go through a
//! transient stage name.

use cairn_protocol::{Link, Name, ObjectId, ObjectKind, STAGE_PREFIX};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub type Topo = BTreeMap<ObjectId, (ObjectKind, BTreeSet<Link>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopoOp {
    Create {
        object: ObjectId,
        link: Link,
    },
    Link {
        object: ObjectId,
        link: Link,
    },
    Unlink {
        object: ObjectId,
        link: Link,
    },
    Move {
        object: ObjectId,
        from: Link,
        to: Link,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("cannot order the change: {0}")]
pub struct ScheduleError(pub String);

/// A stage name unique within one schedule: a hard-linked object can have several links staged.
pub fn stage_link(o: ObjectId, n: usize) -> Link {
    Link {
        parent: ObjectId::ROOT,
        name: Name::new(format!("{STAGE_PREFIX}{o}-{n}").into_bytes())
            .expect("a stage name is valid"),
    }
}

pub fn is_stage(link: &Link) -> bool {
    link.parent == ObjectId::ROOT && link.name.as_bytes().starts_with(STAGE_PREFIX.as_bytes())
}

struct Sim {
    kinds: HashMap<ObjectId, ObjectKind>,
    links: HashMap<ObjectId, BTreeSet<Link>>,
    entries: HashMap<Link, ObjectId>,
    children: HashMap<ObjectId, usize>,
}

impl Sim {
    fn new(from: &Topo) -> Self {
        let mut sim = Self {
            kinds: HashMap::new(),
            links: HashMap::new(),
            entries: HashMap::new(),
            children: HashMap::new(),
        };
        for (o, (kind, links)) in from {
            sim.kinds.insert(*o, *kind);
            for l in links {
                sim.add(*o, l);
            }
        }
        sim
    }

    fn add(&mut self, o: ObjectId, l: &Link) {
        self.links.entry(o).or_default().insert(l.clone());
        self.entries.insert(l.clone(), o);
        *self.children.entry(l.parent).or_default() += 1;
    }

    fn remove(&mut self, o: ObjectId, l: &Link) {
        if let Some(set) = self.links.get_mut(&o) {
            set.remove(l);
        }
        self.entries.remove(l);
        if let Some(n) = self.children.get_mut(&l.parent) {
            *n -= 1;
        }
    }

    fn parent_live(&self, p: ObjectId) -> bool {
        p == ObjectId::ROOT || self.links.get(&p).is_some_and(|s| !s.is_empty())
    }

    fn can_place(&self, o: ObjectId, l: &Link) -> bool {
        if !self.parent_live(l.parent) || self.entries.contains_key(l) {
            return false;
        }
        if self.kinds.get(&o) == Some(&ObjectKind::Dir) {
            // Walk up from the new parent; meeting the directory itself would be a cycle.
            let mut at = l.parent;
            let mut steps = 0;
            while at != ObjectId::ROOT {
                if at == o || steps > self.links.len() {
                    return false;
                }
                let Some(up) = self.links.get(&at).and_then(|s| s.iter().next()) else {
                    return false;
                };
                at = up.parent;
                steps += 1;
            }
        }
        true
    }

    fn has_children(&self, o: ObjectId) -> bool {
        self.children.get(&o).copied().unwrap_or(0) > 0
    }
}

#[derive(Debug)]
enum Pending {
    Unlink(ObjectId, Link),
    Move(ObjectId, Link, Link),
    Create(ObjectId, Link),
    Link(ObjectId, Link),
}

/// The steps that turn `from` into `to`. Every step is valid in the state the previous one left.
pub fn schedule(from: &Topo, to: &Topo) -> Result<Vec<TopoOp>, ScheduleError> {
    let mut sim = Sim::new(from);
    for (o, (kind, _)) in to {
        sim.kinds.insert(*o, *kind);
    }

    let empty = BTreeSet::new();
    let mut pending = Vec::new();
    let ids: BTreeSet<ObjectId> = from.keys().chain(to.keys()).copied().collect();
    for o in ids {
        let was = from.get(&o).map_or(&empty, |n| &n.1);
        let will = to.get(&o).map_or(&empty, |n| &n.1);
        let mut gone: Vec<&Link> = was.difference(will).collect();
        let mut added: Vec<&Link> = will.difference(was).collect();
        while !gone.is_empty() && !added.is_empty() {
            pending.push(Pending::Move(
                o,
                gone.remove(0).clone(),
                added.remove(0).clone(),
            ));
        }
        for l in gone {
            pending.push(Pending::Unlink(o, l.clone()));
        }
        let fresh = was.is_empty();
        for (i, l) in added.into_iter().enumerate() {
            pending.push(if fresh && i == 0 {
                Pending::Create(o, l.clone())
            } else {
                Pending::Link(o, l.clone())
            });
        }
    }

    let mut out = Vec::with_capacity(pending.len());
    let mut staged = 0;
    while !pending.is_empty() {
        let before = pending.len();
        let mut blocked = Vec::new();
        for p in pending.drain(..) {
            let ready = match &p {
                Pending::Unlink(o, _) => {
                    sim.kinds.get(o) != Some(&ObjectKind::Dir) || !sim.has_children(*o)
                }
                Pending::Move(o, _, to) => sim.can_place(*o, to),
                // A further link needs the object to exist first.
                Pending::Link(o, l) => {
                    sim.links.get(o).is_some_and(|s| !s.is_empty()) && sim.can_place(*o, l)
                }
                Pending::Create(o, l) => sim.can_place(*o, l),
            };
            if !ready {
                blocked.push(p);
                continue;
            }
            match p {
                Pending::Unlink(o, l) => {
                    sim.remove(o, &l);
                    out.push(TopoOp::Unlink { object: o, link: l });
                }
                Pending::Move(o, f, t) => {
                    sim.remove(o, &f);
                    sim.add(o, &t);
                    out.push(TopoOp::Move {
                        object: o,
                        from: f,
                        to: t,
                    });
                }
                Pending::Create(o, l) => {
                    sim.add(o, &l);
                    out.push(TopoOp::Create { object: o, link: l });
                }
                Pending::Link(o, l) => {
                    sim.add(o, &l);
                    out.push(TopoOp::Link { object: o, link: l });
                }
            }
        }
        pending = blocked;
        if !pending.is_empty() && pending.len() == before {
            // Deadlocked: a swap or a cycle. Lift one mover out to its stage name.
            let stageable = pending.iter_mut().find_map(|p| match p {
                Pending::Move(o, f, _) if !is_stage(f) => Some((*o, f)),
                _ => None,
            });
            let Some((o, f)) = stageable else {
                return Err(ScheduleError(format!(
                    "{} step(s) stuck: {:?}",
                    pending.len(),
                    pending.first()
                )));
            };
            staged += 1;
            let stage = stage_link(o, staged);
            let old = std::mem::replace(f, stage.clone());
            sim.remove(o, &old);
            sim.add(o, &stage);
            out.push(TopoOp::Move {
                object: o,
                from: old,
                to: stage,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    fn link(parent: ObjectId, n: &str) -> Link {
        Link {
            parent,
            name: Name::new(n.as_bytes().to_vec()).unwrap(),
        }
    }

    fn node(kind: ObjectKind, links: &[Link]) -> (ObjectKind, BTreeSet<Link>) {
        (kind, links.iter().cloned().collect())
    }

    /// Replays the steps by the tree's own rules, which is what a server does to a batch.
    fn check_replays(from: &Topo, to: &Topo, ops: &[TopoOp]) {
        let mut sim = Sim::new(from);
        for (o, (k, _)) in to {
            sim.kinds.insert(*o, *k);
        }
        for op in ops {
            match op {
                TopoOp::Create { object, link } | TopoOp::Link { object, link } => {
                    assert!(sim.can_place(*object, link), "{op:?}");
                    sim.add(*object, link);
                }
                TopoOp::Unlink { object, link } => {
                    assert_eq!(sim.entries.get(link), Some(object), "{op:?}");
                    if sim.kinds[object] == ObjectKind::Dir {
                        assert!(!sim.has_children(*object), "{op:?}");
                    }
                    sim.remove(*object, link);
                }
                TopoOp::Move { object, from, to } => {
                    assert_eq!(sim.entries.get(from), Some(object), "{op:?}");
                    assert!(sim.can_place(*object, to), "{op:?}");
                    sim.remove(*object, from);
                    sim.add(*object, to);
                }
            }
        }
        let mut want: HashMap<&Link, ObjectId> = HashMap::new();
        for (o, (_, ls)) in to {
            for l in ls {
                want.insert(l, *o);
            }
        }
        assert_eq!(sim.entries.len(), want.len());
        for (l, o) in want {
            assert_eq!(sim.entries.get(l), Some(&o));
        }
    }

    #[test]
    fn swapping_two_names_goes_through_a_stage() {
        let (a, b) = (ObjectId::random(), ObjectId::random());
        let (n1, n2) = (link(ObjectId::ROOT, "1"), link(ObjectId::ROOT, "2"));
        let from: Topo = [
            (a, node(ObjectKind::File, std::slice::from_ref(&n1))),
            (b, node(ObjectKind::File, std::slice::from_ref(&n2))),
        ]
        .into();
        let to: Topo = [
            (a, node(ObjectKind::File, &[n2])),
            (b, node(ObjectKind::File, &[n1])),
        ]
        .into();
        let ops = schedule(&from, &to).unwrap();
        check_replays(&from, &to, &ops);
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn nesting_a_directory_under_its_child_works_when_the_child_moves_out() {
        let (a, b) = (ObjectId::random(), ObjectId::random());
        let from: Topo = [
            (a, node(ObjectKind::Dir, &[link(ObjectId::ROOT, "a")])),
            (b, node(ObjectKind::Dir, &[link(a, "b")])),
        ]
        .into();
        let to: Topo = [
            (b, node(ObjectKind::Dir, &[link(ObjectId::ROOT, "b")])),
            (a, node(ObjectKind::Dir, &[link(b, "a")])),
        ]
        .into();
        let ops = schedule(&from, &to).unwrap();
        check_replays(&from, &to, &ops);
    }

    #[test]
    fn hard_links_are_created_then_linked() {
        let f = ObjectId::random();
        let d = ObjectId::random();
        let from: Topo = Topo::new();
        let to: Topo = [
            (d, node(ObjectKind::Dir, &[link(ObjectId::ROOT, "d")])),
            (
                f,
                node(ObjectKind::File, &[link(ObjectId::ROOT, "x"), link(d, "y")]),
            ),
        ]
        .into();
        let ops = schedule(&from, &to).unwrap();
        check_replays(&from, &to, &ops);
    }

    #[test]
    fn deleting_a_tree_removes_children_first() {
        let (a, b, f) = (ObjectId::random(), ObjectId::random(), ObjectId::random());
        let from: Topo = [
            (a, node(ObjectKind::Dir, &[link(ObjectId::ROOT, "a")])),
            (b, node(ObjectKind::Dir, &[link(a, "b")])),
            (f, node(ObjectKind::File, &[link(b, "f")])),
        ]
        .into();
        let ops = schedule(&from, &Topo::new()).unwrap();
        check_replays(&from, &Topo::new(), &ops);
    }

    /// A random forest: directories nest, files may have several links.
    fn random_topo(rng: &mut StdRng, ids: &[(ObjectId, ObjectKind)]) -> Topo {
        let names = ["a", "b", "c", "d", "e"];
        let mut topo = Topo::new();
        let mut dirs = vec![ObjectId::ROOT];
        let mut used = BTreeSet::new();
        for (id, kind) in ids {
            if rng.random_bool(0.2) {
                continue;
            }
            let links = if *kind == ObjectKind::File {
                rng.random_range(1..=2)
            } else {
                1
            };
            let mut set = BTreeSet::new();
            for _ in 0..links {
                let parent = dirs[rng.random_range(0..dirs.len())];
                let l = link(parent, names[rng.random_range(0..names.len())]);
                if used.insert(l.clone()) {
                    set.insert(l);
                }
            }
            if set.is_empty() {
                continue;
            }
            topo.insert(*id, (*kind, set));
            if *kind == ObjectKind::Dir {
                dirs.push(*id);
            }
        }
        topo
    }

    #[test]
    fn any_tree_can_become_any_other() {
        for seed in 0..3000 {
            let mut rng = StdRng::seed_from_u64(seed);
            let ids: Vec<_> = (0..12)
                .map(|i| {
                    (
                        ObjectId::random(),
                        if i % 2 == 0 {
                            ObjectKind::Dir
                        } else {
                            ObjectKind::File
                        },
                    )
                })
                .collect();
            let from = random_topo(&mut rng, &ids);
            let to = random_topo(&mut rng, &ids);
            let ops = schedule(&from, &to).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            check_replays(&from, &to, &ops);
        }
    }
}
