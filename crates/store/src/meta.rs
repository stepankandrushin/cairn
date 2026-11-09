//! The metadata store trait: all a server needs from a database.

use anyhow::Result;
use cairn_protocol::wire::{Access, Change, CommitRequest, ConflictEntry, FolderAccess, Member};
use cairn_protocol::{Hash, ObjectId, OpReject, RevisionBody, RevisionId};

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

/// A user as the server's administrator lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub created: i64,
    pub tokens: u64,
    pub folders: Vec<FolderAccess>,
}

/// A token as listed: never the token itself, which is not stored, but its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    /// The first digits of `hash`, by which a person names it.
    pub id: String,
    pub hash: Hash,
    pub user: String,
    pub label: String,
    pub created: i64,
}

pub trait MetaStore: Send {
    fn head(&mut self, ns: &str) -> Result<u64>;
    fn namespace_exists(&mut self, ns: &str) -> Result<bool>;
    fn create_namespace(&mut self, ns: &str) -> Result<()>;
    /// All or nothing. Durable when this returns `Committed`. `user` is whose token sent it, kept in
    /// the journal with it.
    fn commit(
        &mut self,
        ns: &str,
        req: &CommitRequest,
        time: i64,
        user: Option<&str>,
    ) -> Result<CommitOutcome>;

    /// Accounts. A user's name is unique; removing one removes its tokens and its access.
    fn add_user(&mut self, name: &str, time: i64) -> Result<()>;
    fn remove_user(&mut self, name: &str) -> Result<()>;
    fn users(&mut self) -> Result<Vec<UserInfo>>;
    /// A token is kept as its hash only.
    fn add_token(&mut self, user: &str, hash: &Hash, label: &str, time: i64) -> Result<()>;
    fn tokens(&mut self) -> Result<Vec<TokenInfo>>;
    fn remove_token(&mut self, hash: &Hash) -> Result<()>;
    /// Whose token has this hash.
    fn token_user(&mut self, hash: &Hash) -> Result<Option<String>>;
    /// What a user may do in a namespace; `None` is nothing at all.
    fn access(&mut self, ns: &str, user: &str) -> Result<Option<Access>>;
    /// Give a user a level in a namespace, or with `None` take all of it away.
    fn set_access(&mut self, ns: &str, user: &str, access: Option<Access>) -> Result<()>;
    fn members(&mut self, ns: &str) -> Result<Vec<Member>>;
    fn folders_of(&mut self, user: &str) -> Result<Vec<FolderAccess>>;
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
            s.commit("n", &first, 1, None).unwrap(),
            CommitOutcome::Committed {
                seq: 1,
                replayed: false
            }
        );
        assert_eq!(
            s.commit("n", &first, 2, None).unwrap(),
            CommitOutcome::Committed {
                seq: 1,
                replayed: true
            }
        );
        assert_eq!(s.head("n").unwrap(), 1);

        // A batch is all or nothing: a bad last op leaves nothing of the first.
        let (b, good) = mkdir(ObjectId::ROOT, "b");
        let (_, bad) = mkdir(ObjectId::ROOT, "a");
        let outcome = s.commit("n", &req(vec![good, bad]), 3, None).unwrap();
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
        s.commit("n", &req(vec![op]), 4, Some("u")).unwrap();
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
            s.commit("n", &req(vec![cycle]), 5, None).unwrap(),
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
        // Each entry says whose token made it, where one did.
        let users: Vec<Option<String>> = s
            .changes("n", 0, 100, usize::MAX)
            .unwrap()
            .into_iter()
            .map(|c| c.user)
            .collect();
        assert_eq!(users, [None, Some("u".to_string())]);
        assert_eq!(
            s.changes_without_ops("n", 1, 100).unwrap()[0]
                .user
                .as_deref(),
            Some("u")
        );

        // Namespaces do not see each other.
        s.create_namespace("m").unwrap();
        assert_eq!(s.head("m").unwrap(), 0);
        assert!(s.changes("m", 0, 100, usize::MAX).unwrap().is_empty());
        assert_eq!(s.namespaces().unwrap(), ["default", "m", "n"]);

        accounts(s);
    }

    fn accounts(s: &mut dyn MetaStore) {
        let base = s.users().unwrap().len();
        s.add_user("alice", 10).unwrap();
        s.add_user("bob", 11).unwrap();
        assert!(s.add_user("alice", 12).is_err(), "a name is one user");

        // A token is found by its hash, and only by it.
        let (ta, tb, tb2) = (Hash::of(b"a"), Hash::of(b"b"), Hash::of(b"b2"));
        s.add_token("alice", &ta, "laptop", 20).unwrap();
        s.add_token("bob", &tb, "", 21).unwrap();
        s.add_token("bob", &tb2, "phone", 22).unwrap();
        assert!(s.add_token("nobody", &Hash::of(b"x"), "", 23).is_err());
        assert_eq!(s.token_user(&ta).unwrap().as_deref(), Some("alice"));
        assert_eq!(s.token_user(&tb2).unwrap().as_deref(), Some("bob"));
        assert_eq!(s.token_user(&Hash::of(b"none")).unwrap(), None);

        // Access is per namespace and per user, and nothing until given.
        assert_eq!(s.access("n", "alice").unwrap(), None);
        s.set_access("n", "alice", Some(Access::Manage)).unwrap();
        s.set_access("n", "bob", Some(Access::Write)).unwrap();
        s.set_access("m", "bob", Some(Access::Read)).unwrap();
        assert_eq!(s.access("n", "alice").unwrap(), Some(Access::Manage));
        assert_eq!(s.access("m", "alice").unwrap(), None);
        assert_eq!(s.access("m", "bob").unwrap(), Some(Access::Read));
        assert_eq!(s.access("nowhere", "bob").unwrap(), None);
        assert!(s.set_access("nowhere", "bob", Some(Access::Read)).is_err());
        assert!(s.set_access("n", "nobody", Some(Access::Read)).is_err());
        // A new level replaces the old one.
        s.set_access("n", "bob", Some(Access::Read)).unwrap();
        assert_eq!(s.access("n", "bob").unwrap(), Some(Access::Read));
        assert_eq!(
            s.members("n").unwrap(),
            [
                Member {
                    user: "alice".into(),
                    access: Access::Manage
                },
                Member {
                    user: "bob".into(),
                    access: Access::Read
                }
            ]
        );
        assert_eq!(
            s.folders_of("bob").unwrap(),
            [
                FolderAccess {
                    folder: "m".into(),
                    access: Access::Read
                },
                FolderAccess {
                    folder: "n".into(),
                    access: Access::Read
                }
            ]
        );
        s.set_access("m", "bob", None).unwrap();
        assert_eq!(s.access("m", "bob").unwrap(), None);

        let users = s.users().unwrap();
        assert_eq!(users.len(), base + 2);
        let bob = users.iter().find(|u| u.name == "bob").unwrap();
        assert_eq!((bob.created, bob.tokens, bob.folders.len()), (11, 2, 1));
        let tokens = s.tokens().unwrap();
        let mine: Vec<_> = tokens.iter().filter(|t| t.user == "bob").collect();
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().any(|t| t.hash == tb2 && t.label == "phone"));

        // One token goes alone; a removed user takes its tokens and access with it.
        s.remove_token(&tb2).unwrap();
        assert_eq!(s.token_user(&tb2).unwrap(), None);
        assert!(s.remove_token(&tb2).is_err());
        assert_eq!(s.token_user(&tb).unwrap().as_deref(), Some("bob"));
        s.remove_user("bob").unwrap();
        assert_eq!(s.token_user(&tb).unwrap(), None);
        assert_eq!(s.members("n").unwrap().len(), 1);
        assert!(s.remove_user("bob").is_err());
        // A user by the same name later is someone new, with nothing of the old one's.
        s.add_user("bob", 30).unwrap();
        assert_eq!(s.access("n", "bob").unwrap(), None);
        assert_eq!(s.token_user(&tb).unwrap(), None);
    }
}
