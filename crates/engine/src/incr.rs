//! A sync that carries a change, at the cost of what changed and of the scan.
//!
//! The full sync decides about every object, but at an agreement it decided nothing about any, and the
//! merge looks at little besides the object itself: its ancestors, the other names in its folders, what is
//! inside it when it goes or moves, every name of a hard-linked file. So an object whose own state and
//! whose neighbours' are as they were at an agreement is decided about as it was then: not at all.
//!
//! The ledger (state.rs, `ledger`) says, for each live object of the base, what the last agreement was:
//! its kind and entries, and what the scan that agreed saw of it, the inode and fingerprint, the content,
//! and for anything but a file a digest of the rest of what was read; a file is its row, which the scan
//! does not read while its fingerprint holds. `dirty` holds every object whose base node or row anything
//! has written since, by triggers in the file. A round then takes:
//!
//! - from the journal, every object an operation names, and the folders its entries are in;
//! - from the scan, every entry the ledger does not account for, every object the ledger has that the scan
//!   did not find as it was, and everything dirty, marked or out of view by name;
//! - closed under the merge's neighbours: ancestors in every tree, every name in each folder any entry of
//!   those objects is in, everything in a folder among them;
//!
//! and runs the full sync's own code on those alone: the trees, rows and scan it is given hold only them,
//! and what they leave out is the same on every side. A decision about an object loaded only as a
//! neighbour means the set was too small: it is widened and the round decided again. A file rewritten in
//! place, whose one name, inode and kind are as the ledger has them, opens no folder: it takes no name.
//!
//! Where the ledger does not speak for a sync, it is rebuilt from the state (rebuild.rs), and the round
//! runs on that; what neither can vouch for goes the full way, said in a log line. In a harness build every
//! incremental round is decided the full way as well, on the same scan, journal and state (sync.rs
//! `oracle_full`), and `compare` fails the sync unless the two agree on every decision; after its
//! finalize, `check_finalized` checks the state against what a full one leaves, and every round on a
//! ledger kept checks it against the ledger rebuilt from the state, as the daemon's check does.

use crate::fsops::birth_mark;
use crate::merge::LNode;
use crate::record::{R, W};
use crate::root::Root;
use crate::scan::{ScanResult, Scanned};
use crate::selection::Selection;
use crate::state::{Fingerprint, Known, Row};
use anyhow::Result;
use cairn_protocol::{Content, Hash, Link, Name, Node, ObjectId, ObjectKind, Op, Tree};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Moved whenever what the ledger means or how it is written changes, so that none written before is read
/// as one of these. 2: integers as varints.
pub const FORMAT: u32 = 2;

/// What the last agreement was for one live object of the base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: ObjectKind,
    /// As in the base node.
    pub links: Vec<Link>,
    /// What the scan that agreed saw of it; `None` for an object out of view here.
    pub seen: Option<Seen>,
}

/// One object as the scan that agreed saw it, and as its row then said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub fp: Fingerprint,
    pub content: Content,
    /// For anything but a file: what else the scan read of it, besides its paths.
    pub digest: Option<Hash>,
}

impl Seen {
    /// What a scan needs of the row behind it: a ledger entry is only read while its row has not changed.
    pub fn known(&self) -> Known<'_> {
        Known {
            fp: &self.fp,
            racy: false,
            incomplete: false,
            content: self.content.clone(),
            scan: None,
        }
    }
}

fn kind_byte(k: ObjectKind) -> u8 {
    match k {
        ObjectKind::File => 0,
        ObjectKind::Dir => 1,
        ObjectKind::Symlink => 2,
    }
}

/// An entry: its format, kind and entries, then what the scan saw, its integers as varints and the rest
/// as `record` writes them.
impl Entry {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::with_capacity(96));
        w.u8(FORMAT as u8);
        w.u8(kind_byte(self.kind));
        crate::record::put_links(&mut w, self.links.iter());
        match &self.seen {
            None => w.u8(0),
            Some(s) => {
                let ctime_same = s.fp.ctime == s.fp.mtime;
                w.u8(1
                    | u8::from(s.fp.btime.is_some()) << 1
                    | u8::from(ctime_same) << 2
                    | u8::from(s.digest.is_some()) << 3);
                w.uv(s.fp.dev);
                w.uv(s.fp.ino);
                w.uv(s.fp.size);
                w.time(s.fp.mtime);
                if !ctime_same {
                    w.time(s.fp.ctime);
                }
                if let Some(t) = s.fp.btime {
                    w.time(t);
                }
                crate::record::put_content(&mut w, &s.content);
                if let Some(d) = &s.digest {
                    w.raw(&d.0);
                }
            }
        }
        w.0
    }

    pub fn decode(b: &[u8]) -> Option<Entry> {
        let mut r = R(b);
        if u32::from(r.u8()?) != FORMAT {
            return None;
        }
        let kind = match r.u8()? {
            0 => ObjectKind::File,
            1 => ObjectKind::Dir,
            2 => ObjectKind::Symlink,
            _ => return None,
        };
        let n = r.count(18)?;
        let mut links = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            links.push(crate::record::get_link(&mut r)?);
        }
        let seen = match r.u8()? {
            0 => None,
            flags if flags & 1 == 1 && flags >> 4 == 0 => {
                let dev = r.uv()?;
                let ino = r.uv()?;
                let size = r.uv()?;
                let mtime = r.time()?;
                let ctime = match flags & 4 != 0 {
                    true => mtime,
                    false => r.time()?,
                };
                let btime = match flags & 2 != 0 {
                    true => Some(r.time()?),
                    false => None,
                };
                let content = crate::record::get_content(&mut r)?;
                let digest = match flags & 8 != 0 {
                    true => Some(Hash(r.array::<32>()?)),
                    false => None,
                };
                Some(Seen {
                    fp: Fingerprint {
                        dev,
                        ino,
                        size,
                        mtime,
                        ctime,
                        btime,
                    },
                    content,
                    digest,
                })
            }
            _ => return None,
        };
        r.0.is_empty().then_some(Entry { kind, links, seen })
    }
}

/// What a scan read of an entry besides its paths, for anything but a file, which is its row.
pub fn digest(n: &Scanned) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(b"cairn ledger entry");
    h.update(&(n.paths.len() as u64).to_le_bytes());
    let fp = &n.fp;
    for v in [fp.dev, fp.ino, fp.size] {
        h.update(&v.to_le_bytes());
    }
    for t in [fp.mtime, fp.ctime] {
        h.update(&t.sec.to_le_bytes());
        h.update(&t.nsec.to_le_bytes());
    }
    match fp.btime {
        Some(t) => {
            h.update(&[1]);
            h.update(&t.sec.to_le_bytes());
            h.update(&t.nsec.to_le_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
    h.update(&[
        u8::from(n.racy),
        u8::from(n.metadata_incomplete),
        u8::from(n.holds_excluded),
        u8::from(n.only_excluded),
    ]);
    // Serializing these cannot fail: plain values, and maps with string keys.
    let content = serde_json::to_vec(&n.content).unwrap_or_default();
    h.update(&(content.len() as u64).to_le_bytes());
    h.update(&content);
    let meta = serde_json::to_vec(&n.metadata).unwrap_or_default();
    h.update(&(meta.len() as u64).to_le_bytes());
    h.update(&meta);
    Hash(*h.finalize().as_bytes())
}

/// The ledger as a sync holds it, in columns by object, sorted: an entry's links as bytes, the scan part
/// whole, and what finds an entry by inode and by folder, sorted too. A few hundred bytes an entry.
#[derive(Default)]
pub struct Ledger {
    ids: Vec<ObjectId>,
    kinds: Vec<ObjectKind>,
    /// The links of entry `i` are `link_bytes[link_off[i]..link_off[i + 1]]`: each a parent and a
    /// length-prefixed name.
    link_off: Vec<u32>,
    link_bytes: Vec<u8>,
    seen: Vec<Option<Seen>>,
    by_inode: Vec<((u64, u64), u32)>,
    children: Vec<(ObjectId, u32)>,
}

impl Ledger {
    /// From the entries, in any order.
    pub fn new(entries: impl IntoIterator<Item = (ObjectId, Entry)>) -> Self {
        let mut all: Vec<(ObjectId, Entry)> = entries.into_iter().collect();
        all.sort_by_key(|(id, _)| *id);
        let mut l = Ledger::default();
        for (id, e) in all {
            l.push(id, e);
        }
        l.index();
        l
    }

    fn push(&mut self, id: ObjectId, e: Entry) {
        if self.link_off.is_empty() {
            self.link_off.push(0);
        }
        self.ids.push(id);
        self.kinds.push(e.kind);
        for link in &e.links {
            self.link_bytes.extend_from_slice(&link.parent.0);
            let name = link.name.as_bytes();
            self.link_bytes
                .extend_from_slice(&(name.len() as u32).to_le_bytes());
            self.link_bytes.extend_from_slice(name);
        }
        self.link_off.push(self.link_bytes.len() as u32);
        self.seen.push(e.seen);
    }

    fn index(&mut self) {
        let mut by_inode = Vec::with_capacity(self.ids.len());
        let mut children = Vec::with_capacity(self.ids.len());
        for i in 0..self.ids.len() {
            if let Some(s) = &self.seen[i] {
                by_inode.push(((s.fp.dev, s.fp.ino), i as u32));
            }
            for (parent, _) in self.raw_links(i) {
                children.push((parent, i as u32));
            }
        }
        by_inode.sort_unstable();
        children.sort_unstable();
        children.shrink_to_fit();
        self.by_inode = by_inode;
        self.children = children;
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub(crate) fn find(&self, id: &ObjectId) -> Option<usize> {
        self.ids.binary_search(id).ok()
    }

    /// One more of the base's objects, after every one before it in order, with no entry yet: a
    /// skeleton of the base, which `rebuilt` makes a ledger of.
    pub(crate) fn push_object(&mut self, id: ObjectId, kind: ObjectKind, links: &BTreeSet<Link>) {
        if self.link_off.is_empty() {
            self.link_off.push(0);
        }
        self.ids.push(id);
        self.kinds.push(kind);
        for link in links {
            self.link_bytes.extend_from_slice(&link.parent.0);
            let name = link.name.as_bytes();
            self.link_bytes
                .extend_from_slice(&(name.len() as u32).to_le_bytes());
            self.link_bytes.extend_from_slice(name);
        }
        self.link_off.push(self.link_bytes.len() as u32);
    }

    /// What finds a skeleton's objects by folder.
    pub(crate) fn index_children(&mut self) {
        let mut children = Vec::with_capacity(self.ids.len());
        for i in 0..self.ids.len() {
            for (parent, _) in self.raw_links(i) {
                children.push((parent, i as u32));
            }
        }
        children.sort_unstable();
        children.shrink_to_fit();
        self.children = children;
    }

    pub(crate) fn ids(&self) -> &[ObjectId] {
        &self.ids
    }

    pub(crate) fn id_at(&self, i: usize) -> ObjectId {
        self.ids[i]
    }

    pub(crate) fn kind_at(&self, i: usize) -> ObjectKind {
        self.kinds[i]
    }

    /// The objects with an entry in this folder, by index, each once.
    pub(crate) fn child_indices(&self, parent: &ObjectId) -> impl Iterator<Item = usize> + '_ {
        let parent = *parent;
        let start = self.children.partition_point(|(p, _)| *p < parent);
        let mut last = None;
        self.children[start..]
            .iter()
            .take_while(move |(p, _)| *p == parent)
            .filter(move |(_, i)| last.replace(*i) != Some(*i))
            .map(|(_, i)| *i as usize)
    }

    /// How many entries object `i` has.
    pub(crate) fn link_count(&self, i: usize) -> usize {
        self.raw_links(i).count()
    }

    /// Whether object `i`'s entries are these, in order.
    pub(crate) fn links_at_are(&self, i: usize, links: &[Link]) -> bool {
        let mut mine = self.raw_links(i);
        for l in links {
            match mine.next() {
                Some((parent, name)) if parent == l.parent && name == l.name.as_bytes() => {}
                _ => return false,
            }
        }
        mine.next().is_none()
    }

    /// The ledger a skeleton of the base makes: the objects `keep` says, each with what was seen of it.
    pub(crate) fn rebuilt(self, mut seen: Vec<Option<Seen>>, keep: &[bool]) -> Ledger {
        let n = keep.iter().filter(|k| **k).count();
        let mut l = Ledger {
            ids: Vec::with_capacity(n),
            kinds: Vec::with_capacity(n),
            link_off: Vec::with_capacity(n + 1),
            link_bytes: Vec::new(),
            seen: Vec::with_capacity(n),
            ..Ledger::default()
        };
        l.link_off.push(0);
        for i in 0..self.ids.len() {
            if !keep[i] {
                continue;
            }
            l.ids.push(self.ids[i]);
            l.kinds.push(self.kinds[i]);
            l.link_bytes.extend_from_slice(
                &self.link_bytes[self.link_off[i] as usize..self.link_off[i + 1] as usize],
            );
            l.link_off.push(l.link_bytes.len() as u32);
            l.seen.push(seen[i].take());
        }
        drop(self);
        l.link_bytes.shrink_to_fit();
        l.index();
        l
    }

    pub fn contains(&self, id: &ObjectId) -> bool {
        self.find(id).is_some()
    }

    pub(crate) fn raw_links(&self, i: usize) -> impl Iterator<Item = (ObjectId, &[u8])> + '_ {
        let mut b = &self.link_bytes[self.link_off[i] as usize..self.link_off[i + 1] as usize];
        std::iter::from_fn(move || {
            if b.is_empty() {
                return None;
            }
            let parent = ObjectId(b[..16].try_into().ok()?);
            let n = u32::from_le_bytes(b[16..20].try_into().ok()?) as usize;
            let name = &b[20..20 + n];
            b = &b[20 + n..];
            Some((parent, name))
        })
    }

    pub(crate) fn links_at(&self, i: usize) -> Vec<Link> {
        self.raw_links(i)
            .filter_map(|(parent, name)| {
                Some(Link {
                    parent,
                    name: Name::new(name.to_vec()).ok()?,
                })
            })
            .collect()
    }

    pub fn kind_of(&self, id: &ObjectId) -> Option<ObjectKind> {
        self.find(id).map(|i| self.kinds[i])
    }

    pub fn links_of(&self, id: &ObjectId) -> Option<Vec<Link>> {
        self.find(id).map(|i| self.links_at(i))
    }

    pub fn seen_of(&self, id: &ObjectId) -> Option<&Seen> {
        self.find(id).and_then(|i| self.seen[i].as_ref())
    }

    /// Whether these are its entries, without taking them apart.
    fn links_are(&self, id: &ObjectId, links: &[Link]) -> bool {
        let Some(i) = self.find(id) else {
            return false;
        };
        let mut mine = self.raw_links(i);
        for l in links {
            match mine.next() {
                Some((parent, name)) if parent == l.parent && name == l.name.as_bytes() => {}
                _ => return false,
            }
        }
        mine.next().is_none()
    }

    /// The whole entry, as stored.
    pub fn entry(&self, id: &ObjectId) -> Option<Entry> {
        let i = self.find(id)?;
        Some(Entry {
            kind: self.kinds[i],
            links: self.links_at(i),
            seen: self.seen[i].clone(),
        })
    }

    /// Every entry the scan saw at this inode.
    pub fn at_inode(&self, key: &(u64, u64)) -> impl Iterator<Item = ObjectId> + '_ {
        let key = *key;
        let start = self.by_inode.partition_point(|(k, _)| *k < key);
        self.by_inode[start..]
            .iter()
            .take_while(move |(k, _)| *k == key)
            .map(|(_, i)| self.ids[*i as usize])
    }

    /// Every entry with a link in this folder.
    pub fn children_of(&self, parent: &ObjectId) -> impl Iterator<Item = ObjectId> + '_ {
        let parent = *parent;
        let start = self.children.partition_point(|(p, _)| *p < parent);
        let mut last = None;
        self.children[start..]
            .iter()
            .take_while(move |(p, _)| *p == parent)
            // A hard link with two names in one folder is one child.
            .filter(move |(_, i)| last.replace(*i) != Some(*i))
            .map(|(_, i)| self.ids[*i as usize])
    }

    /// The entry at this name in this folder, as the ledger has it.
    pub fn children_named(&self, parent: &ObjectId, name: &Name) -> Option<ObjectId> {
        self.children_of(parent).find(|c| {
            self.find(c).is_some_and(|i| {
                self.raw_links(i)
                    .any(|(p, n)| p == *parent && n == name.as_bytes())
            })
        })
    }

    /// Every entry the scan saw, by inode.
    fn by_inode(&self) -> impl Iterator<Item = ((u64, u64), ObjectId, &Seen)> + '_ {
        self.by_inode.iter().filter_map(|(key, i)| {
            let i = *i as usize;
            Some((*key, self.ids[i], self.seen[i].as_ref()?))
        })
    }

    /// Whether more than one entry the scan saw is at this inode.
    fn shares_inode(&self, key: &(u64, u64)) -> bool {
        self.at_inode(key).nth(1).is_some()
    }

    /// Every entry the scan saw.
    pub fn iter_seen(&self) -> impl Iterator<Item = (ObjectId, &Seen)> + '_ {
        self.ids
            .iter()
            .zip(&self.seen)
            .filter_map(|(id, s)| s.as_ref().map(|s| (*id, s)))
    }

    /// Read from the state, in order, each entry taken apart as it comes.
    fn load(root: &Root) -> Result<Option<Ledger>> {
        let n = root.state.ledger_len()?;
        let mut l = Ledger {
            ids: Vec::with_capacity(n),
            kinds: Vec::with_capacity(n),
            link_off: Vec::with_capacity(n + 1),
            link_bytes: Vec::with_capacity(n * 36),
            seen: Vec::with_capacity(n),
            ..Ledger::default()
        };
        let mut bad = false;
        root.state
            .each_ledger_entry(&mut |id, bytes| match Entry::decode(bytes) {
                Some(e) if l.ids.last().is_none_or(|last| *last < id) => l.push(id, e),
                // An entry that does not parse is a ledger this build cannot read.
                _ => bad = true,
            })?;
        if bad {
            return Ok(None);
        }
        l.ids.shrink_to_fit();
        l.link_bytes.shrink_to_fit();
        l.index();
        Ok(Some(l))
    }
}

/// What the ledger stands on: the build, configuration and selection it was written under, and the
/// entries on the server this device left out of view then.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub format: u32,
    pub inputs: Hash,
    pub reserved: BTreeSet<Link>,
}

/// Everything besides the state, the journal and the tree that an object's decision follows from.
pub fn inputs(root: &Root, selection: &Selection) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(b"cairn ledger inputs");
    h.update(&FORMAT.to_le_bytes());
    let mut part = |b: &[u8]| {
        h.update(&(b.len() as u64).to_le_bytes());
        h.update(b);
    };
    part(crate::steady::build().as_bytes());
    part(&serde_json::to_vec(&root.config).unwrap_or_default());
    match selection.rules.text_hash() {
        Some(r) => part(&r.0),
        None => part(b""),
    }
    part(&serde_json::to_vec(&selection.objects).unwrap_or_default());
    part(&serde_json::to_vec(&selection.copies).unwrap_or_default());
    part(&serde_json::to_vec(&selection.unheld).unwrap_or_default());
    Hash(*h.finalize().as_bytes())
}

/// A sync that may take what changed alone: the ledger it holds, and the journal it has read.
pub struct Session {
    pub ledger: Ledger,
    pub anchor: Anchor,
    pub journal: Journal,
    /// Manifests rows of this sync held at some point: what a finalize may find nothing holds.
    pub released: BTreeSet<Hash>,
    /// Why the ledger is rebuilt from the state rather than read, when it is: the next round rebuilds
    /// it while `to_rebuild`, and every round takes what the rebuild found unsettled as written since.
    pub rebuilt: Option<String>,
    pub to_rebuild: bool,
    pub unsettled: BTreeSet<ObjectId>,
    /// How many objects the ledger speaks for, or will once rebuilt: what a round is measured against.
    pub size: usize,
}

/// Why a sync cannot read the ledger, and whether rebuilding it from the state stands in for it.
pub struct Closed {
    pub why: String,
    pub rebuild: bool,
}

/// In a harness build, `CAIRN_FP_LEDGER_REBUILD=1` rebuilds the ledger from the state in every sync
/// that could read it, so that the oracle compares the rebuild with the full sync everywhere.
pub fn forced_rebuild() -> bool {
    cfg!(feature = "failpoints") && std::env::var("CAIRN_FP_LEDGER_REBUILD").is_ok_and(|v| v == "1")
}

/// Fewer than this many objects is never too many to take alone, whatever the size of the tree.
const FLOOR: usize = 4_096;

impl Session {
    /// The ledger, when it speaks for this sync; otherwise why not, for the log, and whether the
    /// ledger rebuilt from the state may stand in for it.
    pub fn open(
        root: &Root,
        selection: &Selection,
        dry_run: bool,
    ) -> Result<std::result::Result<Session, Closed>> {
        let closed = |why: &str, rebuild: bool| {
            Ok(Err(Closed {
                why: why.into(),
                rebuild,
            }))
        };
        if dry_run {
            return closed("a dry run goes the full way", false);
        }
        if !selection.copies.is_empty() {
            return closed(
                "copies kept here by `cairn exclude` are found by inode, which needs the whole tree",
                false,
            );
        }
        // A first sync has nothing to rebuild from.
        let rebuild = root.state.has_base()?;
        let Some(text) = root.state.ledger_anchor()? else {
            return closed(
                "no full sync has agreed with the server since this build first ran here",
                rebuild,
            );
        };
        let Ok(anchor) = serde_json::from_str::<Anchor>(&text) else {
            return closed("the ledger was written by another build", rebuild);
        };
        if anchor.format != FORMAT {
            return closed("the ledger was written by another build", rebuild);
        }
        if anchor.inputs != inputs(root, selection) {
            return closed(
                "the build, the configuration, the rules or what is excluded have changed since the last full sync",
                rebuild,
            );
        }
        if root.state.pending()? {
            return closed(
                "a mark or a queued commit from a run that did not finish is left",
                rebuild,
            );
        }
        if forced_rebuild() {
            return closed("asked for by the harness", rebuild);
        }
        let Some(ledger) = Ledger::load(root)? else {
            return closed("the ledger was written by another build", rebuild);
        };
        Ok(Ok(Session {
            size: ledger.len(),
            ledger,
            anchor,
            journal: Journal::default(),
            released: BTreeSet::new(),
            rebuilt: None,
            to_rebuild: false,
            unsettled: BTreeSet::new(),
        }))
    }

    /// A session whose ledger its first round rebuilds from the state.
    pub fn rebuilding(root: &Root, selection: &Selection, why: String) -> Result<Session> {
        Ok(Session {
            size: root.state.base_len()?,
            ledger: Ledger::default(),
            anchor: Anchor {
                format: FORMAT,
                inputs: inputs(root, selection),
                reserved: BTreeSet::new(),
            },
            journal: Journal::default(),
            released: BTreeSet::new(),
            rebuilt: Some(why),
            to_rebuild: true,
            unsettled: BTreeSet::new(),
        })
    }

    /// From the next round on, the ledger rebuilt from the state, for this reason.
    pub fn rebuild(&mut self, root: &Root, selection: &Selection, why: String) -> Result<()> {
        let journal = std::mem::take(&mut self.journal);
        let released = std::mem::take(&mut self.released);
        *self = Session {
            journal,
            released,
            ..Session::rebuilding(root, selection, why)?
        };
        Ok(())
    }

    /// More than this, and the full sync costs less.
    pub fn limit(&self) -> usize {
        let configured = cfg!(feature = "failpoints")
            .then(|| std::env::var("CAIRN_FP_INCR_LIMIT").ok()?.parse().ok())
            .flatten();
        configured.unwrap_or_else(|| (self.size / 4).max(FLOOR))
    }
}

/// What a round knows of the state before it scans.
pub struct Before<'a> {
    pub ledger: &'a Ledger,
    /// Every object whose base node or row has been written since its entry, and the base nodes and
    /// rows those have now.
    pub dirty: &'a BTreeSet<ObjectId>,
    pub dirty_nodes: &'a BTreeMap<ObjectId, Node>,
    pub dirty_rows: &'a BTreeMap<ObjectId, Row>,
    /// What the journal since the cursor does.
    pub journal: &'a Journal,
    /// The scan was given every row as a glance (the round that rebuilds the ledger), not the ledger's
    /// entries and the rows written since: every row but those of `unscanned`, which the selection's
    /// view takes.
    pub glanced: bool,
    pub unscanned: &'a BTreeSet<ObjectId>,
}

impl Before<'_> {
    fn is_dirty(&self, id: &ObjectId) -> bool {
        self.dirty.contains(id)
    }

    /// Written since, and nothing now: no base node or row, no entry in the ledger, nothing in the
    /// journal. What a row moved to another object leaves, as a join's adoption leaves the ID the scan
    /// gave each file found here before it took the server's. No part of the tree, so no part of what a
    /// round on it costs.
    pub fn holds_nothing(&self, id: &ObjectId) -> bool {
        self.is_dirty(id)
            && !self.dirty_nodes.contains_key(id)
            && !self.dirty_rows.contains_key(id)
            && !self.journal.touched.contains(id)
            && !self.ledger.contains(id)
    }

    /// The base's entries of an object.
    fn base_links(&self, id: &ObjectId) -> Vec<Link> {
        if self.is_dirty(id) {
            self.dirty_nodes
                .get(id)
                .map(|n| n.links.iter().cloned().collect())
                .unwrap_or_default()
        } else {
            self.ledger.links_of(id).unwrap_or_default()
        }
    }

    fn kind(&self, id: &ObjectId) -> Option<ObjectKind> {
        if *id == ObjectId::ROOT {
            return Some(ObjectKind::Dir);
        }
        let base = if self.is_dirty(id) {
            self.dirty_nodes.get(id).map(Node::kind)
        } else {
            self.ledger.kind_of(id)
        };
        base.or_else(|| self.journal.kinds.get(id).copied())
    }

    /// Every row's view for the scan, as a map of all of them would give it: on two rows at one inode,
    /// the later object's.
    pub fn scan_rows(&self) -> HashMap<(u64, u64), Known<'_>> {
        let mut out: HashMap<(u64, u64), Known<'_>> = HashMap::with_capacity(self.ledger.len());
        // The ledger's entries by inode, and of several at one inode the later object, then the rows
        // written since where they are later still.
        let mut owner: HashMap<(u64, u64), ObjectId> = HashMap::new();
        for (key, id, s) in self.ledger.by_inode() {
            if self.is_dirty(&id) {
                continue;
            }
            if let Some(o) = owner.get(&key) {
                if *o > id {
                    continue;
                }
            }
            if out.insert(key, s.known()).is_some() || self.ledger.shares_inode(&key) {
                owner.insert(key, id);
            }
        }
        for (id, row) in self.dirty_rows {
            let key = (row.fp.dev, row.fp.ino);
            let later = match owner.get(&key) {
                Some(o) => o < id,
                None => self
                    .ledger
                    .at_inode(&key)
                    .filter(|x| !self.is_dirty(x))
                    .all(|x| x < *id),
            };
            if later {
                owner.insert(key, *id);
                out.insert(key, row.known());
            }
        }
        out
    }
}

/// The journal since the cursor, by what it touches.
#[derive(Default)]
pub struct Journal {
    pub ops: Vec<Op>,
    /// Every object an operation names.
    pub touched: BTreeSet<ObjectId>,
    /// Every entry an operation gives or takes, by object.
    pub links: HashMap<ObjectId, Vec<Link>>,
    /// What operations put in each folder.
    pub into: HashMap<ObjectId, Vec<ObjectId>>,
    /// The kind of each object the journal makes.
    pub kinds: HashMap<ObjectId, ObjectKind>,
    /// Objects moved, or linked back.
    pub moved: BTreeSet<ObjectId>,
    /// The commit of every update of each object, and the objects anything but an update names.
    updates: HashMap<ObjectId, Vec<cairn_protocol::OpId>>,
    named: HashSet<ObjectId>,
}

impl Journal {
    /// Whether anything in the journal gives or takes an entry.
    pub fn moves_any(&self) -> bool {
        !self.links.is_empty()
    }

    /// Whether the journal names this object in updates alone, and if `commit`, of that commit alone.
    pub fn only_updated(&self, x: &ObjectId, commit: Option<cairn_protocol::OpId>) -> bool {
        !self.named.contains(x)
            && self
                .updates
                .get(x)
                .is_some_and(|by| commit.is_none_or(|commit| by.iter().all(|c| *c == commit)))
    }

    /// An operation of the commit `commit`.
    pub fn add_of(&mut self, op: &Op, commit: cairn_protocol::OpId) {
        if let Op::Update { object, .. } = op {
            self.updates.entry(*object).or_default().push(commit);
        }
        self.add(op);
    }

    pub fn add(&mut self, op: &Op) {
        match op {
            Op::Update { .. } => {}
            Op::Create { object, .. }
            | Op::Link { object, .. }
            | Op::Unlink { object, .. }
            | Op::Move { object, .. } => {
                self.named.insert(*object);
            }
            Op::Note { .. } => self.named.extend(op.noted()),
        }
        let mut link = |object: ObjectId, l: &Link, into: bool| {
            self.links.entry(object).or_default().push(l.clone());
            if into {
                self.into.entry(l.parent).or_default().push(object);
            }
        };
        match op {
            Op::Create {
                object,
                link: l,
                body,
                ..
            } => {
                link(*object, l, true);
                self.kinds.insert(*object, body.content.kind());
                self.touched.insert(*object);
            }
            Op::Link { object, link: l } => {
                link(*object, l, true);
                self.moved.insert(*object);
                self.touched.insert(*object);
            }
            Op::Unlink {
                object, link: l, ..
            } => {
                link(*object, l, false);
                self.touched.insert(*object);
            }
            Op::Move { object, from, to } => {
                link(*object, from, false);
                link(*object, to, true);
                self.moved.insert(*object);
                self.touched.insert(*object);
            }
            Op::Update { object, .. } => {
                self.touched.insert(*object);
            }
            Op::Note { .. } => self.touched.extend(op.noted()),
        }
        self.ops.push(op.clone());
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// The ledger's word on what a scan with a scope does not list (scan.rs `Vouch`): what the agreement found
/// there, for every folder that nothing marked as written since, nothing in the journal or a mark names,
/// and that holds nothing such.
pub struct Word<'a> {
    b: &'a Before<'a>,
    /// Not the ledger's to vouch for: written since, named by the journal or a mark, at more than one
    /// name, or at an inode another entry has.
    not_vouched: HashSet<ObjectId>,
    /// Folders whose own listing a round reads: one any of those has or had an entry in, or one the
    /// journal puts something in or takes something from.
    open_here: HashSet<ObjectId>,
    /// Those, and every folder above them.
    above: HashSet<ObjectId>,
    /// The folders to list for them, by path, and whether all under each.
    pub folders: crate::scope::Folders,
}

impl<'a> Word<'a> {
    /// `named` is what besides the journal and the marks of written since a round takes up: marks of
    /// this sync's own, names this filesystem cannot hold, and what a decision fell on.
    pub fn new(b: &'a Before<'a>, named: &BTreeSet<ObjectId>, rules: bool) -> Self {
        // A harness build can leave all of this to the rounds' widening, which must find it alone.
        if cairn_failpoint::unsupported("sync.scope.ahead") {
            return Word {
                b,
                not_vouched: HashSet::new(),
                open_here: HashSet::new(),
                above: HashSet::new(),
                folders: crate::scope::Folders::new(),
            };
        }
        let l = b.ledger;
        let mut not_vouched: HashSet<ObjectId> = HashSet::new();
        not_vouched.extend(b.dirty.iter().copied());
        not_vouched.extend(b.journal.touched.iter().copied());
        not_vouched.extend(named.iter().copied());
        for i in 0..l.len() {
            let shared = l.seen[i]
                .as_ref()
                .is_some_and(|s| l.shares_inode(&(s.fp.dev, s.fp.ino)));
            if l.link_count(i) > 1 || shared {
                not_vouched.insert(l.ids[i]);
            }
        }
        let mut open_here: HashSet<ObjectId> = HashSet::new();
        let mut whole: HashSet<ObjectId> = HashSet::new();
        for x in &not_vouched {
            for p in b.base_links(x).iter().map(|l| l.parent) {
                open_here.insert(p);
            }
            for link in l.links_of(x).unwrap_or_default() {
                open_here.insert(link.parent);
            }
            if let Some(links) = b.journal.links.get(x) {
                open_here.extend(links.iter().map(|l| l.parent));
            }
            if let Some(row) = b.dirty_rows.get(x) {
                open_here.extend(row.held.iter().flatten().map(|l| l.parent));
                if let Some(s) = &row.synced {
                    open_here.extend(s.links.iter().map(|l| l.parent));
                }
            }
            // A folder a round decides about has every name in it looked at, and one the journal moves,
            // with rules by path, everything under it.
            if b.kind(x) == Some(ObjectKind::Dir) {
                open_here.insert(*x);
                if rules && b.journal.moved.contains(x) {
                    whole.insert(*x);
                }
            }
        }
        open_here.extend(b.journal.into.keys().copied());
        open_here.insert(ObjectId::ROOT);
        let mut above: HashSet<ObjectId> = HashSet::new();
        let mut folders = crate::scope::Folders::new();
        let mut w = Word {
            b,
            not_vouched,
            open_here: HashSet::new(),
            above: HashSet::new(),
            folders: crate::scope::Folders::new(),
        };
        for p in &open_here {
            let mut at = Some(*p);
            let mut steps = 0;
            while let Some(x) = at {
                if !above.insert(x) || x == ObjectId::ROOT || steps > 4096 {
                    break;
                }
                at = w.parent_of(&x);
                steps += 1;
            }
            if let Some(path) = w.path_of(p) {
                folders.insert(path, whole.contains(p));
            }
        }
        w.open_here = open_here;
        w.above = above;
        w.folders = folders;
        w
    }

    /// The one folder a folder is in: the base's, where something wrote it since, else the ledger's.
    fn parent_of(&self, x: &ObjectId) -> Option<ObjectId> {
        let links = self.b.base_links(x);
        match links.as_slice() {
            [one] => Some(one.parent),
            _ => None,
        }
    }

    /// Where a folder is here, as the agreement had it, or as the base has it where something wrote it
    /// since: `None` if it is not here to be listed.
    fn path_of(&self, x: &ObjectId) -> Option<Vec<Name>> {
        let mut out = Vec::new();
        let mut at = *x;
        for _ in 0..4096 {
            if at == ObjectId::ROOT {
                out.reverse();
                return Some(out);
            }
            let links = match self.b.ledger.links_of(&at) {
                Some(l) if !self.b.is_dirty(&at) => l,
                _ => self.b.base_links(&at),
            };
            let [one] = links.as_slice() else {
                return None;
            };
            out.push(one.name.clone());
            at = one.parent;
        }
        None
    }

    fn vouched(&self, x: &ObjectId) -> bool {
        !self.not_vouched.contains(x) && !self.above.contains(x)
    }

    /// Where a folder is here, for a scope.
    pub fn path(&self, x: &ObjectId) -> Option<Vec<Name>> {
        self.path_of(x)
    }
}

impl crate::scan::Vouch for Word<'_> {
    fn object_at(
        &self,
        parent: ObjectId,
        name: &Name,
        st: &crate::fsops::Stat,
    ) -> Option<ObjectId> {
        let l = self.b.ledger;
        l.at_inode(&(st.dev, st.ino)).find(|x| {
            l.find(x).is_some_and(|i| {
                l.raw_links(i)
                    .any(|(p, n)| p == parent && n == name.as_bytes())
            })
        })
    }

    fn quiet(&self, folder: ObjectId, node: &mut Scanned) -> bool {
        let l = self.b.ledger;
        if self.b.is_dirty(&folder) || self.b.journal.touched.contains(&folder) {
            return false;
        }
        let Some(li) = l.find(&folder) else {
            return false;
        };
        let Some(seen) = l.seen[li].as_ref() else {
            return false;
        };
        let key = (node.fp.dev, node.fp.ino);
        if l.kinds[li] != ObjectKind::Dir
            || (seen.fp.dev, seen.fp.ino) != key
            || l.at_inode(&key).count() != 1
            || l.link_count(li) != 1
        {
            return false;
        }
        // What is under it is the agreement's, so whatever was excluded there then still is: the one
        // reading of the flags that gives the agreement's digest is the walk's.
        for (holds, only) in [(false, false), (true, false), (true, true)] {
            node.holds_excluded = holds;
            node.only_excluded = only;
            if Some(digest(node)) == seen.digest {
                return true;
            }
        }
        node.holds_excluded = false;
        node.only_excluded = false;
        false
    }

    fn takes_all_but(&self, folder: ObjectId, except: &BTreeSet<Name>) -> bool {
        if self.open_here.contains(&folder) || self.not_vouched.contains(&folder) {
            return false;
        }
        let l = self.b.ledger;
        for i in l.child_indices(&folder) {
            let x = l.ids[i];
            let named = l
                .raw_links(i)
                .filter(|(p, _)| *p == folder)
                .all(|(_, n)| except.iter().any(|e| e.as_bytes() == n));
            if !named && !self.vouched(&x) {
                return false;
            }
        }
        true
    }

    fn has_child(&self, folder: ObjectId, name: &Name) -> bool {
        self.b.ledger.children_named(&folder, name).is_some()
    }

    fn under(
        &self,
        folder: ObjectId,
        path: &[Name],
        except: &BTreeSet<Name>,
        found: &HashSet<(u64, u64)>,
        out: &mut Vec<Scanned>,
    ) {
        let l = self.b.ledger;
        let mut stack: Vec<(ObjectId, Vec<Name>, bool)> = vec![(folder, path.to_vec(), true)];
        while let Some((f, at, top)) = stack.pop() {
            for i in l.child_indices(&f) {
                let Some(seen) = l.seen[i].as_ref() else {
                    continue;
                };
                for (p, n) in l.raw_links(i) {
                    if p != f {
                        continue;
                    }
                    let Ok(name) = Name::new(n.to_vec()) else {
                        continue;
                    };
                    if top && except.contains(&name) {
                        continue;
                    }
                    if found.contains(&(seen.fp.dev, seen.fp.ino)) {
                        continue;
                    }
                    let mut p = at.clone();
                    p.push(name);
                    if l.kinds[i] == ObjectKind::Dir {
                        stack.push((l.ids[i], p.clone(), false));
                    }
                    let known = seen.known();
                    out.push(Scanned {
                        paths: vec![p],
                        fp: seen.fp.clone(),
                        racy: false,
                        content: seen.content.clone(),
                        metadata: known.scan.cloned().unwrap_or_default(),
                        metadata_incomplete: false,
                        holds_excluded: false,
                        only_excluded: false,
                    });
                }
            }
        }
    }

    fn paths_of(&self, inode: (u64, u64)) -> Vec<Vec<Name>> {
        let l = self.b.ledger;
        let mut out = Vec::new();
        for x in l.at_inode(&inode) {
            for link in l.links_of(&x).unwrap_or_default() {
                if let Some(mut p) = self.path_of(&link.parent) {
                    p.push(link.name);
                    out.push(p);
                }
            }
        }
        out
    }
}

/// Where the root stands among a scan's nodes.
const ROOT_NODE: usize = usize::MAX;

/// A scan, entry by entry, against the ledger.
pub struct Classified<'s> {
    scan: &'s ScanResult,
    /// The object each node's inode says it is.
    pub provisional: Vec<Option<ObjectId>>,
    /// Found as the ledger has it.
    pub quiet: Vec<bool>,
    pub node_of: FxMap<ObjectId, usize>,
    pub(crate) dir_at: FxMap<&'s [Name], usize>,
    local_children: FxMap<usize, Vec<usize>>,
    /// What the ledger cannot vouch for, and the folders whose every name the merge may look at.
    pub seeds: BTreeSet<ObjectId>,
    pub open: BTreeSet<ObjectId>,
    pub changed: Vec<usize>,
    /// Rewritten in place: found at the inode, kind and one entry the ledger has, which nothing has
    /// written since and the journal does not name. Such an object takes no name, so the round needs
    /// the folders above it and not what is beside it.
    pub in_place: HashSet<ObjectId>,
}

impl<'s> Classified<'s> {
    fn parent_node(&self, path: &[Name]) -> Option<usize> {
        match path.len() {
            0 => None,
            1 => Some(ROOT_NODE),
            n => self.dir_at.get(&path[..n - 1]).copied(),
        }
    }

    fn object_of_node(&self, i: usize) -> Option<ObjectId> {
        if i == ROOT_NODE {
            Some(ObjectId::ROOT)
        } else {
            self.provisional[i]
        }
    }

    /// The entries a node has, by what its folders' inodes say they are.
    fn links_of(&self, n: &Scanned) -> Option<Vec<Link>> {
        let mut out = Vec::with_capacity(n.paths.len());
        for p in &n.paths {
            let parent = self.object_of_node(self.parent_node(p)?)?;
            out.push(Link {
                parent,
                name: p.last()?.clone(),
            });
        }
        out.sort();
        out.dedup();
        Some(out)
    }

    /// What classification made of an object, for the oracle's report when it disagrees.
    pub fn explain(&self, b: &Before<'_>, id: &ObjectId, r: &Reach) -> String {
        let node = self.node_of.get(id).map(|&i| {
            let n = &self.scan.nodes[i];
            let digest = (!matches!(n.content, Content::File { .. })).then(|| digest(n));
            format!(
                "node {} at {:?}, quiet {}, fp {:?}, racy {}, digest {:?}, links {:?}",
                i,
                n.paths
                    .iter()
                    .map(|p| crate::scan::display_path(p))
                    .collect::<Vec<_>>(),
                self.quiet[i],
                n.fp,
                n.racy,
                digest,
                self.links_of(n)
            )
        });
        let entry = b.ledger.entry(id);
        format!(
            "{id}: in the set {} (decided {}), dirty {}, named by the journal {}, ledger {:?}, {}",
            r.all.contains(id),
            r.core.contains(id),
            b.is_dirty(id),
            b.journal.touched.contains(id),
            entry,
            node.unwrap_or_else(|| "no node by inode".into())
        )
    }

    /// The object of the folder a path is in, by its inode, as far as the scan saw it.
    pub fn folder_of(&self, path: &[Name]) -> Option<ObjectId> {
        self.object_of_node(self.parent_node(path)?)
    }

    /// Not to be taken as the ledger has it, whatever the ledger says: a mark names its inode.
    pub fn unquiet(&mut self, i: usize) {
        if !self.quiet[i] {
            return;
        }
        self.quiet[i] = false;
        self.changed.push(i);
        if let Some(x) = self.provisional[i] {
            self.seeds.insert(x);
            self.in_place.remove(&x);
        }
        for p in &self.scan.nodes[i].paths {
            if let Some(o) = self.parent_node(p).and_then(|j| self.object_of_node(j)) {
                self.open.insert(o);
            }
        }
    }

    /// What the scan found as the ledger has it and a round leaves out: live on both sides, and here.
    pub fn quiet_outside(&self, r: &Reach) -> usize {
        (0..self.quiet.len())
            .filter(|i| self.quiet[*i])
            .filter(|i| self.provisional[*i].is_some_and(|x| !r.all.contains(&x)))
            .count()
    }

    /// Why a round cannot go on with what it classified: a folder, or anything it found as the ledger
    /// has it, that identification took for another object than its inode said.
    pub fn misidentified(
        &self,
        r: &Reach,
        identified: &crate::identify::Identified,
    ) -> Option<String> {
        for &i in &r.nodes {
            let n = &self.scan.nodes[i];
            let Some(p) = self.provisional[i] else {
                continue;
            };
            if !self.quiet[i] && n.content != Content::Dir {
                continue;
            }
            let same = identified
                .scanned
                .get(&p)
                .is_some_and(|s| (s.fp.dev, s.fp.ino) == (n.fp.dev, n.fp.ino));
            if !same {
                return Some(format!(
                    "{} was not identified as the object its inode says",
                    n.paths
                        .first()
                        .map(|p| crate::scan::display_path(p))
                        .unwrap_or_default()
                ));
            }
        }
        None
    }
}

/// Every object besides those a round decides about that its decisions touch: if the reach was too
/// small for them, the round decides again with these taken in.
pub fn decided_outside(
    reach: &Reach,
    visible: &Tree,
    before_twins: &BTreeMap<ObjectId, LNode>,
    identified: &crate::identify::Identified,
    plan: &crate::merge::Plan,
    loaded: &BTreeMap<ObjectId, Row>,
    rows: &BTreeMap<ObjectId, Row>,
) -> BTreeSet<ObjectId> {
    let mut out = BTreeSet::new();
    // What is in neither was made new this round, from what the scan did not account for.
    let mut mark = |x: ObjectId| {
        if reach.all.contains(&x) && !reach.core.contains(&x) {
            out.insert(x);
        }
    };
    for (a, b) in &plan.adopted {
        mark(*a);
        mark(*b);
    }
    for c in &plan.copies {
        mark(c.object);
        mark(c.copy);
    }
    for r in &plan.reports {
        mark(r.object);
    }
    for (o, _) in &plan.notes {
        mark(*o);
    }
    for (id, t) in &plan.target {
        let r = visible.get_live(*id);
        let l = plan.local.get(id);
        let quiet = r.is_some_and(|r| r.links == t.links && r.body == t.body)
            && l.is_some_and(|l| {
                l.links == t.links
                    && l.content == t.body.content
                    && l.apply_from == t.body.metadata
                    && !l.carried
            });
        if !quiet {
            mark(*id);
        }
    }
    for (id, _) in visible.live() {
        if !plan.target.contains_key(&id) {
            mark(id);
        }
    }
    for id in plan.local.keys().chain(identified.local.keys()) {
        if !plan.target.contains_key(id) {
            mark(*id);
        }
    }
    for (id, l) in before_twins {
        if identified.local.get(id) != Some(l) {
            mark(*id);
        }
    }
    for id in loaded.keys().chain(rows.keys()) {
        if loaded.get(id) != rows.get(id) {
            mark(*id);
        }
    }
    out.remove(&ObjectId::ROOT);
    out
}

/// Every node against the ledger. Nothing here decides anything: a node is quiet only if the full sync
/// would find in it what it found at the agreement, and everything else is for the round to look at.
pub fn classify<'s>(b: &Before<'_>, scan: &'s ScanResult) -> Classified<'s> {
    let nodes = &scan.nodes;
    let mut dir_at: FxMap<&'s [Name], usize> = FxMap::default();
    for (i, n) in nodes.iter().enumerate() {
        if n.content == Content::Dir {
            for p in &n.paths {
                dir_at.insert(p.as_slice(), i);
            }
        }
    }
    let mut dirty_by_inode: HashMap<(u64, u64), Vec<ObjectId>> = HashMap::new();
    for (id, row) in b.dirty_rows {
        dirty_by_inode
            .entry((row.fp.dev, row.fp.ino))
            .or_default()
            .push(*id);
    }

    // By inode, as identification goes first: of the rows at it of the same kind and birth, the one whose
    // whole fingerprint holds, the later object on a tie.
    let provisional: Vec<Option<ObjectId>> = par_map(nodes.len(), |i| {
        let n = &nodes[i];
        let key = (n.fp.dev, n.fp.ino);
        let mut best: Option<(bool, ObjectId)> = None;
        let mut consider = |id: ObjectId, fp: &Fingerprint, kind: ObjectKind| {
            if kind == n.content.kind() && birth_mark(fp.btime) == birth_mark(n.fp.btime) {
                let here = (fp.current() == n.fp.current(), id);
                if best.is_none_or(|b| here > b) {
                    best = Some(here);
                }
            }
        };
        for x in b.ledger.at_inode(&key).filter(|x| !b.is_dirty(x)) {
            if let Some(s) = b.ledger.seen_of(&x) {
                consider(x, &s.fp, s.content.kind());
            }
        }
        for y in dirty_by_inode.get(&key).into_iter().flatten() {
            let row = &b.dirty_rows[y];
            consider(*y, &row.fp, row.content.kind());
        }
        best.map(|(_, id)| id)
    });

    let mut c = Classified {
        scan,
        provisional,
        quiet: Vec::new(),
        node_of: FxMap::default(),
        dir_at,
        local_children: FxMap::default(),
        seeds: BTreeSet::new(),
        open: BTreeSet::new(),
        changed: Vec::new(),
        in_place: HashSet::new(),
    };
    c.node_of.reserve(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        if let Some(x) = c.provisional[i] {
            c.node_of.insert(x, i);
        }
        for p in &n.paths {
            if let Some(parent) = c.parent_node(p) {
                c.local_children.entry(parent).or_default().push(i);
            }
        }
    }

    // Nodes taken from the agreement were not read: their digests are the agreement's.
    let read = scan.vouched_from.min(nodes.len());
    let mut digests = digests(&nodes[..read]);
    digests.resize(nodes.len(), None);
    // A harness build can take a file of one name as the ledger has it, whatever the scan found, as a
    // defect in what follows would: for the tests of the daemon's check on the incremental path.
    let blind: Option<String> = cfg!(feature = "failpoints")
        .then(|| std::env::var("CAIRN_FP_INCR_BLIND").ok())
        .flatten();
    let blind = |n: &Scanned| {
        blind.as_ref().is_some_and(|b| {
            n.paths
                .iter()
                .any(|p| p.last().is_some_and(|l| l.as_bytes() == b.as_bytes()))
        })
    };
    // Each node, the ledger's entry it is quietly, if it is.
    let quiet: Vec<Option<usize>> = par_map(nodes.len(), |i| {
        let n = &nodes[i];
        let x = c.provisional[i]?;
        if b.is_dirty(&x) || b.journal.touched.contains(&x) {
            return None;
        }
        let li = b.ledger.find(&x)?;
        let seen = b.ledger.seen[li].as_ref()?;
        let key = (n.fp.dev, n.fp.ino);
        if dirty_by_inode.contains_key(&key)
            // Two entries of the ledger at one inode say nothing about either.
            || b.ledger.at_inode(&key).filter(|y| !b.is_dirty(y)).count() != 1
            || b.ledger.kinds[li] != n.content.kind()
            || (seen.fp.dev, seen.fp.ino) != key
        {
            return None;
        }
        let same_links = if n.paths.len() == 1 {
            let p = &n.paths[0];
            let parent = c.object_of_node(c.parent_node(p)?)?;
            let name = p.last()?.as_bytes();
            let mut mine = b.ledger.raw_links(li);
            mine.next() == Some((parent, name)) && mine.next().is_none()
        } else {
            b.ledger.links_are(&x, &c.links_of(n)?)
        };
        if !same_links {
            return None;
        }
        let same = match n.content {
            Content::File { .. } => seen.known().vouches_for(&n.fp) && n.content == seen.content,
            _ if scan.vouched(i) => true,
            // The digest holds the fingerprint too.
            _ => digests[i].is_some() && digests[i] == seen.digest,
        };
        (same || blind(n)).then_some(li)
    });
    let mut found = vec![false; b.ledger.len()];
    for li in quiet.iter().flatten() {
        found[*li] = true;
    }
    c.quiet = quiet.iter().map(Option::is_some).collect();
    let in_place: Vec<bool> = par_map(nodes.len(), |i| {
        !c.quiet[i]
            && c.provisional[i].is_some_and(|x| {
                rewritten_in_place(b, &c, &nodes[i], digests[i], x, &dirty_by_inode)
            })
    });
    c.in_place = (0..nodes.len())
        .filter(|i| in_place[*i])
        .filter_map(|i| c.provisional[i])
        .collect();
    for (li, seen) in b.ledger.seen.iter().enumerate() {
        if seen.is_some() && !found[li] {
            c.seeds.insert(b.ledger.ids[li]);
        }
    }
    c.seeds.extend(b.dirty.iter().copied());
    c.seeds.extend(b.journal.touched.iter().copied());
    for (i, n) in nodes.iter().enumerate() {
        if c.quiet[i] {
            continue;
        }
        c.changed.push(i);
        if let Some(x) = c.provisional[i] {
            c.seeds.insert(x);
            if c.in_place.contains(&x) {
                continue;
            }
        }
        for p in &n.paths {
            if let Some(o) = c.parent_node(p).and_then(|j| c.object_of_node(j)) {
                c.open.insert(o);
            }
        }
    }
    c
}

/// Whether a node that is not quiet is its object rewritten in place, here or elsewhere: at the inode,
/// kind and one entry the base has, with nothing that could give it a name, a conflict copy above all.
/// So not written since its ledger entry, and either not named by the journal or named only in updates
/// of it while it is as it was here; or written since for what this sync pushed of it or brought here,
/// which its row holds as shared with the server and the scan finds as the row has it, with the journal
/// naming it in updates alone, and of the commit that pushed it, if one did.
fn rewritten_in_place(
    b: &Before<'_>,
    c: &Classified<'_>,
    n: &Scanned,
    digest: Option<Hash>,
    x: ObjectId,
    dirty_by_inode: &HashMap<(u64, u64), Vec<ObjectId>>,
) -> bool {
    let [path] = n.paths.as_slice() else {
        return false;
    };
    let (Some(parent), Some(name)) = (
        c.parent_node(path).and_then(|j| c.object_of_node(j)),
        path.last(),
    ) else {
        return false;
    };
    let link = Link {
        parent,
        name: name.clone(),
    };
    let key = (n.fp.dev, n.fp.ino);
    let ledger_at = b.ledger.at_inode(&key).filter(|y| !b.is_dirty(y)).count();
    if !b.is_dirty(&x) {
        let Some(li) = b.ledger.find(&x) else {
            return false;
        };
        let Some(seen) = &b.ledger.seen[li] else {
            return false;
        };
        let as_it_was = || match n.content {
            Content::File { .. } => seen.known().vouches_for(&n.fp) && n.content == seen.content,
            _ => digest.is_some() && digest == seen.digest,
        };
        return (!b.journal.touched.contains(&x)
            || (b.journal.only_updated(&x, None) && as_it_was()))
            && !dirty_by_inode.contains_key(&key)
            && ledger_at == 1
            && b.ledger.kinds[li] == n.content.kind()
            && (seen.fp.dev, seen.fp.ino) == key
            && b.ledger.links_are(&x, std::slice::from_ref(&link));
    }
    let (Some(node), Some(row)) = (b.dirty_nodes.get(&x), b.dirty_rows.get(&x)) else {
        return false;
    };
    let Some(synced) = &row.synced else {
        return false;
    };
    let one = |links: &BTreeSet<Link>| links.len() == 1 && links.contains(&link);
    let as_the_row_has_it = matches!(n.content, Content::File { .. })
        && row.known().vouches_for(&n.fp)
        && n.content == row.content;
    b.journal.only_updated(&x, synced.commit)
        && as_the_row_has_it
        && synced.body.content == row.content
        && !synced.unapplied
        && synced.folder_time.is_none()
        && one(&node.links)
        && one(&synced.links)
        && row.held.as_ref().is_none_or(one)
        && node.kind() == n.content.kind()
        && row.content.kind() == n.content.kind()
        && synced.body.content.kind() == n.content.kind()
        && (row.fp.dev, row.fp.ino) == key
        && ledger_at == 0
        && dirty_by_inode
            .get(&key)
            .is_some_and(|at| at.as_slice() == [x])
}

/// `f` of every index below `n`, in order, on as many threads as there are.
pub(crate) fn par_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 8);
    let piece = n.div_ceil(threads).max(1);
    let f = &f;
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..n)
            .step_by(piece)
            .map(|start| {
                s.spawn(move || (start..(start + piece).min(n)).map(f).collect::<Vec<T>>())
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}

/// A cheap hasher for the maps a round builds over the whole scan, of what is on this disk: nobody
/// chooses those keys against it.
#[derive(Default, Clone, Copy)]
pub struct Fx(u64);

impl std::hash::Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

pub type FxMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<Fx>>;

/// Where the scan found an inode it had found at another place already. Only something moving while the
/// walk goes does that, from a folder walked to one not yet: the full sync takes the first place for the
/// object and the second for something new, while the part a round loads keeps one node an inode.
pub fn inode_twice(scan: &ScanResult) -> Option<String> {
    let mut seen: std::collections::HashSet<(u64, u64), std::hash::BuildHasherDefault<Fx>> =
        std::collections::HashSet::with_capacity_and_hasher(scan.nodes.len(), Default::default());
    scan.nodes
        .iter()
        .find(|n| !seen.insert((n.fp.dev, n.fp.ino)))
        .map(|n| {
            n.paths
                .first()
                .map_or_else(String::new, |p| crate::scan::display_path(p))
        })
}

/// The digest of every node but a file's, on as many threads as there are.
pub(crate) fn digests(nodes: &[Scanned]) -> Vec<Option<Hash>> {
    const PIECE: usize = 16_384;
    let pieces: Vec<&[Scanned]> = nodes.chunks(PIECE).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 8)
        .min(pieces.len().max(1));
    let mut out: Vec<(usize, Vec<Option<Hash>>)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(piece) = pieces.get(i) else {
                            return done;
                        };
                        let hashes = piece
                            .iter()
                            .map(|n| {
                                (!matches!(n.content, Content::File { .. })).then(|| digest(n))
                            })
                            .collect();
                        done.push((i, hashes));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().flat_map(|(_, h)| h).collect()
}

/// The objects a round decides about, and those it loads only for them: everything the merge looks at
/// besides each object itself.
#[derive(Default)]
pub struct Reach {
    /// Decided about here: everything the ledger cannot vouch for, and what a decision fell on.
    pub core: BTreeSet<ObjectId>,
    /// Those, and their neighbours.
    pub all: BTreeSet<ObjectId>,
    /// The scan's nodes for them, and every node the ledger does not account for.
    pub nodes: BTreeSet<usize>,
    /// Every folder whose every name the round looks at.
    pub opened: HashSet<ObjectId>,
    above: HashSet<ObjectId>,
    core_work: Vec<ObjectId>,
    work: Vec<ObjectId>,
}

impl Reach {
    fn add(&mut self, x: ObjectId) {
        if self.all.insert(x) {
            self.work.push(x);
        }
    }

    fn add_core(&mut self, x: ObjectId) {
        self.add(x);
        if self.core.insert(x) {
            self.core_work.push(x);
        }
    }
}

struct Closing<'a, 's> {
    b: &'a Before<'a>,
    c: &'a Classified<'s>,
    /// The base's objects in each folder, for the objects the ledger no longer speaks for.
    dirty_children: HashMap<ObjectId, Vec<ObjectId>>,
    /// Whether rules by path decide what is in view, so that a folder the journal moves can take
    /// everything in it out of view, or bring it in.
    rules: bool,
    /// Decided about here for what a decision fell on or a mark names, whatever the scan found.
    also: &'a BTreeSet<ObjectId>,
    r: Reach,
}

impl Closing<'_, '_> {
    /// Every folder an object has or had an entry in, on any side.
    fn parents(&self, x: &ObjectId) -> Vec<ObjectId> {
        let mut out: Vec<ObjectId> = self.b.base_links(x).into_iter().map(|l| l.parent).collect();
        if let Some(links) = self.b.journal.links.get(x) {
            out.extend(links.iter().map(|l| l.parent));
        }
        if let Some(row) = self.b.dirty_rows.get(x) {
            out.extend(row.held.iter().flatten().map(|l| l.parent));
            if let Some(s) = &row.synced {
                out.extend(s.links.iter().map(|l| l.parent));
            }
        }
        if let Some(&i) = self.c.node_of.get(x) {
            for p in &self.c.scan.nodes[i].paths {
                if let Some(o) = self.c.parent_node(p).and_then(|j| self.c.object_of_node(j)) {
                    out.push(o);
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    fn base_children(&self, p: &ObjectId) -> Vec<ObjectId> {
        let mut out: Vec<ObjectId> = self
            .b
            .ledger
            .children_of(p)
            .filter(|c| !self.b.is_dirty(c))
            .collect();
        out.extend(self.dirty_children.get(p).into_iter().flatten().copied());
        out
    }

    /// A node and the folders above it, as the scan found them.
    fn include(&mut self, i: usize) {
        let mut stack = vec![i];
        while let Some(i) = stack.pop() {
            if i == ROOT_NODE || !self.r.nodes.insert(i) {
                continue;
            }
            if let Some(x) = self.c.provisional[i] {
                self.r.add(x);
            }
            for p in &self.c.scan.nodes[i].paths {
                if let Some(j) = self.c.parent_node(p) {
                    stack.push(j);
                }
            }
        }
    }

    fn ancestors(&mut self, p: ObjectId) {
        let mut stack = vec![p];
        while let Some(p) = stack.pop() {
            if !self.r.above.insert(p) {
                continue;
            }
            self.r.add(p);
            stack.extend(self.parents(&p));
        }
    }

    /// Every name in a folder, on every side.
    fn open(&mut self, p: ObjectId) {
        if !self.r.opened.insert(p) {
            return;
        }
        self.r.add(p);
        for c in self.base_children(&p) {
            self.r.add(c);
        }
        for c in self.b.journal.into.get(&p).cloned().into_iter().flatten() {
            self.r.add(c);
        }
        let node = if p == ObjectId::ROOT {
            Some(ROOT_NODE)
        } else {
            self.c.node_of.get(&p).copied()
        };
        if let Some(node) = node {
            for i in self
                .c
                .local_children
                .get(&node)
                .cloned()
                .into_iter()
                .flatten()
            {
                self.include(i);
            }
        }
    }

    /// Everything under an object in the base, decided about here.
    fn subtree_core(&mut self, x: ObjectId) {
        let mut stack = vec![x];
        let mut seen = HashSet::new();
        while let Some(x) = stack.pop() {
            if !seen.insert(x) {
                continue;
            }
            self.r.add_core(x);
            stack.extend(self.base_children(&x));
        }
    }

    fn run(&mut self) {
        loop {
            if let Some(x) = self.r.work.pop() {
                for p in self.parents(&x) {
                    self.ancestors(p);
                }
                if let Some(&i) = self.c.node_of.get(&x) {
                    self.include(i);
                }
                continue;
            }
            if let Some(x) = self.r.core_work.pop() {
                // A rewrite in place takes no name: the folders above it are in the closure already.
                if !self.c.in_place.contains(&x) || self.also.contains(&x) {
                    for p in self.parents(&x) {
                        self.open(p);
                    }
                }
                if self.b.kind(&x) == Some(ObjectKind::Dir) || self.node_is_dir(&x) {
                    self.open(x);
                    if self.rules && self.b.journal.moved.contains(&x) {
                        self.subtree_core(x);
                    }
                }
                continue;
            }
            return;
        }
    }

    fn node_is_dir(&self, x: &ObjectId) -> bool {
        self.c
            .node_of
            .get(x)
            .is_some_and(|i| self.c.scan.nodes[*i].content == Content::Dir)
    }
}

/// The objects a round must decide about, from what changed and what a decision fell on before.
pub fn reach(
    b: &Before<'_>,
    c: &Classified<'_>,
    rules: bool,
    also: &BTreeSet<ObjectId>,
    beside: &BTreeSet<ObjectId>,
) -> Reach {
    let mut dirty_children: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for (id, n) in b.dirty_nodes {
        for l in &n.links {
            dirty_children.entry(l.parent).or_default().push(*id);
        }
    }
    let mut k = Closing {
        b,
        c,
        dirty_children,
        rules,
        also,
        r: Reach::default(),
    };
    k.r.add_core(ObjectId::ROOT);
    for x in c.seeds.iter().chain(also) {
        k.r.add_core(*x);
    }
    for x in beside {
        k.r.add(*x);
    }
    for p in &c.open {
        k.open(*p);
    }
    for links in b.journal.links.values() {
        for l in links {
            k.open(l.parent);
        }
    }
    for &i in &c.changed {
        k.include(i);
    }
    // What the scan could not see stands for what was there, whole.
    let scan = c.scan;
    for path in &scan.unobserved {
        let Some((name, _)) = path.split_last() else {
            continue;
        };
        let Some(parent) = c.parent_node(path).and_then(|j| c.object_of_node(j)) else {
            continue;
        };
        for x in k.base_children(&parent) {
            if b.base_links(&x)
                .iter()
                .any(|l| l.parent == parent && &l.name == name)
            {
                k.subtree_core(x);
            }
        }
    }
    for inode in &scan.unobserved_inodes {
        let at: Vec<ObjectId> = b.ledger.at_inode(inode).collect();
        for x in at {
            k.subtree_core(x);
        }
        for (y, row) in b.dirty_rows {
            if (row.fp.dev, row.fp.ino) == *inode {
                k.subtree_core(*y);
            }
        }
    }
    for path in &scan.unlisted {
        let node = if path.is_empty() {
            Some(ROOT_NODE)
        } else {
            c.dir_at.get(path.as_slice()).copied()
        };
        if let Some(x) = node.and_then(|j| c.object_of_node(j)) {
            k.subtree_core(x);
        }
    }
    k.run();
    k.r
}

/// The part of the tree a round works on: the base, the server's tree, the rows and the scan, each
/// holding only what the reach does.
pub struct World {
    pub base: Tree,
    pub remote: Tree,
    pub rows: BTreeMap<ObjectId, Row>,
    pub scan: ScanResult,
}

/// Which row a scan's map of rows gives for an inode, when it was given without the metadata: the
/// ledger's, or with every row glanced at, whichever row it was.
fn ledger_source(b: &Before<'_>, key: (u64, u64)) -> Option<ObjectId> {
    let ledger = b.ledger.at_inode(&key).filter(|x| !b.is_dirty(x)).max();
    let dirty = b
        .dirty_rows
        .iter()
        .filter(|(id, r)| (r.fp.dev, r.fp.ino) == key && !b.unscanned.contains(id))
        .map(|(id, _)| *id)
        .max();
    match (ledger, dirty) {
        (Some(x), Some(y)) if y > x => b.glanced.then_some(y),
        (None, y) if b.glanced => y,
        (x, _) => x,
    }
}

/// A file the scan did not read was given its row's metadata, which a ledger entry or a glance does not
/// carry: this puts it back, for every such node in `nodes`, from `rows`.
pub fn fill_unread(
    b: &Before<'_>,
    nodes: &mut [Scanned],
    rows: &BTreeMap<ObjectId, Row>,
) -> Result<()> {
    for n in nodes {
        if !matches!(n.content, Content::File { .. }) {
            continue;
        }
        let Some(x) = ledger_source(b, (n.fp.dev, n.fp.ino)) else {
            continue;
        };
        let vouched = match b.dirty_rows.get(&x).filter(|_| b.is_dirty(&x)) {
            Some(row) => row.known().vouches_for(&n.fp),
            None => b
                .ledger
                .seen_of(&x)
                .is_some_and(|seen| seen.known().vouches_for(&n.fp)),
        };
        if !vouched {
            continue;
        }
        let Some(row) = rows.get(&x) else {
            anyhow::bail!("the row of {x}, which the scan stood for, is not in the state");
        };
        n.metadata = row.scan.clone();
    }
    Ok(())
}

/// Why a round goes the full way instead, when it does.
pub type Decline = String;

/// The trees, rows and scan of the reach, from the state, the journal and the scan.
pub fn world(
    root: &Root,
    b: &Before<'_>,
    scan: &ScanResult,
    r: &Reach,
) -> Result<std::result::Result<World, Decline>> {
    let want: Vec<ObjectId> = r.all.iter().filter(|x| !b.is_dirty(x)).copied().collect();
    let mut nodes = root.state.nodes_of(&want)?;
    let mut rows = root.state.rows_of(&want)?;
    for x in r.all.iter().filter(|x| b.is_dirty(x)) {
        if let Some(n) = b.dirty_nodes.get(x) {
            nodes.insert(*x, n.clone());
        }
        if let Some(row) = b.dirty_rows.get(x) {
            rows.insert(*x, row.clone());
        }
    }
    let base = Tree::from_nodes(nodes);
    let mut remote = base.clone();
    for op in &b.journal.ops {
        if let Err(e) = remote.apply(op) {
            return Ok(Err(format!(
                "the journal since the cursor does not replay on the part of the tree loaded for it ({e})"
            )));
        }
    }
    let mut kept: Vec<Scanned> = r.nodes.iter().map(|i| scan.nodes[*i].clone()).collect();
    fill_unread(b, &mut kept, &rows)?;
    Ok(Ok(World {
        base,
        remote,
        rows,
        scan: scan.with_nodes(kept),
    }))
}

/// What an agreement says of one object, for the ledger.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Agreed, as this entry says.
    Put(Entry),
    /// Gone from the server and from here: nothing to remember.
    Drop,
    /// Not something an agreement can vouch for: every round takes it up until one can.
    Dirty,
}

/// What a finalizing round knows of each object it held, for the ledger.
pub struct Agreement<'a> {
    /// The server's tree, which becomes the base.
    pub remote: &'a Tree,
    /// Live on the server and out of view here.
    pub hidden: &'a BTreeSet<ObjectId>,
    /// As finalize leaves them.
    pub rows: &'a BTreeMap<ObjectId, Row>,
    pub local: &'a BTreeMap<ObjectId, LNode>,
    /// What this round's scan read of each object it found, besides a file's row: `None` for a file.
    pub seen: &'a HashMap<ObjectId, Option<Hash>>,
}

impl Agreement<'_> {
    pub fn verdict(&self, id: ObjectId) -> Verdict {
        if id == ObjectId::ROOT {
            // Always among what a round decides about.
            return Verdict::Drop;
        }
        let row = self.rows.get(&id);
        let Some(n) = self.remote.get_live(id) else {
            return if row.is_some() {
                Verdict::Dirty
            } else {
                Verdict::Drop
            };
        };
        let links: Vec<Link> = n.links.iter().cloned().collect();
        let local = self.local.get(&id);
        if let (Some(row), Some(l), Some(digest)) = (row, local, self.seen.get(&id)) {
            let is_file = matches!(row.content, Content::File { .. });
            let settled = !row.racy
                && !row.incomplete
                && row.synced.is_none()
                && row.applied.as_ref() == Some(&row.scan)
                && row.held.as_ref().is_none_or(|h| *h == n.links)
                && !l.carried
                && l.links == n.links
                && row.content.kind() == n.kind()
                && digest.is_some() != is_file;
            if !settled {
                return Verdict::Dirty;
            }
            return Verdict::Put(Entry {
                kind: n.kind(),
                links,
                seen: Some(Seen {
                    fp: row.fp.clone(),
                    content: row.content.clone(),
                    digest: *digest,
                }),
            });
        }
        if row.is_none() && local.is_none() && self.hidden.contains(&id) {
            return Verdict::Put(Entry {
                kind: n.kind(),
                links,
                seen: None,
            });
        }
        Verdict::Dirty
    }
}

/// What this round's scan read of each object it found, for the ledger: a digest of anything but a file.
pub fn seen_of(scanned: &BTreeMap<ObjectId, Scanned>) -> HashMap<ObjectId, Option<Hash>> {
    let nodes: Vec<(&ObjectId, &Scanned)> = scanned.iter().collect();
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 8);
    let piece = nodes.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let workers: Vec<_> = nodes
            .chunks(piece)
            .map(|chunk| {
                s.spawn(move || {
                    chunk
                        .iter()
                        .map(|(id, n)| {
                            let d = (!matches!(n.content, Content::File { .. })).then(|| digest(n));
                            (**id, d)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}

/// The ledger after a finalize: whole after a full round, or for what an incremental one held, or,
/// with a ledger rebuilt from the state, whole: the rebuilt entries for what the round did not hold and
/// the round's own for what it did.
pub fn ledger_write<'a>(
    agreement: &Agreement<'_>,
    held: Option<(&'a Session, &'a BTreeSet<ObjectId>, &BTreeSet<ObjectId>)>,
    anchor: &Anchor,
) -> Result<crate::state::LedgerWrite<'a>> {
    let rebuilt = held.is_some_and(|(s, _, _)| s.rebuilt.is_some());
    let mut w = crate::state::LedgerWrite {
        whole: held.is_none() || rebuilt,
        anchor: Some(serde_json::to_string(anchor)?),
        ..Default::default()
    };
    if let (Some((s, all, _)), true) = (held, rebuilt) {
        w.rest = Some(Box::new(move |put| {
            for i in 0..s.ledger.len() {
                let id = s.ledger.ids[i];
                if all.contains(&id) || s.unsettled.contains(&id) {
                    continue;
                }
                let e = Entry {
                    kind: s.ledger.kinds[i],
                    links: s.ledger.links_at(i),
                    seen: s.ledger.seen[i].clone(),
                };
                put(id, e.encode())?;
            }
            Ok(())
        }));
    }
    let ids: BTreeSet<ObjectId> = match held {
        Some((_, all, _)) => all
            .iter()
            .copied()
            .chain(agreement.rows.keys().copied())
            .collect(),
        None => agreement
            .remote
            .live()
            .map(|(id, _)| id)
            .chain(agreement.rows.keys().copied())
            .collect(),
    };
    for id in ids {
        let verdict = agreement.verdict(id);
        match (verdict, held) {
            (Verdict::Put(e), None) => w.put.push((id, e.encode())),
            (Verdict::Drop, None) => {}
            (Verdict::Dirty, None) => w.dirty.push(id),
            (Verdict::Put(e), Some(_)) if rebuilt => w.put.push((id, e.encode())),
            (Verdict::Drop, Some(_)) if rebuilt => {}
            (Verdict::Dirty, Some(_)) if rebuilt => w.dirty.push(id),
            (Verdict::Put(e), Some((s, _, dirty))) => {
                if dirty.contains(&id) || s.ledger.entry(&id).as_ref() != Some(&e) {
                    w.put.push((id, e.encode()));
                }
                w.clean.push(id);
            }
            (Verdict::Drop, Some((s, _, _))) => {
                if s.ledger.contains(&id) {
                    w.drop.push(id);
                }
                w.clean.push(id);
            }
            (Verdict::Dirty, Some(_)) => w.dirty.push(id),
        }
    }
    Ok(w)
}

/// New identities for what a round makes: random, or in a harness build drawn from one seed per round,
/// so that the round and the oracle's full one beside it name the same new things alike.
#[derive(Clone, Copy)]
pub struct Ids {
    seed: Option<[u8; 16]>,
}

impl Ids {
    pub fn for_round() -> Self {
        Self {
            seed: oracle().then(rand::random),
        }
    }

    /// For something new the scan found, by its inode and first name.
    pub fn of_scanned(&self, n: &Scanned) -> ObjectId {
        let Some(seed) = self.seed else {
            return ObjectId::random();
        };
        let mut h = blake3::Hasher::new();
        h.update(b"cairn new object");
        h.update(&seed);
        h.update(&n.fp.dev.to_le_bytes());
        h.update(&n.fp.ino.to_le_bytes());
        for p in n.paths.iter().take(1) {
            for name in p {
                h.update(&(name.as_bytes().len() as u64).to_le_bytes());
                h.update(name.as_bytes());
            }
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&h.finalize().as_bytes()[..16]);
        ObjectId(id)
    }

    /// For what a merge makes, in the order it makes them.
    pub fn counter(&self) -> impl FnMut() -> ObjectId {
        let seed = self.seed;
        let mut n = 0u64;
        move || {
            let Some(seed) = seed else {
                return ObjectId::random();
            };
            n += 1;
            let mut h = blake3::Hasher::new();
            h.update(b"cairn merged object");
            h.update(&seed);
            h.update(&n.to_le_bytes());
            let mut id = [0u8; 16];
            id.copy_from_slice(&h.finalize().as_bytes()[..16]);
            ObjectId(id)
        }
    }
}

/// In a harness build, every incremental round is decided the full way as well, on the same scan,
/// journal and state, and the two must agree; `CAIRN_FP_INCR_ORACLE=0` turns that off, to see the
/// incremental path as a release build runs it.
pub fn oracle() -> bool {
    cfg!(feature = "failpoints") && std::env::var("CAIRN_FP_INCR_ORACLE").map_or(true, |v| v != "0")
}

/// Whether the incremental path is to be tried at all: `CAIRN_FP_INCR=0` in a harness build turns it
/// off, for tests of the full path alone.
pub fn enabled() -> bool {
    !cfg!(feature = "failpoints") || std::env::var("CAIRN_FP_INCR").map_or(true, |v| v != "0")
}

/// What a round decided before it acted, in the terms the oracle compares.
pub struct Decisions {
    pub local: BTreeMap<ObjectId, LNode>,
    pub shared: BTreeMap<ObjectId, crate::merge::Shared>,
    /// The rows as the round leaves them before the apply: the view's and the scan's.
    pub rows: BTreeMap<ObjectId, Row>,
    pub vanished: Option<crate::sync::Hold>,
    pub twins: Vec<cairn_protocol::Skip>,
    pub target: BTreeMap<ObjectId, crate::merge::TNode>,
    pub copies: Vec<crate::merge::ConflictCopy>,
    pub adopted: BTreeMap<ObjectId, ObjectId>,
    pub reports: Vec<crate::merge::Report>,
    pub notes: Vec<(ObjectId, String)>,
    pub ops: Vec<Op>,
    pub steps: Vec<crate::schedule::TopoOp>,
    pub moving: BTreeMap<Vec<Name>, Vec<Name>>,
    pub reserved: BTreeSet<Link>,
}

/// An operation without what is random in it.
fn plain(op: &Op) -> Op {
    let mut op = op.clone();
    match &mut op {
        Op::Create { revision, .. } | Op::Update { revision, .. } => {
            *revision = cairn_protocol::RevisionId::ZERO;
        }
        _ => {}
    }
    op
}

/// The incremental round's decisions against the full sync's on the same scan, journal and state: the
/// same about every object it held, and nothing at all about the rest.
pub fn compare(
    incr: &Decisions,
    full: &Decisions,
    held: &BTreeSet<ObjectId>,
    before: &BTreeMap<ObjectId, Row>,
    remote: &Tree,
) -> std::result::Result<(), String> {
    let mut diffs: Vec<String> = Vec::new();
    let mut note = |what: &str, id: Option<&ObjectId>, a: String, b: String| {
        if diffs.len() < 20 {
            let at = id.map_or_else(String::new, |i| format!(" {i}"));
            diffs.push(format!("{what}{at}:\n  incremental {a}\n  full        {b}"));
        }
    };
    fn part<'m, V>(
        m: &'m BTreeMap<ObjectId, V>,
        held: &BTreeSet<ObjectId>,
    ) -> BTreeMap<&'m ObjectId, &'m V> {
        m.iter().filter(|(k, _)| held.contains(k)).collect()
    }
    let sorted = |r: &[crate::merge::Report]| {
        let mut r = r.to_vec();
        r.sort();
        format!("{r:?}")
    };
    let ops = |o: &[Op]| format!("{:?}", o.iter().map(plain).collect::<Vec<_>>());
    let pairs = [
        (
            "identified",
            format!("{:?}", part(&incr.local, held)),
            format!("{:?}", part(&full.local, held)),
        ),
        (
            "shared",
            format!("{:?}", part(&incr.shared, held)),
            format!("{:?}", part(&full.shared, held)),
        ),
        (
            "rows",
            format!("{:?}", part(&incr.rows, held)),
            format!("{:?}", part(&full.rows, held)),
        ),
        (
            "target",
            format!("{:?}", part(&incr.target, held)),
            format!("{:?}", part(&full.target, held)),
        ),
        (
            "mass delete",
            format!("{:?}", incr.vanished),
            format!("{:?}", full.vanished),
        ),
        (
            "twins",
            format!("{:?}", incr.twins),
            format!("{:?}", full.twins),
        ),
        (
            "copies",
            format!("{:?}", incr.copies),
            format!("{:?}", full.copies),
        ),
        (
            "adopted",
            format!("{:?}", incr.adopted),
            format!("{:?}", full.adopted),
        ),
        ("reports", sorted(&incr.reports), sorted(&full.reports)),
        (
            "notes",
            format!("{:?}", incr.notes),
            format!("{:?}", full.notes),
        ),
        ("operations", ops(&incr.ops), ops(&full.ops)),
        (
            "steps",
            format!("{:?}", incr.steps),
            format!("{:?}", full.steps),
        ),
        (
            "folders moving",
            format!("{:?}", incr.moving),
            format!("{:?}", full.moving),
        ),
        (
            "out of view",
            format!("{:?}", incr.reserved),
            format!("{:?}", full.reserved),
        ),
    ];
    for (what, a, b) in pairs {
        if a != b {
            note(
                what,
                None,
                a.chars().take(3000).collect(),
                b.chars().take(3000).collect(),
            );
        }
    }

    // Nothing about anything else: no row written, and what it is to be is what the server has and
    // what is here.
    for (id, row) in &full.rows {
        if !held.contains(id) && before.get(id) != Some(row) {
            note(
                "a row outside the set changes",
                Some(id),
                format!("{:?}", before.get(id)),
                format!("{row:?}"),
            );
        }
    }
    for id in before.keys() {
        if !held.contains(id) && !full.rows.contains_key(id) {
            note(
                "a row outside the set goes",
                Some(id),
                String::new(),
                String::new(),
            );
        }
    }
    for (id, t) in &full.target {
        if held.contains(id) {
            continue;
        }
        let r = remote.get_live(*id);
        let l = full.local.get(id);
        let quiet = r.is_some_and(|r| r.links == t.links && r.body == t.body)
            && l.is_some_and(|l| {
                l.links == t.links
                    && l.content == t.body.content
                    && l.apply_from == t.body.metadata
                    && !l.carried
            });
        if !quiet {
            note(
                "decided about outside the set",
                Some(id),
                String::new(),
                format!("target {t:?}, server {r:?}, here {l:?}"),
            );
        }
    }
    for (id, l) in &full.local {
        if !held.contains(id) && !full.target.contains_key(id) {
            note(
                "here, outside the set, and not kept",
                Some(id),
                String::new(),
                format!("{l:?}"),
            );
        }
    }
    for (id, _) in remote.live() {
        if !held.contains(&id) && id != ObjectId::ROOT && !full.target.contains_key(&id) {
            note(
                "on the server, outside the set, and not kept",
                Some(&id),
                String::new(),
                String::new(),
            );
        }
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(diffs.join("\n"))
    }
}

/// The oracle's full round, for the check after a finalize.
pub struct FullRound {
    pub decisions: Decisions,
    /// The server's tree whole, which the base must be after the finalize.
    pub remote: Tree,
    pub local: BTreeMap<ObjectId, LNode>,
    pub seen: HashMap<ObjectId, Option<Hash>>,
    pub hidden: BTreeSet<ObjectId>,
}

/// After an incremental round's finalize, in a harness build: the state is what a full finalize would
/// leave, the ledger never vouches for what a full one would not, and no row lost its manifest.
pub fn check_finalized(root: &Root, full: &FullRound) -> Result<()> {
    let base = root.state.load_base()?;
    if base.nodes != full.remote.nodes {
        let first = base
            .nodes
            .keys()
            .chain(full.remote.nodes.keys())
            .find(|id| base.nodes.get(id) != full.remote.nodes.get(id));
        anyhow::bail!(
            "after an incremental finalize the base is not the server's tree, first at {first:?}: {:?} against {:?}",
            first.and_then(|id| base.nodes.get(id)),
            first.and_then(|id| full.remote.nodes.get(id))
        );
    }
    let rows = root.state.load_rows()?;
    for (id, row) in &rows {
        let settled = match full.local.get(id) {
            None => false,
            Some(n) if n.carried => true,
            Some(n) => {
                row.held.as_ref() == Some(&n.links)
                    && if row.incomplete {
                        row.synced.is_none()
                    } else {
                        row.applied.as_ref() == Some(&row.scan) && row.synced.is_none()
                    }
            }
        };
        if !settled {
            anyhow::bail!(
                "after an incremental finalize the row of {id} is not what a full finalize leaves: {row:?}"
            );
        }
    }
    let agreement = Agreement {
        remote: &base,
        hidden: &full.hidden,
        rows: &rows,
        local: &full.local,
        seen: &full.seen,
    };
    let entries: HashMap<ObjectId, Vec<u8>> = root.state.ledger_entries()?.into_iter().collect();
    let dirty = root.state.dirty()?;
    let ids: BTreeSet<ObjectId> = base
        .live()
        .map(|(id, _)| id)
        .chain(rows.keys().copied())
        .chain(entries.keys().copied())
        .collect();
    for id in ids {
        if dirty.contains(&id) {
            continue;
        }
        let stored = entries.get(&id).and_then(|b| Entry::decode(b));
        match agreement.verdict(id) {
            Verdict::Dirty => anyhow::bail!(
                "after an incremental finalize the ledger vouches for {id}, which a full one would leave to be looked at again: {stored:?}"
            ),
            Verdict::Drop if id != ObjectId::ROOT && stored.is_some() => anyhow::bail!(
                "after an incremental finalize the ledger still has {id}, which has gone"
            ),
            Verdict::Put(e) if stored.as_ref() != Some(&e) => anyhow::bail!(
                "after an incremental finalize the ledger's entry for {id} is {stored:?}, where a full one writes {e:?}"
            ),
            _ => {}
        }
    }
    let have = root.state.manifest_hashes()?;
    for (id, row) in &rows {
        if let Content::File { manifest, .. } = &row.content {
            if !have.contains(manifest) {
                anyhow::bail!("after an incremental finalize the manifest of {id}'s row has gone");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::{Bytes, Timestamp};

    fn fp(n: u64) -> Fingerprint {
        Fingerprint {
            dev: 7,
            ino: n,
            size: 3,
            mtime: Timestamp { sec: -5, nsec: 9 },
            ctime: Timestamp {
                sec: 1_700_000_000,
                nsec: 999_999_999,
            },
            btime: (n % 2 == 0).then_some(Timestamp { sec: 1, nsec: 2 }),
        }
    }

    fn name(n: &str) -> Name {
        Name::new(n.as_bytes().to_vec()).unwrap()
    }

    fn scanned(path: &[&str], fp: Fingerprint, content: Content) -> Scanned {
        Scanned {
            paths: vec![path.iter().map(|n| name(n)).collect()],
            fp,
            racy: false,
            content,
            metadata: cairn_protocol::Metadata::default(),
            metadata_incomplete: false,
            holds_excluded: false,
            only_excluded: false,
        }
    }

    fn scan_of(nodes: Vec<Scanned>) -> ScanResult {
        ScanResult {
            root: scanned(&[], fp(1), Content::Dir),
            nodes,
            skips: Vec::new(),
            unobserved: Vec::new(),
            refused: Vec::new(),
            unobserved_inodes: Vec::new(),
            unidentified: false,
            unlisted: Vec::new(),
            unstable: false,
            new_manifests: Vec::new(),
            stale_temps: Vec::new(),
            rescued_temps: Vec::new(),
            rescued: Vec::new(),
            excluded: Vec::new(),
            excluded_ids: Vec::new(),
            stopped: false,
            started: std::time::Instant::now(),
            vouched_from: usize::MAX,
            listed: None,
        }
    }

    fn file() -> Content {
        Content::File {
            manifest: Hash([7; 32]),
            size: 3,
        }
    }

    /// A tree the ledger agreed on: `d` at the root holding `x` and `y`, `d/e` holding `z`, and `f` at
    /// the root, each with its scan as the ledger has it.
    struct Agreed {
        ledger: Ledger,
        nodes: Vec<Scanned>,
        ids: BTreeMap<&'static str, ObjectId>,
    }

    fn agreed() -> Agreed {
        let tree: [(&str, &[&str], u64, bool); 6] = [
            ("d", &["d"], 10, true),
            ("x", &["d", "x"], 11, false),
            ("y", &["d", "y"], 12, false),
            ("e", &["d", "e"], 13, true),
            ("z", &["d", "e", "z"], 14, false),
            ("f", &["f"], 15, false),
        ];
        let mut ids = BTreeMap::new();
        for (i, (n, ..)) in tree.iter().enumerate() {
            ids.insert(*n, ObjectId([i as u8 + 1; 16]));
        }
        let mut entries = Vec::new();
        let mut nodes = Vec::new();
        for (n, path, ino, dir) in tree {
            let parent = match path.len() {
                1 => ObjectId::ROOT,
                _ => ids[path[path.len() - 2]],
            };
            let content = if dir { Content::Dir } else { file() };
            // As a scan and this build's records have one: on Windows with no birth time.
            let fp = fp(ino).current();
            let node = scanned(path, fp.clone(), content.clone());
            entries.push((
                ids[n],
                Entry {
                    kind: content.kind(),
                    links: vec![Link {
                        parent,
                        name: name(path[path.len() - 1]),
                    }],
                    seen: Some(Seen {
                        fp,
                        content,
                        digest: dir.then(|| digest(&node)),
                    }),
                },
            ));
            nodes.push(node);
        }
        Agreed {
            ledger: Ledger::new(entries),
            nodes,
            ids,
        }
    }

    /// Classified and closed over, with these objects marked and this journal.
    fn decide(
        a: &Agreed,
        nodes: Vec<Scanned>,
        dirty: &[&str],
        journal: Journal,
        rules: bool,
    ) -> (Vec<bool>, Reach) {
        let dirty: BTreeSet<ObjectId> = dirty.iter().map(|n| a.ids[n]).collect();
        let (dn, dr) = (BTreeMap::new(), BTreeMap::new());
        let b = Before {
            ledger: &a.ledger,
            dirty: &dirty,
            dirty_nodes: &dn,
            dirty_rows: &dr,
            journal: &journal,
            glanced: false,
            unscanned: &BTreeSet::new(),
        };
        let scan = scan_of(nodes);
        let c = classify(&b, &scan);
        let r = reach(&b, &c, rules, &BTreeSet::new(), &BTreeSet::new());
        (c.quiet.clone(), r)
    }

    #[test]
    fn what_the_scan_finds_as_the_ledger_has_it_is_quiet_and_nothing_else_is() {
        let a = agreed();
        let (quiet, r) = decide(&a, a.nodes.clone(), &[], Journal::default(), false);
        assert!(quiet.iter().all(|q| *q), "{quiet:?}");
        assert_eq!(
            r.all,
            BTreeSet::from([ObjectId::ROOT, a.ids["d"], a.ids["f"]])
        );
    }

    /// On a filesystem whose rename moves no time, only the entries tell a renamed file.
    #[test]
    fn a_file_renamed_with_its_times_unmoved_is_not_quiet() {
        let a = agreed();
        let mut nodes = a.nodes.clone();
        nodes[1].paths = vec![vec![name("d"), name("renamed")]];
        let (quiet, r) = decide(&a, nodes, &[], Journal::default(), false);
        assert!(!quiet[1], "{quiet:?}");
        assert!(r.core.contains(&a.ids["x"]));
        assert!(r.all.contains(&a.ids["y"]), "its folder is opened");
    }

    /// A folder's metadata changed with no time moving: the digest of what the scan read tells it.
    #[test]
    fn a_folder_changed_with_its_times_unmoved_is_not_quiet() {
        let a = agreed();
        let mut nodes = a.nodes.clone();
        nodes[3].metadata.mode = 0o700;
        let (quiet, r) = decide(&a, nodes, &[], Journal::default(), false);
        assert!(!quiet[3], "{quiet:?}");
        assert!(r.core.contains(&a.ids["e"]));
        assert!(r.all.contains(&a.ids["z"]), "what is in it is opened");
    }

    /// What has been written since the ledger's entry is never quiet, whatever the scan finds.
    #[test]
    fn a_marked_object_is_not_quiet() {
        let a = agreed();
        let (quiet, r) = decide(&a, a.nodes.clone(), &["x"], Journal::default(), false);
        assert!(!quiet[1], "{quiet:?}");
        assert!(r.core.contains(&a.ids["x"]));
    }

    /// A file gone from a folder whose times did not move: the folder is opened by the object's own
    /// entries, so what is beside it is in sight, and the folders above it are there however little
    /// else under them is.
    #[test]
    fn what_is_gone_brings_its_folders_and_what_is_beside_it() {
        let a = agreed();
        let mut nodes = a.nodes.clone();
        nodes.remove(4); // d/e/z
        let (_, r) = decide(&a, nodes.clone(), &[], Journal::default(), false);
        assert!(r.core.contains(&a.ids["z"]));
        assert!(r.all.contains(&a.ids["e"]) && r.all.contains(&a.ids["d"]));
        nodes.remove(1); // d/x
        let (_, r) = decide(&a, nodes, &[], Journal::default(), false);
        assert!(r.all.contains(&a.ids["y"]), "{:?}", r.all);
    }

    /// Something gone deep down, from folders whose times did not move: every folder above it is loaded,
    /// as the merge reads paths and cycles through them, however little of the tree is otherwise taken.
    #[test]
    fn what_is_gone_deep_down_brings_every_folder_above_it() {
        let mut ids = BTreeMap::new();
        let tree: [(&'static str, &[&str], u64, bool); 4] = [
            ("p", &["p"], 20, true),
            ("q", &["p", "q"], 21, true),
            ("r", &["p", "q", "r"], 22, true),
            ("s", &["p", "q", "r", "s"], 23, false),
        ];
        for (i, (n, ..)) in tree.iter().enumerate() {
            ids.insert(*n, ObjectId([i as u8 + 40; 16]));
        }
        let mut entries = Vec::new();
        let mut nodes = Vec::new();
        for (n, path, ino, dir) in tree {
            let parent = match path.len() {
                1 => ObjectId::ROOT,
                _ => ids[path[path.len() - 2]],
            };
            let content = if dir { Content::Dir } else { file() };
            let node = scanned(path, fp(ino), content.clone());
            entries.push((
                ids[n],
                Entry {
                    kind: content.kind(),
                    links: vec![Link {
                        parent,
                        name: name(path[path.len() - 1]),
                    }],
                    seen: Some(Seen {
                        fp: fp(ino),
                        content,
                        digest: dir.then(|| digest(&node)),
                    }),
                },
            ));
            nodes.push(node);
        }
        let a = Agreed {
            ledger: Ledger::new(entries),
            nodes,
            ids,
        };
        let mut nodes = a.nodes.clone();
        nodes.pop(); // p/q/r/s
        let (_, r) = decide(&a, nodes, &[], Journal::default(), false);
        for n in ["s", "r", "q", "p"] {
            assert!(r.all.contains(&a.ids[n]), "{n}: {:?}", r.all);
        }
    }

    /// One inode at two places is a scan that saw something move; one inode with two names is a hard
    /// link, which the scan gives one node.
    #[test]
    fn an_inode_at_two_places_is_found_and_a_hard_link_is_not() {
        let linked = Scanned {
            paths: vec![vec![name("h1")], vec![name("h2")]],
            ..scanned(&["h1"], fp(7), file())
        };
        let mut nodes = vec![scanned(&["a"], fp(5), file()), linked];
        assert_eq!(inode_twice(&scan_of(nodes.clone())), None);
        nodes.push(scanned(&["q", "m"], fp(5), file()));
        assert_eq!(inode_twice(&scan_of(nodes)), Some("q/m".into()));
    }

    /// With rules, a folder the server moves can take all in it out of view or into it: all of it is
    /// decided about.
    #[test]
    fn with_rules_a_folder_the_server_moves_is_decided_about_whole() {
        let a = agreed();
        let mut journal = Journal::default();
        journal.add(&Op::Move {
            object: a.ids["d"],
            from: Link {
                parent: ObjectId::ROOT,
                name: name("d"),
            },
            to: Link {
                parent: ObjectId::ROOT,
                name: name("cache"),
            },
        });
        let (_, r) = decide(&a, a.nodes.clone(), &[], journal, true);
        for n in ["d", "x", "y", "e", "z"] {
            assert!(r.core.contains(&a.ids[n]), "{n}");
        }
    }

    /// An agreement vouches only for what a full finalize leaves settled: anything a later scan must
    /// read again, or that shares state ahead of the base, or that this scan did not see as the server
    /// has it, stays to be looked at.
    #[test]
    fn an_agreement_vouches_only_for_what_is_settled() {
        use cairn_protocol::{Metadata, RevisionBody, RevisionId};
        let parent = ObjectId::ROOT;
        let link = Link {
            parent,
            name: Name::new(b"f".to_vec()).unwrap(),
        };
        let id = ObjectId([9; 16]);
        let content = Content::File {
            manifest: Hash([1; 32]),
            size: 3,
        };
        let mut remote = Tree::new();
        remote.nodes.insert(
            id,
            Node {
                rev: RevisionId([2; 16]),
                body: RevisionBody {
                    content: content.clone(),
                    metadata: Metadata::default(),
                },
                links: BTreeSet::from([link.clone()]),
            },
        );
        let remote = Tree::from_nodes(remote.nodes);
        let settled = Row {
            fp: fp(4),
            racy: false,
            content: content.clone(),
            scan: Metadata::default(),
            applied: Some(Metadata::default()),
            synced: None,
            incomplete: false,
            held: Some(BTreeSet::from([link.clone()])),
        };
        let here = LNode {
            links: BTreeSet::from([link.clone()]),
            content: content.clone(),
            stored: Metadata::default(),
            apply_from: Metadata::default(),
            carried: false,
            conflict_of: None,
        };
        let seen = HashMap::from([(id, None)]);
        let hidden = BTreeSet::new();
        let verdict = |row: &Row, l: &LNode, seen: &HashMap<ObjectId, Option<Hash>>| {
            let rows = BTreeMap::from([(id, row.clone())]);
            let local = BTreeMap::from([(id, l.clone())]);
            Agreement {
                remote: &remote,
                hidden: &hidden,
                rows: &rows,
                local: &local,
                seen,
            }
            .verdict(id)
        };
        assert!(matches!(verdict(&settled, &here, &seen), Verdict::Put(_)));
        let mut racy = settled.clone();
        racy.racy = true;
        let mut incomplete = settled.clone();
        incomplete.incomplete = true;
        let mut unapplied = settled.clone();
        unapplied.applied = None;
        let mut synced = settled.clone();
        synced.synced = Some(crate::state::Synced {
            body: remote.nodes[&id].body.clone(),
            links: BTreeSet::from([link.clone()]),
            applied: Metadata::default(),
            commit: None,
            unapplied: false,
            folder_time: None,
        });
        let mut held = settled.clone();
        held.held = Some(BTreeSet::new());
        for (what, row) in [
            ("racy", &racy),
            ("incomplete", &incomplete),
            ("not applied", &unapplied),
            ("shared ahead", &synced),
            ("held otherwise", &held),
        ] {
            assert_eq!(verdict(row, &here, &seen), Verdict::Dirty, "{what}");
        }
        let mut carried = here.clone();
        carried.carried = true;
        assert_eq!(
            verdict(&settled, &carried, &seen),
            Verdict::Dirty,
            "carried"
        );
        let mut elsewhere = here.clone();
        elsewhere.links = BTreeSet::new();
        assert_eq!(
            verdict(&settled, &elsewhere, &seen),
            Verdict::Dirty,
            "elsewhere here"
        );
        assert_eq!(
            verdict(&settled, &here, &HashMap::new()),
            Verdict::Dirty,
            "not scanned"
        );
        assert_eq!(
            verdict(&settled, &here, &HashMap::from([(id, Some(Hash([3; 32])))])),
            Verdict::Dirty,
            "a file with a digest"
        );
    }

    #[test]
    fn an_entry_reads_back_as_it_was_written_and_nothing_else_reads_as_one() {
        let link = |p: u8, name: &[u8]| Link {
            parent: ObjectId([p; 16]),
            name: Name::new(name.to_vec()).unwrap(),
        };
        let entries = [
            Entry {
                kind: ObjectKind::File,
                links: vec![link(0, b"a"), link(3, b"\xff not utf-8")],
                seen: Some(Seen {
                    fp: fp(1),
                    content: Content::File {
                        manifest: Hash([4; 32]),
                        size: 3,
                    },
                    digest: None,
                }),
            },
            Entry {
                kind: ObjectKind::Dir,
                links: vec![link(0, b"d")],
                seen: Some(Seen {
                    fp: fp(2),
                    content: Content::Dir,
                    digest: Some(Hash([5; 32])),
                }),
            },
            Entry {
                kind: ObjectKind::Symlink,
                links: vec![link(1, b"l")],
                seen: Some(Seen {
                    fp: fp(3),
                    content: Content::Symlink {
                        target: Bytes(b"../x".to_vec()),
                    },
                    digest: Some(Hash([6; 32])),
                }),
            },
            Entry {
                kind: ObjectKind::Dir,
                links: vec![link(0, b"out of view")],
                seen: None,
            },
        ];
        for e in &entries {
            let b = e.encode();
            assert_eq!(Entry::decode(&b).as_ref(), Some(e));
            let mut longer = b.clone();
            longer.push(0);
            assert_eq!(Entry::decode(&longer), None, "trailing bytes");
            assert_eq!(Entry::decode(&b[..b.len() - 1]), None, "cut short");
            let mut other = b.clone();
            other[0] = FORMAT as u8 + 1;
            assert_eq!(Entry::decode(&other), None, "another format");
        }
    }
}
