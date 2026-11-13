//! What was deleted, and bringing it back: the tray's "Recently deleted", `cairn deleted` and
//! `cairn undelete`. Read from the journal, as the conflict inbox is, until the server keeps a trash of
//! its own (PLAN.md, milestone 4).
//!
//! A delete is the unlink that leaves an object with no entry. The server keeps that object's last
//! state, and `Op::Link` brings it back as itself, so bringing something back reverts nothing and
//! replaces nothing: each object returns to the entry it last had, holding what it last held, under
//! another name if that one has since been taken.

use crate::client::{Client, Retries};
use crate::merge::{conflict_name, in_git_dir};
use crate::root::Config;
use crate::scan::display_path;
use anyhow::{anyhow, bail, Result};
use cairn_failpoint::hit;
use cairn_protocol::local::{DeletedItem, UndeletedItem};
use cairn_protocol::wire::{Change, CommitRequest, CommitResponse};
use cairn_protocol::{Content, Link, Name, ObjectId, ObjectKind, Op, OpId};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What one device deletes inside a folder within this long before deleting the folder is part of
/// the folder's delete: one sync's commits, however many it took, which other devices' may come
/// between.
pub const GROUP_SECS: i64 = 600;

/// Operations in one commit of a restore. One commit is all or nothing; a restore larger than this goes
/// in several, each leaving a valid tree, since every folder comes back before what is in it.
const MAX_OPS: usize = 100_000;
const MAX_BYTES: usize = 48 * 1024 * 1024;

/// More levels than this is a corrupt index, not a deep tree.
const MAX_DEPTH: usize = 1 << 20;

#[derive(Clone, Debug)]
struct Gone {
    seq: u64,
    time: i64,
    device: u32,
    link: Link,
}

#[derive(Clone, Debug)]
struct Obj {
    kind: ObjectKind,
    size: u64,
    /// Its entries now: the first, and any further hard links.
    link: Option<Link>,
    more: Vec<Link>,
    /// Set while it has no entry: the unlink that left it with none.
    gone: Option<Box<Gone>>,
}

impl Obj {
    fn live(&self) -> bool {
        self.link.is_some()
    }
}

/// The journal, replayed into what a list of deletes needs: every object's kind, size and entries, and
/// for those deleted, by whom, when and from where. No bodies, so it holds a fraction of a `Tree`.
#[derive(Default)]
pub struct Index {
    objects: HashMap<ObjectId, Obj>,
    devices: Vec<String>,
    device_ix: HashMap<String, u32>,
    /// The last sequence replayed.
    pub head: u64,
}

/// One delete as a person sees it: a file, or a folder with what was deleted with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deleted {
    pub object: ObjectId,
    /// The commit that deleted it, the last of its folder's.
    pub seq: u64,
    pub time: i64,
    pub device_name: String,
    /// Where it was: its folder's path now, or where that folder was if it is deleted too.
    pub path: Vec<Name>,
    pub kind: ObjectKind,
    /// A file's size; for a folder, the bytes of the files deleted with it.
    pub bytes: u64,
    /// For a folder, the files deleted with it.
    pub files: u64,
    /// Its folder is deleted too, and comes back with it, empty of anything else.
    pub folder_gone: bool,
}

/// One thing brought back, and where it is now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Undeleted {
    pub object: ObjectId,
    pub path: Vec<Name>,
    /// Where it was when deleted.
    pub was: Vec<Name>,
    /// Its name had been taken since, so it came back beside what has it now.
    pub renamed: bool,
    /// Objects that came back with it, itself and any folder above it included.
    pub objects: usize,
}

#[derive(Clone, Debug, Default)]
pub struct UndeleteOutcome {
    /// The commits made, in order: one unless the restore was too large for one.
    pub seqs: Vec<u64>,
    pub items: Vec<Undeleted>,
    /// Asked for but not deleted now, with where each is.
    pub already: Vec<(ObjectId, Vec<Name>)>,
}

impl Index {
    pub fn new() -> Self {
        let mut objects = HashMap::new();
        objects.insert(
            ObjectId::ROOT,
            Obj {
                kind: ObjectKind::Dir,
                size: 0,
                link: None,
                more: Vec::new(),
                gone: None,
            },
        );
        Index {
            objects,
            ..Default::default()
        }
    }

    /// Replays what the journal has after `head`.
    pub fn update(&mut self, client: &Client) -> Result<()> {
        loop {
            let resp = client.changes(self.head)?;
            if resp.changes.is_empty() {
                break;
            }
            for change in &resp.changes {
                self.apply(change)?;
                self.head = change.seq;
            }
            hit("deleted.page");
            if self.head >= resp.head {
                break;
            }
        }
        Ok(())
    }

    fn device(&mut self, name: &str) -> u32 {
        if let Some(i) = self.device_ix.get(name) {
            return *i;
        }
        let i = self.devices.len() as u32;
        self.devices.push(name.to_string());
        self.device_ix.insert(name.to_string(), i);
        i
    }

    pub fn apply(&mut self, change: &Change) -> Result<()> {
        let device = self.device(&change.device_name);
        let corrupt = |o: &ObjectId| {
            anyhow!(
                "the journal does not replay at seq {}: object {o}",
                change.seq
            )
        };
        for op in &change.ops {
            match op {
                Op::Create {
                    object, body, link, ..
                } => {
                    self.objects.insert(
                        *object,
                        Obj {
                            kind: body.content.kind(),
                            size: size_of(&body.content),
                            link: Some(link.clone()),
                            more: Vec::new(),
                            gone: None,
                        },
                    );
                }
                Op::Update { object, body, .. } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    o.size = size_of(&body.content);
                }
                Op::Link { object, link } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    if o.link.is_none() {
                        o.link = Some(link.clone());
                    } else {
                        o.more.push(link.clone());
                    }
                    o.gone = None;
                }
                Op::Unlink { object, link, .. } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    if o.link.as_ref() == Some(link) {
                        o.link = o.more.pop();
                    } else if let Some(i) = o.more.iter().position(|l| l == link) {
                        o.more.swap_remove(i);
                    } else {
                        return Err(corrupt(object));
                    }
                    if o.link.is_none() {
                        o.gone = Some(Box::new(Gone {
                            seq: change.seq,
                            time: change.time,
                            device,
                            link: link.clone(),
                        }));
                    }
                }
                Op::Move { object, from, to } => {
                    let o = self
                        .objects
                        .get_mut(object)
                        .ok_or_else(|| corrupt(object))?;
                    if o.link.as_ref() == Some(from) {
                        o.link = Some(to.clone());
                    } else if let Some(l) = o.more.iter_mut().find(|l| *l == from) {
                        *l = to.clone();
                    } else {
                        return Err(corrupt(object));
                    }
                }
                Op::Note { .. } => {}
            }
        }
        Ok(())
    }

    fn get(&self, o: &ObjectId) -> Option<&Obj> {
        self.objects.get(o)
    }

    /// Where an object is, or was when it was deleted, through folders deleted too.
    fn path(&self, o: ObjectId) -> Option<Vec<Name>> {
        let mut out = Vec::new();
        let mut at = o;
        while at != ObjectId::ROOT {
            if out.len() > MAX_DEPTH {
                return None;
            }
            let obj = self.get(&at)?;
            let link = obj.link.as_ref().or(obj.gone.as_ref().map(|g| &g.link))?;
            out.push(link.name.clone());
            at = link.parent;
        }
        out.reverse();
        Some(out)
    }

    /// `x` was deleted as part of deleting the folder `p`, which it was last in.
    fn nested(x: &Gone, p: &Gone) -> bool {
        x.device == p.device && x.seq <= p.seq && p.time - x.time <= GROUP_SECS
    }

    /// For each deleted folder, what was deleted with it.
    fn with_folder(&self) -> HashMap<ObjectId, Vec<ObjectId>> {
        let mut out: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        for (id, o) in &self.objects {
            let Some(g) = &o.gone else { continue };
            let Some(p) = self.get(&g.link.parent) else {
                continue;
            };
            if let Some(pg) = &p.gone {
                if Self::nested(g, pg) {
                    out.entry(g.link.parent).or_default().push(*id);
                }
            }
        }
        out
    }

    /// The object and everything deleted with it, folders before what was in them.
    fn group(&self, top: ObjectId, with: &HashMap<ObjectId, Vec<ObjectId>>) -> Vec<ObjectId> {
        let mut out = vec![top];
        let mut i = 0;
        while i < out.len() {
            if let Some(kids) = with.get(&out[i]) {
                out.extend(kids.iter().copied());
            }
            i += 1;
        }
        out
    }

    /// Everything deleted since `since` (seconds since the epoch, the server's clock) and still
    /// deleted, newest first: a folder once, with what went with it.
    pub fn list(&self, since: i64) -> Vec<Deleted> {
        let with = self.with_folder();
        let mut out = Vec::new();
        for (id, o) in &self.objects {
            let Some(g) = &o.gone else { continue };
            if g.time < since {
                continue;
            }
            let parent = self.get(&g.link.parent);
            let folder_gone = parent.is_some_and(|p| !p.live() && g.link.parent != ObjectId::ROOT);
            if parent
                .and_then(|p| p.gone.as_ref())
                .is_some_and(|pg| Self::nested(g, pg))
            {
                continue;
            }
            let (mut bytes, mut files) = (0, 0);
            for m in self.group(*id, &with) {
                let mo = &self.objects[&m];
                if mo.kind == ObjectKind::File {
                    bytes += mo.size;
                    files += 1;
                }
            }
            if o.kind != ObjectKind::Dir {
                files = 0;
            }
            out.push(Deleted {
                object: *id,
                seq: g.seq,
                time: g.time,
                device_name: self.devices[g.device as usize].clone(),
                path: self.path(*id).unwrap_or_else(|| vec![g.link.name.clone()]),
                kind: o.kind,
                bytes,
                files,
                folder_gone,
            });
        }
        out.sort_by(|a, b| b.seq.cmp(&a.seq).then_with(|| a.path.cmp(&b.path)));
        out
    }

    /// The operations that bring these back, and what each brings: an object deleted with its folder
    /// comes back with it, a deleted folder above it comes back empty of anything else, and a name
    /// taken since gets the name a restore gives, beside what has it now.
    fn plan(&self, ids: &[ObjectId]) -> Result<(Vec<Op>, UndeleteOutcome)> {
        let with = self.with_folder();
        let mut out = UndeleteOutcome::default();
        let mut ops = Vec::new();
        // Where each object brought back is going, and the names taken in the folders it goes to.
        let mut placed: HashMap<ObjectId, Link> = HashMap::new();
        let mut taken: HashMap<ObjectId, HashSet<Name>> = HashMap::new();
        let mut bare: HashSet<ObjectId> = HashSet::new();
        let live_names = self.live_names(ids);
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(*id) {
                continue;
            }
            let o = self
                .get(id)
                .ok_or_else(|| anyhow!("{id} is not in this root's history"))?;
            if *id == ObjectId::ROOT {
                bail!("the root folder cannot be deleted, nor brought back");
            }
            if o.live() {
                out.already.push((*id, self.path(*id).unwrap_or_default()));
                continue;
            }
            if placed.contains_key(id) && !bare.contains(id) {
                // Already coming back with something asked for before it.
                continue;
            }
            let mut count = 0;
            // The deleted folders above it come back first, empty of anything else.
            let mut chain = Vec::new();
            let mut up = o.gone.as_ref().expect("deleted").link.parent;
            while up != ObjectId::ROOT && !placed.contains_key(&up) {
                let u = self
                    .get(&up)
                    .ok_or_else(|| anyhow!("folder {up} is not in this root's history"))?;
                if u.live() {
                    break;
                }
                chain.push(up);
                up = u.gone.as_ref().expect("deleted").link.parent;
                if chain.len() > MAX_DEPTH {
                    bail!("the folders above {id} do not end");
                }
            }
            let above = chain.len();
            let mut members: Vec<ObjectId> = chain.into_iter().rev().collect();
            if bare.remove(id) {
                // Back already as the folder of something asked for before it: what went with it is
                // what is left to bring.
                members.extend(self.group(*id, &with).into_iter().skip(1));
            } else {
                members.extend(self.group(*id, &with));
            }
            for (i, m) in members.iter().enumerate() {
                if placed.contains_key(m) {
                    continue;
                }
                let Some(g) = &self.objects[m].gone else {
                    continue;
                };
                let parent = g.link.parent;
                let folder_new = placed.contains_key(&parent);
                let names = taken.entry(parent).or_insert_with(|| {
                    if folder_new {
                        HashSet::new()
                    } else {
                        live_names.get(&parent).cloned().unwrap_or_default()
                    }
                });
                let mut name = g.link.name.clone();
                let mut attempt = 0;
                while names.contains(&name) {
                    let folder = self.placed_path(parent, &placed);
                    name = conflict_name(
                        &g.link.name,
                        "restored",
                        &format!("seq {}", g.seq),
                        attempt,
                        in_git_dir(&folder),
                    );
                    attempt += 1;
                }
                names.insert(name.clone());
                let link = Link { parent, name };
                placed.insert(*m, link.clone());
                ops.push(Op::Link { object: *m, link });
                if i < above {
                    bare.insert(*m);
                }
                count += 1;
            }
            out.items.push(Undeleted {
                object: *id,
                path: Vec::new(),
                was: self.path(*id).unwrap_or_default(),
                renamed: false,
                objects: count,
            });
        }
        for item in &mut out.items {
            let link = &placed[&item.object];
            item.path = self.placed_path(link.parent, &placed);
            item.path.push(link.name.clone());
            // Renamed if it, or a folder above it that came back, has a name other than its last.
            let mut at = item.object;
            while let Some(l) = placed.get(&at) {
                let was = self.objects[&at].gone.as_ref().map(|g| &g.link.name);
                if was != Some(&l.name) {
                    item.renamed = true;
                    break;
                }
                at = l.parent;
            }
        }
        Ok((ops, out))
    }

    /// The names in use in the live folders that what is asked for would come back into.
    fn live_names(&self, ids: &[ObjectId]) -> HashMap<ObjectId, HashSet<Name>> {
        let mut folders = HashSet::new();
        for id in ids {
            let mut at = *id;
            for _ in 0..MAX_DEPTH {
                let Some(o) = self.get(&at) else { break };
                if o.live() {
                    folders.insert(at);
                    break;
                }
                let Some(g) = &o.gone else { break };
                at = g.link.parent;
                if at == ObjectId::ROOT {
                    folders.insert(at);
                    break;
                }
            }
        }
        let mut out: HashMap<ObjectId, HashSet<Name>> = HashMap::new();
        for o in self.objects.values() {
            for l in o.link.iter().chain(o.more.iter()) {
                if folders.contains(&l.parent) {
                    out.entry(l.parent).or_default().insert(l.name.clone());
                }
            }
        }
        out
    }

    /// A folder's path, through what this restore is bringing back.
    fn placed_path(&self, folder: ObjectId, placed: &HashMap<ObjectId, Link>) -> Vec<Name> {
        let mut out = Vec::new();
        let mut at = folder;
        while at != ObjectId::ROOT && out.len() <= MAX_DEPTH {
            let Some(link) = placed
                .get(&at)
                .or_else(|| self.get(&at).and_then(|o| o.link.as_ref()))
            else {
                break;
            };
            out.push(link.name.clone());
            at = link.parent;
        }
        out.reverse();
        out
    }
}

fn size_of(c: &Content) -> u64 {
    match c {
        Content::File { size, .. } => *size,
        _ => 0,
    }
}

/// The index of this root's journal, replayed from the start.
pub fn index(config: &Config) -> Result<Index> {
    let client = Client::new(config, Retries::Short)?;
    let mut index = Index::new();
    index.update(&client)?;
    Ok(index)
}

/// Bring back what was deleted, by the objects `list` names, as new commits on the server; a sync
/// then brings it to each device. The index is brought up to the head first, and again after.
pub fn undelete(config: &Config, index: &mut Index, ids: &[ObjectId]) -> Result<UndeleteOutcome> {
    let client = Client::new(config, Retries::Short)?;
    let mut tries = 0;
    loop {
        index.update(&client)?;
        let (ops, mut outcome) = index.plan(ids)?;
        if ops.is_empty() {
            return Ok(outcome);
        }
        hit("undelete.planned");
        match commit_all(&client, config, ops) {
            Ok(seqs) => {
                outcome.seqs = seqs;
                hit("undelete.committed");
                index.update(&client)?;
                return Ok(outcome);
            }
            // Refused before anything of it went in: something changed under it. Once more, from the
            // journal as it is now.
            Err(Refused::First(why)) if tries == 0 => {
                tries += 1;
                let _ = why;
            }
            Err(Refused::First(why)) => {
                bail!("the server refused it, as something there just changed; nothing came back, try again: {why}")
            }
            Err(Refused::Later { done, why }) => {
                bail!(
                    "only part of it came back, in sequence(s) {done:?}, before the server refused the rest; \
                     what is still deleted is still listed: {why}"
                )
            }
            Err(Refused::Error(e)) => return Err(e),
        }
    }
}

enum Refused {
    First(String),
    Later { done: Vec<u64>, why: String },
    Error(anyhow::Error),
}

fn commit_all(client: &Client, config: &Config, ops: Vec<Op>) -> Result<Vec<u64>, Refused> {
    let mut seqs = Vec::new();
    let mut batch: Vec<Op> = Vec::new();
    let mut bytes = 0;
    let mut batches = Vec::new();
    for op in ops {
        let n = serde_json::to_vec(&op).map_or(0, |v| v.len());
        if !batch.is_empty() && (batch.len() >= MAX_OPS || bytes + n > MAX_BYTES) {
            batches.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        bytes += n;
        batch.push(op);
    }
    batches.push(batch);
    for ops in batches {
        let req = CommitRequest {
            op_id: OpId::random(),
            device: config.device,
            device_name: config.device_name.clone(),
            ops,
        };
        match client.commit(&req) {
            Ok(CommitResponse::Committed { seq }) => seqs.push(seq),
            Ok(CommitResponse::Rejected { reject }) if seqs.is_empty() => {
                return Err(Refused::First(format!("{reject:?}")))
            }
            Ok(CommitResponse::Rejected { reject }) => {
                return Err(Refused::Later {
                    done: seqs,
                    why: format!("{reject:?}"),
                })
            }
            Err(e) if seqs.is_empty() => return Err(Refused::Error(e)),
            Err(e) => {
                return Err(Refused::Error(e.context(format!(
                    "only part of it came back, in sequence(s) {seqs:?}; what is still deleted is still listed"
                ))))
            }
        }
    }
    Ok(seqs)
}

impl Deleted {
    /// As the local API and `cairn deleted --json` carry it.
    pub fn item(&self) -> DeletedItem {
        DeletedItem {
            id: self.object.to_string(),
            seq: self.seq,
            time: self.time,
            device_name: self.device_name.clone(),
            path: display_path(&self.path),
            kind: self.kind,
            bytes: self.bytes,
            files: self.files,
            folder_gone: self.folder_gone,
        }
    }
}

impl Undeleted {
    pub fn item(&self) -> UndeletedItem {
        UndeletedItem {
            id: self.object.to_string(),
            path: display_path(&self.path),
            was: display_path(&self.was),
            renamed: self.renamed,
            objects: self.objects as u64,
        }
    }
}

/// The IDs `cairn deleted` and the local API give, read back.
pub fn parse_ids(ids: &[String]) -> Result<Vec<ObjectId>> {
    ids.iter()
        .map(|s| {
            s.trim()
                .parse::<ObjectId>()
                .map_err(|_| anyhow!("{s} is not an ID that `cairn deleted` gives"))
        })
        .collect()
}

/// Seconds since the epoch, `days` ago.
pub fn days_ago(days: u32) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    now - i64::from(days) * 86_400
}

/// A daemon's index, kept between the requests of a person going through the list and bringing
/// things back, and let go once nobody has asked for [`KEEP`]: it holds every object of the root.
pub struct Cache {
    slot: Arc<Mutex<Option<(Instant, Index)>>>,
}

pub const KEEP: Duration = Duration::from_secs(600);

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Cache {
    pub fn new() -> Self {
        Cache {
            slot: Arc::new(Mutex::new(None)),
        }
    }

    /// Runs `f` on the index, brought up to the server's head first. One at a time: a second request
    /// waits for the first rather than reading the journal again beside it.
    pub fn with<T>(
        &self,
        config: &Config,
        f: impl FnOnce(&mut Index, &Config) -> Result<T>,
    ) -> Result<T> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = slot.is_none();
        let (used, index) = slot.get_or_insert_with(|| (Instant::now(), Index::new()));
        *used = Instant::now();
        let client = Client::new(config, Retries::Short)?;
        if let Err(e) = index.update(&client) {
            // Half replayed is no index: the next request starts again.
            *slot = None;
            return Err(e);
        }
        let out = f(index, config);
        if fresh {
            let weak = Arc::downgrade(&self.slot);
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(30));
                let Some(slot) = weak.upgrade() else { return };
                let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
                match &*slot {
                    Some((used, _)) if used.elapsed() < KEEP => {}
                    _ => {
                        *slot = None;
                        return;
                    }
                }
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::{DeviceId, Metadata, RevisionBody, RevisionId};

    fn name(s: &str) -> Name {
        Name::new(s.as_bytes().to_vec()).unwrap()
    }
    fn id(n: u8) -> ObjectId {
        let mut b = [0u8; 16];
        b[15] = n;
        ObjectId(b)
    }
    fn file(size: u64) -> RevisionBody {
        RevisionBody {
            content: Content::File {
                manifest: cairn_protocol::Hash([0; 32]),
                size,
            },
            metadata: Metadata::default(),
        }
    }
    fn dir() -> RevisionBody {
        RevisionBody {
            content: Content::Dir,
            metadata: Metadata::default(),
        }
    }
    fn link(parent: ObjectId, n: &str) -> Link {
        Link {
            parent,
            name: name(n),
        }
    }
    fn create(o: u8, parent: ObjectId, n: &str, body: RevisionBody) -> Op {
        Op::Create {
            object: id(o),
            revision: RevisionId::random(),
            body,
            link: link(parent, n),
            conflict_of: None,
        }
    }
    fn unlink(o: u8, parent: ObjectId, n: &str) -> Op {
        Op::Unlink {
            object: id(o),
            link: link(parent, n),
            base: None,
        }
    }
    struct J {
        index: Index,
        seq: u64,
    }
    impl J {
        fn new() -> Self {
            J {
                index: Index::new(),
                seq: 0,
            }
        }
        fn commit(&mut self, device: &str, time: i64, ops: Vec<Op>) -> u64 {
            self.seq += 1;
            self.index
                .apply(&Change {
                    seq: self.seq,
                    op_id: OpId::random(),
                    device: DeviceId::random(),
                    device_name: device.into(),
                    time,
                    ops,
                    user: None,
                })
                .unwrap();
            self.index.head = self.seq;
            self.seq
        }
    }
    fn names(p: &[Name]) -> String {
        crate::scan::display_path(p)
    }

    /// A folder deleted over several commits, with another device's commit between them, is one
    /// entry with its files; a file deleted from it long before stays an entry of its own.
    #[test]
    fn a_folder_deleted_over_several_commits_is_one_entry() {
        let mut j = J::new();
        let r = ObjectId::ROOT;
        j.commit(
            "a",
            100,
            vec![
                create(1, r, "d", dir()),
                create(2, id(1), "one", file(10)),
                create(3, id(1), "two", file(20)),
                create(4, id(1), "old", file(5)),
                create(5, r, "other", file(1)),
            ],
        );
        j.commit("a", 200, vec![unlink(4, id(1), "old")]);
        j.commit("a", 5000, vec![unlink(2, id(1), "one")]);
        j.commit("b", 5001, vec![unlink(5, r, "other")]);
        let last = j.commit("a", 5002, vec![unlink(3, id(1), "two"), unlink(1, r, "d")]);
        let list = j.index.list(0);
        let shown: Vec<_> = list
            .iter()
            .map(|d| {
                (
                    names(&d.path),
                    d.device_name.as_str(),
                    d.files,
                    d.bytes,
                    d.folder_gone,
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("d".to_string(), "a", 2, 30, false),
                ("other".to_string(), "b", 0, 1, false),
                ("d/old".to_string(), "a", 0, 5, true),
            ]
        );
        assert_eq!(list[0].seq, last);
        assert!(j.index.list(1000).iter().all(|d| names(&d.path) != "d/old"));
    }

    #[test]
    fn bringing_a_folder_back_brings_what_went_with_it_and_keeps_a_name_taken_since() {
        let mut j = J::new();
        let r = ObjectId::ROOT;
        j.commit(
            "a",
            100,
            vec![
                create(1, r, "d", dir()),
                create(2, id(1), "f", file(10)),
                create(3, id(1), "sub", dir()),
                create(4, id(3), "g", file(3)),
            ],
        );
        j.commit(
            "a",
            200,
            vec![
                unlink(4, id(3), "g"),
                unlink(3, id(1), "sub"),
                unlink(2, id(1), "f"),
                unlink(1, r, "d"),
            ],
        );
        j.commit("b", 300, vec![create(9, r, "d", dir())]);
        let (ops, out) = j.index.plan(&[id(1)]).unwrap();
        assert_eq!(ops.len(), 4);
        // Folders before what is in them, every one a link of the object itself.
        let Op::Link { object, link } = &ops[0] else {
            panic!()
        };
        assert_eq!(*object, id(1));
        assert_eq!(link.parent, r);
        assert_eq!(
            crate::scan::display_path(std::slice::from_ref(&link.name)),
            "d (conflict restored seq 2)"
        );
        assert_eq!(out.items.len(), 1);
        assert!(out.items[0].renamed);
        assert_eq!(out.items[0].objects, 4);
        let pos = |o: u8| {
            ops.iter()
                .position(|op| matches!(op, Op::Link { object, .. } if *object == id(o)))
                .unwrap()
        };
        assert!(pos(3) < pos(4));
        for op in ops {
            j.index
                .apply(&Change {
                    seq: 4,
                    op_id: OpId::random(),
                    device: DeviceId::random(),
                    device_name: "a".into(),
                    time: 400,
                    ops: vec![op],
                    user: None,
                })
                .unwrap();
        }
        assert!(j.index.list(0).is_empty());
    }

    #[test]
    fn a_file_whose_folder_is_gone_brings_the_folder_back_bare() {
        let mut j = J::new();
        let r = ObjectId::ROOT;
        j.commit(
            "a",
            100,
            vec![
                create(1, r, "d", dir()),
                create(2, id(1), "f", file(10)),
                create(3, id(1), "g", file(3)),
            ],
        );
        j.commit("a", 200, vec![unlink(2, id(1), "f")]);
        j.commit("a", 9000, vec![unlink(3, id(1), "g"), unlink(1, r, "d")]);
        let (ops, out) = j.index.plan(&[id(2)]).unwrap();
        let objects: Vec<_> = ops
            .iter()
            .map(|op| match op {
                Op::Link { object, .. } => *object,
                _ => panic!(),
            })
            .collect();
        assert_eq!(objects, [id(1), id(2)], "the folder, bare, then the file");
        assert_eq!(names(&out.items[0].path), "d/f");
        assert!(!out.items[0].renamed);
        // Asked for together, the folder's own delete comes back too, and nothing twice.
        let (ops, out) = j.index.plan(&[id(2), id(1)]).unwrap();
        assert_eq!(ops.len(), 3);
        assert_eq!(out.items.len(), 2);
    }

    #[test]
    fn a_hard_link_removed_is_no_delete_and_one_brought_back_is_listed_no_more() {
        let mut j = J::new();
        let r = ObjectId::ROOT;
        j.commit(
            "a",
            100,
            vec![
                create(1, r, "f", file(10)),
                Op::Link {
                    object: id(1),
                    link: link(r, "g"),
                },
            ],
        );
        j.commit("a", 200, vec![unlink(1, r, "f")]);
        assert!(j.index.list(0).is_empty());
        j.commit("a", 300, vec![unlink(1, r, "g")]);
        assert_eq!(names(&j.index.list(0)[0].path), "g");
        j.commit(
            "a",
            400,
            vec![Op::Link {
                object: id(1),
                link: link(r, "g"),
            }],
        );
        assert!(j.index.list(0).is_empty());
        let (ops, out) = j.index.plan(&[id(1)]).unwrap();
        assert!(ops.is_empty());
        assert_eq!(names(&out.already[0].1), "g");
    }

    #[test]
    fn a_move_is_followed_and_the_path_is_the_folder_s_now() {
        let mut j = J::new();
        let r = ObjectId::ROOT;
        j.commit(
            "a",
            100,
            vec![create(1, r, "d", dir()), create(2, id(1), "f", file(10))],
        );
        j.commit("a", 200, vec![unlink(2, id(1), "f")]);
        j.commit(
            "a",
            300,
            vec![Op::Move {
                object: id(1),
                from: link(r, "d"),
                to: link(r, "e"),
            }],
        );
        assert_eq!(names(&j.index.list(0)[0].path), "e/f");
    }
}
