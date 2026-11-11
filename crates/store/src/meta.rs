//! The metadata store trait: all a server needs from a database.

use anyhow::Result;
use cairn_protocol::wire::{Access, Change, CommitRequest, ConflictEntry, FolderAccess, Member};
use cairn_protocol::{Hash, Link, Name, ObjectId, ObjectKind, OpReject, RevisionBody, RevisionId};

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

/// An object's head: its kind, head revision and that revision's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub kind: ObjectKind,
    pub revision: RevisionId,
    pub body: RevisionBody,
}

/// One entry of a directory, with its object's head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub name: Name,
    pub object: ObjectId,
    pub head: Head,
}

/// A revision with the commit that made it: none for a namespace root's first, made with the namespace.
#[derive(Debug, Clone)]
pub struct Revisioned {
    pub revision: StoredRevision,
    pub time: i64,
    pub device_name: String,
    pub user: Option<String>,
}

/// An object shared with a user, or by a public link: one or the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRow {
    pub id: [u8; 16],
    pub ns: String,
    pub object: ObjectId,
    /// Whom it is shared with.
    pub user: Option<String>,
    /// A public link's token.
    pub link: Option<String>,
    pub access: Access,
    /// The user who made it, a manager of the namespace then.
    pub by: String,
    pub created: i64,
    pub expires: Option<i64>,
}

pub trait MetaStore: Send {
    fn head(&mut self, ns: &str) -> Result<u64>;
    fn namespace_exists(&mut self, ns: &str) -> Result<bool>;
    /// Makes the namespace if it is not there; refused for a name another one had, which reaches that.
    fn create_namespace(&mut self, ns: &str) -> Result<()>;
    /// The namespace a name reaches, by its name now: the name itself, or what the namespace whose
    /// former name it is is called now. `None` for none.
    fn resolve_namespace(&mut self, name: &str) -> Result<Option<String>>;
    /// Renames a namespace, all at once: its former name goes on reaching it, and so no other may
    /// take that name. Its chunks' directory, named after it, is the caller's to move (`Repo`).
    fn rename_namespace(&mut self, from: &str, to: &str) -> Result<()>;
    /// The names a namespace had before, which still reach it.
    fn former_names(&mut self, ns: &str) -> Result<Vec<String>>;
    /// Sets a user's own name for a namespace, with `if_none` only if they have none, or with `None`
    /// takes it away; the name they have then.
    fn set_folder_name(
        &mut self,
        ns: &str,
        user: &str,
        name: Option<&str>,
        if_none: bool,
    ) -> Result<Option<String>>;
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
    /// Up to `limit` entries after `after`, in order, and no more once their operations, as the JSON
    /// they are sent as, pass `max_bytes`: always at least one.
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

    /// A directory's entries now, in name-byte order, each with its object's head.
    fn children(&mut self, ns: &str, dir: ObjectId) -> Result<Vec<Child>>;
    /// An object's head, deleted or not; `None` if there is no such object.
    fn object_head(&mut self, ns: &str, object: ObjectId) -> Result<Option<Head>>;
    /// The entries naming an object now: none for a deleted one or the root.
    fn links_of(&mut self, ns: &str, object: ObjectId) -> Result<Vec<Link>>;
    fn revision(&mut self, ns: &str, revision: RevisionId) -> Result<Option<StoredRevision>>;
    /// Every revision of an object, newest first, each with the commit that made it.
    fn history(&mut self, ns: &str, object: ObjectId) -> Result<Vec<Revisioned>>;
    /// Up to `limit` entries before `before`, newest first, and no more once their operations pass
    /// `max_bytes`: always at least one if there is one.
    fn changes_before(
        &mut self,
        ns: &str,
        before: u64,
        limit: u32,
        max_bytes: usize,
    ) -> Result<Vec<Change>>;

    /// Shares. Removing a user removes the shares it made and those made with it.
    fn add_share(&mut self, share: &ShareRow) -> Result<()>;
    /// A namespace's shares, or one object's there, oldest first.
    fn shares(&mut self, ns: &str, object: Option<ObjectId>) -> Result<Vec<ShareRow>>;
    fn share(&mut self, id: &[u8; 16]) -> Result<Option<ShareRow>>;
    fn share_by_link(&mut self, link: &str) -> Result<Option<ShareRow>>;
    /// What is shared with a user, oldest first.
    fn shares_to(&mut self, user: &str) -> Result<Vec<ShareRow>>;
    /// Whether there was such a share in that namespace to remove.
    fn remove_share(&mut self, ns: &str, id: &[u8; 16]) -> Result<bool>;
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
    use cairn_protocol::{Content, DeviceId, Metadata, Op, OpId};

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
        assert_eq!(s.namespaces().unwrap(), ["cloud", "m", "n"]);

        tree_reads(s, a, c);
        accounts(s);
        shares(s);
        names(s);
    }

    /// What the web interface reads: a folder's entries, an object's head, links and history, and the
    /// journal read back from the head.
    fn tree_reads(s: &mut dyn MetaStore, a: ObjectId, c: ObjectId) {
        let root = s.children("n", ObjectId::ROOT).unwrap();
        assert_eq!(
            root.iter()
                .map(|ch| (ch.name.as_bytes().to_vec(), ch.object))
                .collect::<Vec<_>>(),
            [(b"a".to_vec(), a)]
        );
        assert_eq!(root[0].head.kind, ObjectKind::Dir);
        assert_eq!(s.children("n", a).unwrap()[0].object, c);
        assert!(s.children("n", c).unwrap().is_empty());
        let head = s.object_head("n", c).unwrap().unwrap();
        assert_eq!(head.kind, ObjectKind::Dir);
        assert_eq!(s.object_head("n", ObjectId::random()).unwrap(), None);
        assert_eq!(
            s.links_of("n", c).unwrap(),
            [Link {
                parent: a,
                name: name("c")
            }]
        );
        assert!(s.links_of("n", ObjectId::ROOT).unwrap().is_empty());
        let stored = s.revision("n", head.revision).unwrap().unwrap();
        assert_eq!((stored.object, stored.seq), (c, 2));
        assert!(s.revision("m", head.revision).unwrap().is_none());

        // An edit is a second revision, listed first, with the commit that made it.
        let mut body = dir();
        body.metadata.mode = 0o700;
        let rev = RevisionId::random();
        let edit = req(vec![Op::Update {
            object: c,
            base: head.revision,
            revision: rev,
            body,
            restored_from: None,
        }]);
        assert!(matches!(
            s.commit("n", &edit, 40, Some("u")).unwrap(),
            CommitOutcome::Committed { seq: 3, .. }
        ));
        let history = s.history("n", c).unwrap();
        assert_eq!(
            history
                .iter()
                .map(|h| (h.revision.revision, h.revision.seq, h.time, h.user.clone()))
                .collect::<Vec<_>>(),
            [
                (rev, 3, 40, Some("u".to_string())),
                (head.revision, 2, 4, Some("u".to_string()))
            ]
        );
        assert_eq!(s.history("n", ObjectId::ROOT).unwrap()[0].time, 0);

        // Back from the head, newest first, within the budget.
        let back = s.changes_before("n", u64::MAX, 100, usize::MAX).unwrap();
        assert_eq!(back.iter().map(|c| c.seq).collect::<Vec<_>>(), [3, 2, 1]);
        assert_eq!(s.changes_before("n", 3, 1, usize::MAX).unwrap()[0].seq, 2);
        assert_eq!(s.changes_before("n", 4, 100, 1).unwrap().len(), 1);
        assert!(s
            .changes_before("n", 1, 100, usize::MAX)
            .unwrap()
            .is_empty());
    }

    fn shares(s: &mut dyn MetaStore) {
        s.add_user("carol", 50).unwrap();
        s.add_user("dave", 51).unwrap();
        let object = ObjectId::random();
        let share = |id: u8, user: Option<&str>, link: Option<&str>| ShareRow {
            id: [id; 16],
            ns: "n".into(),
            object,
            user: user.map(str::to_string),
            link: link.map(str::to_string),
            access: Access::Read,
            by: "carol".into(),
            created: 60 + i64::from(id),
            expires: None,
        };
        s.add_share(&share(1, Some("dave"), None)).unwrap();
        s.add_share(&share(2, None, Some("tok"))).unwrap();
        // A share is with a user or a link, never both or neither, and names users there are.
        assert!(s.add_share(&share(3, Some("dave"), Some("t2"))).is_err());
        assert!(s.add_share(&share(4, None, None)).is_err());
        assert!(s.add_share(&share(5, Some("nobody"), None)).is_err());
        assert!(
            s.add_share(&share(6, None, Some("tok"))).is_err(),
            "a link is one share"
        );

        assert_eq!(
            s.shares("n", None).unwrap(),
            [share(1, Some("dave"), None), share(2, None, Some("tok"))]
        );
        assert_eq!(s.shares("n", Some(object)).unwrap().len(), 2);
        assert!(s.shares("n", Some(ObjectId::random())).unwrap().is_empty());
        assert!(s.shares("m", None).unwrap().is_empty());
        assert_eq!(
            s.share(&[1; 16]).unwrap(),
            Some(share(1, Some("dave"), None))
        );
        assert_eq!(s.share(&[9; 16]).unwrap(), None);
        assert_eq!(
            s.share_by_link("tok").unwrap(),
            Some(share(2, None, Some("tok")))
        );
        assert_eq!(s.share_by_link("other").unwrap(), None);
        assert_eq!(s.shares_to("dave").unwrap(), [share(1, Some("dave"), None)]);
        assert!(s.shares_to("carol").unwrap().is_empty());

        // Removed only from its own namespace.
        assert!(!s.remove_share("m", &[1; 16]).unwrap());
        assert!(s.remove_share("n", &[1; 16]).unwrap());
        assert!(!s.remove_share("n", &[1; 16]).unwrap());

        // A user removed takes the shares made with it and by it.
        s.add_share(&share(7, Some("dave"), None)).unwrap();
        s.remove_user("dave").unwrap();
        assert_eq!(s.shares("n", None).unwrap(), [share(2, None, Some("tok"))]);
        s.remove_user("carol").unwrap();
        assert!(s.shares("n", None).unwrap().is_empty());
        assert_eq!(s.share_by_link("tok").unwrap(), None);
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
                    access: Access::Read,
                    name: None,
                },
                FolderAccess {
                    folder: "n".into(),
                    access: Access::Read,
                    name: None,
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

    fn names(s: &mut dyn MetaStore) {
        s.create_namespace("old").unwrap();
        s.create_namespace("taken").unwrap();
        s.add_user("erin", 40).unwrap();
        s.set_access("old", "erin", Some(Access::Write)).unwrap();
        let mine = |s: &mut dyn MetaStore| {
            let all = s.folders_of("erin").unwrap();
            assert_eq!(all.len(), 1, "{all:?}");
            (all[0].folder.clone(), all[0].name.clone())
        };
        assert_eq!(mine(s), ("old".into(), None));

        // A user's own name: a device's offer is taken only while they have none, a choice always.
        let set = |s: &mut dyn MetaStore, name: Option<&str>, if_none: bool| {
            s.set_folder_name("old", "erin", name, if_none)
        };
        assert_eq!(
            set(s, Some("cloud"), true).unwrap().as_deref(),
            Some("cloud")
        );
        assert_eq!(
            set(s, Some("other"), true).unwrap().as_deref(),
            Some("cloud")
        );
        assert_eq!(
            set(s, Some("Фото 2026"), false).unwrap().as_deref(),
            Some("Фото 2026")
        );
        for bad in ["", " padded", "tab\there", &"x".repeat(65)] {
            assert!(set(s, Some(bad), false).is_err(), "{bad:?}");
        }
        assert!(s
            .set_folder_name("nowhere", "erin", Some("x"), false)
            .is_err());
        assert!(s
            .set_folder_name("old", "nobody", Some("x"), false)
            .is_err());
        assert_eq!(mine(s), ("old".into(), Some("Фото 2026".into())));

        // Renamed: the name now and the former one reach it; nothing else may take the former one.
        assert!(s.rename_namespace("old", "taken").is_err());
        assert!(s.rename_namespace("old", "bad name").is_err());
        assert!(s.rename_namespace("old", "old").is_err());
        assert!(s.rename_namespace("nowhere", "x").is_err());
        assert_eq!(s.resolve_namespace("old").unwrap().as_deref(), Some("old"));
        let head = s.head("old").unwrap();
        s.rename_namespace("old", "new").unwrap();
        assert!(!s.namespace_exists("old").unwrap());
        assert!(s.namespace_exists("new").unwrap());
        assert_eq!(s.resolve_namespace("old").unwrap().as_deref(), Some("new"));
        assert_eq!(s.resolve_namespace("new").unwrap().as_deref(), Some("new"));
        assert_eq!(s.resolve_namespace("none").unwrap(), None);
        assert_eq!(s.former_names("new").unwrap(), ["old"]);
        assert_eq!(s.head("new").unwrap(), head);
        assert_eq!(s.access("new", "erin").unwrap(), Some(Access::Write));
        assert_eq!(mine(s), ("new".into(), Some("Фото 2026".into())));
        assert!(s.create_namespace("old").is_err());
        assert!(s.rename_namespace("taken", "old").is_err());
        assert!(s.resolve_namespace("taken").unwrap().is_some());
        // And back: its name its own again, and the one it left a former name in turn.
        s.rename_namespace("new", "old").unwrap();
        assert_eq!(s.former_names("old").unwrap(), ["new"]);
        assert_eq!(s.resolve_namespace("new").unwrap().as_deref(), Some("old"));

        // Their own name taken away, they see the folder's; a user removed takes theirs with them.
        assert_eq!(set(s, None, false).unwrap(), None);
        assert_eq!(mine(s), ("old".into(), None));
        set(s, Some("again"), false).unwrap();
        s.remove_user("erin").unwrap();
        s.add_user("erin", 41).unwrap();
        s.set_access("old", "erin", Some(Access::Read)).unwrap();
        assert_eq!(mine(s), ("old".into(), None));
    }
}
