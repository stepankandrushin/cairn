//! Reading a sync root. Whatever cannot be read becomes a skip with a reason, and a path this scan
//! could not observe is recorded as such, so it is never mistaken for a delete.

use crate::chunker::manifest_of;
use crate::fsops::{
    birth_mark, is_busy, is_invalid, is_not_found, open_for_writing, AsFd, Dir, FileType, OwnedFd,
    Stat,
};
use crate::metadata::{capture, Accounts};
use crate::refusal::{describe, Refusal};
use crate::root::aside_origin;
use crate::state::{Fingerprint, Known, Looked};
use cairn_failpoint::hit;
use cairn_protocol::name::display_bytes;
use cairn_protocol::{
    Bytes, Content, Hash, Metadata, Name, ObjectId, Skip, SkipKind, Timestamp, STATE_DIR,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// A write within this window of a read may leave the timestamps it read unchanged.
pub(crate) const RACY_WINDOW_NS: i128 = 100_000_000;

/// A file changed more recently than this may be in the middle of a save: a program that truncates
/// and then writes leaves an empty file that reads as perfectly steady. It waits this long first.
const STABLE_WINDOW_NS: i128 = 250_000_000;

/// A file another program has open for writing, or one emptied a moment ago, is in the middle of being
/// written until this long after its last change, however steady it reads: on a busy machine a writer
/// can stall for longer than the stable window between emptying a file and filling it. An emptied file
/// that is still open for writing waits for the writer however long it takes.
const WRITER_WINDOW_NS: i128 = 2_000_000_000;

/// How often a file that changed under a read is tried again within one scan.
const READ_ATTEMPTS: u32 = 3;

/// In the state directory: what a running daemon last reported.
pub const STATE_FILE_DAEMON: &str = "daemon.json";

/// One inode: several paths when hard-linked.
#[derive(Clone, Debug)]
pub struct Scanned {
    pub paths: Vec<Vec<Name>>,
    pub fp: Fingerprint,
    pub racy: bool,
    pub content: Content,
    pub metadata: Metadata,
    /// Some metadata could not be read, so this scan says nothing about what changed.
    pub metadata_incomplete: bool,
    /// A directory with something excluded somewhere under it, which deleting it would destroy.
    pub holds_excluded: bool,
    /// A directory with nothing under it but what is excluded.
    pub only_excluded: bool,
}

/// An entry a scan given a sieve kept nothing of but this: what the ledger has it as, quietly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quiet {
    /// Its place among every node the walk found, in the order a scan without a sieve keeps them.
    pub seq: u32,
    /// The folder it is in, in `folders`.
    pub folder: u32,
    pub object: ObjectId,
}

/// A folder a scan found, read or taken from the agreement: where it is, and where its node and the
/// quiet entries in it are.
#[derive(Clone, Debug)]
pub struct Folder {
    /// The folder it is in; the root's is itself.
    pub parent: u32,
    pub name: Option<Name>,
    /// Where its own node is kept: `None` for the root, and for one the walk went into and then lost.
    pub node: Option<Slot>,
    /// The object its node's inode says it is, which a sieve took what is in it to be in.
    pub object: Option<ObjectId>,
    /// The quiet entries in it, read and then taken from the agreement: ranges of `quiet`.
    pub quiet: (u32, u32),
    pub vouched: (u32, u32),
}

/// The root's place in `folders`.
pub const ROOT_FOLDER: u32 = 0;

/// Where a scan keeps a node: whole, as `nodes[k]`, or quiet, as `quiet[q]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Kept(u32),
    Quiet(u32),
}

#[derive(Debug)]
pub struct ScanResult {
    pub root: Scanned,
    /// Every node kept whole, in walk order; without a sieve, every node the walk found.
    pub nodes: Vec<Scanned>,
    /// The place in the walk of each of `nodes`, where a sieve dropped some; `None` where node `i` is the
    /// walk's `i`-th.
    pub seqs: Option<Vec<u32>>,
    /// What a sieve kept of the rest, each folder's together.
    pub quiet: Vec<Quiet>,
    /// For a check, the sieve's witness of each of `quiet` as it was read.
    pub witnesses: Vec<[u8; 16]>,
    /// How many nodes the walk found, kept whole or quiet.
    pub total: usize,
    /// Every folder found, the root first.
    pub folders: Vec<Folder>,
    /// `folders` by parent and name, for finding one by its path.
    pub folders_by_name: Vec<u32>,
    /// Every node, whole, as a scan without a sieve has them: for the harness's oracle alone.
    pub all: Option<Vec<Scanned>>,
    /// What a sync with nothing to do compares itself with (steady.rs), when it was asked for.
    pub digest: Option<Hash>,
    /// Where the walk found an inode it had found at another place already.
    pub inode_twice: Option<String>,
    pub skips: Vec<Skip>,
    /// Entries this scan could not observe, each with everything under it.
    pub unobserved: Vec<Vec<Name>>,
    /// Those of them the system refused this process, or that failed to read: not in motion, but held
    /// as they were until they can be read. With `unlisted`, what a sync counts before taking a tree
    /// most of which it may not read for one to sync.
    pub refused: Vec<Vec<Name>>,
    /// The inodes of those, which may be known objects under a new name.
    pub unobserved_inodes: Vec<(u64, u64)>,
    /// Something could not even be stat'ed, so it may be anything: no absence proves a delete.
    pub unidentified: bool,
    /// Directories that were seen but whose contents could not be listed.
    pub unlisted: Vec<Vec<Name>>,
    /// The tree changed while it was walked, so a missing entry proves nothing.
    pub unstable: bool,
    pub new_manifests: Vec<(Hash, Vec<u8>)>,
    /// This device's leftover temporaries that a read-only directory kept from being removed.
    pub stale_temps: Vec<(Vec<Name>, Vec<u8>)>,
    /// Temporaries that are a file this device has a record of, by inode and birth time, with an edit in
    /// them: where, and what they were.
    pub rescued_temps: Vec<(Vec<Name>, Vec<u8>, Fingerprint)>,
    /// The objects those were, once kept under a name of their own.
    pub rescued: Vec<cairn_protocol::ObjectId>,
    /// The topmost entries left alone because this device excludes them.
    pub excluded: Vec<Vec<Name>>,
    /// What each of those was: device, inode, birth time and whether a directory, all a filter may
    /// decide by.
    pub excluded_ids: Vec<(u64, u64, Option<Timestamp>, bool)>,
    /// A probe stopped where a full scan would have read a file or removed a leftover.
    pub stopped: bool,
    /// When the walk began: whatever happened in the tree before this, it saw.
    pub started: Instant,
    /// Nodes from here on were not read but taken from the last agreement, for a scan with a scope:
    /// what it found in the folders it did not list.
    pub vouched_from: usize,
    /// The folders a scan with a scope listed; `None` for one that listed the whole tree.
    pub listed: Option<BTreeSet<Vec<Name>>>,
}

impl ScanResult {
    /// The same scan, with only these of its nodes: what a round that takes part of the tree is given.
    pub fn with_nodes(&self, nodes: Vec<Scanned>) -> ScanResult {
        let nodes_len = nodes.len();
        ScanResult {
            root: self.root.clone(),
            total: nodes_len,
            nodes,
            seqs: None,
            quiet: Vec::new(),
            witnesses: Vec::new(),
            folders: Vec::new(),
            folders_by_name: Vec::new(),
            all: None,
            digest: None,
            inode_twice: None,
            skips: self.skips.clone(),
            unobserved: self.unobserved.clone(),
            refused: self.refused.clone(),
            unobserved_inodes: self.unobserved_inodes.clone(),
            unidentified: self.unidentified,
            unlisted: self.unlisted.clone(),
            unstable: self.unstable,
            new_manifests: Vec::new(),
            stale_temps: self.stale_temps.clone(),
            rescued_temps: self.rescued_temps.clone(),
            rescued: self.rescued.clone(),
            excluded: self.excluded.clone(),
            excluded_ids: self.excluded_ids.clone(),
            stopped: self.stopped,
            started: self.started,
            vouched_from: nodes_len,
            listed: self.listed.clone(),
        }
    }

    /// A scan of these nodes alone, every one of them read: one made by hand, as a test makes it.
    pub fn of(root: Scanned, nodes: Vec<Scanned>) -> ScanResult {
        let part = ScanResult {
            root: root.clone(),
            nodes: Vec::new(),
            seqs: None,
            quiet: Vec::new(),
            witnesses: Vec::new(),
            total: 0,
            folders: Vec::new(),
            folders_by_name: Vec::new(),
            all: None,
            digest: None,
            inode_twice: None,
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
            started: Instant::now(),
            vouched_from: usize::MAX,
            listed: None,
        };
        let mut out = ScanResult {
            vouched_from: usize::MAX,
            ..part.with_nodes(nodes)
        };
        // Every folder a node is in or is, as a walk finds them.
        let mut folders = vec![Folder {
            parent: ROOT_FOLDER,
            name: None,
            node: None,
            object: None,
            quiet: (0, 0),
            vouched: (0, 0),
        }];
        let mut at: BTreeMap<Vec<Name>, u32> = BTreeMap::new();
        at.insert(Vec::new(), ROOT_FOLDER);
        let mut folder = |path: &[Name], folders: &mut Vec<Folder>| -> u32 {
            let mut f = ROOT_FOLDER;
            for i in 1..=path.len() {
                f = match at.get(&path[..i]) {
                    Some(f) => *f,
                    None => {
                        folders.push(Folder {
                            parent: f,
                            name: Some(path[i - 1].clone()),
                            node: None,
                            object: None,
                            quiet: (0, 0),
                            vouched: (0, 0),
                        });
                        let new = (folders.len() - 1) as u32;
                        at.insert(path[..i].to_vec(), new);
                        new
                    }
                };
            }
            f
        };
        for (k, n) in out.nodes.iter().enumerate() {
            for p in &n.paths {
                if let Some((_, parent)) = p.split_last() {
                    folder(parent, &mut folders);
                }
                if n.content == Content::Dir {
                    let f = folder(p, &mut folders);
                    folders[f as usize].node = Some(Slot::Kept(k as u32));
                }
            }
        }
        let mut by_name: Vec<u32> = (1..folders.len() as u32).collect();
        by_name.sort_unstable_by(|a, b| {
            let (a, b) = (&folders[*a as usize], &folders[*b as usize]);
            (a.parent, &a.name).cmp(&(b.parent, &b.name))
        });
        out.folders = folders;
        out.folders_by_name = by_name;
        let mut seen = Inodes::default();
        out.inode_twice = out
            .nodes
            .iter()
            .find(|n| !seen.insert((n.fp.dev, n.fp.ino)))
            .map(|n| {
                n.paths
                    .first()
                    .map_or_else(String::new, |p| display_path(p))
            });
        out
    }

    /// The same scan with every node whole, as a scan without a sieve has them, where it kept them all.
    pub fn whole(&self) -> Option<ScanResult> {
        let all = self.all.clone()?;
        Some(ScanResult {
            vouched_from: self.vouched_from,
            ..self.with_nodes(all)
        })
    }

    /// Whether node `i` was taken from the last agreement rather than read.
    pub fn vouched(&self, i: usize) -> bool {
        i >= self.vouched_from
    }

    /// The place in the walk of `nodes[k]`.
    pub fn seq(&self, k: usize) -> usize {
        match &self.seqs {
            Some(s) => s[k] as usize,
            None => k,
        }
    }

    /// The folder at this path, if the walk found one there.
    pub fn folder_at(&self, path: &[Name]) -> Option<u32> {
        if self.folders.is_empty() {
            return None;
        }
        let mut at = ROOT_FOLDER;
        for name in path {
            let i = self
                .folders_by_name
                .binary_search_by(|f| {
                    let f = &self.folders[*f as usize];
                    (f.parent, f.name.as_ref()).cmp(&(at, Some(name)))
                })
                .ok()?;
            at = self.folders_by_name[i];
        }
        Some(at)
    }

    /// Where a folder is, from the root.
    pub fn folder_path(&self, mut f: u32) -> Vec<Name> {
        let mut out = Vec::new();
        while f != ROOT_FOLDER {
            let folder = &self.folders[f as usize];
            if let Some(name) = &folder.name {
                out.push(name.clone());
            }
            f = folder.parent;
        }
        out.reverse();
        out
    }
}

/// What a scan knows of a file by its inode, to tell one that has not changed without reading it: a map of
/// rows, or the ledger and what was written since (incr.rs), asked one inode at a time.
pub trait KnownRows: Sync {
    fn known(&self, key: &(u64, u64)) -> Option<Looked<'_>>;
    /// About how many, for the progress a scan shows.
    fn count(&self) -> usize;
}

impl KnownRows for HashMap<(u64, u64), Known<'_>> {
    fn known(&self, key: &(u64, u64)) -> Option<Looked<'_>> {
        self.get(key).map(Looked::from)
    }

    fn count(&self) -> usize {
        self.len()
    }
}

/// What a scan given the ledger keeps of what it reads (incr.rs `Sieve`): every entry the ledger does not
/// account for, whole, and of the rest the object each is, so that a round holds what changed and not
/// the tree.
pub trait Sieve: Sync {
    /// The object a folder is, by what its node's inode says, once everything in it has been walked.
    fn folder(&self, node: &Scanned) -> Option<ObjectId>;
    /// The object this node, in the folder that is `parent`, is quietly: found as the ledger has it.
    /// `None` keeps it whole.
    fn quiet(&self, parent: Option<ObjectId>, node: &Scanned) -> Option<ObjectId>;
    /// The same, of what a scope took from the agreement under a folder it vouches for all the way down,
    /// in which no entry shares an inode with another: the ledger's own entry, in the folder that is
    /// `parent`.
    fn quiet_vouched(&self, parent: Option<ObjectId>, child: &Child<'_>) -> bool;
    /// What is kept of a quiet node to tell later that it was read as it is made again, for a check of
    /// the ledger it was sorted by: `None` where nothing is to be checked.
    fn witness(&self, node: &Scanned) -> Option<[u8; 16]>;
    /// Whether it keeps a witness of each quiet node, so that one taken from the agreement, never read,
    /// is not to be quiet.
    fn witnessing(&self) -> bool;
}

/// The part of the tree a scan lists, when a daemon's watcher says where things changed (scope.rs): the
/// root, the folders in `list`, and any folder on the way that is not as the last agreement found it.
/// What every other folder holds is taken from that agreement, by `vouch`, after every node read.
pub struct Scope<'a> {
    pub list: &'a crate::scope::Folders,
    pub vouch: &'a dyn Vouch,
}

/// One entry of what the agreement found in a folder: the object, its name, whether it is a folder, its
/// inode, and where the agreement keeps it, for the sieve.
#[derive(Clone, Copy, Debug)]
pub struct Child<'v> {
    pub object: ObjectId,
    pub name: &'v [u8],
    pub dir: bool,
    pub inode: (u64, u64),
    pub place: u32,
}

/// The last agreement's word on the folders a scan does not list (incr.rs `Word`).
pub trait Vouch: Sync {
    /// The object the agreement had at this inode, under this name in the folder that is `parent`.
    fn object_at(&self, parent: ObjectId, name: &Name, st: &Stat) -> Option<ObjectId>;
    /// Whether this folder, just read but for what is under it, is as the agreement found it; if so,
    /// the flags of what the agreement found under it are set.
    fn quiet(&self, folder: ObjectId, node: &mut Scanned) -> bool;
    /// Whether everything in this folder but the names in `except`, all the way down, is the
    /// agreement's to vouch for, and the folder's own listing too.
    fn takes_all_but(&self, folder: ObjectId, except: &BTreeSet<Name>) -> bool;
    /// Whether the agreement had an entry of this name in this folder.
    fn has_child(&self, folder: ObjectId, name: &Name) -> bool;
    /// What the agreement found in this folder, each name once.
    fn children(&self, folder: ObjectId) -> Vec<Child<'_>>;
    /// What the agreement found of this object, as a node at `path`.
    fn node(&self, object: ObjectId, path: Vec<Name>) -> Option<Scanned>;
    /// The folders the agreement had something this scan read at, where the scan did not read it: the
    /// folder of each such path.
    fn agreed_elsewhere(&self, scan: &ScanResult) -> Vec<Vec<Name>>;
}

/// The folders a scan lists, as a tree of names.
#[derive(Default, Debug)]
struct Trie {
    list: bool,
    whole: bool,
    children: BTreeMap<Name, Trie>,
}

impl Trie {
    fn of(list: &crate::scope::Folders) -> Trie {
        let mut root = Trie {
            list: true,
            ..Trie::default()
        };
        for (path, whole) in list {
            let mut at = &mut root;
            for name in path {
                at = at.children.entry(name.clone()).or_default();
            }
            at.list = true;
            at.whole |= *whole;
        }
        root
    }
}

/// Where a folder stands in a scan with a scope: its part of the list, whether all under it is to be
/// listed, and the object the agreement had at it.
#[derive(Clone, Copy)]
struct At<'t> {
    trie: Option<&'t Trie>,
    whole: bool,
    object: Option<ObjectId>,
}

impl At<'_> {
    const WHOLE: At<'static> = At {
        trie: None,
        whole: true,
        object: None,
    };
}

pub struct ScanOptions<'a> {
    pub temp_prefix: &'a [u8],
    pub accounts: &'a Accounts,
    /// What is known by inode, to avoid reading unchanged files.
    pub rows: &'a dyn KnownRows,
    pub remove_own_temps: bool,
    /// What this device was about to place, and at which names, by a run that may have been killed.
    pub arriving: &'a [(Content, Vec<Name>)],
    /// Whether an entry is excluded, given that its parent is not.
    pub excluded: &'a dyn Fn(&[Name], &Stat) -> bool,
    /// Stop instead of reading a file's content or removing a leftover: for a sync that expects to
    /// have nothing to do, which reads no file.
    pub probe: bool,
    /// List only part of the tree, and take the rest from the last agreement.
    pub scope: Option<&'a Scope<'a>>,
    /// When the scan is to say it began, if before the call: when the scope it was given was taken.
    pub started: Option<Instant>,
    /// Keep of what the ledger accounts for only the object it is.
    pub sieve: Option<&'a dyn Sieve>,
    /// Keep every node whole as well, in `all`, for the harness's oracle.
    pub keep_all: bool,
    /// Work out the digest a sync with nothing to do compares itself with.
    pub digest: bool,
    /// Keep no node but what more names may come to: for a scan that is its digest alone.
    pub discard: bool,
}

pub fn fingerprint(st: &Stat) -> Fingerprint {
    Fingerprint {
        dev: st.dev,
        ino: st.ino,
        size: st.size,
        mtime: st.mtime,
        ctime: st.ctime,
        btime: birth_mark(st.btime),
    }
}

pub(crate) fn now_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i128)
}

pub(crate) fn ns(t: Timestamp) -> i128 {
    i128::from(t.sec) * 1_000_000_000 + i128::from(t.nsec)
}

/// Whether a later write could leave these timestamps as they are. ctime alone decides: every write
/// moves it, and unlike mtime it cannot be set by hand.
pub fn is_racy(st: &Stat, precision_ns: u32) -> bool {
    racy_within(st, precision_ns, RACY_WINDOW_NS)
}

/// As `is_racy`, for a window of `window_ns`.
pub fn racy_within(st: &Stat, precision_ns: u32, window_ns: i128) -> bool {
    now_ns() - ns(st.ctime) < window_ns + i128::from(precision_ns)
}

/// Wait out what is left of the stable window since this file last changed.
pub(crate) fn wait_until_stable(st: &Stat) {
    let age = now_ns() - ns(st.ctime);
    if age < STABLE_WINDOW_NS {
        // A clock stepped backwards makes the age negative; the wait is still bounded.
        let wait = (STABLE_WINDOW_NS - age).min(STABLE_WINDOW_NS);
        std::thread::sleep(std::time::Duration::from_nanos(wait as u64));
    }
}

pub fn display_path(path: &[Name]) -> String {
    let parts: Vec<String> = path.iter().map(|n| display_bytes(n.as_bytes())).collect();
    parts.join("/")
}

/// A directory as it looked when listed: its folder, device and inode, mtime and ctime.
type Listed = (u32, (u64, u64), Timestamp, Timestamp);

/// How many nodes of folders done with wait before they are sorted together.
const SETTLE_BATCH: usize = 16_384;

/// The nodes of a folder the walk is in, held until its own node is read: with a sieve, each is then
/// kept whole or quiet. Each with its place in the walk, and its own folder if it is one.
struct Open {
    folder: u32,
    children: Vec<(u32, Scanned, Option<u32>)>,
}

/// A cheap hash for the walk's set of inodes, which nobody chooses against it.
#[derive(Default, Clone, Copy)]
struct InodeHash(u64);

impl std::hash::Hasher for InodeHash {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.write_u64(u64::from(*b));
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type Inodes = HashSet<(u64, u64), std::hash::BuildHasherDefault<InodeHash>>;

struct Walk<'a> {
    opts: &'a ScanOptions<'a>,
    precision_ns: u32,
    /// Kept whole, and each one's place in the walk.
    nodes: Vec<Scanned>,
    seqs: Vec<u32>,
    quiet: Vec<Quiet>,
    witnesses: Vec<[u8; 16]>,
    /// Folders done with, whose nodes wait to be sorted, and how many those are.
    done: Vec<(Open, Option<ObjectId>)>,
    done_nodes: usize,
    /// Nodes found so far.
    seq: u32,
    folders: Vec<Folder>,
    open: Vec<Open>,
    /// Every inode a node was made for, and the first found at another place already.
    inodes: Inodes,
    inode_twice: Option<String>,
    /// Hard-linked files and symlinks, whose names the walk finds one by one: kept whole from the first,
    /// and counted into the digest at the end.
    linked_slots: Vec<u32>,
    all: Option<Vec<Option<Scanned>>>,
    digest: Option<crate::steady::Stream>,
    linked: HashMap<(u64, u64), usize>,
    skips: Vec<Skip>,
    unobserved: Vec<Vec<Name>>,
    refused: Vec<Vec<Name>>,
    unobserved_inodes: Vec<(u64, u64)>,
    unidentified: bool,
    current_inode: Option<(u64, u64)>,
    unlisted: Vec<Vec<Name>>,
    new_manifests: BTreeMap<Hash, Vec<u8>>,
    stale_temps: Vec<(Vec<Name>, Vec<u8>)>,
    rescued_temps: Vec<(Vec<Name>, Vec<u8>, Fingerprint)>,
    /// Each directory with what it looked like when listed.
    dirs: Vec<Listed>,
    excluded: Vec<Vec<Name>>,
    /// Entries seen so far that are neither excluded nor directories holding only what is.
    solid: u64,
    excluded_ids: Vec<(u64, u64, Option<Timestamp>, bool)>,
    stopped: bool,
    /// What a scan with a scope takes from the agreement once the walk is done: each folder's object,
    /// its place in `folders`, the names in it read here, and what its node's inode says it is.
    take: Vec<(ObjectId, u32, BTreeSet<Name>, Option<ObjectId>)>,
    listed: BTreeSet<Vec<Name>>,
    /// Folders taken from the agreement with something excluded under them, for the folders above.
    hidden_excluded: usize,
    /// A name the scope gave is in a folder listed under another form, as a filesystem that ignores
    /// case finds it: the scope cannot be matched to the tree, and the whole tree is listed instead.
    scope_missed: bool,
}

/// What was moved aside by a killed run, if it can be someone's only copy of something: not while another
/// name holds it, and not if it is the file as this device has it on record, or the finished download
/// that was about to replace it.
pub fn aside_holds_edit(dir: &Dir, temp: &[u8], opts: &ScanOptions) -> Option<Fingerprint> {
    let origin = aside_origin(temp.strip_prefix(opts.temp_prefix)?)?;
    let st = dir.stat(temp).ok()?;
    if st.nlink > 1 && st.file_type != FileType::Directory {
        return None;
    }
    let content = match st.file_type {
        FileType::RegularFile => dir
            .open_read(temp)
            .and_then(|fd| manifest_of(fd, st.size))
            .ok()
            .map(|m| Content::File {
                manifest: m.hash(),
                size: st.size,
            }),
        FileType::Symlink => dir.read_link(temp).ok().map(|target| Content::Symlink {
            target: Bytes(target),
        }),
        _ => None,
    };
    let same_inode = opts
        .rows
        .known(&(st.dev, st.ino))
        .filter(|row| birth_mark(row.fp.btime) == birth_mark(st.btime));
    let on_record = same_inode.is_some_and(|row| Some(&row.content) == content.as_ref());
    let on_its_way = opts.arriving.iter().any(|(on_its_way, names)| {
        Some(on_its_way) == content.as_ref()
            && names.iter().any(|n| n.as_bytes().starts_with(origin))
    });
    (!on_record && !on_its_way).then(|| fingerprint(&st))
}

/// The skip a read refused or failed makes, in the system's own words: not allowed, held by another
/// program for now, or failed. Each is held as it was and read again, never taken for a delete.
fn read_failure(e: &std::io::Error) -> (SkipKind, String) {
    match crate::refusal::of(e) {
        Some(Refusal::Permission) => (SkipKind::PermissionDenied, describe(e)),
        // Another program has it open and lets no one else read it, as a program writing a file
        // often does on Windows. Its close is the end of that.
        Some(Refusal::Busy) if is_busy(e) => (
            SkipKind::KeepsChanging,
            format!("another program has it open and lets no one read it: {e}"),
        ),
        _ => (SkipKind::ReadError, describe(e)),
    }
}

impl Walk<'_> {
    /// A folder found in the one the walk is in, named `name`.
    fn new_folder(&mut self, name: &Name) -> u32 {
        let parent = self.open.last().map_or(ROOT_FOLDER, |o| o.folder);
        self.folders.push(Folder {
            parent,
            name: Some(name.clone()),
            node: None,
            object: None,
            quiet: (0, 0),
            vouched: (0, 0),
        });
        (self.folders.len() - 1) as u32
    }

    /// The walk's next node, in the folder it is in; `folder` if it is a folder's own, `linked` if it is
    /// a hard link, whose later names come to it.
    fn push(&mut self, node: Scanned, folder: Option<u32>, linked: bool) {
        let seq = self.seq;
        self.seq += 1;
        if !self.opts.discard
            && !self.inodes.insert((node.fp.dev, node.fp.ino))
            && self.inode_twice.is_none()
        {
            self.inode_twice = Some(
                node.paths
                    .first()
                    .map_or_else(String::new, |p| display_path(p)),
            );
        }
        if let Some(all) = &mut self.all {
            all.push((!linked).then(|| node.clone()));
        }
        // What a sieve sorts is counted into the digest as it is sorted, on several threads.
        if let (Some(d), false, None) = (&mut self.digest, linked, self.opts.sieve) {
            d.node(seq, &node, self.opts.rows);
        }
        if self.opts.discard && !linked {
            return;
        }
        // Without a sieve, or a node more names may come to: kept whole, as found.
        if self.opts.sieve.is_none() || linked {
            let slot = self.nodes.len() as u32;
            if linked {
                self.linked_slots.push(slot);
            }
            if let Some(f) = folder {
                self.folders[f as usize].node = Some(Slot::Kept(slot));
            }
            self.nodes.push(node);
            self.seqs.push(seq);
            return;
        }
        match self.open.last_mut() {
            Some(open) => open.children.push((seq, node, folder)),
            None => {
                self.nodes.push(node);
                self.seqs.push(seq);
            }
        }
    }

    /// The nodes held for a folder, now that its own is read, which says what it is: each kept whole or
    /// quiet, as the sieve says. Held with others until there are enough to sort on several threads, in
    /// the order they came.
    fn settle(&mut self, open: Open, object: Option<ObjectId>) {
        if self.opts.sieve.is_none() || open.children.is_empty() {
            return self.settled(open, object, Vec::new());
        }
        self.done_nodes += open.children.len();
        self.done.push((open, object));
        if self.done_nodes >= SETTLE_BATCH {
            self.flush();
        }
    }

    /// Every folder held for sorting, sorted.
    fn flush(&mut self) {
        let done = std::mem::take(&mut self.done);
        self.done_nodes = 0;
        let Some(sieve) = self.opts.sieve else {
            for (open, object) in done {
                self.settled(open, object, Vec::new());
            }
            return;
        };
        let each: Vec<(Option<ObjectId>, u32, &Scanned)> = done
            .iter()
            .flat_map(|(open, object)| {
                open.children
                    .iter()
                    .map(move |(seq, n, _)| (*object, *seq, n))
            })
            .collect();
        let (rows, digest) = (self.opts.rows, self.digest.is_some());
        let sorted = crate::incr::par_map(each.len(), |i| {
            let (object, seq, node) = each[i];
            let term = digest.then(|| crate::steady::node_term(seq, node, rows));
            let quiet = sieve.quiet(object, node).map(|x| (x, sieve.witness(node)));
            (quiet, term)
        });
        drop(each);
        let mut decided = Vec::with_capacity(sorted.len());
        for (quiet, term) in sorted {
            if let (Some(d), Some(term)) = (&mut self.digest, term) {
                d.add(term);
            }
            decided.push(quiet);
        }
        let mut sorted = decided.into_iter();
        for (open, object) in done {
            let these: Vec<_> = sorted.by_ref().take(open.children.len()).collect();
            self.settled(open, object, these);
        }
    }

    /// A folder's nodes, each as `sorted` says: quiet, with its object and witness, or kept whole.
    fn settled(
        &mut self,
        open: Open,
        object: Option<ObjectId>,
        sorted: Vec<Option<(ObjectId, Option<[u8; 16]>)>>,
    ) {
        let start = self.quiet.len() as u32;
        let mut sorted = sorted.into_iter();
        for (seq, node, folder) in open.children {
            let slot = match sorted.next().flatten() {
                Some((x, witness)) => {
                    if let Some(w) = witness {
                        self.witnesses.push(w);
                    }
                    self.quiet.push(Quiet {
                        seq,
                        folder: open.folder,
                        object: x,
                    });
                    Slot::Quiet((self.quiet.len() - 1) as u32)
                }
                None => {
                    self.nodes.push(node);
                    self.seqs.push(seq);
                    Slot::Kept((self.nodes.len() - 1) as u32)
                }
            };
            if let Some(f) = folder {
                self.folders[f as usize].node = Some(slot);
            }
        }
        let f = &mut self.folders[open.folder as usize];
        f.quiet = (start, self.quiet.len() as u32);
        f.object = object;
    }

    /// Into a folder, whose entries are held until it is done, where there is a sieve.
    fn enter(&mut self, folder: u32) {
        self.open.push(Open {
            folder,
            children: Vec::new(),
        });
    }

    /// Out of a folder, whose own node is `node`, `None` where it went before its node was read: what was
    /// held for it settled by what the node says the folder is.
    fn leave(&mut self, node: Option<&Scanned>) {
        let object = node.and_then(|n| self.opts.sieve.and_then(|s| s.folder(n)));
        self.leave_as(object);
    }

    fn leave_as(&mut self, object: Option<ObjectId>) {
        if let Some(open) = self.open.pop() {
            self.settle(open, object);
        }
    }

    fn skip(&mut self, path: &[Name], kind: SkipKind, detail: Option<String>) {
        self.skips.push(Skip {
            path: display_path(path),
            kind,
            detail,
        });
    }

    fn unobservable(&mut self, path: &[Name], kind: SkipKind, detail: Option<String>) {
        self.skip(path, kind, detail);
        if matches!(kind, SkipKind::PermissionDenied | SkipKind::ReadError) {
            self.refused.push(path.to_vec());
        }
        self.unobserved.push(path.to_vec());
        match self.current_inode {
            Some(inode) => self.unobserved_inodes.push(inode),
            None => self.unidentified = true,
        }
    }

    /// Returns whether the directory could be listed.
    fn walk_dir(&mut self, dir: &Dir, path: &mut Vec<Name>, st_mnt: u64, at: At<'_>) -> bool {
        hit("scan.dir");
        if self.opts.scope.is_some() {
            self.listed.insert(path.clone());
        }
        let is_root = path.is_empty();
        let listed = dir.stat_self().and_then(|st| Ok((st, dir.list()?)));
        let (st, names) = match listed {
            Ok(v) => v,
            Err(e) => {
                let (kind, detail) = read_failure(&e);
                self.skip(path, kind, Some(detail));
                self.unlisted.push(path.clone());
                return false;
            }
        };
        let mut removed_temp = false;
        let mut entries = Vec::with_capacity(names.len());
        for raw in names {
            if is_root && raw == STATE_DIR {
                continue;
            }
            if raw.starts_with(self.opts.temp_prefix) {
                if self.opts.probe {
                    self.stopped = true;
                    return true;
                }
                // Our own leftover from a killed run: nothing that has to stay, unless it is the old
                // file of a swap with an edit in it.
                if self.opts.remove_own_temps {
                    if let Some(fp) = aside_holds_edit(dir, &raw, self.opts) {
                        self.rescued_temps.push((path.clone(), raw, fp));
                        continue;
                    }
                    if dir
                        .stat(&raw)
                        .is_ok_and(|s| s.file_type != FileType::Directory)
                    {
                        match dir.unlink(&raw) {
                            Ok(()) => removed_temp = true,
                            Err(_) => self.stale_temps.push((path.clone(), raw)),
                        }
                    }
                }
                continue;
            }
            match Name::new(raw) {
                Ok(name) => entries.push(name),
                Err(e) => self.skip(path, SkipKind::ReadError, Some(e.to_string())),
            }
        }
        if let (Some(t), false) = (at.trie, at.whole) {
            for name in t.children.keys() {
                if !entries.contains(name) && dir.stat(name.as_bytes()).is_ok() {
                    self.scope_missed = true;
                }
            }
        }
        // Removing a temporary moved this directory's own timestamps.
        let st = if removed_temp {
            dir.stat_self().unwrap_or(st)
        } else {
            st
        };
        let folder = self.open.last().map_or(ROOT_FOLDER, |o| o.folder);
        self.dirs
            .push((folder, (st.dev, st.ino), st.mtime, st.ctime));
        for name in entries {
            if self.stopped {
                break;
            }
            path.push(name);
            self.entry(dir, path, st_mnt, at);
            path.pop();
        }
        true
    }

    fn entry(&mut self, dir: &Dir, path: &mut Vec<Name>, parent_mnt: u64, at: At<'_>) {
        let name = path.last().expect("an entry has a name").clone();
        let raw = name.as_bytes();
        self.current_inode = None;
        crate::progress::entry();
        let st = match dir.stat(raw) {
            Ok(st) => st,
            Err(e) if is_not_found(&e) => {
                return self.unobservable(path, SkipKind::VanishedDuringScan, None);
            }
            Err(e) => {
                let (kind, detail) = read_failure(&e);
                return self.unobservable(path, kind, Some(detail));
            }
        };
        self.current_inode = Some((st.dev, st.ino));
        if (self.opts.excluded)(path, &st) {
            self.excluded.push(path.clone());
            let is_dir = st.file_type == FileType::Directory;
            self.excluded_ids.push((st.dev, st.ino, st.btime, is_dir));
            return;
        }
        if st.file_type != FileType::Directory {
            self.solid += 1;
        }
        if st.mnt_id != 0 && parent_mnt != 0 && st.mnt_id != parent_mnt {
            return self.unobservable(path, SkipKind::MountPoint, None);
        }
        match st.file_type {
            FileType::RegularFile => self.file(dir, path, st),
            FileType::Directory => self.subdir(dir, path, st, at),
            FileType::Symlink => self.symlink(dir, path, st),
            FileType::Fifo => self.skip(path, SkipKind::Fifo, None),
            FileType::Socket => self.skip(path, SkipKind::Socket, None),
            FileType::CharacterDevice => self.skip(path, SkipKind::CharDevice, None),
            FileType::BlockDevice => self.skip(path, SkipKind::BlockDevice, None),
            FileType::Unknown => self.skip(
                path,
                SkipKind::Unsupported,
                Some(crate::fsops::UNKNOWN_TYPE.into()),
            ),
        }
    }

    /// The second and later names of an inode join the first.
    fn seen_link(&mut self, path: &[Name], st: &Stat) -> bool {
        if st.nlink < 2 {
            return false;
        }
        match self.linked.get(&(st.dev, st.ino)) {
            Some(&i) => {
                self.nodes[i].paths.push(path.to_vec());
                true
            }
            None => {
                self.linked.insert((st.dev, st.ino), self.nodes.len());
                false
            }
        }
    }

    fn capture_meta(
        &mut self,
        dir: &Dir,
        path: &[Name],
        raw: &[u8],
        st: &Stat,
    ) -> (Metadata, bool) {
        let mut problems = Vec::new();
        let m = capture(
            dir,
            raw,
            st,
            self.opts.accounts,
            self.precision_ns,
            &mut problems,
        );
        let incomplete = !problems.is_empty();
        if incomplete {
            self.skip(path, SkipKind::ReadError, Some(problems.join("; ")));
        }
        (m, incomplete)
    }

    fn file(&mut self, dir: &Dir, path: &[Name], st: Stat) {
        if self.seen_link(path, &st) {
            return;
        }
        let raw = path.last().expect("an entry has a name").as_bytes();
        let fp = fingerprint(&st);
        let linked = st.nlink > 1;
        if let Some(row) = self.opts.rows.known(&(st.dev, st.ino)) {
            if row.vouches_for(&fp) {
                let node = Scanned {
                    paths: vec![path.to_vec()],
                    fp,
                    racy: false,
                    content: row.content.clone(),
                    metadata: row.scan.cloned().unwrap_or_default(),
                    metadata_incomplete: false,
                    holds_excluded: false,
                    only_excluded: false,
                };
                self.push(node, None, linked);
                return;
            }
        }
        for attempt in 0..READ_ATTEMPTS {
            let settling = dir.stat(raw).unwrap_or_else(|_| st.clone());
            wait_until_stable(&settling);
            let opened =
                cairn_failpoint::refuse("scan.open", raw).and_then(|()| dir.open_read_once(raw));
            let fd = match opened {
                Ok(fd) => fd,
                Err(e) if is_not_found(&e) => {
                    self.forget_link(&st);
                    return self.unobservable(path, SkipKind::VanishedDuringScan, None);
                }
                // Refused, by an access list, a lock or a program that guards files: held as it was,
                // in the system's own words, and read again next time. Never a delete.
                Err(e) => {
                    self.forget_link(&st);
                    let (kind, detail) = read_failure(&e);
                    return self.unobservable(path, kind, Some(detail));
                }
            };
            let before = match dir.stat(raw) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if before.file_type != FileType::RegularFile || before.ctime != settling.ctime {
                continue;
            }
            if let Some(why) = self.being_written(&fd, &before) {
                self.forget_link(&st);
                return self.unobservable(path, SkipKind::KeepsChanging, Some(why.into()));
            }
            if self.opts.probe {
                self.forget_link(&st);
                self.stopped = true;
                return;
            }
            crate::progress::reading(&display_path(path), before.size);
            hit("scan.file.reading");
            let read = manifest_of(&fd, before.size);
            crate::progress::read_done();
            crate::fsops::read_once_done(fd.as_fd(), 0, before.size);
            let manifest = match read {
                Ok(m) => m,
                Err(e) => {
                    // A file shrinking under the read ends the read early: that is a change, not damage.
                    if attempt + 1 < READ_ATTEMPTS {
                        continue;
                    }
                    self.forget_link(&st);
                    let (kind, detail) = read_failure(&e);
                    return self.unobservable(path, kind, Some(detail));
                }
            };
            let after = match crate::fsops::fstat(fd.as_fd()) {
                Ok(a) => a,
                Err(_) => continue,
            };
            let same_inode = after.ino == before.ino;
            let (mtime, ctime) = dir
                .stat(raw)
                .map(|s| (s.mtime, s.ctime))
                .unwrap_or_default();
            if !same_inode
                || after.size != before.size
                || mtime != before.mtime
                || ctime != before.ctime
            {
                continue;
            }
            let (metadata, metadata_incomplete) = self.capture_meta(dir, path, raw, &before);
            let bytes = manifest.encode();
            let hash = Hash::of(&bytes);
            self.new_manifests.insert(hash, bytes);
            let node = Scanned {
                paths: vec![path.to_vec()],
                fp: fingerprint(&before),
                racy: is_racy(&before, self.precision_ns),
                content: Content::File {
                    manifest: hash,
                    size: before.size,
                },
                metadata,
                metadata_incomplete,
                holds_excluded: false,
                only_excluded: false,
            };
            self.push(node, None, linked);
            return;
        }
        self.forget_link(&st);
        self.unobservable(path, SkipKind::KeepsChanging, None);
    }

    /// Why this file, steady as it reads, may be halfway through a save. A program that truncates and
    /// then writes leaves an empty file, and one that writes in pieces a part of one.
    fn being_written(&self, fd: &OwnedFd, st: &Stat) -> Option<&'static str> {
        let recent = now_ns() - ns(st.ctime) < WRITER_WINDOW_NS;
        let emptied = st.size == 0
            && self
                .opts
                .rows
                .known(&(st.dev, st.ino))
                .is_some_and(|row| row.fp.size > 0);
        match open_for_writing(fd.as_fd()) {
            Some(true) if emptied => {
                Some("emptied, and another program still has it open to write")
            }
            Some(true) if recent => Some("another program has it open and is writing it"),
            None if emptied && recent => {
                Some("emptied a moment ago, as a program saving it does first")
            }
            _ => None,
        }
    }

    fn forget_link(&mut self, st: &Stat) {
        self.linked.remove(&(st.dev, st.ino));
    }

    fn symlink(&mut self, dir: &Dir, path: &[Name], st: Stat) {
        if self.seen_link(path, &st) {
            return;
        }
        let raw = path.last().expect("an entry has a name").as_bytes();
        match dir.read_link(raw) {
            Ok(target) => {
                let (metadata, metadata_incomplete) = self.capture_meta(dir, path, raw, &st);
                let node = Scanned {
                    paths: vec![path.to_vec()],
                    fp: fingerprint(&st),
                    racy: is_racy(&st, self.precision_ns),
                    content: Content::Symlink {
                        target: Bytes(target),
                    },
                    metadata,
                    metadata_incomplete,
                    holds_excluded: false,
                    only_excluded: false,
                };
                self.push(node, None, st.nlink > 1);
            }
            Err(e) => {
                self.forget_link(&st);
                let kind = if is_not_found(&e) {
                    SkipKind::VanishedDuringScan
                } else if is_invalid(&e) {
                    // Replaced by something else since the stat.
                    SkipKind::VanishedDuringScan
                } else {
                    SkipKind::ReadError
                };
                self.unobservable(path, kind, Some(e.to_string()));
            }
        }
    }

    /// A folder, in a scan with a scope: listed if the scope says so or it is not as the agreement left
    /// it, gone through without listing on the way to one listed below it, and otherwise taken whole
    /// from the agreement.
    fn subdir(&mut self, dir: &Dir, path: &mut Vec<Name>, st: Stat, parent: At<'_>) {
        let Some(scope) = self.opts.scope else {
            return self.listed_subdir(dir, path, st, At::WHOLE);
        };
        if parent.whole {
            return self.listed_subdir(dir, path, st, At::WHOLE);
        }
        let name = path.last().expect("an entry has a name").clone();
        let object = parent
            .object
            .and_then(|p| scope.vouch.object_at(p, &name, &st));
        let trie = parent.trie.and_then(|t| t.children.get(&name));
        let listed = At {
            trie,
            whole: trie.is_some_and(|t| t.whole),
            object,
        };
        match trie {
            Some(t) if t.list || t.whole => self.listed_subdir(dir, path, st, listed),
            Some(t) => {
                if !self.through(dir, path, &st, t, object) {
                    self.listed_subdir(dir, path, st, listed);
                }
            }
            None => {
                if !self.taken(dir, path, &st, object) {
                    self.listed_subdir(dir, path, st, listed);
                }
            }
        }
    }

    /// A folder's own node as the agreement had it, read now, if it is as the agreement found it and
    /// `except` and what is under the rest are the agreement's to vouch for.
    fn as_agreed(
        &self,
        dir: &Dir,
        path: &[Name],
        st: &Stat,
        object: Option<ObjectId>,
        except: &BTreeSet<Name>,
    ) -> Option<(ObjectId, Scanned)> {
        let scope = self.opts.scope?;
        let x = object?;
        if !scope.vouch.takes_all_but(x, except) {
            return None;
        }
        let raw = path.last()?.as_bytes();
        let mut problems = Vec::new();
        let metadata = capture(
            dir,
            raw,
            st,
            self.opts.accounts,
            self.precision_ns,
            &mut problems,
        );
        if !problems.is_empty() {
            return None;
        }
        let mut node = Scanned {
            paths: vec![path.to_vec()],
            fp: fingerprint(st),
            racy: false,
            content: Content::Dir,
            metadata,
            metadata_incomplete: false,
            holds_excluded: false,
            only_excluded: false,
        };
        scope.vouch.quiet(x, &mut node).then_some((x, node))
    }

    /// The node of a folder taken from the agreement, at `folder`, counted as a walk of it would count it.
    fn keep_agreed(&mut self, x: ObjectId, node: Scanned, except: BTreeSet<Name>, folder: u32) {
        if node.holds_excluded {
            self.hidden_excluded += 1;
        }
        if !node.only_excluded {
            self.solid += 1;
        }
        let is = self.opts.sieve.and_then(|s| s.folder(&node));
        self.push(node, Some(folder), false);
        self.take.push((x, folder, except, is));
    }

    /// A folder outside the scope, taken from the agreement if it is as the agreement left it.
    fn taken(&mut self, dir: &Dir, path: &[Name], st: &Stat, object: Option<ObjectId>) -> bool {
        let Some((x, node)) = self.as_agreed(dir, path, st, object, &BTreeSet::new()) else {
            return false;
        };
        let folder = self.new_folder(path.last().expect("an entry has a name"));
        self.keep_agreed(x, node, BTreeSet::new(), folder);
        true
    }

    /// A folder above some the scope lists: each of those gone into, without listing this one, if it is
    /// as the agreement left it and so is everything else in it, which is taken from the agreement.
    fn through(
        &mut self,
        dir: &Dir,
        path: &mut Vec<Name>,
        st: &Stat,
        t: &Trie,
        object: Option<ObjectId>,
    ) -> bool {
        let Some(scope) = self.opts.scope else {
            return false;
        };
        let except: BTreeSet<Name> = t.children.keys().cloned().collect();
        let Some((x, node)) = self.as_agreed(dir, path, st, object, &except) else {
            return false;
        };
        if !except.iter().all(|n| scope.vouch.has_child(x, n)) {
            return false;
        }
        let raw = path.last().expect("an entry has a name").clone();
        let Ok(child) = dir.open_dir(raw.as_bytes()) else {
            return false;
        };
        // Each folder on the way down is one the agreement had, and still a folder.
        for name in &except {
            if !child
                .stat(name.as_bytes())
                .is_ok_and(|s| s.file_type == FileType::Directory)
            {
                return false;
            }
        }
        hit("scan.dir.through");
        let at = At {
            trie: Some(t),
            whole: false,
            object: Some(x),
        };
        let folder = self.new_folder(&raw);
        self.enter(folder);
        for name in &except {
            path.push(name.clone());
            self.entry(&child, path, st.mnt_id, at);
            path.pop();
        }
        self.leave(Some(&node));
        self.keep_agreed(x, node, except, folder);
        true
    }

    fn listed_subdir(&mut self, dir: &Dir, path: &mut Vec<Name>, st: Stat, at: At<'_>) {
        let raw = path.last().expect("an entry has a name").clone();
        let raw = raw.as_bytes();
        match dir.open_dir(raw) {
            Ok(child) => {
                let (excluded_before, solid_before) =
                    (self.excluded.len() + self.hidden_excluded, self.solid);
                let folder = self.new_folder(path.last().expect("an entry has a name"));
                self.enter(folder);
                let listed = self.walk_dir(&child, path, st.mnt_id, at);
                let holds_excluded = self.excluded.len() + self.hidden_excluded > excluded_before;
                let only_excluded = holds_excluded && listed && self.solid == solid_before;
                // An empty directory is as real as a file, to whatever holds it.
                if !only_excluded && self.solid == solid_before {
                    self.solid += 1;
                }
                self.current_inode = Some((st.dev, st.ino));
                // Read after the walk: removing a leftover temporary moves the times.
                let st = match dir.stat(raw) {
                    Ok(s) if s.file_type == FileType::Directory => s,
                    _ => {
                        self.leave(None);
                        return self.unobservable(path, SkipKind::VanishedDuringScan, None);
                    }
                };
                let (metadata, incomplete) = self.capture_meta(dir, path, raw, &st);
                let metadata_incomplete = incomplete || !listed;
                let node = Scanned {
                    paths: vec![path.clone()],
                    fp: fingerprint(&st),
                    racy: false,
                    content: Content::Dir,
                    metadata,
                    metadata_incomplete,
                    holds_excluded,
                    only_excluded,
                };
                self.leave(Some(&node));
                self.push(node, Some(folder), false);
            }
            Err(e) if crate::refusal::of(&e) == Some(Refusal::Permission) => {
                self.solid += 1;
                self.skip(path, SkipKind::PermissionDenied, Some(describe(&e)));
                self.unlisted.push(path.clone());
                // A directory no one here can read must not push that state to devices that can:
                // like its contents, its record stands as it was until it can be observed.
                let (metadata, _) = self.capture_meta(dir, path, raw, &st);
                let node = Scanned {
                    paths: vec![path.clone()],
                    fp: fingerprint(&st),
                    racy: false,
                    content: Content::Dir,
                    metadata,
                    metadata_incomplete: true,
                    holds_excluded: false,
                    only_excluded: false,
                };
                let folder = self.new_folder(path.last().expect("an entry has a name"));
                self.push(node, Some(folder), false);
            }
            Err(e) if is_not_found(&e) => {
                self.solid += 1;
                self.unobservable(path, SkipKind::VanishedDuringScan, None)
            }
            // Replaced by a symlink or a file since the stat: never walked through.
            Err(e) => {
                self.solid += 1;
                self.unobservable(path, SkipKind::VanishedDuringScan, Some(e.to_string()))
            }
        }
    }

    /// Whether every directory listed still looks as it did then, at the same path: the same inode,
    /// and times that an entry made, removed or renamed in it would have moved. So an entry moved
    /// from a folder the walk had yet to list into one it had listed, which neither listing holds, is
    /// a scan to take again, not a delete. Through no symlink, so a folder swapped for one counts.
    fn dirs_unchanged(&self, root: &Dir) -> bool {
        self.dirs.iter().all(|(folder, inode, mtime, ctime)| {
            root.stat_dir_at(&path_of(&self.folders, *folder))
                .is_ok_and(|s| (s.dev, s.ino) == *inode && s.mtime == *mtime && s.ctime == *ctime)
        })
    }

    /// What a scope took from the agreement, each folder taken all the way down but what the walk read
    /// elsewhere: quiet where the sieve says so, and otherwise whole, as the agreement found it.
    fn take_from_agreement(&mut self, vouch: &dyn Vouch) {
        // What was read stands; of what is taken, an inode taken twice is the tree moving.
        let mut taken = Inodes::default();
        for (x, folder, except, is) in std::mem::take(&mut self.take) {
            // Each folder with the object the ledger has it as, and the one its inode says.
            let mut stack: Vec<(ObjectId, u32, Option<ObjectId>, bool)> =
                vec![(x, folder, is, true)];
            while let Some((f, at, is, top)) = stack.pop() {
                if self.folders[at as usize].object.is_none() {
                    self.folders[at as usize].object = is;
                }
                let start = self.quiet.len() as u32;
                let mut path: Option<Vec<Name>> = None;
                for child in vouch.children(f) {
                    let (object, dir, inode) = (child.object, child.dir, child.inode);
                    if top && except.iter().any(|e| e.as_bytes() == child.name) {
                        continue;
                    }
                    if self.inodes.contains(&inode) {
                        continue;
                    }
                    let quiet = self
                        .opts
                        .sieve
                        .is_some_and(|s| !s.witnessing() && s.quiet_vouched(is, &child));
                    // A name of its own only where something keeps one.
                    let name = || Name::new(child.name.to_vec()).ok();
                    let whole = (!quiet || self.all.is_some()).then(|| {
                        let p = path.get_or_insert_with(|| path_of(&self.folders, at));
                        let mut p = p.clone();
                        p.push(name()?);
                        vouch.node(object, p)
                    });
                    let whole = whole.flatten();
                    if !quiet && whole.is_none() {
                        continue;
                    }
                    let seq = self.seq;
                    self.seq += 1;
                    if !taken.insert(inode) && self.inode_twice.is_none() {
                        let mut p = path_of(&self.folders, at);
                        p.extend(name());
                        self.inode_twice = Some(display_path(&p));
                    }
                    let sub = dir.then(|| {
                        self.folders.push(Folder {
                            parent: at,
                            name: name(),
                            node: None,
                            object: None,
                            quiet: (0, 0),
                            vouched: (0, 0),
                        });
                        (self.folders.len() - 1) as u32
                    });
                    if let Some(all) = &mut self.all {
                        all.push(whole.clone());
                    }
                    let slot = match (quiet, whole) {
                        (true, _) => {
                            self.quiet.push(Quiet {
                                seq,
                                folder: at,
                                object,
                            });
                            Some(Slot::Quiet((self.quiet.len() - 1) as u32))
                        }
                        (false, Some(node)) => {
                            self.nodes.push(node);
                            self.seqs.push(seq);
                            Some(Slot::Kept((self.nodes.len() - 1) as u32))
                        }
                        (false, None) => None,
                    };
                    if let Some(sub) = sub {
                        self.folders[sub as usize].node = slot;
                        // What is in it is in the folder its node's inode says it is.
                        let in_it = match slot {
                            Some(Slot::Quiet(_)) => Some(object),
                            Some(Slot::Kept(k)) => self
                                .opts
                                .sieve
                                .and_then(|s| s.folder(&self.nodes[k as usize])),
                            None => None,
                        };
                        stack.push((object, sub, in_it, false));
                    }
                }
                self.folders[at as usize].vouched = (start, self.quiet.len() as u32);
            }
        }
    }
}

/// Where a folder is, from the root, in a walk's table of them.
fn path_of(folders: &[Folder], mut f: u32) -> Vec<Name> {
    let mut out = Vec::new();
    while f != ROOT_FOLDER {
        let folder = &folders[f as usize];
        if let Some(name) = &folder.name {
            out.push(name.clone());
        }
        f = folder.parent;
    }
    out.reverse();
    out
}

pub fn scan(root: &Dir, opts: &ScanOptions<'_>) -> std::io::Result<ScanResult> {
    let started = opts.started.unwrap_or_else(Instant::now);
    crate::progress::scanning(match opts.scope {
        None => opts.rows.count() as u64,
        Some(_) => 0,
    });
    let precision_ns = root.fstype_precision_ns();
    let mut walk = Walk {
        opts,
        precision_ns,
        nodes: Vec::new(),
        seqs: Vec::new(),
        quiet: Vec::new(),
        witnesses: Vec::new(),
        done: Vec::new(),
        done_nodes: 0,
        seq: 0,
        folders: vec![Folder {
            parent: ROOT_FOLDER,
            name: None,
            node: None,
            object: None,
            quiet: (0, 0),
            vouched: (0, 0),
        }],
        open: Vec::new(),
        inodes: Inodes::default(),
        inode_twice: None,
        linked_slots: Vec::new(),
        all: opts.keep_all.then(Vec::new),
        digest: opts.digest.then(crate::steady::Stream::new),
        linked: HashMap::new(),
        skips: Vec::new(),
        unobserved: Vec::new(),
        refused: Vec::new(),
        unobserved_inodes: Vec::new(),
        unidentified: false,
        current_inode: None,
        unlisted: Vec::new(),
        new_manifests: BTreeMap::new(),
        stale_temps: Vec::new(),
        rescued_temps: Vec::new(),
        dirs: Vec::new(),
        excluded: Vec::new(),
        solid: 0,
        excluded_ids: Vec::new(),
        stopped: false,
        take: Vec::new(),
        listed: BTreeSet::new(),
        hidden_excluded: 0,
        scope_missed: false,
    };
    let st = root.stat_self()?;
    let mut path = Vec::new();
    let trie = opts.scope.map(|s| Trie::of(s.list));
    let at = match &trie {
        Some(t) => At {
            trie: Some(t),
            whole: false,
            object: Some(ObjectId::ROOT),
        },
        None => At::WHOLE,
    };
    walk.enter(ROOT_FOLDER);
    walk.walk_dir(root, &mut path, st.mnt_id, at);
    walk.leave_as(Some(ObjectId::ROOT));
    walk.flush();
    if walk.scope_missed {
        hit("scan.scope_missed");
        let whole = ScanOptions {
            scope: None,
            started: Some(started),
            ..*opts
        };
        return scan(root, &whole);
    }
    hit("scan.done");
    // A probe that stopped says nothing more, and is not worth the second look.
    let unstable = !walk.stopped && !walk.dirs_unchanged(root);
    // Every name of a hard link is found by now.
    for slot in std::mem::take(&mut walk.linked_slots) {
        let node = &walk.nodes[slot as usize];
        if let Some(d) = &mut walk.digest {
            d.node(walk.seqs[slot as usize], node, opts.rows);
        }
        if let Some(all) = &mut walk.all {
            all[walk.seqs[slot as usize] as usize] = Some(node.clone());
        }
    }
    let vouched_from = walk.seq as usize;
    if let Some(scope) = opts.scope {
        walk.take_from_agreement(scope.vouch);
    }
    let mut folders_by_name: Vec<u32> = (1..walk.folders.len() as u32).collect();
    folders_by_name.sort_unstable_by(|a, b| {
        let (a, b) = (&walk.folders[*a as usize], &walk.folders[*b as usize]);
        (a.parent, &a.name).cmp(&(b.parent, &b.name))
    });
    // Without a sieve every node is the walk's in order.
    let seqs = opts.sieve.is_some().then_some(walk.seqs);

    let st = root.stat_self()?;
    let mut problems = Vec::new();
    let metadata = capture(root, b".", &st, opts.accounts, precision_ns, &mut problems);
    let mut result = ScanResult {
        root: Scanned {
            paths: Vec::new(),
            fp: fingerprint(&st),
            racy: false,
            content: Content::Dir,
            metadata,
            metadata_incomplete: !problems.is_empty(),
            holds_excluded: false,
            only_excluded: false,
        },
        total: walk.seq as usize,
        nodes: walk.nodes,
        seqs,
        quiet: walk.quiet,
        witnesses: walk.witnesses,
        folders: walk.folders,
        folders_by_name,
        all: walk.all.map(|all| all.into_iter().flatten().collect()),
        digest: None,
        inode_twice: walk.inode_twice,
        skips: walk.skips,
        unobserved: walk.unobserved,
        refused: walk.refused,
        unobserved_inodes: walk.unobserved_inodes,
        unidentified: walk.unidentified,
        unlisted: walk.unlisted,
        unstable,
        new_manifests: walk.new_manifests.into_iter().collect(),
        stale_temps: walk.stale_temps,
        rescued_temps: walk.rescued_temps,
        rescued: Vec::new(),
        excluded: walk.excluded,
        excluded_ids: walk.excluded_ids,
        stopped: walk.stopped,
        started,
        vouched_from,
        listed: opts.scope.map(|_| walk.listed),
    };
    if let Some(d) = walk.digest {
        result.digest = Some(d.finish(&result, opts.rows));
    }
    Ok(result)
}

/// A scan with a scope, given again the folders its first walk found an inode read here had been in at
/// the agreement and did not list, as when something moved there or a name was linked to it with no
/// event from where it was: so that what the agreement had there is read too, not taken.
pub fn scan_scoped(root: &Dir, opts: &ScanOptions<'_>) -> std::io::Result<ScanResult> {
    let Some(scope) = opts.scope else {
        return scan(root, opts);
    };
    let mut list = scope.list.clone();
    let mut result = scan(root, opts)?;
    for _ in 0..2 {
        let Some(listed) = &result.listed else {
            return Ok(result);
        };
        let mut more = Vec::new();
        for folder in scope.vouch.agreed_elsewhere(&result) {
            if !listed.contains(&folder) && !list.contains_key(&folder) && !more.contains(&folder) {
                more.push(folder);
            }
        }
        if more.is_empty() {
            return Ok(result);
        }
        for f in more {
            list.insert(f, false);
        }
        let again = Scope {
            list: &list,
            vouch: scope.vouch,
        };
        let opts = ScanOptions {
            scope: Some(&again),
            ..*opts
        };
        result = scan(root, &opts)?;
    }
    Ok(result)
}
