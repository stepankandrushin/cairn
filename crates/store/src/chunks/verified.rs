//! A bounded memory of reads, never a substitute for verification on download or fsck.
use cairn_protocol::Hash;
use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    // Coarse filesystem clocks can give two writes the same timestamp. A recent or future
    // timestamp cannot vouch for an immutable file yet, even when a read verified its bytes.
    pub(super) fn old_enough(&self) -> bool {
        let Ok(sec) = u64::try_from(self.ctime.0) else {
            return false;
        };
        let Ok(nsec) = u32::try_from(self.ctime.1) else {
            return false;
        };
        if nsec >= 1_000_000_000 {
            return false;
        }
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|now| now.checked_sub(Duration::new(sec, nsec)))
            .is_some_and(|age| age >= Duration::from_secs(2))
    }

    pub(super) fn of(m: &std::fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}

#[derive(Default)]
pub(super) struct Verified {
    entries: HashMap<Hash, (Stamp, Instant)>,
    // One slot per insertion; stale entries are removed along with the map entry.
    order: VecDeque<Hash>,
}

impl Verified {
    pub(super) fn has(&self, hash: &Hash, stamp: &Stamp) -> bool {
        let ttl = Duration::from_secs(cairn_failpoint::tuned("chunk.verified_secs", 3600));
        self.entries
            .get(hash)
            .is_some_and(|(s, at)| s == stamp && at.elapsed() < ttl)
    }

    pub(super) fn forget(&mut self, hash: &Hash) {
        if self.entries.remove(hash).is_some() {
            self.order.retain(|h| h != hash);
        }
    }

    pub(super) fn put(&mut self, hash: Hash, stamp: Stamp) {
        if !stamp.old_enough() {
            return;
        }
        // Refreshing a stale entry keeps its eviction position, and never duplicates its slot.
        if let Some(entry) = self.entries.get_mut(&hash) {
            *entry = (stamp, Instant::now());
            return;
        }
        let cap = cairn_failpoint::tuned("chunk.verified_count", 131_072) as usize;
        if cap == 0 {
            return;
        }
        while self.entries.len() >= cap {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
        self.order.push_back(hash);
        self.entries.insert(hash, (stamp, Instant::now()));
    }
}
