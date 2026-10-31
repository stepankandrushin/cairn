//! Making the local tree match a plan. Every file arrives the same way: temporary in the same
//! directory → verified → fsync → metadata → check the destination has not changed → atomic rename.
//! No step overwrites anything it has not checked, and a kill at any point leaves whole files at
//! synced paths (invariant 4).

use crate::client::Client;
use crate::fsops::{self, is_exists, is_not_found, Dir};
use crate::identify::CopyIntent;
use crate::merge::{ConflictCopy, Plan, TNode};
use crate::metadata::{self, Accounts};
use crate::root::Root;
use crate::scan::{fingerprint, Scanned};
use crate::schedule::{is_stage, schedule, Topo, TopoOp};
use crate::state::{Fingerprint, Row, Synced};
use anyhow::{anyhow, Context, Result};
use cairn_failpoint::{check, hit};
use cairn_protocol::wire::MAX_CHUNK_BATCH_BYTES;
use cairn_protocol::{
    Bytes, ChunkRef, Content, Hash, Link, Manifest, Metadata, Name, ObjectId, ObjectKind, Skip,
    SkipKind, Tree, STAGE_PREFIX,
};
use rustix::fd::AsFd;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// A staged entry and where it came from, so that a crash can put it back.
#[derive(Debug, Serialize, Deserialize)]
pub struct StageIntent {
    pub stage: String,
    pub origin: Vec<Name>,
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

/// Why one step did not happen.
#[derive(Debug, thiserror::Error)]
enum StepError {
    /// The tree moved under this run; the next round sees it and decides again.
    #[error("{0}")]
    Changed(String),
    /// This step cannot be done here, and repeating it now would not help.
    #[error("{0}")]
    Failed(String),
    /// Stop everything: nothing more can be written.
    #[error("disk full")]
    DiskFull(#[source] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<std::io::Error> for StepError {
    fn from(e: std::io::Error) -> Self {
        match e.raw_os_error() {
            Some(28) | Some(122) => StepError::DiskFull(e),
            Some(17) | Some(39) => StepError::Changed(e.to_string()),
            _ if is_not_found(&e) => StepError::Changed(e.to_string()),
            _ => StepError::Failed(e.to_string()),
        }
    }
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
}

#[derive(Debug, Default)]
pub struct ApplyOutcome {
    /// Something moved under this run; try again.
    pub changed: Vec<Skip>,
    /// Could not be applied; reported, and the server still has it.
    pub failed: Vec<Skip>,
    /// Metadata fields this filesystem could not take.
    pub warnings: Vec<Skip>,
    pub applied_steps: u64,
    /// This run stopped before changing the tree's structure; nothing further should be pushed.
    pub halted: bool,
    /// The filesystem was written to, so a fresh scan must confirm the result.
    pub touched: bool,
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
    euid: u32,
    outcome: ApplyOutcome,
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
            euid: rustix::process::geteuid().as_raw(),
            outcome: ApplyOutcome::default(),
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
        Ok(dir.open_dir(&name)?)
    }

    /// Make a directory writable for a moment if its mode forbids this user, remembering durably to
    /// put it back.
    fn writable(&mut self, parent: ObjectId, dir: &Dir) -> Result<(), StepError> {
        if self.euid == 0 || self.relaxed.contains_key(&parent) {
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
                if (fp.dev, fp.ino, fp.size, fp.mtime, fp.btime)
                    == (now.dev, now.ino, now.size, now.mtime, now.btime)
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

    /// Bring the chunks that the plan's new content needs into the local cache before touching the
    /// tree, so that a full disk or a lost server shows up while nothing has changed yet.
    fn prefetch(&mut self, plan: &Plan) -> Result<(), StepError> {
        let Some(client) = self.client else {
            return Ok(());
        };
        let mut want: Vec<(Hash, u32)> = Vec::new();
        let mut seen = HashSet::new();
        // A conflict copy's content is here on disk, and the server has yet to see it.
        let copies: BTreeSet<ObjectId> = plan.copies.iter().map(|c| c.copy).collect();
        for (id, t) in &plan.target {
            let Content::File { manifest, .. } = &t.body.content else {
                continue;
            };
            if copies.contains(id)
                || self
                    .cur
                    .get(id)
                    .is_some_and(|c| c.content == t.body.content)
            {
                continue;
            }
            let m = self.manifest(manifest)?;
            // A chunk the old local file holds comes from there instead.
            let old = match self.cur.get(id).map(|c| c.content.clone()) {
                Some(Content::File { manifest: old, .. }) => self
                    .manifest(&old)
                    .map(|m| m.chunks().map(|c| c.hash).collect::<HashSet<_>>())
                    .unwrap_or_default(),
                _ => HashSet::new(),
            };
            for c in m.chunks() {
                if !old.contains(&c.hash) && seen.insert(c.hash) {
                    want.push((c.hash, c.len));
                }
            }
        }
        let cache = self.root.state_dir.open_dir(b"chunks")?;
        let mut batch: Vec<Hash> = Vec::new();
        let mut size = 0usize;
        for (hash, len) in want {
            // A chunk already here is a transfer resumed after an interruption.
            if cache.try_stat(&Self::cache_path(&hash))?.is_some() {
                continue;
            }
            if size + len as usize > MAX_CHUNK_BATCH_BYTES && !batch.is_empty() {
                Self::fetch_batch(client, &cache, &batch)?;
                batch.clear();
                size = 0;
            }
            batch.push(hash);
            size += len as usize;
        }
        if !batch.is_empty() {
            Self::fetch_batch(client, &cache, &batch)?;
        }
        Ok(())
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
        let done = self.write_temp_inner(dir, &tmp, id, t, display);
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
                rustix::fs::ftruncate(&fd, *size).map_err(std::io::Error::from)?;
                // Where the old version of this object is, to reuse its chunks.
                let old = self.old_chunks(id)?;
                for (offset, chunk) in m.placed_chunks() {
                    let source = old.as_ref().and_then(|(d, name, map)| {
                        map.get(&chunk.hash).map(|at| (d, name.as_slice(), *at))
                    });
                    let data = self.read_chunk(chunk, source)?;
                    check("apply.temp.write")?;
                    fsops::write_all_at(fd.as_fd(), &data, offset)?;
                }
                hit("apply.temp.written");
                rustix::fs::fsync(&fd).map_err(std::io::Error::from)?;
                hit("apply.temp.synced");
                self.apply_meta(dir, tmp, kind, &t.body.metadata, None, display);
                rustix::fs::fsync(&fd).map_err(std::io::Error::from)?;
                hit("apply.temp.meta");
                return Ok(());
            }
        }
        self.apply_meta(dir, tmp, kind, &t.body.metadata, None, display);
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
    ) {
        let problems = metadata::apply(
            dir,
            name,
            kind,
            want,
            current,
            &self.root.config.policy,
            self.accounts,
        );
        for p in problems {
            self.outcome.warnings.push(Skip {
                path: show(display),
                kind: SkipKind::CannotApply,
                detail: Some(p),
            });
        }
    }

    /// Make a new object appear at its first link.
    fn create(&mut self, id: ObjectId, link: &Link, t: &TNode) -> Result<(), StepError> {
        let (dir, name) = self.locate(link)?;
        self.writable(link.parent, &dir)?;
        self.touched_dirs.insert(link.parent);
        let display = step_path(&self.cur, link);
        let mut written = None;
        if t.body.content == Content::Dir {
            check("apply.mkdir")?;
            // Private until the final pass gives it its real mode.
            dir.mkdir(&name, 0o700)?;
            hit("apply.mkdir.done");
            self.touched_dirs.insert(id);
        } else {
            let tmp = self.write_temp(&dir, id, t, &display)?;
            // Taken before the rename: any later difference but ctime is someone's edit.
            written = Some(fingerprint(&dir.stat(&tmp)?));
            hit("apply.create.before_rename");
            let placed = dir.rename_new(&tmp, &dir, &name);
            if let Err(e) = placed {
                let _ = dir.unlink(&tmp);
                return Err(e.into());
            }
            hit("apply.create.renamed");
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
        from_dir.hard_link(&from_name, &to_dir, &to_name)?;
        hit("apply.link.done");
        self.touched_dirs.insert(link.parent);
        self.refresh_fp(id, &to_dir, &to_name);
        if let Some(n) = self.cur.get_mut(&id) {
            n.links.insert(link.clone());
        }
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
            let aside = self.root.temp_name();
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

    fn move_entry(&mut self, id: ObjectId, from: &Link, to: &Link) -> Result<(), StepError> {
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
        if dir.exchange(&tmp, &name)? {
            hit("apply.replace.swapped");
            if !self.still_as_scanned(id, &dir, &tmp) {
                dir.exchange(&tmp, &name)?;
                let _ = dir.unlink(&tmp);
                let _ = self.root.state.drop_intent(&relink_key);
                return Err(StepError::Changed(
                    "the file changed during this sync".into(),
                ));
            }
            dir.unlink(&tmp)?;
        } else {
            dir.rename_over(&tmp, &dir, &name)?;
        }
        hit("apply.replace.renamed");
        self.touched_dirs.insert(first.parent);
        // The other hard links follow, each atomically.
        for other in links.iter().skip(1) {
            let (odir, oname) = self.locate(other)?;
            self.writable(other.parent, &odir)?;
            let otmp = self.root.temp_name();
            dir.hard_link(&name, &odir, &otmp)?;
            hit("apply.replace.relink");
            odir.rename_over(&otmp, &odir, &oname)?;
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
            conflict_of: plan.target[&copy.copy]
                .conflict_of
                .clone()
                .expect("a copy has its origin"),
        };
        for (from, _) in &copy.moves {
            let (dir, name) = self.locate(from)?;
            self.check_fp(copy.object, &dir, &name)?;
        }
        self.root
            .state
            .put_intent(&format!("{INTENT_COPY}{}", copy.copy), &intent)?;
        hit("apply.copy.intent");
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
                    );
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
                );
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
                Err(e) => self.record(id, show(&display), e)?,
            }
        }
        Ok(())
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
        let in_step = in_step.filter(|_| untouched);
        let synced = in_step.map(|(t, _)| Synced {
            body: t.body.clone(),
            links: t.links.clone(),
            applied: scan.clone(),
            commit: None,
        });
        let old = self.rows.get(&id);
        let row = Row {
            fp: fingerprint(&st),
            // Only a scan, which reads the file, may vouch that a fingerprint means this content: an
            // edit since the rename would otherwise be recorded as the content just written.
            racy: true,
            content: cur.content.clone(),
            incomplete: !problems.is_empty(),
            scan,
            applied: old.and_then(|o| o.applied.clone()),
            synced: if untouched {
                synced.or_else(|| old.and_then(|o| o.synced.clone()))
            } else {
                None
            },
        };
        self.root.state.store_rows(&[(id, &row)], &[])?;
        self.rows.insert(id, row);
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
            let Ok((dir, name)) = self.locate(&link) else {
                continue;
            };
            if let Err(e) = self.store_one(plan, id, &dir, &name) {
                self.record(id, show(&step_path(&self.cur, &link)), e)?;
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

    /// Reach the plan's target as far as possible.
    pub fn run(mut self, plan: &Plan) -> Result<(ApplyOutcome, BTreeMap<ObjectId, CurNode>)> {
        // Download first: a full disk or a lost server is found before anything moves.
        if let Err(e) = self.prefetch(plan) {
            self.record(ObjectId::ROOT, String::new(), e)?;
            self.outcome.halted = true;
            return Ok((self.outcome, self.cur));
        }
        hit("apply.prefetched");
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
        for step in &steps {
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
                TopoOp::Move { object, from, to } => self.move_entry(*object, from, to),
            };
            match done {
                Ok(()) => self.outcome.applied_steps += 1,
                Err(e) => self.record(object, display, e)?,
            }
        }
        self.apply_updates(plan)?;
        // Durable before any row says so, or a power cut would leave a row claiming a file that the
        // disk does not have, and the old file would read as a local revert.
        self.sync_touched_dirs()?;
        self.store_placed_files(plan)?;
        self.finish_dirs(plan)?;
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
            restore_staged(root, &stage, name, &intent.origin, &mut notes)?;
        }
        root.state.drop_intent(&key)?;
    }
    // Anything in the stage with no intent is kept too, not dropped.
    for name in stage.list()? {
        let label =
            Name::new(format!("recovered-{}", String::from_utf8_lossy(&name)).into_bytes())?;
        restore_staged(root, &stage, &name, &[label], &mut notes)?;
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

/// Point the remaining names at the new inode, if the rename to it happened, and only where a name
/// still holds the old file untouched: an edit made since stays where it is.
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
    for other in &intent.others {
        let Some((name, parents)) = other.split_last() else {
            continue;
        };
        let dir = root.dir.open_path(parents.iter())?;
        let Some(now) = dir.try_stat(name.as_bytes())? else {
            continue;
        };
        if (now.ino, now.size, now.mtime) != (intent.old.ino, intent.old.size, intent.old.mtime) {
            continue;
        }
        // The timestamps can be set back: only the content says the old file is untouched.
        if let Some(want) = intent.old_manifest {
            let fd = dir.open_read(name.as_bytes())?;
            if crate::chunker::manifest_of(&fd, now.size)?.hash() != want {
                continue;
            }
        }
        relaxed_for_a_moment(root, &dir, parents, || {
            let tmp = root.temp_name();
            first_dir.hard_link(first_name.as_bytes(), &dir, &tmp)?;
            hit("recover.relink.linked");
            dir.rename_over(&tmp, &dir, name.as_bytes())?;
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
    notes: &mut Vec<String>,
) -> Result<()> {
    let Some((last, parents)) = origin.split_last() else {
        return Ok(());
    };
    let at_origin = root.dir.open_path(parents.iter());
    if let Ok(dir) = at_origin {
        match stage.rename_new(name, &dir, last.as_bytes()) {
            Ok(()) => return Ok(()),
            Err(e) if is_exists(&e) || is_not_found(&e) => {}
            Err(e) => return Err(e.into()),
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
                },
            )
        })
        .collect()
}
