//! The metadata store trait: all a server needs from a database.

use anyhow::Result;
use cairn_protocol::wire::{Change, CommitRequest, ConflictEntry};
use cairn_protocol::{ObjectId, OpReject, RevisionBody, RevisionId};

#[derive(Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Durable. `replayed` means this operation ID had already committed at `seq`.
    Committed {
        seq: u64,
        replayed: bool,
    },
    Rejected {
        op_index: usize,
        reason: OpReject,
    },
}

/// A revision as stored, for `fsck` and history.
#[derive(Debug, Clone)]
pub struct StoredRevision {
    pub revision: RevisionId,
    pub object: ObjectId,
    pub seq: u64,
    pub body: RevisionBody,
}

pub trait MetaStore: Send {
    fn head(&mut self, ns: &str) -> Result<u64>;
    fn namespace_exists(&mut self, ns: &str) -> Result<bool>;
    fn create_namespace(&mut self, ns: &str) -> Result<()>;
    /// All or nothing. Durable when this returns `Committed`.
    fn commit(&mut self, ns: &str, req: &CommitRequest, time: i64) -> Result<CommitOutcome>;
    /// Up to `limit` entries after `after`, in order, and no more once their operations, as stored,
    /// pass `max_bytes`: always at least one.
    fn changes(
        &mut self,
        ns: &str,
        after: u64,
        limit: u32,
        max_bytes: usize,
    ) -> Result<Vec<Change>>;
    /// As `changes`, without the operations: which entry is where, for a client checking its cursor.
    fn changes_without_ops(&mut self, ns: &str, after: u64, limit: u32) -> Result<Vec<Change>> {
        let mut out = self.changes(ns, after, limit, usize::MAX)?;
        for c in &mut out {
            c.ops.clear();
        }
        Ok(out)
    }
    fn conflicts(&mut self, ns: &str) -> Result<Vec<ConflictEntry>>;
    fn revisions(&mut self, ns: &str) -> Result<Vec<StoredRevision>>;
    /// Current entries and heads, for `fsck` to compare with a replay of the journal.
    fn tree_state(&mut self, ns: &str) -> Result<TreeState>;
    fn namespaces(&mut self) -> Result<Vec<String>>;
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TreeState {
    pub entries: Vec<(ObjectId, Vec<u8>, ObjectId)>,
    pub heads: Vec<(ObjectId, RevisionId)>,
}

/// The shared suite: every store implementation must pass it.
#[cfg(test)]
pub mod suite {
    use super::*;
    use cairn_protocol::{Content, DeviceId, Link, Metadata, Name, Op, OpId};

    fn name(s: &str) -> Name {
        Name::new(s.as_bytes().to_vec()).unwrap()
    }
    fn dir() -> RevisionBody {
        RevisionBody {
            content: Content::Dir,
            metadata: Metadata::default(),
        }
    }
    fn req(ops: Vec<Op>) -> CommitRequest {
        CommitRequest {
            op_id: OpId::random(),
            device: DeviceId::random(),
            device_name: "d".into(),
            ops,
        }
    }
    fn mkdir(parent: ObjectId, n: &str) -> (ObjectId, Op) {
        let object = ObjectId::random();
        (
            object,
            Op::Create {
                object,
                revision: RevisionId::random(),
                body: dir(),
                link: Link {
                    parent,
                    name: name(n),
                },
                conflict_of: None,
            },
        )
    }

    pub fn run(s: &mut dyn MetaStore) {
        s.create_namespace("n").unwrap();
        assert!(s.namespace_exists("n").unwrap());
        assert!(!s.namespace_exists("other").unwrap());
        assert_eq!(s.head("n").unwrap(), 0);

        // A commit gets a sequence number, and its operation ID replays that answer.
        let (a, op) = mkdir(ObjectId::ROOT, "a");
        let first = req(vec![op]);
        assert_eq!(
            s.commit("n", &first, 1).unwrap(),
            CommitOutcome::Committed {
                seq: 1,
                replayed: false
            }
        );
        assert_eq!(
            s.commit("n", &first, 2).unwrap(),
            CommitOutcome::Committed {
                seq: 1,
                replayed: true
            }
        );
        assert_eq!(s.head("n").unwrap(), 1);

        // A batch is all or nothing: a bad last op leaves nothing of the first.
        let (b, good) = mkdir(ObjectId::ROOT, "b");
        let (_, bad) = mkdir(ObjectId::ROOT, "a");
        let outcome = s.commit("n", &req(vec![good, bad]), 3).unwrap();
        assert!(matches!(
            outcome,
            CommitOutcome::Rejected {
                op_index: 1,
                reason: OpReject::NameTaken(_)
            }
        ));
        assert_eq!(s.head("n").unwrap(), 1);
        assert!(s
            .tree_state("n")
            .unwrap()
            .heads
            .iter()
            .all(|(o, _)| *o != b));

        // Moving a directory into itself is the concurrent-cycle rule.
        let (c, op) = mkdir(a, "c");
        s.commit("n", &req(vec![op]), 4).unwrap();
        let cycle = Op::Move {
            object: a,
            from: Link {
                parent: ObjectId::ROOT,
                name: name("a"),
            },
            to: Link {
                parent: c,
                name: name("a"),
            },
        };
        assert!(matches!(
            s.commit("n", &req(vec![cycle]), 5).unwrap(),
            CommitOutcome::Rejected {
                reason: OpReject::Cycle(_),
                ..
            }
        ));

        // The journal gives the changes after a cursor, in order.
        let changes = s.changes("n", 1, 100, usize::MAX).unwrap();
        assert_eq!(changes.iter().map(|c| c.seq).collect::<Vec<_>>(), [2]);
        assert_eq!(s.changes("n", 0, 1, usize::MAX).unwrap().len(), 1);
        // A byte budget stops the answer after the entry that passes it, never before the first.
        assert_eq!(s.changes("n", 0, 100, 1).unwrap().len(), 1);
        assert_eq!(s.changes("n", 1, 100, 1).unwrap()[0].seq, 2);

        // Namespaces do not see each other.
        s.create_namespace("m").unwrap();
        assert_eq!(s.head("m").unwrap(), 0);
        assert!(s.changes("m", 0, 100, usize::MAX).unwrap().is_empty());
        assert_eq!(s.namespaces().unwrap(), ["default", "m", "n"]);
    }
}
