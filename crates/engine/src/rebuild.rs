//! The ledger rebuilt from the state (PLAN.md, Sync): for each object, what a full sync that decided
//! nothing would leave in the ledger, worked out from the base, the rows and a scan, with no more of the
//! tree held than the ledger itself, a few hundred bytes an entry.
//!
//! The base is read once into a skeleton, every live object's kind and entries in columns; then the
//! rows, in object order, each against the scan's node at its inode. An object is settled when its row
//! is what a full finalize leaves (read whole, not racy, nothing on its way, the metadata it was agreed
//! at, held at the base's entries), and the scan finds it as its row has it, at the base's entries and
//! as the object its inode says; or, with no row, when it is out of view here. A settled object gets the
//! entry `Agreement::verdict` would write for it; anything else is unsettled, which a round takes as
//! written since, so that it decides about it as the full sync would.
//!
//! The same pass is the daemon's check of the ledger it keeps (`check`): an object that ledger vouches
//! for, whose entry is not the one the state says, is a defect.

use crate::fsops::birth_mark;
use crate::incr::{Entry, FxMap, Ledger, Seen};
use crate::root::Root;
use crate::scan::Scanned;
use crate::selection::Selection;
use crate::state::{Glance, Row, StateTable, Stored};
use anyhow::Result;
use cairn_protocol::{Content, Link, Metadata, Name, Node, ObjectId, ObjectKind};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// What the state says of one object.
#[derive(Debug, PartialEq, Eq)]
pub enum Rebuilt {
    /// Settled: the entry a full finalize that decided nothing writes.
    Put(Entry),
    /// Gone from the base and with no row: nothing to remember.
    Drop,
    /// Something a full sync would look at, and why.
    Unsettled(&'static str),
    /// A file settled in the state that the scan found changed: the entry the last agreement left, which
    /// a round finds the file not as, and why.
    Changed(Entry, &'static str),
}

/// Rows and base nodes read at a time: with a batch on each thread and one waiting for each, about a
/// megabyte in flight.
const BATCH: usize = 256;

/// What reads batches of the state, each to the function it is given.
type Reader<'r, B> = &'r mut dyn FnMut(&mut dyn FnMut(B) -> Result<()>) -> Result<()>;

/// `work` of each batch `read` gives, on as many threads as there are, and `sink` of each result in
/// the order `read` gave them, as soon as it is its turn: no more is held than the batches in flight.
fn in_parallel<B: Send, R: Send>(
    read: Reader<'_, B>,
    work: &(dyn Fn(B) -> Result<R> + Sync),
    sink: &mut dyn FnMut(R) -> Result<()>,
) -> Result<()> {
    // Four keep up with the reading, and each thread's allocator holds what it parsed last.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().clamp(1, 4));
    let (to_work, from_read) = std::sync::mpsc::sync_channel::<(usize, B)>(threads);
    let (to_sink, done) = std::sync::mpsc::channel::<(usize, Result<R>)>();
    let from_read = std::sync::Mutex::new(from_read);
    std::thread::scope(|s| -> Result<()> {
        for _ in 0..threads {
            let to_sink = to_sink.clone();
            let from_read = &from_read;
            s.spawn(move || loop {
                let next = from_read.lock().unwrap_or_else(|e| e.into_inner()).recv();
                let Ok((i, batch)) = next else {
                    return;
                };
                if to_sink.send((i, work(batch))).is_err() {
                    return;
                }
            });
        }
        drop(to_sink);
        let mut waiting: BTreeMap<usize, Result<R>> = BTreeMap::new();
        let mut next_out = 0;
        let mut drain = |waiting: &mut BTreeMap<usize, Result<R>>, next_out: &mut usize| {
            while let Some(r) = waiting.remove(next_out) {
                *next_out += 1;
                sink(r?)?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let mut n = 0;
        let read_all = read(&mut |batch| {
            to_work
                .send((n, batch))
                .map_err(|_| anyhow::anyhow!("the rebuild's workers stopped"))?;
            n += 1;
            while let Ok((i, r)) = done.try_recv() {
                waiting.insert(i, r);
            }
            drain(&mut waiting, &mut next_out)
        });
        drop(to_work);
        // Every worker ends once the batches sent are done, whatever happened here.
        while let Ok((i, r)) = done.recv() {
            waiting.insert(i, r);
        }
        read_all?;
        drain(&mut waiting, &mut next_out)?;
        if next_out != n {
            anyhow::bail!("the rebuild lost a batch of the state");
        }
        Ok(())
    })
}

/// Every live object of the base besides the root, with its kind and entries, in object order.
pub fn skeleton(root: &Root) -> Result<Ledger> {
    type Parsed = Vec<(ObjectId, Option<(ObjectKind, Node)>)>;
    let mut sk = Ledger::with_room(root.state.base_len()?);
    let work = |batch: Vec<(ObjectId, Stored)>| -> Result<Parsed> {
        batch
            .into_iter()
            .map(|(id, stored)| {
                if id == ObjectId::ROOT {
                    return Ok((id, None));
                }
                let Some(node) = stored.node()? else {
                    return Ok((id, None));
                };
                Ok((id, (!node.links.is_empty()).then(|| (node.kind(), node))))
            })
            .collect()
    };
    in_parallel(
        &mut |send| root.state.each_in_batches(StateTable::Base, BATCH, send),
        &work,
        &mut |parsed| {
            for (id, live) in parsed {
                if let Some((kind, node)) = live {
                    sk.push_object(id, kind, &node.links);
                }
            }
            Ok(())
        },
    )?;
    sk.index_children();
    Ok(sk)
}

/// What is in view here of each skeleton object, as the server's tree is seen through the selection
/// (`selection::visible`): how many of its entries, and the topmost entries left out. `None` with
/// nothing left out of view, where every entry is.
pub struct Visible {
    kept: Vec<u32>,
    pub left_out: BTreeSet<Link>,
}

pub fn visible(sk: &Ledger, selection: &Selection) -> Option<Visible> {
    walk(sk, selection, &|_| false)
}

/// The same walk, of the base as the merge's view of it: something excluded by identity that this device
/// holds (`held`) stays in it, to go the way a delete elsewhere goes.
fn walk(sk: &Ledger, selection: &Selection, held: &dyn Fn(ObjectId) -> bool) -> Option<Visible> {
    if selection.rules.is_empty() && selection.objects.is_empty() && selection.unheld.is_empty() {
        return None;
    }
    let mut v = Visible {
        kept: vec![0; sk.len()],
        left_out: BTreeSet::new(),
    };
    let mut stack: Vec<(ObjectId, Vec<Name>)> = vec![(ObjectId::ROOT, Vec::new())];
    while let Some((dir, path)) = stack.pop() {
        for i in sk.child_indices(&dir) {
            let id = sk.id_at(i);
            let is_dir = sk.kind_at(i) == ObjectKind::Dir;
            let names: Vec<Name> = sk
                .raw_links(i)
                .filter(|(p, _)| *p == dir)
                .filter_map(|(_, n)| Name::new(n.to_vec()).ok())
                .collect();
            for name in names {
                let link = Link {
                    parent: dir,
                    name: name.clone(),
                };
                let mut at = path.clone();
                at.push(name);
                if (selection.objects.contains(&id) && !held(id))
                    || selection.is_unheld(id, &link)
                    || selection.rules.excludes(&at, is_dir)
                {
                    v.left_out.insert(link);
                } else {
                    v.kept[i] += 1;
                    if is_dir {
                        stack.push((id, at));
                    }
                }
            }
        }
    }
    Some(v)
}

/// The objects with a row that the selection's view takes the rows of, before a scan is given them:
/// wholly out of view in the base as the merge sees it, and so, with the journal moving nothing, on the
/// server too. `None` where the journal moves something and there are such objects: which of them it
/// brings back into view is the whole tree's to say.
pub fn purged(
    sk: &Ledger,
    selection: &Selection,
    glance: &[(ObjectId, Glance)],
    journal_moves: bool,
) -> Option<BTreeSet<ObjectId>> {
    let has_row = |id: ObjectId| glance.binary_search_by_key(&id, |(x, _)| *x).is_ok();
    let Some(base_view) = walk(sk, selection, &has_row) else {
        return Some(BTreeSet::new());
    };
    let out: BTreeSet<ObjectId> = glance
        .iter()
        .filter_map(|(id, _)| {
            sk.find(id)
                .filter(|si| base_view.kept[*si] == 0)
                .map(|_| *id)
        })
        .collect();
    (out.is_empty() || !journal_moves).then_some(out)
}

/// Where the scan's nodes stand: the folder node at each path, the object each node's inode says it is,
/// and the node each object is so found at.
pub struct Places<'a, 's> {
    pub nodes: &'s [Scanned],
    pub dir_at: &'a FxMap<&'s [Name], usize>,
    pub provisional: &'a [Option<ObjectId>],
    pub node_of: &'a FxMap<ObjectId, usize>,
}

/// Where the scan found each object, for the rows judged against it: every node whole (`Places`), or
/// what a sieve kept, with what it kept quiet made again from the ledger and the row (incr.rs `Sifted`).
pub trait Found: Sync {
    /// The node the scan found this object at, by its inode, given the row it is judged against; `Err`
    /// with why, where what the scan read cannot have been what the row says.
    fn found(&self, id: &ObjectId, row: &Row) -> Option<Result<Cow<'_, Scanned>, &'static str>>;
    /// The entries a node has, by what its folders' inodes say they are.
    fn links_of(&self, n: &Scanned) -> Option<Vec<Link>>;
}

impl Found for Places<'_, '_> {
    fn found(&self, id: &ObjectId, _: &Row) -> Option<Result<Cow<'_, Scanned>, &'static str>> {
        self.node_of
            .get(id)
            .map(|&i| Ok(Cow::Borrowed(&self.nodes[i])))
    }

    fn links_of(&self, n: &Scanned) -> Option<Vec<Link>> {
        let mut out = Vec::with_capacity(n.paths.len());
        for p in &n.paths {
            let parent = match p.len() {
                0 => return None,
                1 => ObjectId::ROOT,
                k => self.provisional[*self.dir_at.get(&p[..k - 1])?]?,
            };
            out.push(Link {
                parent,
                name: p.last()?.clone(),
            });
        }
        out.sort();
        out.dedup();
        Some(out)
    }
}

/// Every row a rebuild's scan is given, but those the selection's view takes, found by inode: a glance at
/// each, as the scan and identification need it.
pub struct Glanced<'g> {
    glance: &'g [(ObjectId, Glance)],
    purged: &'g BTreeSet<ObjectId>,
    /// The rows by inode number, then device and object: each one's inode number, and the row.
    inos: Vec<u64>,
    at: Vec<u32>,
}

impl<'g> Glanced<'g> {
    pub fn new(glance: &'g [(ObjectId, Glance)], purged: &'g BTreeSet<ObjectId>) -> Self {
        let mut order: Vec<(u64, u64, u32)> = glance
            .iter()
            .enumerate()
            .filter(|(_, (id, _))| !purged.contains(id))
            .map(|(i, (_, g))| (g.fp.ino, g.fp.dev, i as u32))
            .collect();
        order.sort_unstable();
        Glanced {
            glance,
            purged,
            inos: order.iter().map(|(ino, _, _)| *ino).collect(),
            at: order.iter().map(|(_, _, i)| *i).collect(),
        }
    }

    /// The rows at an inode, in object order.
    fn at_inode(&self, key: (u64, u64)) -> impl Iterator<Item = &'g (ObjectId, Glance)> + '_ {
        let (dev, ino) = key;
        let start = self.inos.partition_point(|i| *i < ino);
        self.inos[start..]
            .iter()
            .zip(&self.at[start..])
            .take_while(move |(i, _)| **i == ino)
            .map(|(_, at)| &self.glance[*at as usize])
            .filter(move |(_, g)| g.fp.dev == dev)
    }

    /// The row of an object.
    fn of(&self, id: &ObjectId) -> Option<&'g Glance> {
        let i = self.glance.binary_search_by_key(id, |(x, _)| *x).ok()?;
        (!self.purged.contains(id)).then(|| &self.glance[i].1)
    }

    /// The object a node's inode says it is, as identification goes first (and `provisional`).
    pub fn provisional(&self, n: &Scanned) -> Option<ObjectId> {
        let mut best: Option<(bool, ObjectId)> = None;
        for (id, g) in self.at_inode((n.fp.dev, n.fp.ino)) {
            if g.content.kind() == n.content.kind()
                && birth_mark(g.fp.btime) == birth_mark(n.fp.btime)
            {
                let here = (g.fp.current() == n.fp.current(), *id);
                if best.is_none_or(|b| here > b) {
                    best = Some(here);
                }
            }
        }
        best.map(|(_, id)| id)
    }
}

impl crate::scan::KnownRows for Glanced<'_> {
    /// On two rows at one inode, the later object's, as a map of them all gives it.
    fn known(&self, key: &(u64, u64)) -> Option<crate::state::Looked<'_>> {
        self.at_inode(*key)
            .last()
            .map(|(_, g)| crate::state::Looked::from(&g.known()))
    }

    fn count(&self) -> usize {
        self.glance.len()
    }
}

/// What a rebuild's scan keeps of what it reads: of what its row and the base's entries account for,
/// the object it is and a witness of what was read, which `of_row` judges made again from the row.
pub struct RowSieve<'a> {
    pub rows: &'a Glanced<'a>,
    pub sk: &'a Ledger,
}

impl crate::scan::Sieve for RowSieve<'_> {
    fn folder(&self, node: &Scanned) -> Option<ObjectId> {
        self.rows.provisional(node)
    }

    fn quiet(&self, parent: Option<ObjectId>, n: &Scanned) -> Option<ObjectId> {
        let parent = parent?;
        let [path] = n.paths.as_slice() else {
            return None;
        };
        let name = path.last()?;
        let x = self.rows.provisional(n)?;
        let g = self.rows.of(&x)?;
        let si = self.sk.find(&x)?;
        let link = Link {
            parent,
            name: name.clone(),
        };
        if self.sk.kind_at(si) != n.content.kind()
            || !self.sk.links_at_are(si, std::slice::from_ref(&link))
            || (g.fp.dev, g.fp.ino) != (n.fp.dev, n.fp.ino)
            || g.content != n.content
        {
            return None;
        }
        let as_its_row = match n.content {
            Content::File { .. } => g.known().vouches_for(&n.fp),
            // Its metadata, which a glance does not hold, is the witness's to tell.
            _ => n.fp == g.fp && !n.racy && !n.metadata_incomplete,
        };
        as_its_row.then_some(x)
    }

    fn quiet_vouched(&self, _: Option<ObjectId>, _: &crate::scan::Child<'_>) -> bool {
        false
    }

    fn witness(&self, node: &Scanned) -> Option<[u8; 16]> {
        Some(crate::incr::witness(node))
    }

    fn witnessing(&self) -> bool {
        true
    }
}

/// Where a rebuild's scan found each object: what it kept whole, and what it kept quiet made again from
/// the row it is judged against and the base's one entry of it, checked against what was read.
pub struct RowFound<'a, 's> {
    pub c: &'a crate::incr::Classified<'s>,
    pub sk: &'a Ledger,
}

impl Found for RowFound<'_, '_> {
    fn found(&self, id: &ObjectId, row: &Row) -> Option<Result<Cow<'_, Scanned>, &'static str>> {
        let scan = self.c.scan();
        let q = match self.c.slot_of(id)? {
            crate::scan::Slot::Kept(k) => return Some(Ok(Cow::Borrowed(&scan.nodes[k as usize]))),
            crate::scan::Slot::Quiet(q) => q as usize,
        };
        let read = scan.witnesses.get(q)?;
        let link = self.sk.links_at(self.sk.find(id)?).into_iter().next()?;
        let mut path = scan.folder_path(scan.quiet[q].folder);
        path.push(link.name);
        let file = matches!(row.content, Content::File { .. });
        let mut n = Scanned {
            paths: vec![path],
            fp: match file {
                true => row.fp.current(),
                false => row.fp.clone(),
            },
            racy: false,
            content: row.content.clone(),
            metadata: match file {
                true => Metadata::default(),
                false => row.scan.clone(),
            },
            metadata_incomplete: false,
            holds_excluded: false,
            only_excluded: false,
        };
        if file {
            return Some(match crate::incr::witness(&n) == *read {
                true => Ok(Cow::Owned(n)),
                false => Err("the scan found another file than its record"),
            });
        }
        for (holds, only) in [(false, false), (true, false), (true, true)] {
            n.holds_excluded = holds;
            n.only_excluded = only;
            if crate::incr::witness(&n) == *read {
                return Some(Ok(Cow::Owned(n)));
            }
        }
        Some(Err("the scan read otherwise of it than its record"))
    }

    fn links_of(&self, n: &Scanned) -> Option<Vec<Link>> {
        self.c.links_of(n)
    }
}

/// The node each object is found at, by what its inode says.
pub fn node_of(provisional: &[Option<ObjectId>]) -> FxMap<ObjectId, usize> {
    let mut out: FxMap<ObjectId, usize> = FxMap::default();
    out.reserve(provisional.len());
    for (i, x) in provisional.iter().enumerate() {
        if let Some(x) = x {
            out.insert(*x, i);
        }
    }
    out
}

/// The folder node at each path the scan found a folder at.
pub fn dir_at(nodes: &[Scanned]) -> FxMap<&[Name], usize> {
    let mut out: FxMap<&[Name], usize> = FxMap::default();
    for (i, n) in nodes.iter().enumerate() {
        if n.content == Content::Dir {
            for p in &n.paths {
                out.insert(p.as_slice(), i);
            }
        }
    }
    out
}

/// What the state says of an object with a row: `si` is its place in the skeleton, if the base has it
/// live.
fn of_row(
    sk: &Ledger,
    vis: Option<&Visible>,
    p: &dyn Found,
    id: ObjectId,
    si: Option<usize>,
    row: &Row,
) -> Rebuilt {
    let Some(si) = si else {
        return Rebuilt::Unsettled("its record has no object in the base");
    };
    let kind = sk.kind_at(si);
    if vis.is_some_and(|v| v.kept[si] as usize != sk.link_count(si)) {
        return Rebuilt::Unsettled("it is out of view here, or one of its names is");
    }
    if row.content.kind() != kind {
        return Rebuilt::Unsettled("its record is of another kind than the base's");
    }
    if row.racy {
        return Rebuilt::Unsettled("its record was read too soon after a change to trust");
    }
    if row.incomplete {
        return Rebuilt::Unsettled("its record was not read whole");
    }
    if row.synced.is_some() {
        return Rebuilt::Unsettled("a change of it is on its way");
    }
    if row.applied.as_ref() != Some(&row.scan) {
        return Rebuilt::Unsettled("its metadata here is not what the base was agreed at");
    }
    if let Some(held) = &row.held {
        let held: Vec<Link> = held.iter().cloned().collect();
        if !sk.links_at_are(si, &held) {
            return Rebuilt::Unsettled("the names it is held at here are not the base's");
        }
    }
    let n = match p.found(&id, row) {
        None => return Rebuilt::Unsettled("the scan did not find it at its inode"),
        Some(Err(why)) => return Rebuilt::Unsettled(why),
        Some(Ok(n)) => n,
    };
    if (n.fp.dev, n.fp.ino) != (row.fp.dev, row.fp.ino) {
        return Rebuilt::Unsettled("the scan found it at another inode than its record's");
    }
    if n.content.kind() != kind {
        return Rebuilt::Unsettled("the scan found something else at its inode");
    }
    if !p.links_of(&n).is_some_and(|l| sk.links_at_are(si, &l)) {
        return Rebuilt::Unsettled("the scan found it at other names than the base's");
    }
    let changed = match n.content {
        Content::File { .. } => !(row.known().vouches_for(&n.fp) && n.content == row.content),
        _ => false,
    };
    let digest = match n.content {
        Content::File { .. } => None,
        _ => {
            if n.fp != row.fp
                || n.racy
                || n.metadata_incomplete
                || n.content != row.content
                || n.metadata != row.scan
            {
                return Rebuilt::Unsettled("the scan read otherwise of it than its record");
            }
            Some(crate::incr::digest(&n))
        }
    };
    let entry = Entry {
        kind,
        links: sk.links_at(si),
        seen: Some(Seen {
            fp: row.fp.clone(),
            content: row.content.clone(),
            digest,
        }),
    };
    // A file rewritten since: its entry stands, as a ledger kept would have it, so that the round finds
    // it rewritten in place, as it does on the ledger kept.
    match changed {
        true => Rebuilt::Changed(entry, "the scan found another file than its record"),
        false => Rebuilt::Put(entry),
    }
}

/// What the state says of a live object of the base with no row.
fn of_rowless(sk: &Ledger, vis: Option<&Visible>, si: usize) -> Rebuilt {
    if vis.is_some_and(|v| v.kept[si] == 0) {
        return Rebuilt::Put(Entry {
            kind: sk.kind_at(si),
            links: sk.links_at(si),
            seen: None,
        });
    }
    Rebuilt::Unsettled("the server has it and this device holds none of it")
}

/// Rows parsed, each with its place in the skeleton and what the state says of it.
type Judged = Vec<(ObjectId, Option<usize>, Rebuilt, Option<Row>)>;

/// Every row against the skeleton and the scan, in object order, to `sink`; the rows of what is not
/// settled are kept, for the round.
fn judge_rows(
    root: &Root,
    sk: &Ledger,
    vis: Option<&Visible>,
    p: &dyn Found,
    skip: &(dyn Fn(&ObjectId) -> bool + Sync),
    sink: &mut dyn FnMut(Judged) -> Result<()>,
) -> Result<()> {
    let work = |batch: Vec<(ObjectId, Stored)>| -> Result<Judged> {
        let mut out = Vec::with_capacity(batch.len());
        for (id, stored) in batch {
            let si = sk.find(&id);
            if skip(&id) {
                out.push((id, si, Rebuilt::Unsettled("taken up by the round"), None));
                continue;
            }
            let Some(row) = stored.row()? else {
                continue;
            };
            match of_row(sk, vis, p, id, si, &row) {
                r @ Rebuilt::Unsettled(_) => out.push((id, si, r, Some(row))),
                r => out.push((id, si, r, None)),
            }
        }
        Ok(out)
    };
    in_parallel(
        &mut |send| root.state.each_in_batches(StateTable::Rows, BATCH, send),
        &work,
        sink,
    )
}

/// A ledger rebuilt from the state, and what it leaves for a round to take up.
pub struct Rebuild {
    pub ledger: Ledger,
    /// What is not settled, which a round takes as written since.
    pub unsettled: BTreeSet<ObjectId>,
    /// The topmost entries on the server out of view here.
    pub reserved: BTreeSet<Link>,
}

impl Rebuild {
    /// Whether another rebuild came to this one.
    pub fn same(&self, other: &Rebuild) -> bool {
        self.difference(other).is_empty()
    }

    /// Where another rebuild differs from this one, for the oracle's report.
    pub fn difference(&self, other: &Rebuild) -> String {
        let mut out = Vec::new();
        if self.unsettled != other.unsettled {
            out.push(format!(
                "unsettled {:?} against {:?}",
                self.unsettled
                    .difference(&other.unsettled)
                    .collect::<Vec<_>>(),
                other
                    .unsettled
                    .difference(&self.unsettled)
                    .collect::<Vec<_>>()
            ));
        }
        if self.reserved != other.reserved {
            out.push(format!(
                "reserved {:?} against {:?}",
                self.reserved, other.reserved
            ));
        }
        let ids: BTreeSet<ObjectId> = self
            .ledger
            .ids()
            .iter()
            .chain(other.ledger.ids())
            .copied()
            .collect();
        for id in ids {
            let (a, b) = (self.ledger.entry(&id), other.ledger.entry(&id));
            if a != b {
                out.push(format!("{id}: {a:?} against {b:?}"));
                if out.len() > 8 {
                    break;
                }
            }
        }
        out.join("; ")
    }
}

/// The ledger from the state and this scan, whose nodes `provisional` says the objects of; `Err` with
/// why, when more is unsettled than `limit`, which the whole tree costs less than.
pub fn rebuild(
    root: &Root,
    (sk, vis): (&Ledger, Option<Visible>),
    p: &dyn Found,
    limit: usize,
) -> Result<std::result::Result<Rebuild, String>> {
    let mut seen = crate::incr::SeenParts::with_room(sk.len());
    let mut keep = vec![false; sk.len()];
    let mut has_row = vec![false; sk.len()];
    let mut unsettled: BTreeSet<ObjectId> = BTreeSet::new();
    let too_many = |n: usize| {
        format!("{n} objects are not as the state says the last agreement left them, more than taking the whole tree costs")
    };
    let mut over = false;
    judge_rows(root, sk, vis.as_ref(), p, &|_| false, &mut |judged| {
        for (id, si, r, _) in judged {
            if let Some(si) = si {
                has_row[si] = true;
            }
            // The root is in every round, and in no ledger.
            if id == ObjectId::ROOT {
                continue;
            }
            match (r, si) {
                (Rebuilt::Put(e) | Rebuilt::Changed(e, _), Some(si)) => {
                    if let Some(s) = &e.seen {
                        seen.put(si, s);
                    }
                    keep[si] = true;
                }
                (_, _) => {
                    unsettled.insert(id);
                    // Read on to the end: the count is for the log.
                    over |= unsettled.len() > limit;
                }
            }
        }
        Ok(())
    })?;
    for si in 0..sk.len() {
        if has_row[si] {
            continue;
        }
        match of_rowless(sk, vis.as_ref(), si) {
            Rebuilt::Put(e) => {
                if let Some(s) = &e.seen {
                    seen.put(si, s);
                }
                keep[si] = true;
            }
            _ => {
                unsettled.insert(sk.id_at(si));
            }
        }
    }
    if over || unsettled.len() > limit {
        return Ok(Err(too_many(unsettled.len())));
    }
    let reserved = vis.map(|v| v.left_out).unwrap_or_default();
    Ok(Ok(Rebuild {
        ledger: sk.rebuilt(seen, &keep),
        unsettled,
        reserved,
    }))
}

/// Where the ledger kept is not what the state says, for an object it vouches for.
pub struct Defect {
    pub object: ObjectId,
    pub what: String,
}

/// The ledger kept, against the state and this scan: every object it vouches for, besides those the
/// round takes up anyway (`skip`), whose entry is not the one the state says, or that it has no word of
/// though the state has it. `places` are the round's own classification's.
pub fn check(
    root: &Root,
    stored: &Ledger,
    selection: &Selection,
    p: &dyn Found,
    skip: &(dyn Fn(&ObjectId) -> bool + Sync),
) -> Result<Vec<Defect>> {
    let sk = skeleton(root)?;
    let vis = visible(&sk, selection);
    let mut has_row = vec![false; sk.len()];
    let mut dead_rows: Vec<ObjectId> = Vec::new();
    let mut defects = Vec::new();
    let compare = |id: ObjectId, r: &Rebuilt| -> Option<String> {
        let entry = stored.entry(&id);
        match (entry, r) {
            (Some(e), Rebuilt::Put(r)) if e == *r => None,
            (None, Rebuilt::Drop) => None,
            (Some(e), Rebuilt::Put(r)) => Some(differs(&e, r)),
            (Some(_), Rebuilt::Unsettled(why) | Rebuilt::Changed(_, why)) => {
                Some((*why).to_string())
            }
            (Some(_), Rebuilt::Drop) => Some("it has gone from the state".into()),
            (None, Rebuilt::Put(_)) => Some("the ledger has no word of it".into()),
            (None, Rebuilt::Unsettled(why) | Rebuilt::Changed(_, why)) => {
                Some(format!("the ledger has no word of it, and {why}"))
            }
        }
    };
    judge_rows(root, &sk, vis.as_ref(), p, skip, &mut |judged| {
        for (id, si, r, _) in judged {
            match si {
                Some(si) => has_row[si] = true,
                None => dead_rows.push(id),
            }
            if skip(&id) {
                continue;
            }
            if let Some(what) = compare(id, &r) {
                defects.push(Defect { object: id, what });
            }
        }
        Ok(())
    })?;
    for (si, row) in has_row.iter().enumerate() {
        let id = sk.id_at(si);
        if *row || skip(&id) {
            continue;
        }
        if let Some(what) = compare(id, &of_rowless(&sk, vis.as_ref(), si)) {
            defects.push(Defect { object: id, what });
        }
    }
    dead_rows.sort_unstable();
    for &id in stored.ids() {
        if skip(&id) || sk.find(&id).is_some() || dead_rows.binary_search(&id).is_ok() {
            continue;
        }
        if let Some(what) = compare(id, &Rebuilt::Drop) {
            defects.push(Defect { object: id, what });
        }
    }
    defects.sort_by_key(|d| d.object);
    Ok(defects)
}

/// Which part of an entry the state says otherwise.
fn differs(kept: &Entry, state: &Entry) -> String {
    let what = if kept.kind != state.kind {
        "the ledger has another kind for it than the base"
    } else if kept.links != state.links {
        "the ledger has other names for it than the base"
    } else {
        match (&kept.seen, &state.seen) {
            (None, Some(_)) => "the ledger has it out of view, and it is in view here",
            (Some(_), None) => "the ledger has it in view, and it is out of view here",
            (Some(a), Some(b)) if a.fp != b.fp => {
                "the ledger has another fingerprint for it than its record"
            }
            (Some(a), Some(b)) if a.content != b.content => {
                "the ledger has other content for it than its record"
            }
            (Some(a), Some(b)) if a.digest != b.digest => {
                "the ledger has another digest of it than the scan read"
            }
            _ => "the ledger has another entry for it",
        }
    };
    what.to_string()
}

/// The paths of these objects, as the base or the row's last scan has them, for the log.
pub fn paths_of(root: &Root, ids: &[ObjectId]) -> Result<HashMap<ObjectId, String>> {
    let tree = root.state.base_subset(ids.iter().copied())?;
    let mut out = HashMap::new();
    for id in ids {
        let path = tree
            .get_live(*id)
            .and_then(|n| n.links.iter().next())
            .and_then(|l| tree.path_of_link(l))
            .map(|p| crate::scan::display_path(&p));
        out.insert(*id, path.unwrap_or_else(|| format!("the object {id}")));
    }
    Ok(out)
}
