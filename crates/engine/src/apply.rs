//! Making the local tree match a plan. Every file arrives the same way: temporary in the same
//! directory → verified → fsync → metadata → check the destination has not changed → atomic rename.
//! No step overwrites anything it has not checked, and a kill at any point leaves whole files at
//! synced paths (invariant 4).

use crate::client::Client;
use crate::fsops::{self, is_exists, is_not_found, is_permission, AsFd, Dir, OwnedFd};
use crate::identify::{Arriving, CopyIntent};
use crate::merge::{ConflictCopy, Plan, TNode};
use crate::metadata::{self, Accounts};
use crate::root::Root;
use crate::scan::{fingerprint, racy_within, Scanned, RACY_WINDOW_NS};
use crate::schedule::{is_stage, schedule, Topo, TopoOp};
use crate::state::{Fingerprint, Row, Synced};
use anyhow::{anyhow, Context, Result};
use cairn_failpoint::{check, hit, refuse_name};
use cairn_protocol::wire::MAX_CHUNK_BATCH_BYTES;
use cairn_protocol::{
    Bytes, ChunkRef, Content, Hash, Link, Manifest, Metadata, Name, ObjectId, ObjectKind, Skip,
    SkipKind, Tree, STAGE_PREFIX,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

/// The losing side of a same-field metadata conflict, for the activity log: noted before the first
/// step, since once the winner is applied nothing on disk says there was a conflict, and gone once the
/// commit that carries it is queued.
#[derive(Debug, Serialize, Deserialize)]
pub struct NoteIntent {
    pub object: ObjectId,
    pub text: String,
}

impl NoteIntent {
    pub fn key(&self) -> String {
        let digest = blake3::hash(self.text.as_bytes());
        format!(
            "{INTENT_NOTE}{}/{}",
            self.object,
            hex::encode(&digest.as_bytes()[..8])
        )
    }
}

/// A staged entry and where it came from, so that a crash can put it back.
#[derive(Debug, Serialize, Deserialize)]
pub struct StageIntent {
    pub stage: String,
    pub origin: Vec<Name>,
    /// Where it was going, for when something else has already taken its origin: half a swap.
    #[serde(default)]
    pub dest: Option<Vec<Name>>,
}

/// A directory made writable for a moment, and the mode it must get back.
#[derive(Debug, Serialize, Deserialize)]
pub struct ModeIntent {
    pub path: Vec<Bytes>,
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
}

pub const INTENT_STAGE: &str = "stage/";
pub const INTENT_MODE: &str = "mode/";
pub const INTENT_COPY: &str = "copy/";
pub const INTENT_RELINK: &str = "relink/";
pub const INTENT_ARRIVING: &str = "arriving/";
pub const INTENT_NOTE: &str = "note/";

/// New content on its way to every hard link of a file.
#[derive(Debug, Serialize, Deserialize)]
pub struct RelinkIntent {
    pub first: Vec<Name>,
    pub others: Vec<Vec<Name>>,
    pub new_ino: u64,
    pub old: Fingerprint,
    #[serde(default)]
    pub old_manifest: Option<Hash>,
}

/// The current local file of an object: its directory, name, and where each of its chunks sits.
type OldVersion = (Dir, Vec<u8>, BTreeMap<Hash, u64>);

const NOT_OBSERVED: &str = "not observed by this scan, so a newer version from the server waits";

/// Manifests asked of the server in one request, and stored in one transaction: one each would cost
/// a round trip and an fsync for every file that comes down.
const MANIFESTS_A_REQUEST: usize = 2_000;

/// What the chunk cache takes on at once for what is about to be placed. A download larger than this
/// comes in slices, each dropped from the cache as its files are written, so that it needs about the
/// tree's size free and not twice that.
const SLICE_BYTES: u64 = 1 << 30;

/// Rows this run writes, stored together: one transaction, and one fsync, for this many.
const ROWS_A_TRANSACTION: u64 = 4_096;

/// Files placed and watched at once, each until a later write could no longer leave its timestamps as
/// they are. A watch holds nobody back: it only notes what is done to the file meanwhile.
const WATCHED_AT_ONCE: usize = 64;

/// A file placed by this run, watched from its last write here: while the watch has seen nothing, nobody
/// has written to it or changed its metadata.
struct Watched {
    id: ObjectId,
    /// Opened read-only once written, to look at it when it settles.
    fd: OwnedFd,
    wd: i32,
    /// What it was once written and synced.
    wrote: Fingerprint,
}

/// The chunks a download still has to bring into the cache, and the files they are for.
#[derive(Default)]
struct Fetch {
    /// For each file still to be written, the chunks it takes from the cache, in the order written.
    needs: HashMap<ObjectId, VecDeque<(Hash, u32)>>,
    /// How many of those still to be written take each chunk.
    users: HashMap<Hash, u32>,
    /// In the cache now.
    cached: HashSet<Hash>,
    /// The files in the order this run writes them, as far as it is known, and where each one is.
    order: Vec<ObjectId>,
    at: HashMap<ObjectId, usize>,
}

/// A size as a person reads it.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// What a file of these bytes takes on disk, at most, in blocks of 4 KiB.
fn on_disk(bytes: u64) -> u64 {
    bytes.div_ceil(4096) * 4096
}

/// Why one step did not happen.
#[derive(Debug, thiserror::Error)]
enum StepError {
    /// The tree moved under this run; the next round sees it and decides again.
    #[error("{0}")]
    Changed(String),
    /// This step cannot be done here, and repeating it now would not help.
    #[error("{0}")]
    Failed(String),
    /// This filesystem cannot hold this name, and trying again will not change that. `twin`: another
    /// name here is the same name to it, and that may go.
    #[error("{why}")]
    Unholdable { why: String, twin: bool },
    /// Stop everything: nothing more can be written.
    #[error("disk full")]
    DiskFull(#[source] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<std::io::Error> for StepError {
    fn from(e: std::io::Error) -> Self {
        if fsops::is_disk_full(&e) {
            StepError::DiskFull(e)
        } else if is_exists(&e) || fsops::is_not_empty(&e) || is_not_found(&e) {
            StepError::Changed(e.to_string())
        } else {
            StepError::Failed(e.to_string())
        }
    }
}

/// Why making `name` here failed for good, if it did: the name's bytes are refused, or a name already
/// here is the same name to this filesystem (by case or Unicode normalization). Anything else, a name
/// taken a moment ago included, is for another round.
fn unholdable(dir: &Dir, name: &[u8], e: std::io::Error) -> StepError {
    let refused = |why: &str| StepError::Unholdable {
        why: why.into(),
        twin: false,
    };
    if fsops::is_bad_name(&e) {
        return refused(fsops::name_problem(name).unwrap_or(fsops::BAD_NAME));
    }
    if fsops::is_name_too_long(&e) {
        return refused("the name is too long for this filesystem");
    }
    if !is_exists(&e) {
        return e.into();
    }
    match twin_of(dir, name) {
        Some(why) => StepError::Unholdable { why, twin: true },
        None => e.into(),
    }
}

fn twin_of(dir: &Dir, name: &[u8]) -> Option<String> {
    let names = dir.list().ok()?;
    if names.iter().any(|n| n == name) {
        return None;
    }
    let st = dir.stat(name).ok()?;
    let twin = names.iter().find(|n| {
        dir.stat(n)
            .is_ok_and(|s| (s.dev, s.ino) == (st.dev, st.ino))
    });
    Some(match twin {
        Some(t) => format!(
            "`{}` here is the same name to this filesystem",
            cairn_protocol::name::display_bytes(t)
        ),
        None => "another name here is the same name to this filesystem".into(),
    })
}

/// Where one local object stands now.
#[derive(Clone, Debug)]
pub struct CurNode {
    pub kind: ObjectKind,
    pub links: BTreeSet<Link>,
    pub content: Content,
    pub apply_from: Metadata,
    /// `None` when it was not observed this scan: it cannot safely be touched.
    pub fp: Option<Fingerprint>,
    /// A directory with something this device excludes under it, which never goes with it.
    pub holds_excluded: bool,
}

#[derive(Debug, Default)]
pub struct ApplyOutcome {
    /// Something moved under this run; try again.
    pub changed: Vec<Skip>,
    /// Could not be applied; reported, and the server still has it.
    pub failed: Vec<Skip>,
    /// Metadata fields this filesystem could not take.
    pub warnings: Vec<Skip>,
    /// Paths whose metadata this run applied in full.
    pub meta_applied: Vec<String>,
    /// Names this filesystem cannot hold, each the topmost: what is under one was not tried.
    pub unholdable: Vec<(crate::selection::Unheld, String)>,
    pub applied_steps: u64,
    /// This run stopped before changing the tree's structure; nothing further should be pushed.
    pub halted: bool,
    /// The filesystem was written to, so a fresh scan must confirm the result.
    pub touched: bool,
    /// What this run placed and found gone before it could record it. A row is what proves that this
    /// device held an entry, and rows are written once the whole run's directories are durable, so a
    /// person removing what is arriving leaves no proof that it was here: it comes down again.
    pub gone: Vec<String>,
}

impl ApplyOutcome {
    pub fn clean(&self) -> bool {
        self.changed.is_empty() && self.failed.is_empty()
    }
}

pub struct Applier<'a> {
    pub root: &'a mut Root,
    pub client: Option<&'a Client>,
    pub accounts: &'a Accounts,
    pub cur: BTreeMap<ObjectId, CurNode>,
    pub rows: &'a mut BTreeMap<ObjectId, Row>,
    pub remote: &'a Tree,
    touched_dirs: BTreeSet<ObjectId>,
    /// Directories relaxed to be writable, with their original mode.
    relaxed: BTreeMap<ObjectId, u32>,
    /// Objects whose metadata and content were written by this run.
    placed: BTreeSet<ObjectId>,
    /// Objects that only got or lost an entry in this run.
    moved: BTreeSet<ObjectId>,
    superuser: bool,
    /// Which mount the root is on, or 0 where that cannot be told.
    root_mnt: u64,
    outcome: ApplyOutcome,
    fetch: Fetch,
    /// Rows written by this run and not yet stored.
    unstored: BTreeSet<ObjectId>,
    /// What notes writes to the files this run places, where the filesystem lets it see them all.
    writes: Option<fsops::WriteWatch>,
    /// The file `write_temp` has just written, watched, and what it was then.
    temp_watch: Option<(OwnedFd, i32, Fingerprint)>,
    /// Files placed by this run, still watched, oldest first.
    watched: VecDeque<Watched>,
    /// Files nothing opened to write between this run writing them and the moment a later write would
    /// have moved their timestamps: what is there is what was written, while the fingerprint holds.
    vouched: HashMap<ObjectId, Fingerprint>,
}

fn step_path(cur: &BTreeMap<ObjectId, CurNode>, link: &Link) -> Vec<Name> {
    let mut path = vec![link.name.clone()];
    let mut at = link.parent;
    while at != ObjectId::ROOT {
        let Some(up) = cur.get(&at).and_then(|n| n.links.iter().next()) else {
            break;
        };
        path.push(up.name.clone());
        at = up.parent;
    }
    path.reverse();
    path
}

fn show(path: &[Name]) -> String {
    crate::scan::display_path(path)
}

impl<'a> Applier<'a> {
    pub fn new(
        root: &'a mut Root,
        client: Option<&'a Client>,
        accounts: &'a Accounts,
        cur: BTreeMap<ObjectId, CurNode>,
        rows: &'a mut BTreeMap<ObjectId, Row>,
        remote: &'a Tree,
    ) -> Self {
        let root_mnt = root.dir.stat_self().map_or(0, |st| st.mnt_id);
        Self {
            root,
            client,
            accounts,
            cur,
            rows,
            remote,
            touched_dirs: BTreeSet::new(),
            relaxed: BTreeMap::new(),
            placed: BTreeSet::new(),
            moved: BTreeSet::new(),
            superuser: fsops::is_superuser(),
            root_mnt,
            outcome: ApplyOutcome::default(),
            fetch: Fetch::default(),
            unstored: BTreeSet::new(),
            writes: fsops::WriteWatch::new(),
            temp_watch: None,
            watched: VecDeque::new(),
            vouched: HashMap::new(),
        }
    }

    /// The open parent directory and final name of an entry.
    fn locate(&self, link: &Link) -> Result<(Dir, Vec<u8>), StepError> {
        if is_stage(link) {
            let stage = self.root.state_dir.open_dir(b"stage")?;
            let name = link.name.as_bytes()[STAGE_PREFIX.len()..].to_vec();
            return Ok((stage, name));
        }
        Ok((
            self.open_dir_object(link.parent)?,
            link.name.as_bytes().to_vec(),
        ))
    }

    fn open_dir_object(&self, o: ObjectId) -> Result<Dir, StepError> {
        if o == ObjectId::ROOT {
            return Ok(self.root.dir.dup()?);
        }
        let link = self
            .cur
            .get(&o)
            .and_then(|n| n.links.iter().next())
            .ok_or_else(|| StepError::Changed(format!("directory {o} is not there")))?
            .clone();
        let (dir, name) = self.locate(&link)?;
        let opened = dir.open_dir(&name)?;
        // As the scan does not cross into another filesystem mounted in the tree, nothing is put there.
        let mnt = opened.stat_self()?.mnt_id;
        if self.root_mnt != 0 && mnt != 0 && mnt != self.root_mnt {
            return Err(StepError::Failed(
                "another filesystem is mounted here; sync does not cross mounts".into(),
            ));
        }
        Ok(opened)
    }

    /// Make a directory writable for a moment if its mode forbids this user, remembering durably to
    /// put it back.
    fn writable(&mut self, parent: ObjectId, dir: &Dir) -> Result<(), StepError> {
        if self.superuser || self.relaxed.contains_key(&parent) {
            return Ok(());
        }
        let st = dir.stat_self()?;
        if st.mode & 0o300 == 0o300 {
            return Ok(());
        }
        let path: Vec<Bytes> = match self.cur.get(&parent).and_then(|n| n.links.iter().next()) {
            Some(link) => step_path(&self.cur, link)
                .into_iter()
                .map(|n| Bytes(n.as_bytes().to_vec()))
                .collect(),
            None => Vec::new(),
        };
        let key = format!("{INTENT_MODE}{parent}");
        self.root.state.put_intent(
            &key,
            &ModeIntent {
                path,
                dev: st.dev,
                ino: st.ino,
                mode: st.mode,
            },
        )?;
        hit("apply.relax.intent");
        dir.chmod(b".", st.mode | 0o300)?;
        hit("apply.relax.done");
        self.relaxed.insert(parent, st.mode);
        Ok(())
    }

    fn check_fp(&self, o: ObjectId, dir: &Dir, name: &[u8]) -> Result<(), StepError> {
        let node = self.cur.get(&o);
        let Some(fp) = node.and_then(|n| n.fp.as_ref()) else {
            return Err(StepError::Failed(NOT_OBSERVED.into()));
        };
        if node.is_some_and(|n| n.kind == ObjectKind::Dir) {
            return Ok(());
        }
        let now = dir
            .try_stat(name)?
            .ok_or_else(|| StepError::Changed("the file vanished".into()))?;
        if fingerprint(&now) != *fp {
            return Err(StepError::Changed(
                "the file changed during this sync".into(),
            ));
        }
        Ok(())
    }

    /// After this run renamed or linked a file its ctime has moved, and only that: a size or mtime
    /// that moved is someone's edit, which must stay visible to the next check.
    /// Size and mtime against the scan, for a file this run has just renamed, which moved its ctime.
    fn still_as_scanned(&self, o: ObjectId, dir: &Dir, name: &[u8]) -> bool {
        let fp = self.cur.get(&o).and_then(|n| n.fp.as_ref());
        match (fp, dir.stat(name)) {
            (Some(fp), Ok(st)) => (fp.ino, fp.size, fp.mtime) == (st.ino, st.size, st.mtime),
            _ => false,
        }
    }

    fn refresh_fp(&mut self, o: ObjectId, dir: &Dir, name: &[u8]) {
        if let (Some(n), Ok(st)) = (self.cur.get_mut(&o), dir.stat(name)) {
            let now = fingerprint(&st);
            if let Some(fp) = n.fp.as_mut() {
                if (
                    fp.dev,
                    fp.ino,
                    fp.size,
                    fp.mtime,
                    fsops::birth_mark(fp.btime),
                ) == (now.dev, now.ino, now.size, now.mtime, now.btime)
                {
                    fp.ctime = now.ctime;
                }
            }
        }
    }

    /// After this run set a file's metadata itself: everything may have moved.
    fn reset_fp(&mut self, o: ObjectId, dir: &Dir, name: &[u8]) {
        if let (Some(n), Ok(st)) = (self.cur.get_mut(&o), dir.stat(name)) {
            n.fp = Some(fingerprint(&st));
        }
    }

    fn manifest(&mut self, hash: &Hash) -> Result<Manifest> {
        if let Some(bytes) = self.root.state.manifest(hash)? {
            return Ok(Manifest::decode(&bytes)?);
        }
        let client = self
            .client
            .ok_or_else(|| anyhow!("manifest {hash} is not available offline"))?;
        let got = client.download(&[*hash])?;
        let (_, bytes) = got
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no manifest {hash}"))?;
        self.root.state.put_manifests(&[(*hash, bytes.clone())])?;
        Ok(Manifest::decode(&bytes)?)
    }

    fn cache_path(hash: &Hash) -> Vec<u8> {
        hash.to_string().into_bytes()
    }

    /// Before touching the tree: the manifests of the plan's new content, room for it on this disk, and
    /// the first slice of its chunks in the local cache, so that a full disk or a lost server shows up
    /// while nothing has changed yet.
    fn prefetch(&mut self, plan: &Plan) -> Result<(), StepError> {
        let Some(client) = self.client else {
            return Ok(());
        };
        // A conflict copy's content is here on disk, and the server has yet to see it.
        let copies: BTreeSet<ObjectId> = plan.copies.iter().map(|c| c.copy).collect();
        let files: Vec<(ObjectId, Hash)> = plan
            .target
            .iter()
            .filter_map(|(id, t)| match &t.body.content {
                Content::File { manifest, .. }
                    if !copies.contains(id)
                        && self.cur.get(id).is_none_or(|c| c.content != t.body.content) =>
                {
                    Some((*id, *manifest))
                }
                _ => None,
            })
            .collect();
        self.fetch_manifests(client, plan, &files)?;

        let cache = self.root.state_dir.open_dir(b"chunks")?;
        let (mut created, mut grows, mut biggest) = (0u64, 0i64, 0u64);
        for (id, manifest) in &files {
            let m = self.manifest(manifest)?;
            // A chunk the old local file holds comes from there instead.
            let (old, old_size) = match self.cur.get(id).map(|c| c.content.clone()) {
                Some(Content::File {
                    manifest: old,
                    size,
                }) => (
                    self.manifest(&old)
                        .map(|m| m.chunks().map(|c| c.hash).collect::<HashSet<_>>())
                        .unwrap_or_default(),
                    Some(size),
                ),
                _ => (HashSet::new(), None),
            };
            let mut needs = VecDeque::new();
            for (_, c) in m.placed_chunks() {
                if !old.contains(&c.hash) {
                    needs.push_back((c.hash, c.len));
                    *self.fetch.users.entry(c.hash).or_default() += 1;
                }
            }
            let data = on_disk(m.chunks().map(|c| u64::from(c.len)).sum());
            match old_size {
                // Written beside the old one, which goes once the new one is in place.
                Some(old) => {
                    grows += data as i64 - on_disk(old) as i64;
                    biggest = biggest.max(data);
                }
                None => created += data,
            }
            self.fetch.needs.insert(*id, needs);
        }
        let mut coming = 0u64;
        let mut seen = HashSet::new();
        for (hash, len) in self.fetch.needs.values().flatten() {
            // A chunk already here is a transfer resumed after an interruption.
            if seen.insert(*hash) {
                if cache.try_stat(&Self::cache_path(hash))?.is_some() {
                    self.fetch.cached.insert(*hash);
                } else {
                    coming += u64::from(*len);
                }
            }
        }
        // Each file's row and manifest, and the cache: the first slice may still be there while the
        // last is fetched.
        let records = 8192 * files.len() as u64;
        let cached = coming.min(2 * cairn_failpoint::tuned("apply.slice_bytes", SLICE_BYTES));
        let needed = created + grows.max(0) as u64 + biggest + cached + records;
        if let Some(free) = fsops::free_space(&self.root.dir)? {
            if needed > free {
                return Err(StepError::Other(anyhow!(
                    "this disk would be full: what comes down needs about {} free here ({} of files, \
                     {} for chunks on their way, {} for this device's records), and {} is free. \
                     Nothing was changed: free some space, or leave folders out with .cairn/ignore or \
                     `cairn exclude`, and sync again",
                    human(needed),
                    human(created + grows.max(0) as u64 + biggest),
                    human(cached),
                    human(records),
                    human(free)
                )));
            }
        }
        self.fetch.order = files.iter().map(|(id, _)| *id).collect();
        self.fetch.at = self
            .fetch
            .order
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, i))
            .collect();
        if let Some(first) = self.fetch.order.first().copied() {
            self.fetch_slice(first)?;
        }
        Ok(())
    }

    /// The manifests of what comes down that this device does not have, many to a request and a
    /// transaction. Each is verified against its hash on the way in.
    fn fetch_manifests(
        &mut self,
        client: &Client,
        plan: &Plan,
        files: &[(ObjectId, Hash)],
    ) -> Result<(), StepError> {
        let mut asked = HashSet::new();
        let mut batch: Vec<Hash> = Vec::new();
        let mut size = 0u64;
        for (id, manifest) in files {
            if !asked.insert(*manifest) || self.root.state.has_manifest(manifest)? {
                continue;
            }
            // About 80 bytes for each chunk of its file: a few huge files make a request big.
            let bytes = match &plan.target[id].body.content {
                Content::File { size, .. } => 128 + size / (512 * 1024) * 80,
                _ => 128,
            };
            if !batch.is_empty()
                && (batch.len() as u64
                    >= cairn_failpoint::tuned(
                        "apply.manifests_a_request",
                        MANIFESTS_A_REQUEST as u64,
                    )
                    || size + bytes > MAX_CHUNK_BATCH_BYTES as u64 / 2)
            {
                self.store_manifests(client, &batch)?;
                batch.clear();
                size = 0;
            }
            batch.push(*manifest);
            size += bytes;
        }
        if !batch.is_empty() {
            self.store_manifests(client, &batch)?;
        }
        Ok(())
    }

    fn store_manifests(&mut self, client: &Client, hashes: &[Hash]) -> Result<(), StepError> {
        let got = client.download(hashes)?;
        check("apply.manifests.store")?;
        self.root.state.put_manifests(&got)?;
        hit("apply.manifests.stored");
        Ok(())
    }

    /// The order in which this run writes files, now that its steps are known: new files as they are
    /// created, then new content for those already here.
    fn order_writes(&mut self, steps: &[TopoOp], plan: &Plan) {
        let mut order = Vec::with_capacity(self.fetch.needs.len());
        let mut placed = HashSet::new();
        for step in steps {
            if let TopoOp::Create { object, .. } = step {
                if self.fetch.needs.contains_key(object) && placed.insert(*object) {
                    order.push(*object);
                }
            }
        }
        for id in plan.target.keys() {
            if self.fetch.needs.contains_key(id) && placed.insert(*id) {
                order.push(*id);
            }
        }
        self.fetch.at = order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        self.fetch.order = order;
    }

    /// Bring the next slice into the cache: what `id` still takes from it, then what the files after it
    /// take, up to a slice's bytes.
    fn fetch_slice(&mut self, id: ObjectId) -> Result<(), StepError> {
        let Some(client) = self.client else {
            return Ok(());
        };
        let limit = cairn_failpoint::tuned("apply.slice_bytes", SLICE_BYTES);
        let after = self
            .fetch
            .at
            .get(&id)
            .map_or(self.fetch.order.len(), |i| i + 1);
        let files = std::iter::once(id).chain(self.fetch.order[after..].iter().copied());
        let mut want: Vec<(Hash, u32)> = Vec::new();
        let mut seen = HashSet::new();
        let mut size = 0u64;
        'files: for file in files {
            for (hash, len) in self.fetch.needs.get(&file).into_iter().flatten() {
                if size >= limit {
                    break 'files;
                }
                if !self.fetch.cached.contains(hash) && seen.insert(*hash) {
                    want.push((*hash, *len));
                    size += u64::from(*len);
                }
            }
        }
        let cache = self.root.state_dir.open_dir(b"chunks")?;
        let mut batch: Vec<Hash> = Vec::new();
        let mut size = 0usize;
        for (hash, len) in want {
            if size + len as usize > MAX_CHUNK_BATCH_BYTES && !batch.is_empty() {
                Self::fetch_batch(client, &cache, &batch)?;
                self.fetch.cached.extend(batch.drain(..));
                size = 0;
            }
            batch.push(hash);
            size += len as usize;
        }
        if !batch.is_empty() {
            Self::fetch_batch(client, &cache, &batch)?;
            self.fetch.cached.extend(batch);
        }
        hit("apply.slice.fetched");
        Ok(())
    }

    /// One more of `id`'s chunks is written, if it took this one from the cache. A chunk nothing else
    /// still to be written takes leaves the cache.
    fn used(&mut self, id: ObjectId, hash: &Hash) {
        let Some(needs) = self.fetch.needs.get_mut(&id) else {
            return;
        };
        if needs.front().is_some_and(|(h, _)| h == hash) {
            needs.pop_front();
            self.release(hash);
        }
    }

    /// Whatever `id` was still to take from the cache, it no longer will in this run.
    fn done_with(&mut self, id: ObjectId) {
        for (hash, _) in self.fetch.needs.remove(&id).unwrap_or_default() {
            self.release(&hash);
        }
    }

    fn release(&mut self, hash: &Hash) {
        let Some(n) = self.fetch.users.get_mut(hash) else {
            return;
        };
        *n -= 1;
        if *n == 0 {
            self.fetch.users.remove(hash);
            if self.fetch.cached.remove(hash) {
                if let Ok(cache) = self.root.state_dir.open_dir(b"chunks") {
                    let _ = cache.unlink(&Self::cache_path(hash));
                }
            }
        }
    }

    /// Stop watching files: each whose file has gone long enough unchanged that a later write would move
    /// its timestamps, and, with `all`, every one. A file the watch saw nothing done to holds what this
    /// run wrote, and its row may say so without a read.
    fn settle_watches(&mut self, all: bool) {
        let precision = self.root.dir.fstype_precision_ns();
        let window = cairn_failpoint::tuned("apply.watch_window_ns", RACY_WINDOW_NS as u64);
        while let Some(front) = self.watched.front() {
            let st = fsops::fstat(front.fd.as_fd()).ok();
            let settled = st
                .as_ref()
                .is_some_and(|st| !racy_within(st, precision, i128::from(window)));
            if !settled && !all && self.watched.len() <= WATCHED_AT_ONCE {
                return;
            }
            let file = self.watched.pop_front().expect("there is a front");
            let Some(writes) = self.writes.as_mut() else {
                continue;
            };
            // Looked at before the events are read: anything after the look shows in its timestamps.
            let untouched = writes.untouched(file.wd);
            writes.forget(file.wd);
            let Some(st) = st.filter(|_| settled) else {
                continue;
            };
            let now = fingerprint(&st);
            let w = &file.wrote;
            if (now.dev, now.ino, now.size, now.mtime) == (w.dev, w.ino, w.size, w.mtime)
                && untouched
            {
                self.vouched.insert(file.id, now);
            }
        }
    }

    /// A watch on what `write_temp` has just written and synced: `None` where nothing can watch it, or
    /// the file changed in between.
    fn watch(
        &mut self,
        dir: &Dir,
        name: &[u8],
        wrote: Fingerprint,
    ) -> Option<(OwnedFd, i32, Fingerprint)> {
        let writes = self.writes.as_mut()?;
        let fd = dir.open_read(name).ok()?;
        let wd = writes.watch(fd.as_fd())?;
        match fsops::fstat(fd.as_fd()) {
            Ok(now) if fingerprint(&now) == wrote => Some((fd, wd, wrote)),
            _ => {
                writes.forget(wd);
                None
            }
        }
    }

    fn drop_temp_watch(&mut self) {
        if let (Some((_, wd, _)), Some(writes)) = (self.temp_watch.take(), self.writes.as_mut()) {
            writes.forget(wd);
        }
    }

    /// After the rename that placed `id`: watched until it settles.
    fn keep_watching(&mut self, id: ObjectId, watch: Option<(OwnedFd, i32, Fingerprint)>) {
        if let Some((fd, wd, wrote)) = watch {
            self.watched.push_back(Watched { id, fd, wd, wrote });
        }
        self.settle_watches(false);
    }

    fn fetch_batch(client: &Client, cache: &Dir, hashes: &[Hash]) -> Result<(), StepError> {
        for (hash, data) in client.download(hashes)? {
            check("apply.cache.write")?;
            let name = Self::cache_path(&hash);
            let tmp = [&name[..], b".part"].concat();
            let _ = cache.unlink(&tmp);
            let fd = cache.create_new(&tmp, 0o600)?;
            let written = fsops::write_all_at(fd.as_fd(), &data, 0);
            drop(fd);
            if let Err(e) = written {
                let _ = cache.unlink(&tmp);
                return Err(e.into());
            }
            cache.rename_over(&tmp, cache, &name)?;
        }
        Ok(())
    }

    /// One chunk's bytes: from the cache, the old local file, or the server, verified in every case.
    fn read_chunk(
        &self,
        chunk: &ChunkRef,
        old: Option<(&Dir, &[u8], u64)>,
    ) -> Result<Vec<u8>, StepError> {
        let cache = self.root.state_dir.open_dir(b"chunks")?;
        if let Ok(fd) = cache.open_read(&Self::cache_path(&chunk.hash)) {
            let mut buf = vec![0u8; chunk.len as usize];
            if fsops::read_exact_at(fd.as_fd(), &mut buf, 0).is_ok() && Hash::of(&buf) == chunk.hash
            {
                return Ok(buf);
            }
        }
        if let Some((dir, name, offset)) = old {
            if let Ok(fd) = dir.open_read(name) {
                let mut buf = vec![0u8; chunk.len as usize];
                if fsops::read_exact_at(fd.as_fd(), &mut buf, offset).is_ok()
                    && Hash::of(&buf) == chunk.hash
                {
                    return Ok(buf);
                }
            }
        }
        let client = self
            .client
            .ok_or_else(|| anyhow!("chunk {} is not available", chunk.hash))?;
        let got = client.download(&[chunk.hash])?;
        let (_, data) = got
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no chunk {}", chunk.hash))?;
        Ok(data)
    }

    /// Write `t`'s content under a temporary name in `dir`, with its metadata, durably.
    fn write_temp(
        &mut self,
        dir: &Dir,
        id: ObjectId,
        t: &TNode,
        display: &[Name],
    ) -> Result<Vec<u8>, StepError> {
        let tmp = self.root.temp_name();
        self.drop_temp_watch();
        let done = self.write_temp_inner(dir, &tmp, id, t, display);
        self.done_with(id);
        if let Err(e) = done {
            let _ = dir.unlink(&tmp);
            return Err(e);
        }
        Ok(tmp)
    }

    fn write_temp_inner(
        &mut self,
        dir: &Dir,
        tmp: &[u8],
        id: ObjectId,
        t: &TNode,
        display: &[Name],
    ) -> Result<(), StepError> {
        let kind = t.body.content.kind();
        match &t.body.content {
            Content::Dir => return Err(StepError::Failed("a directory has no temporary".into())),
            Content::Symlink { target } => {
                check("apply.temp.create")?;
                dir.symlink(&target.0, tmp)?;
            }
            Content::File { manifest, size } => {
                check("apply.temp.create")?;
                let fd = dir.create_new(tmp, 0o600)?;
                hit("apply.temp.created");
                let m = self.manifest(manifest).map_err(StepError::Other)?;
                fsops::set_len(fd.as_fd(), *size, crate::chunker::has_holes(&m))?;
                // Where the old version of this object is, to reuse its chunks.
                let old = self.old_chunks(id)?;
                for (offset, chunk) in m.placed_chunks() {
                    let source = old.as_ref().and_then(|(d, name, map)| {
                        map.get(&chunk.hash).map(|at| (d, name.as_slice(), *at))
                    });
                    let wanted = self.fetch.needs.get(&id).and_then(|n| n.front());
                    if wanted
                        .is_some_and(|(h, _)| *h == chunk.hash && !self.fetch.cached.contains(h))
                    {
                        self.fetch_slice(id)?;
                    }
                    let data = self.read_chunk(chunk, source)?;
                    check("apply.temp.write")?;
                    fsops::write_all_at(fd.as_fd(), &data, offset)?;
                    self.used(id, &chunk.hash);
                }
                crate::chunker::restore_holes(fd.as_fd(), &m);
                hit("apply.temp.written");
                fsops::sync_fd(fd.as_fd())?;
                hit("apply.temp.synced");
                // Short of room for a field, the temporary is no whole arrival: it goes, and so does the run.
                self.apply_meta(dir, tmp, kind, &t.body.metadata, None, display)?;
                fsops::sync_fd(fd.as_fd())?;
                hit("apply.temp.meta");
                // From here until a later write would show, whatever is done to it is seen.
                let wrote = fingerprint(&fsops::fstat(fd.as_fd())?);
                drop(fd);
                self.temp_watch = self.watch(dir, tmp, wrote);
                return Ok(());
            }
        }
        self.apply_meta(dir, tmp, kind, &t.body.metadata, None, display)?;
        hit("apply.temp.meta");
        Ok(())
    }

    fn old_chunks(&mut self, id: ObjectId) -> Result<Option<OldVersion>, StepError> {
        let Some(node) = self.cur.get(&id).cloned() else {
            return Ok(None);
        };
        let Content::File { manifest, .. } = node.content else {
            return Ok(None);
        };
        let Some(link) = node.links.iter().next() else {
            return Ok(None);
        };
        let Ok(m) = self.manifest(&manifest) else {
            return Ok(None);
        };
        let (dir, name) = self.locate(link)?;
        let map = m.placed_chunks().map(|(o, c)| (c.hash, o)).collect();
        Ok(Some((dir, name, map)))
    }

    fn apply_meta(
        &mut self,
        dir: &Dir,
        name: &[u8],
        kind: ObjectKind,
        want: &Metadata,
        current: Option<&Metadata>,
        display: &[Name],
    ) -> Result<(), StepError> {
        let problems = metadata::apply_unless_full(
            dir,
            name,
            kind,
            want,
            current,
            &self.root.config.policy,
            self.accounts,
        )
        .map_err(StepError::DiskFull)?;
        if problems.is_empty() {
            self.outcome.meta_applied.push(show(display));
        }
        for p in problems {
            self.outcome.warnings.push(Skip {
                path: show(display),
                kind: SkipKind::CannotApply,
                detail: Some(p),
            });
        }
        Ok(())
    }

    /// Make a new object appear at its first link.
    fn create(&mut self, id: ObjectId, link: &Link, t: &TNode) -> Result<(), StepError> {
        let (dir, name) = self.locate(link)?;
        self.writable(link.parent, &dir)?;
        self.touched_dirs.insert(link.parent);
        let display = step_path(&self.cur, link);
        let mut written = None;
        if matches!(t.body.content, Content::Symlink { .. }) && !fsops::HOLDS_SYMLINKS {
            return Err(StepError::Unholdable {
                why: "a symlink, which this device does not make yet".into(),
                twin: false,
            });
        }
        if t.body.content == Content::Dir {
            check("apply.mkdir")?;
            // Private until the final pass gives it its real mode.
            if let Err(e) = refuse_name(&name).and_then(|()| dir.mkdir(&name, 0o700)) {
                return Err(unholdable(&dir, &name, e));
            }
            hit("apply.mkdir.done");
            self.touched_dirs.insert(id);
        } else {
            let tmp = self.write_temp(&dir, id, t, &display)?;
            let watch = self.temp_watch.take();
            // Taken before the rename: any later difference but ctime is someone's edit.
            written = Some(fingerprint(&dir.stat(&tmp)?));
            hit("apply.create.before_rename");
            let placed = refuse_name(&name).and_then(|()| dir.rename_new(&tmp, &dir, &name));
            if let Err(e) = placed {
                let _ = dir.unlink(&tmp);
                return Err(unholdable(&dir, &name, e));
            }
            hit("apply.create.renamed");
            self.keep_watching(id, watch);
        }
        let st = dir.stat(&name)?;
        self.cur.insert(
            id,
            CurNode {
                kind: t.body.content.kind(),
                links: [link.clone()].into(),
                content: t.body.content.clone(),
                apply_from: t.body.metadata.clone(),
                fp: Some(written.unwrap_or_else(|| fingerprint(&st))),
                holds_excluded: false,
            },
        );
        self.refresh_fp(id, &dir, &name);
        self.placed.insert(id);
        Ok(())
    }

    fn add_link(&mut self, id: ObjectId, link: &Link) -> Result<(), StepError> {
        let from = self
            .cur
            .get(&id)
            .and_then(|n| n.links.iter().next())
            .ok_or_else(|| StepError::Changed("the file to link is not there".into()))?
            .clone();
        let (from_dir, from_name) = self.locate(&from)?;
        let (to_dir, to_name) = self.locate(link)?;
        self.writable(link.parent, &to_dir)?;
        self.check_fp(id, &from_dir, &from_name)?;
        check("apply.link")?;
        let linked =
            refuse_name(&to_name).and_then(|()| from_dir.hard_link(&from_name, &to_dir, &to_name));
        if let Err(e) = linked {
            return Err(unholdable(&to_dir, &to_name, e));
        }
        hit("apply.link.done");
        self.touched_dirs.insert(link.parent);
        self.refresh_fp(id, &to_dir, &to_name);
        if let Some(n) = self.cur.get_mut(&id) {
            n.links.insert(link.clone());
        }
        self.moved.insert(id);
        Ok(())
    }

    fn remove_link(&mut self, id: ObjectId, link: &Link) -> Result<(), StepError> {
        let (dir, name) = self.locate(link)?;
        let kind = self.cur.get(&id).map(|n| n.kind);
        self.writable(link.parent, &dir)?;
        check("apply.unlink")?;
        if kind == Some(ObjectKind::Dir) {
            if self.cur.get(&id).is_some_and(|n| n.fp.is_none()) {
                return Err(StepError::Failed(NOT_OBSERVED.into()));
            }
            // A directory that gained something since the scan is not empty, and stays.
            dir.rmdir(&name)?;
        } else {
            self.check_fp(id, &dir, &name)?;
            // Out of the way first, and looked at once more where nothing can reach it: no gap
            // between the look and the unlink for an edit to fall into.
            let aside = self.root.aside_name(&name);
            hit("apply.unlink.before_aside");
            dir.rename_new(&name, &dir, &aside)?;
            hit("apply.unlink.aside");
            if !self.still_as_scanned(id, &dir, &aside) {
                dir.rename_new(&aside, &dir, &name)?;
                return Err(StepError::Changed(
                    "the file changed during this sync".into(),
                ));
            }
            dir.unlink(&aside)?;
        }
        hit("apply.unlink.done");
        self.touched_dirs.insert(link.parent);
        if let Some(n) = self.cur.get_mut(&id) {
            n.links.remove(link);
            if let Some(other) = n.links.iter().next().cloned() {
                if let Ok((d, nm)) = self.locate(&other) {
                    self.refresh_fp(id, &d, &nm);
                }
            }
        }
        if self.cur.get(&id).is_some_and(|n| n.links.is_empty()) {
            self.cur.remove(&id);
        }
        Ok(())
    }

    /// Where an object staged now is going, once the plan has reached its target.
    fn destination(&self, plan: &Plan, id: ObjectId, from: &Link) -> Option<Vec<Name>> {
        let links = &plan.target.get(&id)?.links;
        let first = links
            .iter()
            .find(|l| **l != *from)
            .or(links.iter().next())?;
        let mut path = vec![first.name.clone()];
        let mut at = first.parent;
        while at != ObjectId::ROOT {
            let up = plan.target.get(&at)?.links.iter().next()?;
            path.push(up.name.clone());
            at = up.parent;
        }
        path.reverse();
        Some(path)
    }

    fn move_entry(
        &mut self,
        plan: &Plan,
        id: ObjectId,
        from: &Link,
        to: &Link,
    ) -> Result<(), StepError> {
        let (from_dir, from_name) = self.locate(from)?;
        let (to_dir, to_name) = self.locate(to)?;
        self.writable(from.parent, &from_dir)?;
        self.writable(to.parent, &to_dir)?;
        self.check_fp(id, &from_dir, &from_name)?;
        if is_stage(to) {
            let origin = step_path(&self.cur, from);
            let key = format!("{INTENT_STAGE}{}", String::from_utf8_lossy(&to_name));
            self.root.state.put_intent(
                &key,
                &StageIntent {
                    stage: String::from_utf8_lossy(&to_name).into_owned(),
                    origin,
                    dest: self.destination(plan, id, from),
                },
            )?;
            hit("apply.stage.intent");
        }
        check("apply.move")?;
        from_dir.rename_new(&from_name, &to_dir, &to_name)?;
        hit("apply.move.done");
        if is_stage(from) {
            self.root.state.drop_intent(&format!(
                "{INTENT_STAGE}{}",
                String::from_utf8_lossy(&from_name)
            ))?;
        }
        self.touched_dirs.insert(from.parent);
        self.touched_dirs.insert(to.parent);
        self.moved.insert(id);
        self.refresh_fp(id, &to_dir, &to_name);
        if let Some(n) = self.cur.get_mut(&id) {
            n.links.remove(from);
            n.links.insert(to.clone());
        }
        Ok(())
    }

    /// Replace an existing file or symlink's content in place, atomically.
    fn replace_content(&mut self, id: ObjectId, t: &TNode) -> Result<(), StepError> {
        let links: Vec<Link> = self.cur[&id].links.iter().cloned().collect();
        let first = links
            .first()
            .ok_or_else(|| StepError::Changed("no entry".into()))?
            .clone();
        let (dir, name) = self.locate(&first)?;
        self.writable(first.parent, &dir)?;
        self.check_fp(id, &dir, &name)?;
        let display = step_path(&self.cur, &first);
        let tmp = self.write_temp(&dir, id, t, &display)?;
        let watch = self.temp_watch.take();
        let relink_key = format!("{INTENT_RELINK}{id}");
        if links.len() > 1 {
            // No single rename can move every name to the new inode: note the work, so a crash in
            // the middle finishes it rather than splitting the object in two.
            let old = self.cur[&id]
                .fp
                .clone()
                .ok_or_else(|| StepError::Failed(NOT_OBSERVED.into()))?;
            let new = dir.stat(&tmp)?;
            let old_manifest = match &self.cur[&id].content {
                Content::File { manifest, .. } => Some(*manifest),
                _ => None,
            };
            let intent = RelinkIntent {
                old_manifest,
                first: step_path(&self.cur, &first),
                others: links[1..].iter().map(|l| step_path(&self.cur, l)).collect(),
                new_ino: new.ino,
                old,
            };
            self.root.state.put_intent(&relink_key, &intent)?;
            hit("apply.relink.intent");
        }
        let replaced = self.cur[&id]
            .fp
            .as_ref()
            .map(|fp| (fp.dev, fp.ino, fp.size, fp.mtime));
        hit("apply.replace.before_check");
        // The last look before the rename: an edit made since the scan is not overwritten.
        if let Err(e) = self.check_fp(id, &dir, &name) {
            let _ = dir.unlink(&tmp);
            let _ = self.root.state.drop_intent(&relink_key);
            return Err(e);
        }
        let written = fingerprint(&dir.stat(&tmp)?);
        hit("apply.replace.before_rename");
        // Swapped, not overwritten: the old file comes out under the temporary name, to be looked at
        // once more, so that no edit can fall between the last look and the replacement.
        let aside = self.root.aside_name(&name);
        // Nothing can be at a random name, and not every filesystem can promise that.
        dir.rename_over(&tmp, &dir, &aside)?;
        hit("apply.replace.aside");
        if swap(self.root, &dir, &aside, &name)? {
            hit("apply.replace.swapped");
            if !self.still_as_scanned(id, &dir, &aside) {
                swap(self.root, &dir, &aside, &name)?;
                let _ = dir.unlink(&aside);
                let _ = self.root.state.drop_intent(&relink_key);
                return Err(StepError::Changed(
                    "the file changed during this sync".into(),
                ));
            }
            dir.unlink(&aside)?;
        } else {
            dir.rename_over(&aside, &dir, &name)?;
        }
        hit("apply.replace.renamed");
        self.keep_watching(id, watch);
        self.touched_dirs.insert(first.parent);
        // The other hard links follow, each atomically. Swapped, as above: what comes out of a name that
        // no longer held what was scanned is someone's edit, kept beside the names, which stay one file.
        let mut edited_name = false;
        for other in links.iter().skip(1) {
            let (odir, oname) = self.locate(other)?;
            self.writable(other.parent, &odir)?;
            let otmp = self.root.aside_name(&oname);
            dir.hard_link(&name, &odir, &otmp)?;
            hit("apply.replace.relink");
            if swap(self.root, &odir, &otmp, &oname)? {
                hit("apply.replace.relink.swapped");
                let was = odir.stat(&otmp)?;
                if Some((was.dev, was.ino, was.size, was.mtime)) == replaced {
                    odir.unlink(&otmp)?;
                } else {
                    // Someone's edit since the scan. This name takes the new version with the rest, or
                    // the file would split in two for everyone, and the edit stays under the aside
                    // name, for the next look to keep as a conflict copy, unless a name still to follow
                    // holds it too.
                    edited_name = true;
                    if was.nlink > 1 {
                        odir.unlink(&otmp)?;
                    }
                }
            } else {
                odir.rename_over(&otmp, &odir, &oname)?;
            }
            self.touched_dirs.insert(other.parent);
        }
        if links.len() > 1 {
            self.root.state.drop_intent(&relink_key)?;
        }
        let node = self.cur.get_mut(&id).expect("replaced object is current");
        node.content = t.body.content.clone();
        node.apply_from = t.body.metadata.clone();
        node.fp = Some(written);
        self.refresh_fp(id, &dir, &name);
        self.placed.insert(id);
        if edited_name {
            return Err(StepError::Changed(
                "another name of this file changed during this sync".into(),
            ));
        }
        Ok(())
    }

    fn record(&mut self, id: ObjectId, display: String, e: StepError) -> Result<()> {
        match e {
            StepError::Changed(detail) => self.outcome.changed.push(Skip {
                path: display,
                kind: SkipKind::KeepsChanging,
                detail: Some(format!("{id:?}: {detail}")),
            }),
            StepError::Failed(detail) => self.outcome.failed.push(Skip {
                path: display,
                kind: if detail == NOT_OBSERVED {
                    SkipKind::ReadError
                } else {
                    SkipKind::CannotApply
                },
                detail: Some(detail),
            }),
            // Only a new entry is left out for it. Anything else, a move onto such a name, stands
            // until a person changes the names, and is tried again meanwhile.
            StepError::Unholdable { why, .. } => self.outcome.failed.push(Skip {
                path: display,
                kind: SkipKind::CannotApply,
                detail: Some(why),
            }),
            StepError::DiskFull(io) => {
                return Err(anyhow::Error::new(io)
                    .context("this disk is full: nothing was lost; free space and sync again"))
            }
            StepError::Other(err) => return Err(err),
        }
        Ok(())
    }

    /// Move local files aside as conflict copies, durably marked as the new objects they now are.
    fn make_copies(&mut self, plan: &Plan) -> Result<()> {
        // Something new kept beside the server's at a name both took is a conflict copy too, and must
        // say so in whichever later run it goes up: the same mark a copy made below has.
        let mut join_copies = Vec::new();
        for (id, t) in &plan.target {
            let (Some(of), Some(cur)) = (&t.conflict_of, self.cur.get(id)) else {
                continue;
            };
            let Some(fp) = &cur.fp else {
                continue;
            };
            let intent = CopyIntent {
                copy: *id,
                dev: fp.dev,
                ino: fp.ino,
                is: Some((cur.kind, fp.btime)),
                conflict_of: of.clone(),
                moves: Vec::new(),
            };
            join_copies.push((format!("{INTENT_COPY}{id}"), intent));
        }
        self.root.state.put_intents(&join_copies)?;
        for copy in &plan.copies {
            let display = copy
                .moves
                .first()
                .map_or_else(String::new, |m| show(&step_path(&self.cur, &m.0)));
            if let Err(e) = self.make_copy(copy, plan) {
                self.record(copy.object, display, e)?;
            }
        }
        Ok(())
    }

    fn make_copy(&mut self, copy: &ConflictCopy, plan: &Plan) -> Result<(), StepError> {
        let original = self
            .cur
            .get(&copy.object)
            .cloned()
            .ok_or_else(|| StepError::Changed("the original is not there".into()))?;
        let fp = original
            .fp
            .clone()
            .ok_or_else(|| StepError::Failed(NOT_OBSERVED.into()))?;
        let intent = CopyIntent {
            copy: copy.copy,
            dev: fp.dev,
            ino: fp.ino,
            is: Some((original.kind, fp.btime)),
            conflict_of: plan.target[&copy.copy]
                .conflict_of
                .clone()
                .expect("a copy has its origin"),
            moves: copy
                .moves
                .iter()
                .map(|(from, to)| (step_path(&self.cur, from), step_path(&self.cur, to)))
                .collect(),
        };
        for (from, _) in &copy.moves {
            let (dir, name) = self.locate(from)?;
            self.check_fp(copy.object, &dir, &name)?;
        }
        // Names of the copy from here on, so that one the original does not get back before a kill
        // comes down again, and does not read as deleted here. In the mark's transaction: a kill
        // between the two would leave the original's names to read as removed here.
        let key = format!("{INTENT_COPY}{}", copy.copy);
        if let Some(row) = self.rows.get_mut(&copy.object) {
            let names = copy.moves.iter().map(|m| &m.0);
            let held = row.held.get_or_insert_with(|| original.links.clone());
            for name in names {
                held.remove(name);
            }
            self.root
                .state
                .put_intent_with_rows(&key, &intent, &[(copy.object, &*row)])?;
            hit("apply.copy.intent");
            hit("apply.copy.given_up");
        } else {
            self.root.state.put_intent(&key, &intent)?;
            hit("apply.copy.intent");
        }
        let mut links = BTreeSet::new();
        for (from, to) in &copy.moves {
            let (dir, name) = self.locate(from)?;
            let (to_dir, to_name) = self.locate(to)?;
            self.writable(from.parent, &dir)?;
            check("apply.copy.rename")?;
            dir.rename_new(&name, &to_dir, &to_name)?;
            hit("apply.copy.renamed");
            self.touched_dirs.insert(from.parent);
            links.insert(to.clone());
        }
        let first = links
            .iter()
            .next()
            .cloned()
            .ok_or_else(|| StepError::Failed("no entry".into()))?;
        let (d, n) = self.locate(&first)?;
        let st = d.stat(&n)?;
        self.cur.remove(&copy.object);
        self.cur.insert(
            copy.copy,
            CurNode {
                kind: original.kind,
                links,
                content: original.content,
                apply_from: original.apply_from,
                fp: Some(fingerprint(&st)),
                holds_excluded: false,
            },
        );
        self.placed.insert(copy.copy);
        Ok(())
    }

    /// Everything else in the target, after structure is right.
    fn apply_updates(&mut self, plan: &Plan) -> Result<()> {
        for (id, t) in &plan.target {
            if *id == ObjectId::ROOT || t.body.content == Content::Dir {
                continue;
            }
            let Some(cur) = self.cur.get(id).cloned() else {
                continue;
            };
            if cur.content != t.body.content {
                if let Err(e) = self.replace_content(*id, t) {
                    let display = cur
                        .links
                        .iter()
                        .next()
                        .map_or_else(String::new, |l| show(&step_path(&self.cur, l)));
                    self.record(*id, display, e)?;
                }
                continue;
            }
            if cur.apply_from != t.body.metadata {
                let Some(link) = cur.links.iter().next().cloned() else {
                    continue;
                };
                let display = step_path(&self.cur, &link);
                let applied = (|| -> Result<(), StepError> {
                    let (dir, name) = self.locate(&link)?;
                    self.check_fp(*id, &dir, &name)?;
                    check("apply.meta")?;
                    self.apply_meta(
                        &dir,
                        &name,
                        cur.kind,
                        &t.body.metadata,
                        Some(&cur.apply_from),
                        &display,
                    )?;
                    hit("apply.meta.done");
                    self.reset_fp(*id, &dir, &name);
                    Ok(())
                })();
                match applied {
                    Ok(()) => {
                        self.placed.insert(*id);
                        if let Some(n) = self.cur.get_mut(id) {
                            n.apply_from = t.body.metadata.clone();
                        }
                    }
                    Err(e) => self.record(*id, show(&display), e)?,
                }
            }
        }
        Ok(())
    }

    /// Directories last, deepest first: creating or removing anything inside one moves its mtime,
    /// and its final mode may forbid the writes above.
    fn finish_dirs(&mut self, plan: &Plan) -> Result<()> {
        let mut dirs: Vec<(usize, ObjectId)> = plan
            .target
            .iter()
            .filter(|(_, t)| t.body.content == Content::Dir)
            .map(|(id, _)| (self.depth(*id), *id))
            .collect();
        dirs.sort_by(|a, b| b.cmp(a));
        for (_, id) in dirs {
            let t = &plan.target[&id];
            let touched = self.touched_dirs.contains(&id);
            let relaxed = self.relaxed.get(&id).copied();
            let Some(cur) = self.cur.get(&id).cloned() else {
                if id == ObjectId::ROOT {
                    continue;
                }
                continue;
            };
            let created = self.placed.contains(&id);
            if !touched && !created && relaxed.is_none() && cur.apply_from == t.body.metadata {
                continue;
            }
            let display = cur
                .links
                .iter()
                .next()
                .map_or_else(Vec::new, |l| step_path(&self.cur, l));
            let finished = (|| -> Result<(), StepError> {
                let (dir, name): (Dir, Vec<u8>) = match cur.links.iter().next() {
                    Some(link) => self.locate(link)?,
                    None => (self.root.dir.dup()?, b".".to_vec()),
                };
                // Opened now: the final mode may not let this user open it afterwards.
                let opened = if touched {
                    Some(dir.open_dir(&name)?)
                } else {
                    None
                };
                // The relaxed bits stand unless policy sets the mode below.
                if let Some(mode) = relaxed {
                    dir.chmod(&name, mode)?;
                }
                check("apply.dir.meta")?;
                let mut current = cur.apply_from.clone();
                // Writes here moved the times, whatever the record said.
                if touched {
                    current.mtime = cairn_protocol::Timestamp {
                        sec: i64::MIN,
                        nsec: 0,
                    };
                }
                // A relaxed mode was just restored to `mode`; compare with what is on disk.
                if let Some(mode) = relaxed {
                    current.mode = mode;
                }
                let is_new = created && cur.apply_from == t.body.metadata;
                self.apply_meta(
                    &dir,
                    &name,
                    ObjectKind::Dir,
                    &t.body.metadata,
                    (!is_new).then_some(&current),
                    &display,
                )?;
                hit("apply.dir.meta.done");
                self.reset_fp(id, &dir, &name);
                if let Some(child) = opened {
                    child.sync()?;
                }
                if let Some(n) = self.cur.get_mut(&id) {
                    n.apply_from = t.body.metadata.clone();
                }
                self.placed.insert(id);
                // Recorded here, deepest first, while its parent can still be entered.
                self.store_one(plan, id, &dir, &name)?;
                Ok(())
            })();
            match finished {
                Ok(()) => self.root.state.drop_intent(&format!("{INTENT_MODE}{id}"))?,
                Err(e) => {
                    if created && matches!(e, StepError::Changed(_)) && !self.dir_is_there(&cur) {
                        self.outcome.gone.push(show(&display));
                    }
                    self.record(id, show(&display), e)?
                }
            }
        }
        Ok(())
    }

    /// Whether a directory is still at its place, for telling it gone from in the way.
    fn dir_is_there(&self, cur: &CurNode) -> bool {
        let Some(link) = cur.links.iter().next() else {
            return true;
        };
        match self.locate(link) {
            Ok((dir, name)) => dir.try_stat(&name).map_or(true, |s| s.is_some()),
            Err(StepError::Changed(_)) => false,
            Err(_) => true,
        }
    }

    fn depth(&self, mut id: ObjectId) -> usize {
        let mut depth = 0;
        while id != ObjectId::ROOT {
            let Some(up) = self.cur.get(&id).and_then(|n| n.links.iter().next()) else {
                break;
            };
            id = up.parent;
            depth += 1;
        }
        depth
    }

    /// Record one object this run wrote: what is on disk now, and that it is in step with the server
    /// if the server's head says the same.
    fn store_one(
        &mut self,
        plan: &Plan,
        id: ObjectId,
        dir: &Dir,
        name: &[u8],
    ) -> Result<(), StepError> {
        let Some(cur) = self.cur.get(&id).cloned() else {
            return Ok(());
        };
        let st = dir.stat(name)?;
        let precision = self.root.dir.fstype_precision_ns();
        let mut problems = Vec::new();
        let scan = metadata::capture(dir, name, &st, self.accounts, precision, &mut problems);
        let in_step = plan
            .target
            .get(&id)
            .zip(self.remote.get_live(id))
            .filter(|(t, r)| t.body == r.body && t.links == r.links);
        // Changed since this run placed it: someone else's edit, so it is not in step with anything.
        let untouched = cur.kind == ObjectKind::Dir
            || cur
                .fp
                .as_ref()
                .is_some_and(|fp| (fp.ino, fp.size, fp.mtime) == (st.ino, st.size, st.mtime));
        // A folder still private from a killed run, which this round's scan could not read in full, stays
        // so: this pass went by what it did not know, and what is on disk is no pair of the record.
        let still_private = self
            .rows
            .get(&id)
            .is_some_and(|o| o.incomplete && o.synced.as_ref().is_some_and(|s| s.unapplied));
        let in_step = in_step.filter(|_| untouched && !still_private);
        let synced = in_step.map(|(t, _)| Synced {
            body: t.body.clone(),
            links: t.links.clone(),
            applied: scan.clone(),
            commit: None,
            unapplied: false,
            folder_time: None,
        });
        let old = self.rows.get(&id);
        let row = Row {
            fp: fingerprint(&st),
            // Only a scan, which reads the file, may vouch that a fingerprint means this content, or
            // this run, for a file its watch saw nothing done to from its write until a later write would
            // show: an edit since the rename would otherwise be recorded as the content just written.
            racy: self.vouched.get(&id) != Some(&fingerprint(&st)),
            content: cur.content.clone(),
            incomplete: !problems.is_empty(),
            scan,
            applied: old.and_then(|o| o.applied.clone()),
            synced: if untouched {
                synced.or_else(|| old.and_then(|o| o.synced.clone()))
            } else {
                None
            },
            held: Some(
                old.and_then(|o| o.held.clone())
                    .unwrap_or_default()
                    .into_iter()
                    .chain(cur.links.iter().cloned())
                    .collect(),
            ),
        };
        self.rows.insert(id, row);
        self.unstored.insert(id);
        if self.unstored.len() as u64
            >= cairn_failpoint::tuned("apply.rows_a_transaction", ROWS_A_TRANSACTION)
        {
            self.store_unstored(&[])?;
        }
        Ok(())
    }

    /// The rows this run wrote and has not stored yet, with the marks they answer, in one transaction.
    /// Each was written once what it says was durable, so storing it later only leaves more for the
    /// next scan to find out.
    fn store_unstored(&mut self, marks: &[String]) -> Result<()> {
        let ids = std::mem::take(&mut self.unstored);
        let list: Vec<(ObjectId, &Row)> = ids
            .iter()
            .filter_map(|id| self.rows.get(id).map(|r| (*id, r)))
            .collect();
        if list.is_empty() && marks.is_empty() {
            return Ok(());
        }
        check("apply.rows.store")?;
        self.root.state.store_rows_dropping(&list, marks)?;
        hit("apply.rows.stored");
        Ok(())
    }

    /// Files and symlinks are recorded before the final directory pass, which may close a parent to
    /// this user.
    fn store_placed_files(&mut self, plan: &Plan) -> Result<()> {
        for id in self.placed.clone() {
            let Some(cur) = self.cur.get(&id) else {
                continue;
            };
            if cur.kind == ObjectKind::Dir {
                continue;
            }
            let Some(link) = cur.links.iter().next().cloned() else {
                continue;
            };
            let display = show(&step_path(&self.cur, &link));
            let (dir, name) = match self.locate(&link) {
                Ok(found) => found,
                Err(StepError::Changed(_)) => {
                    self.outcome.gone.push(display);
                    continue;
                }
                Err(_) => continue,
            };
            match self.store_one(plan, id, &dir, &name) {
                Ok(()) => {}
                Err(StepError::Changed(e)) if dir.try_stat(&name).is_ok_and(|s| s.is_none()) => {
                    self.outcome.gone.push(display.clone());
                    self.record(id, display, StepError::Changed(e))?;
                }
                Err(e) => self.record(id, display, e)?,
            }
        }
        Ok(())
    }

    fn sync_touched_dirs(&mut self) -> Result<()> {
        for id in self.touched_dirs.clone() {
            // A directory gone or closed to this user holds nothing this run placed.
            if let Ok(dir) = self.open_dir_object(id) {
                dir.sync()?;
            }
        }
        Ok(())
    }

    /// An entry that arrived from the server is shared with it, not a move made here: a device killed
    /// before its next finalize must not push it back over what happens to the object meanwhile.
    fn store_moved(&mut self, plan: &Plan) -> Result<()> {
        let mut changed = Vec::new();
        for id in &self.moved {
            if self.placed.contains(id) {
                continue;
            }
            let (Some(t), Some(r), Some(c)) = (
                plan.target.get(id),
                self.remote.get_live(*id),
                self.cur.get(id),
            ) else {
                continue;
            };
            // All of it, on disk too: a content step that failed leaves a difference that is real.
            if t.body != r.body
                || t.links != r.links
                || c.links != t.links
                || c.content != t.body.content
                || c.apply_from != t.body.metadata
            {
                continue;
            }
            let Some(row) = self.rows.get_mut(id) else {
                continue;
            };
            if let Some(held) = row.held.as_mut() {
                held.extend(t.links.iter().cloned());
            }
            // What could not be read fully is no pair of the record: a folder still private from a killed
            // run, unreadable now, would lose its mark and keep 0700 as a mode this device holds.
            if !row.incomplete {
                row.synced = Some(Synced {
                    body: t.body.clone(),
                    links: t.links.clone(),
                    applied: row.scan.clone(),
                    commit: None,
                    unapplied: false,
                    folder_time: None,
                });
            }
            changed.push(*id);
        }
        let list: Vec<(ObjectId, &Row)> = changed.iter().map(|id| (*id, &self.rows[id])).collect();
        self.root.state.store_rows(&list, &[])
    }

    /// Before any of it goes: from here on this device does not claim what it is about to remove, so
    /// that if the server later has it after all, the absence does not read as a delete here. A scan
    /// takes back whatever turns out still to be here.
    fn give_up_what_goes(&mut self, steps: &[TopoOp]) -> Result<()> {
        let mut changed = BTreeSet::new();
        for step in steps {
            if let TopoOp::Unlink { object, link } = step {
                let Some(held) = self.rows.get_mut(object).and_then(|r| r.held.as_mut()) else {
                    continue;
                };
                if held.remove(link) {
                    changed.insert(*object);
                }
            }
        }
        if !changed.is_empty() {
            let list: Vec<(ObjectId, &Row)> =
                changed.iter().map(|id| (*id, &self.rows[id])).collect();
            self.root.state.store_rows(&list, &[])?;
            hit("apply.given_up");
        }
        Ok(())
    }

    /// Before anything moves, in one transaction: what the server has for whatever is about to change
    /// here, so that the next scan can tell what arrives from what this device did itself, whatever
    /// stops this run.
    fn note_arriving(&self, plan: &Plan) -> Vec<(String, Arriving)> {
        let mut marks = Vec::new();
        for (id, t) in &plan.target {
            let Some(r) = self.remote.get_live(*id) else {
                continue;
            };
            if let Some(c) = self.cur.get(id) {
                let here = c.content == t.body.content
                    && c.apply_from == t.body.metadata
                    && c.links == t.links;
                if here || c.fp.is_none() || (c.content == t.body.content && self.unmatched(*id)) {
                    continue;
                }
            }
            marks.push((
                format!("{INTENT_ARRIVING}{id}"),
                Arriving {
                    object: *id,
                    body: r.body.clone(),
                    links: r.links.clone(),
                    names: t.links.clone(),
                    folder_touched: false,
                    policy: Some(self.root.config.policy.clone()),
                    made: t.body.content == Content::Dir && !self.cur.contains_key(id),
                },
            ));
        }
        self.note_touched_folders(plan, &mut marks);
        marks
    }

    /// A step inside a folder moves the folder's own timestamp, which only the last pass puts back:
    /// until then that is no change of anyone's.
    fn note_touched_folders(&self, plan: &Plan, marks: &mut Vec<(String, Arriving)>) {
        let mut at: BTreeMap<ObjectId, usize> = marks
            .iter()
            .enumerate()
            .map(|(i, (_, a))| (a.object, i))
            .collect();
        let removed = self.cur.keys().filter(|id| !plan.target.contains_key(id));
        let parents: BTreeSet<ObjectId> = at
            .keys()
            .chain(removed)
            .flat_map(|id| {
                let now = self.cur.get(id).into_iter().flat_map(|c| &c.links);
                let then = plan.target.get(id).into_iter().flat_map(|t| &t.links);
                now.chain(then).map(|l| l.parent)
            })
            .collect();
        for parent in parents {
            if let Some(i) = at.get(&parent) {
                marks[*i].1.folder_touched = true;
                continue;
            }
            let (Some(r), Some(t), Some(c)) = (
                self.remote.get_live(parent),
                plan.target.get(&parent),
                self.cur.get(&parent),
            ) else {
                continue;
            };
            if r.rev == cairn_protocol::RevisionId::ZERO || c.fp.is_none() || self.unmatched(parent)
            {
                continue;
            }
            at.insert(parent, marks.len());
            marks.push((
                format!("{INTENT_ARRIVING}{parent}"),
                Arriving {
                    object: parent,
                    body: r.body.clone(),
                    links: r.links.clone(),
                    names: t.links.clone(),
                    folder_touched: true,
                    policy: Some(self.root.config.policy.clone()),
                    made: false,
                },
            ));
        }
    }

    /// Here before this run, as a joining device's files are, with no record of ever having matched
    /// the server's: its next scan joins the two records again, whatever got there, so there is nothing
    /// for a mark to tell apart, and one would make the server's whole record look arrived.
    fn unmatched(&self, id: ObjectId) -> bool {
        self.rows
            .get(&id)
            .is_some_and(|r| r.applied.is_none() && r.synced.is_none())
    }

    /// What is recorded and in step with the server needs no mark any more. What the target only shares
    /// a part of with the server keeps its mark until the next scan has answered it.
    fn drop_settled_marks(&mut self, plan: &Plan, marks: &[(String, Arriving)]) -> Result<()> {
        let done: Vec<String> = marks
            .iter()
            .filter(|(_, a)| {
                plan.target
                    .get(&a.object)
                    .is_some_and(|t| t.body == a.body && t.links == a.links)
            })
            .map(|(key, _)| key.clone())
            .collect();
        self.store_unstored(&done)
    }

    /// Reach the plan's target as far as possible.
    pub fn run(mut self, plan: &Plan) -> Result<(ApplyOutcome, BTreeMap<ObjectId, CurNode>)> {
        // Download first: a full disk or a lost server is found before anything moves.
        if let Err(e) = self.prefetch(plan) {
            self.record(ObjectId::ROOT, String::new(), e)?;
            self.outcome.halted = true;
            return Ok((self.outcome, self.cur));
        }
        hit("apply.prefetched");
        let notes: Vec<(String, NoteIntent)> = plan
            .notes
            .iter()
            .map(|(object, text)| {
                let note = NoteIntent {
                    object: *object,
                    text: text.clone(),
                };
                (note.key(), note)
            })
            .collect();
        self.root.state.put_intents(&notes)?;
        let marks = self.note_arriving(plan);
        self.root.state.put_intents(&marks)?;
        hit("apply.noted");
        self.make_copies(plan)?;
        if !self.outcome.clean() {
            // A local edit that did not get out of the way must not be overwritten by what comes next.
            self.outcome.halted = true;
            return Ok((self.outcome, self.cur));
        }

        let to: Topo = plan
            .target
            .iter()
            .filter(|(id, _)| **id != ObjectId::ROOT)
            .map(|(id, t)| (*id, (t.body.content.kind(), t.links.clone())))
            .collect();
        let from: Topo = self
            .cur
            .iter()
            .filter(|(id, _)| **id != ObjectId::ROOT)
            .map(|(id, c)| (*id, (c.kind, c.links.clone())))
            .collect();
        let steps = schedule(&from, &to).context("ordering local changes")?;
        self.order_writes(&steps, plan);
        self.give_up_what_goes(&steps)?;
        // Folders this filesystem cannot hold: what goes in them waits with them.
        let mut unheld_dirs: BTreeSet<ObjectId> = BTreeSet::new();
        for step in &steps {
            if let TopoOp::Create { link, .. } | TopoOp::Link { link, .. } = step {
                if unheld_dirs.contains(&link.parent) {
                    if let TopoOp::Create { object, .. } = step {
                        unheld_dirs.insert(*object);
                    }
                    continue;
                }
            }
            // What it holds was never synced and has no other copy, so it stays, and so does the
            // folder. That is no change to confirm: nothing was written.
            if let TopoOp::Unlink { object, link } = step {
                if self.cur.get(object).is_some_and(|n| n.holds_excluded) {
                    if let Some(n) = self.cur.get_mut(object) {
                        n.links.remove(link);
                    }
                    self.cur.remove(object);
                    // No longer anything the server knows: if it makes a folder here, they merge.
                    self.rows.remove(object);
                    self.root.state.store_rows(&[], &[*object])?;
                    continue;
                }
            }
            let (object, display) = match step {
                TopoOp::Create { object, link }
                | TopoOp::Link { object, link }
                | TopoOp::Unlink { object, link } => (*object, show(&step_path(&self.cur, link))),
                TopoOp::Move { object, to, .. } => (*object, show(&step_path(&self.cur, to))),
            };
            let done = match step {
                TopoOp::Create { object, link } => self.create(*object, link, &plan.target[object]),
                TopoOp::Link { object, link } => self.add_link(*object, link),
                TopoOp::Unlink { object, link } => self.remove_link(*object, link),
                TopoOp::Move { object, from, to } => self.move_entry(plan, *object, from, to),
            };
            match (done, step) {
                (Ok(()), _) => self.outcome.applied_steps += 1,
                (
                    Err(StepError::Unholdable { why, twin }),
                    TopoOp::Create { link, .. } | TopoOp::Link { link, .. },
                ) => {
                    if matches!(step, TopoOp::Create { .. }) {
                        unheld_dirs.insert(object);
                    }
                    let unheld = crate::selection::Unheld {
                        object,
                        link: link.clone(),
                        why,
                        twin,
                    };
                    self.outcome.unholdable.push((unheld, display));
                }
                (Err(e), _) => self.record(object, display, e)?,
            }
        }
        hit("apply.created");
        self.apply_updates(plan)?;
        // Durable before any row says so, or a power cut would leave a row claiming a file that the
        // disk does not have, and the old file would read as a local revert.
        self.sync_touched_dirs()?;
        self.settle_watches(true);
        hit("apply.dirs.synced");
        self.store_placed_files(plan)?;
        self.store_moved(plan)?;
        self.finish_dirs(plan)?;
        // Anything that did not happen is for the next scan to find out.
        if self.outcome.clean() {
            self.drop_settled_marks(plan, &marks)?;
        }
        self.store_unstored(&[])?;
        self.outcome.touched =
            self.outcome.applied_steps > 0 || !self.placed.is_empty() || !plan.copies.is_empty();
        hit("apply.finished");
        Ok((self.outcome, self.cur))
    }
}

/// Undo what a killed run left half-done, before looking at the tree.
pub fn recover(root: &mut Root) -> Result<Vec<String>> {
    let mut notes = Vec::new();

    // Staged entries go back where they came from.
    let stage = root.state_dir.open_dir(b"stage")?;
    for (key, intent) in root.state.intents::<StageIntent>(INTENT_STAGE)? {
        hit("recover.stage");
        let name = intent.stage.as_bytes();
        if stage.try_stat(name)?.is_some() {
            restore_staged(
                root,
                &stage,
                name,
                &intent.origin,
                intent.dest.as_deref(),
                &mut notes,
            )?;
        }
        root.state.drop_intent(&key)?;
    }
    // A scratch entry a killed run left in the state directory, which held nothing.
    for name in root.state_dir.list()? {
        if name.starts_with(SCRATCH_PREFIX.as_bytes()) {
            let _ = root.state_dir.unlink(&name);
        }
    }

    // Anything in the stage with no intent is kept too, not dropped.
    for name in stage.list()? {
        let label =
            Name::new(format!("recovered-{}", String::from_utf8_lossy(&name)).into_bytes())?;
        restore_staged(root, &stage, &name, &[label], None, &mut notes)?;
    }

    // A conflict copy moved aside under some of its names takes the rest of them, or the names it still
    // has would read as the copy's own once the original's come down, and one removed elsewhere would
    // come back holding this device's version. The mark stays until the next finished sync.
    for (_, intent) in root.state.intents::<CopyIntent>(INTENT_COPY)? {
        hit("recover.copy");
        for (from, to) in &intent.moves {
            if let Err(e) = finish_copy_move(root, &intent, from, to) {
                notes.push(format!(
                    "{}: could not finish moving it aside as a conflict copy after an interrupted sync: {e}",
                    show(from)
                ));
            }
        }
    }

    // New content that reached only some names of a file reaches the rest.
    for (key, intent) in root.state.intents::<RelinkIntent>(INTENT_RELINK)? {
        hit("recover.relink");
        if let Err(e) = finish_relink(root, &intent) {
            notes.push(format!(
                "{}: could not finish updating its hard links after an interrupted sync: {e}",
                show(&intent.first)
            ));
        }
        root.state.drop_intent(&key)?;
    }

    // Directories relaxed for a moment get their real mode back.
    for (key, intent) in root.state.intents::<ModeIntent>(INTENT_MODE)? {
        hit("recover.mode");
        let names: Result<Vec<Name>, _> =
            intent.path.iter().map(|b| Name::new(b.0.clone())).collect();
        if let Ok(names) = names {
            if let Ok(dir) = root.dir.open_path(names.iter()) {
                if dir
                    .stat_self()
                    .is_ok_and(|s| s.ino == intent.ino && s.dev == intent.dev)
                {
                    dir.chmod(b".", intent.mode)?;
                }
            }
        }
        root.state.drop_intent(&key)?;
    }
    Ok(notes)
}

/// Swap what two names in `dir` hold, `a` one of this run's temporaries: in one step where the filesystem
/// can, and otherwise in three, as on NTFS. There a third name, another temporary, holds what is at `b`
/// meanwhile, so that whenever a kill comes each file is under a name, and whatever is under a temporary
/// is looked at by the next scan, kept if it holds an edit. A plain rename over `b` would take an edit
/// made in the moment since the last look. False where neither works, as without hard links.
fn swap(root: &Root, dir: &Dir, a: &[u8], b: &[u8]) -> std::io::Result<bool> {
    if dir.exchange(a, b)? {
        return Ok(true);
    }
    let held = root.aside_name(b);
    if dir.hard_link(b, dir, &held).is_err() {
        return Ok(false);
    }
    hit("apply.swap.held");
    if let Err(e) = dir.rename_over(a, dir, b) {
        let _ = dir.unlink(&held);
        return Err(e);
    }
    hit("apply.swap.renamed");
    dir.rename_over(&held, dir, a)?;
    Ok(true)
}

/// One name of a conflict copy to the name the plan gave it, if that name still holds the copy and
/// nothing has taken the new one.
fn finish_copy_move(root: &Root, intent: &CopyIntent, from: &[Name], to: &[Name]) -> Result<()> {
    let (Some((from_name, from_parents)), Some((to_name, to_parents))) =
        (from.split_last(), to.split_last())
    else {
        return Ok(());
    };
    let Ok(dir) = root.dir.open_path(from_parents.iter()) else {
        return Ok(());
    };
    let Some(st) = dir.try_stat(from_name.as_bytes())? else {
        return Ok(());
    };
    let kind = match st.file_type {
        fsops::FileType::Directory => ObjectKind::Dir,
        fsops::FileType::Symlink => ObjectKind::Symlink,
        _ => ObjectKind::File,
    };
    let same = (st.dev, st.ino) == (intent.dev, intent.ino)
        && intent
            .is
            .as_ref()
            .is_none_or(|(k, btime)| *k == kind && *btime == st.btime);
    if !same {
        return Ok(());
    }
    let Ok(to_dir) = root.dir.open_path(to_parents.iter()) else {
        return Ok(());
    };
    relaxed_for_a_moment(root, &dir, from_parents, || {
        match dir.rename_new(from_name.as_bytes(), &to_dir, to_name.as_bytes()) {
            Ok(()) => hit("recover.copy.renamed"),
            // Something took the name meanwhile: the copy keeps this one, and the merge finds it.
            Err(e) if is_exists(&e) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    })
}

/// Point the remaining names at the new inode, if the rename to it happened. A name that no longer holds
/// the old file untouched has someone's edit in it, made in the moment before the kill or since: it gets
/// the new inode too, and the edit is left under an aside name for the scan that follows to keep under a
/// conflict name. Left where it is, it would split the file in two, the edit taking that name from the
/// server's version for every device.
fn finish_relink(root: &Root, intent: &RelinkIntent) -> Result<()> {
    let Some((first_name, first_parents)) = intent.first.split_last() else {
        return Ok(());
    };
    let first_dir = root.dir.open_path(first_parents.iter())?;
    let Some(st) = first_dir.try_stat(first_name.as_bytes())? else {
        return Ok(());
    };
    if st.ino != intent.new_ino {
        return Ok(());
    }
    let old = (intent.old.ino, intent.old.size, intent.old.mtime);
    for other in &intent.others {
        let Some((name, parents)) = other.split_last() else {
            continue;
        };
        let dir = root.dir.open_path(parents.iter())?;
        let Some(now) = dir.try_stat(name.as_bytes())? else {
            continue;
        };
        // Already done, or a folder a person put there, which no file takes the place of.
        if now.ino == intent.new_ino || now.file_type == fsops::FileType::Directory {
            continue;
        }
        let mut untouched = (now.ino, now.size, now.mtime) == old;
        // The timestamps can be set back: only the content says the old file is untouched.
        if let (true, Some(want)) = (untouched, intent.old_manifest) {
            let fd = dir.open_read(name.as_bytes())?;
            untouched = crate::chunker::manifest_of(&fd, now.size)?.hash() == want;
        }
        relaxed_for_a_moment(root, &dir, parents, || {
            let tmp = root.aside_name(name.as_bytes());
            first_dir.hard_link(first_name.as_bytes(), &dir, &tmp)?;
            hit("recover.relink.linked");
            // Swapped, as in `replace_content`: an edit since that look is not overwritten.
            if swap(root, &dir, &tmp, name.as_bytes())? {
                hit("recover.relink.swapped");
                let was = dir.stat(&tmp)?;
                // What was there: the old file untouched, or an edit that another name still holds,
                // the aside a killed swap left for one, or else only this one, which stays.
                if (untouched && (was.ino, was.size, was.mtime) == old) || was.nlink > 1 {
                    dir.unlink(&tmp)?;
                }
            } else if untouched {
                dir.rename_over(&tmp, &dir, name.as_bytes())?;
            } else {
                // No swap on this filesystem: the edit stays at its name rather than risk it.
                dir.unlink(&tmp)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn restore_staged(
    root: &Root,
    stage: &Dir,
    name: &[u8],
    origin: &[Name],
    dest: Option<&[Name]>,
    notes: &mut Vec<String>,
) -> Result<()> {
    let Some(last) = origin.last() else {
        return Ok(());
    };
    for place in std::iter::once(origin).chain(dest) {
        let Some((place_name, place_parents)) = place.split_last() else {
            continue;
        };
        if let Ok(dir) = root.dir.open_path(place_parents.iter()) {
            match stage.rename_new(name, &dir, place_name.as_bytes()) {
                Ok(()) => return Ok(()),
                Err(e) if is_exists(&e) || is_not_found(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    // The original place is taken or gone. It stays, under a name of its own at the top.
    for attempt in 0u32.. {
        let label = format!("{last} (recovered from interrupted sync {})", attempt + 1);
        match stage.rename_new(name, &root.dir, label.as_bytes()) {
            Ok(()) => {
                notes.push(format!(
                    "{} could not go back in place after an interrupted sync; it is now at {label}",
                    show(origin)
                ));
                return Ok(());
            }
            Err(e) if is_exists(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Remove a killed run's temporary from a read-only directory, relaxing it only for the moment and
/// durably noting the mode to go back to.
pub fn remove_stale_temp(root: &Root, path: &[Name], name: &[u8]) -> Result<()> {
    let dir = root.dir.open_path(path.iter())?;
    relaxed_for_a_moment(root, &dir, path, || {
        dir.unlink(name)?;
        hit("clean.removed");
        Ok(())
    })
}

/// What this filesystem makes of `want` on something new, which is how new content arrives: asked of it
/// on a scratch entry, under the policy the run that placed the file applied. The entry is made beside
/// the file, in `folder`, so that what a folder gives what is made in it, an inherited ACL or a
/// set-group-ID folder's group, is in it too, and the folder's time is put back after. Without a folder,
/// for a dry run, or where the folder cannot be written to, it is made in the state directory instead,
/// which writes nothing in the tree. What a file holds that differs from this was changed since, and what
/// `want` asks for and this does not hold is no one's doing. `None` for a folder, which is not asked; an
/// error if it cannot be asked, a disk too full for one more entry say, since what is there would then
/// stand for what arrived, and a change made here would never go up.
pub fn what_arrives(
    root: &Root,
    accounts: &Accounts,
    content: &Content,
    want: &Metadata,
    policy: &metadata::ApplyPolicy,
    folder: Option<&[Name]>,
) -> Result<Option<Metadata>> {
    if *content == Content::Dir {
        return Ok(None);
    }
    // Windows keeps no record of what a folder gives a new file.
    let folder = folder.filter(|_| cfg!(unix));
    let beside = match folder {
        Some(path) => {
            let dir = root.dir.open_path(path.iter())?;
            let before = dir.stat_self()?;
            let name = root.temp_name();
            match scratch(root, accounts, &dir, &name, content, want, policy) {
                Err(e) if is_permission(&e) => None,
                done => {
                    let now = dir.stat_self()?;
                    if now.mtime != before.mtime {
                        dir.set_mtime(b".", before.mtime)?;
                    }
                    Some(done?)
                }
            }
        }
        None => None,
    };
    if let Some(held) = beside {
        return Ok(Some(held));
    }
    let state = root.dir.open_dir(cairn_protocol::STATE_DIR)?;
    let name = format!("{SCRATCH_PREFIX}{}", hex::encode(rand::random::<[u8; 8]>()));
    Ok(Some(scratch(
        root,
        accounts,
        &state,
        name.as_bytes(),
        content,
        want,
        policy,
    )?))
}

/// The name of a scratch entry in the state directory, which a killed run can leave.
pub const SCRATCH_PREFIX: &str = "arriving-probe";

fn scratch(
    root: &Root,
    accounts: &Accounts,
    dir: &Dir,
    name: &[u8],
    content: &Content,
    want: &Metadata,
    policy: &metadata::ApplyPolicy,
) -> std::io::Result<Metadata> {
    match content {
        Content::File { .. } => drop(dir.create_new(name, 0o600)?),
        Content::Symlink { target } => dir.symlink(&target.0, name)?,
        Content::Dir => return Err(std::io::Error::other("a folder is not asked")),
    }
    // Short of room, it would hold less than the file did, and the difference would read as a change.
    let applied =
        metadata::apply_unless_full(dir, name, content.kind(), want, None, policy, accounts);
    let held = applied.and_then(|_| dir.stat(name)).map(|st| {
        let precision = root.dir.fstype_precision_ns();
        metadata::capture(dir, name, &st, accounts, precision, &mut Vec::new())
    });
    let gone = dir.unlink(name);
    let held = held?;
    gone?;
    Ok(held)
}

/// A killed run's temporary that is a file it was replacing or removing, and no longer holds what was
/// scanned: someone edited it in the moment before, and this is the only copy. It stays, under a conflict
/// name, and goes up as a new file.
pub fn keep_rescued_temp(
    root: &Root,
    path: &[Name],
    temp: &[u8],
    original: &Name,
    date: &str,
) -> Result<String> {
    let dir = root.dir.open_path(path.iter())?;
    let mut kept = None;
    relaxed_for_a_moment(root, &dir, path, || {
        for attempt in 0u32.. {
            let name = crate::merge::conflict_name(
                original,
                &root.config.device_name,
                date,
                attempt,
                crate::merge::in_git_dir(path),
            );
            match dir.rename_new(temp, &dir, name.as_bytes()) {
                Ok(()) => {
                    kept = Some(name);
                    break;
                }
                Err(e) if is_exists(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        hit("clean.rescued");
        Ok(())
    })?;
    let at = kept.map_or_else(String::new, |name| {
        let mut full = path.to_vec();
        full.push(name);
        show(&full)
    });
    Ok(format!(
        "{at}: an edit made while a sync was replacing or removing this file; it is kept under this name"
    ))
}

/// Run `f` with a directory writable, durably noting the mode to go back to.
fn relaxed_for_a_moment(
    root: &Root,
    dir: &Dir,
    path: &[Name],
    f: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let st = dir.stat_self()?;
    if st.mode & 0o300 == 0o300 {
        return f();
    }
    let key = format!("{INTENT_MODE}temp-{}-{}", st.dev, st.ino);
    let bytes = path.iter().map(|n| Bytes(n.as_bytes().to_vec())).collect();
    root.state.put_intent(
        &key,
        &ModeIntent {
            path: bytes,
            dev: st.dev,
            ino: st.ino,
            mode: st.mode,
        },
    )?;
    hit("clean.relax.intent");
    dir.chmod(b".", st.mode | 0o300)?;
    hit("clean.relaxed");
    let done = f();
    dir.chmod(b".", st.mode)?;
    root.state.drop_intent(&key)?;
    done
}

/// Read one object's chunk for upload, verifying that the file still holds it.
pub fn read_for_upload(
    root: &Root,
    cur: &BTreeMap<ObjectId, CurNode>,
    id: ObjectId,
    offset: u64,
    chunk: &ChunkRef,
) -> Result<Option<Vec<u8>>> {
    let Some(link) = cur.get(&id).and_then(|n| n.links.iter().next()) else {
        return Ok(None);
    };
    let path = step_path(cur, link);
    let Some((name, parents)) = path.split_last() else {
        return Ok(None);
    };
    let Ok(dir) = root.dir.open_path(parents.iter()) else {
        return Ok(None);
    };
    let Ok(fd) = dir.open_read(name.as_bytes()) else {
        return Ok(None);
    };
    let mut buf = vec![0u8; chunk.len as usize];
    if fsops::read_exact_at(fd.as_fd(), &mut buf, offset).is_err() || Hash::of(&buf) != chunk.hash {
        return Ok(None);
    }
    Ok(Some(buf))
}

pub fn cur_from_identified(
    local: &BTreeMap<ObjectId, crate::merge::LNode>,
    scanned: &BTreeMap<ObjectId, Scanned>,
    adopted: &BTreeMap<ObjectId, ObjectId>,
) -> BTreeMap<ObjectId, CurNode> {
    let reverse: BTreeMap<ObjectId, ObjectId> = adopted.iter().map(|(a, b)| (*b, *a)).collect();
    local
        .iter()
        .map(|(id, n)| {
            let original = reverse.get(id).copied().unwrap_or(*id);
            let fp = (!n.carried)
                .then(|| scanned.get(&original).map(|s| s.fp.clone()))
                .flatten();
            (
                *id,
                CurNode {
                    kind: n.kind(),
                    links: n.links.clone(),
                    content: n.content.clone(),
                    apply_from: n.apply_from.clone(),
                    fp,
                    holds_excluded: scanned.get(&original).is_some_and(|s| s.holds_excluded),
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a case-insensitive filesystem, as macOS has by default; elsewhere there is nothing to see.
    #[test]
    fn a_name_the_same_as_another_here_is_unholdable_and_a_taken_one_is_not() {
        let dir = std::env::temp_dir().join(format!("cairn-twin-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("Foo.txt"), b"x").unwrap();
        let folds = std::fs::metadata(dir.join("foo.txt")).is_ok();
        let d = Dir::open_root(&dir).unwrap();
        let exists = fsops::exists_error;
        // Taken by exactly this name: something got there first, for another round.
        assert!(matches!(
            unholdable(&d, b"Foo.txt", exists()),
            StepError::Changed(_)
        ));
        if folds {
            match unholdable(&d, b"foo.txt", exists()) {
                StepError::Unholdable { why, twin: true } => {
                    assert!(why.contains("Foo.txt"), "{why}")
                }
                other => panic!("{other:?}"),
            }
        }
        let ilseq = fsops::bad_name_error();
        assert!(matches!(
            unholdable(&d, b"\xff", ilseq),
            StepError::Unholdable { twin: false, .. }
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
