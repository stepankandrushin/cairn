//! Retention and collection of one namespace (docs/format.md, "Collection"): a pass of the retention
//! rule, which gives revisions up, then a mark and sweep, which removes the chunks no revision it keeps
//! needs. A folder with no policy is left alone, its orphans too.
//!
//! The pass works in transactions of at most `PRUNE_BATCH` revisions, and only ever gives up more, so a
//! kill anywhere in it leaves the store whole. The sweep commits its `gc_runs` row first; marks from a
//! connection of its own that only reads, begun under its gate alone so that every commit is either in
//! what it reads or vouches for what it names; never takes a chunk vouched for, held, or written within
//! the upload grace; and removes in batches of `DELETE_BATCH`, each under the gate alone and the
//! backup's lock shared. A kill mid-batch leaves fewer orphans and nothing else; the run is marked
//! interrupted at the next start, and the next run starts over.

use crate::chunks::{ChunkStore, FsChunkStore};
use crate::meta::MetaStore;
use crate::repo::{Repo, BACKUP_LOCK};
use crate::retention::Rule;
use anyhow::{anyhow, bail, Context, Result};
use cairn_failpoint::hit;
use cairn_protocol::{Hash, Manifest, RevisionId};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Revisions given up in one transaction.
pub const PRUNE_BATCH: usize = 10_000;
/// Chunks removed under the gate at a time.
pub const DELETE_BATCH: usize = 256;

/// How a collection goes.
#[derive(Clone, Debug)]
pub struct Settings {
    /// D: what was given up less than this long before the run keeps its content, in seconds.
    pub delay: i64,
    /// G: a chunk written less than this long before the run is not taken.
    pub grace: Duration,
    /// The time the rule and the cutoff go by, in seconds since the epoch: the clock's.
    pub now: i64,
    /// Say what it would do, and change nothing.
    pub dry_run: bool,
}

/// What a run did, or with `dry_run` would do.
#[derive(Clone, Debug, Default)]
pub struct Collected {
    /// The policy as text; `None`, and nothing done, for a folder with none.
    pub policy: Option<String>,
    /// Revisions that name content, and those the policy gives up now that were not given up before.
    pub considered: usize,
    pub newly_pruned: u64,
    /// Revisions given up whose content this run keeps, as they were given up after its cutoff.
    pub awaiting: usize,
    /// Manifests and chunks the revisions it keeps name.
    pub marked: usize,
    /// Chunks nothing it keeps names, old enough to take, and of them those removed, with their bytes.
    pub candidates: usize,
    pub removed: u64,
    pub bytes: u64,
    /// The `gc_runs` row.
    pub run: Option<i64>,
    /// The policy changed while the pass ran, which stopped it.
    pub policy_changed: bool,
}

/// What the run tells as it goes, the thumbnails it lets go of with their manifests, and whether to
/// stop, as a server stopping asks.
pub struct Hooks<'a> {
    pub say: &'a dyn Fn(&str),
    pub forget: &'a mut dyn FnMut(&[Hash]),
    pub stop: &'a dyn Fn() -> bool,
}

/// A run asked to stop: it ends where it is, as a kill would leave it.
fn stopping(hooks: &Hooks<'_>) -> Result<()> {
    if (hooks.stop)() {
        bail!("the process is stopping");
    }
    Ok(())
}

/// The data directory's `backup.lock`, taken shared before each batch: a backup holds it exclusively
/// for its whole run, and no batch removes anything meanwhile.
struct BackupLock(std::fs::File);

impl BackupLock {
    fn open(root: &Path) -> Result<Self> {
        let path = root.join(BACKUP_LOCK);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        Ok(BackupLock(file))
    }

    /// Shared, waiting while a backup holds it; whether it waited.
    #[cfg(unix)]
    fn shared(&self, say: &dyn Fn(&str)) -> Result<bool> {
        use rustix::fs::{flock, FlockOperation};
        if flock(&self.0, FlockOperation::NonBlockingLockShared).is_ok() {
            return Ok(false);
        }
        say(&format!(
            "a backup holds {BACKUP_LOCK}: the collection waits for it to end before it removes anything"
        ));
        hit("server.gc.backup_wait");
        flock(&self.0, FlockOperation::LockShared)
            .map_err(|e| anyhow!("take {BACKUP_LOCK} shared: {e}"))?;
        Ok(true)
    }

    #[cfg(unix)]
    fn release(&self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }

    /// No backup script coordinates with a server on Windows: the lock is the file being there.
    #[cfg(windows)]
    fn shared(&self, _say: &dyn Fn(&str)) -> Result<bool> {
        Ok(false)
    }

    #[cfg(windows)]
    fn release(&self) {}
}

/// Runs the policy of namespace `ns` and collects its chunks, or with `dry_run` says what that would
/// do. With the server's own repo, or one this process has to itself; with `dry_run`, any.
pub fn collect(repo: &Repo, ns: &str, s: &Settings, hooks: &mut Hooks<'_>) -> Result<Collected> {
    let mut out = Collected::default();
    let Some(row) = repo.with_meta(|m| m.retention(ns))? else {
        return Ok(out);
    };
    let policy = row.policy()?;
    out.policy = Some(policy.to_string());
    if !s.dry_run && !repo.owns_directory() {
        bail!("a collection runs in the server, or with it stopped");
    }

    // The pass: what the policy gives up now, from the journal as one read sees it.
    let mut reader = repo.reader()?;
    reader.pin()?;
    let read = (|| -> Result<(crate::retention::Outcome, HashMap<RevisionId, i64>)> {
        let mut rule = Rule::new(&policy, s.now);
        let mut after = 0;
        loop {
            let page = reader.changes(ns, after, 1000, 64 << 20)?;
            let Some(last) = page.last() else { break };
            after = last.seq;
            for c in &page {
                rule.apply(c)?;
            }
        }
        let pruned: HashMap<RevisionId, i64> = reader.all_pruned(ns)?.into_iter().collect();
        Ok((rule.finish(), pruned))
    })();
    reader.unpin()?;
    let (outcome, mut pruned) = read?;
    out.considered = outcome.considered;
    let new: Vec<RevisionId> = outcome
        .prune
        .into_iter()
        .filter(|r| !pruned.contains_key(r))
        .collect();
    hit("server.retention.planned");
    if s.dry_run {
        out.newly_pruned = new.len() as u64;
        for r in &new {
            pruned.insert(*r, s.now);
        }
    } else {
        let rows = cairn_failpoint::tuned("server.retention.batch_rows", PRUNE_BATCH as u64).max(1);
        for batch in new.chunks(rows as usize) {
            match repo.with_meta(|m| m.prune(ns, batch, s.now, &row))? {
                Some(n) => out.newly_pruned += n,
                None => {
                    // Set again meanwhile: the next run goes by the new one.
                    out.policy_changed = true;
                    (hooks.say)(&format!(
                        "folder {ns:?}: its retention policy changed while it was applied; the next collection goes by the new one"
                    ));
                    return Ok(out);
                }
            }
            hit("server.retention.pruned");
        }
    }
    sweep(repo, ns, s, hooks, &mut out)?;
    Ok(out)
}

fn sweep(
    repo: &Repo,
    ns: &str,
    s: &Settings,
    hooks: &mut Hooks<'_>,
    out: &mut Collected,
) -> Result<()> {
    let started = SystemTime::now();
    let before = started
        .checked_sub(s.grace)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    // Never before a cutoff a run before had, as a clock set back would make it: what was given up
    // before that may be gone already.
    let before_now = repo.with_meta(|m| m.swept_before(ns))?;
    let cutoff = s
        .now
        .saturating_sub(s.delay)
        .max(before_now.unwrap_or(i64::MIN));
    // The row first, so that whatever follows is known to have begun.
    let run = if s.dry_run {
        None
    } else {
        let id = repo
            .with_meta(|m| m.gc_begin(ns, s.now, cutoff))
            .context("record the collection's start; nothing was removed")?;
        hit("server.gc.begun");
        Some(id)
    };
    out.run = run;
    let ended = |state: &str, out: &Collected, error: Option<&str>| -> Result<()> {
        if let Some(id) = run {
            repo.with_meta(|m| {
                m.gc_update(
                    id,
                    state,
                    out.removed,
                    out.bytes,
                    (state != "deleting").then_some(s.now),
                    error,
                )
            })?;
        }
        Ok(())
    };
    let marked = match mark(repo, ns, s, cutoff, out, hooks) {
        Ok(m) => m,
        Err(e) => {
            let _ = ended("aborted", out, Some(&format!("{e:#}")));
            return Err(e.context("nothing was removed"));
        }
    };
    let (store, marked, manifests, _vouching) = marked;
    let result = (|| -> Result<()> {
        let candidates: Vec<Hash> = store
            .stored_before(before)?
            .into_iter()
            .filter(|h| marked.binary_search(h).is_err())
            .collect();
        out.candidates = candidates.len();
        if s.dry_run {
            for h in &candidates {
                out.bytes += store.len_of(h)?.unwrap_or(0);
            }
            return Ok(());
        }
        ended("deleting", out, None)?;
        hit("server.gc.deleting");
        let backup = BackupLock::open(repo.root())?;
        let each = cairn_failpoint::tuned("server.gc.batch_chunks", DELETE_BATCH as u64).max(1);
        for batch in candidates.chunks(each as usize) {
            stopping(hooks)?;
            backup.shared(hooks.say)?;
            hit("server.gc.batch");
            let mut removed = Vec::new();
            let done = (|| -> Result<()> {
                let _alone = store.exclusive();
                hit("server.gc.batch_locked");
                for h in batch {
                    if let Some(n) = store.remove_unless_spared(h, before)? {
                        removed.push(*h);
                        out.removed += 1;
                        out.bytes += n;
                    }
                }
                Ok(())
            })();
            backup.release();
            done?;
            store.sync_fans(&removed)?;
            let gone: Vec<Hash> = removed
                .into_iter()
                .filter(|h| manifests.contains(h))
                .collect();
            if !gone.is_empty() {
                (hooks.forget)(&gone);
            }
            ended("deleting", out, None)?;
            hit("server.gc.batched");
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            ended("done", out, None)?;
            hit("server.gc.done");
            Ok(())
        }
        Err(e) => {
            let state = if out.removed > 0 {
                "interrupted"
            } else {
                "aborted"
            };
            let _ = ended(state, out, Some(&format!("{e:#}")));
            Err(e)
        }
    }
}

type Marked = (
    std::sync::Arc<FsChunkStore>,
    Vec<Hash>,
    HashSet<Hash>,
    Option<VouchGuard>,
);

/// Stops vouching on the store it began on, however the run ends.
struct VouchGuard(std::sync::Arc<FsChunkStore>);

impl Drop for VouchGuard {
    fn drop(&mut self) {
        self.0.stop_vouching();
    }
}

/// What the revisions a run keeps name, sorted, every manifest any revision names, and the vouching
/// begun for it.
fn mark(
    repo: &Repo,
    ns: &str,
    s: &Settings,
    cutoff: i64,
    out: &mut Collected,
    hooks: &Hooks<'_>,
) -> Result<Marked> {
    let store = if s.dry_run && !repo.owns_directory() {
        // Beside a server, nothing of its store is made or emptied.
        std::sync::Arc::new(repo.chunks_read_only(ns)?)
    } else {
        repo.chunks(ns)?
    };
    let mut reader = repo.reader()?;
    let vouching = if s.dry_run {
        reader.pin()?;
        None
    } else {
        // No commit is between the check of its chunks and its commit: each one before this is in what
        // the reader sees, each one after vouches for what it names.
        let _alone = store.exclusive();
        store.start_vouching();
        let guard = VouchGuard(store.clone());
        reader.pin()?;
        Some(guard)
    };
    hit("server.gc.pinned");
    let read = (|| -> Result<_> {
        let revisions = reader.revisions(ns)?;
        let pruned: HashMap<RevisionId, i64> = reader.all_pruned(ns)?.into_iter().collect();
        Ok((revisions, pruned))
    })();
    reader.unpin()?;
    let (revisions, pruned) = read?;
    let mut keep: HashSet<Hash> = HashSet::new();
    let mut manifests: HashSet<Hash> = HashSet::new();
    for r in &revisions {
        let named: Vec<Hash> = match &r.body.content {
            cairn_protocol::Content::File { manifest, .. } => vec![*manifest],
            _ => Vec::new(),
        }
        .into_iter()
        .chain(r.body.metadata.streams.iter().map(|st| st.manifest))
        .collect();
        manifests.extend(named.iter().copied());
        match pruned.get(&r.revision) {
            Some(&at) if at < cutoff => {}
            Some(_) => {
                out.awaiting += 1;
                keep.extend(named);
            }
            None => keep.extend(named),
        }
    }
    let mut marked: Vec<Hash> = Vec::with_capacity(keep.len() * 2);
    for (i, m) in keep.iter().enumerate() {
        if i % 1024 == 0 {
            stopping(hooks)?;
        }
        let bytes = store.get(m).with_context(|| {
            format!("manifest {m}, which a revision kept names, could not be read")
        })?;
        let Some(bytes) = bytes else {
            bail!("manifest {m}, which a revision kept names, is not in the store: fsck says more");
        };
        let manifest =
            Manifest::decode(&bytes).with_context(|| format!("manifest {m} does not decode"))?;
        marked.push(*m);
        marked.extend(manifest.chunks().map(|c| c.hash));
    }
    marked.sort_unstable();
    marked.dedup();
    out.marked = marked.len();
    hit("server.gc.marked");
    Ok((store, marked, manifests, vouching))
}
