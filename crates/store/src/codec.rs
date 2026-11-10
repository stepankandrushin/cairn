//! The store's records in binary (docs/format.md, "Metadata"), in the encoding of
//! `cairn_protocol::record`: a revision's body, kept once, in `revisions`, and a commit's operations,
//! in `journal`, each create and update naming its revision there rather than carrying the body
//! again. What the server answers is made from them, so it is the JSON it always was.

use anyhow::{anyhow, Result};
use cairn_protocol::record::{get_body, get_link, put_body, put_link, R, W};
use cairn_protocol::{ConflictOf, ObjectId, Op, RevisionBody, RevisionId};
use std::collections::HashMap;

/// The first byte of a body or of a commit's operations: moved whenever the layout below changes.
pub const FORMAT: u8 = 1;

/// A revision's body: the format, its content, its metadata.
pub fn encode_body(b: &RevisionBody) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(96));
    w.u8(FORMAT);
    put_body(&mut w, b);
    w.0
}

pub fn decode_body(b: &[u8]) -> Result<RevisionBody> {
    let bad = || anyhow!("a revision's body in the database does not parse");
    let mut r = R(b);
    if r.u8().ok_or_else(bad)? != FORMAT {
        return Err(bad());
    }
    let body = get_body(&mut r).ok_or_else(bad)?;
    if !r.0.is_empty() {
        return Err(bad());
    }
    Ok(body)
}

const CREATE: u8 = 0;
const LINK: u8 = 1;
const UNLINK: u8 = 2;
const MOVE: u8 = 3;
const UPDATE: u8 = 4;
const NOTE: u8 = 5;
/// The op's one optional field is there: a create's `conflict_of`, an unlink's `base`, an
/// update's `restored_from`, a note's `also`: its count, at least one, then each object.
const HAS_OPTION: u8 = 1 << 3;
/// A create's or update's body is here, after the rest, rather than in `revisions` under its
/// revision. Only a move from an older schema writes it, for a revision whose row there does not
/// hold what the commit said.
const BODY_HERE: u8 = 1 << 4;

/// A commit's operations: the format, their count, then each. `body_here(i)` says whether operation
/// `i`'s body is written into it; otherwise it is the body `revisions` holds under its revision. A
/// note's text, once written, is the number of its first appearance in the commit the next times,
/// as a join gives thousands of notes few texts.
pub fn encode_ops(ops: &[Op], body_here: impl Fn(usize) -> bool) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(16 + ops.len() * 64));
    w.u8(FORMAT);
    w.uv(ops.len() as u64);
    let mut texts: HashMap<&str, u64> = HashMap::new();
    for (i, op) in ops.iter().enumerate() {
        let here = op.body().is_some() && body_here(i);
        let flags = |kind: u8, option: bool| {
            let mut tag = kind;
            if option {
                tag |= HAS_OPTION;
            }
            if here {
                tag |= BODY_HERE;
            }
            tag
        };
        match op {
            Op::Create {
                object,
                revision,
                link,
                conflict_of,
                ..
            } => {
                w.u8(flags(CREATE, conflict_of.is_some()));
                w.raw(&object.0);
                w.raw(&revision.0);
                put_link(&mut w, link);
                if let Some(c) = conflict_of {
                    w.raw(&c.object.0);
                    w.raw(&c.competing_revision.0);
                }
            }
            Op::Link { object, link } => {
                w.u8(flags(LINK, false));
                w.raw(&object.0);
                put_link(&mut w, link);
            }
            Op::Unlink { object, link, base } => {
                w.u8(flags(UNLINK, base.is_some()));
                w.raw(&object.0);
                put_link(&mut w, link);
                if let Some(b) = base {
                    w.raw(&b.0);
                }
            }
            Op::Move { object, from, to } => {
                w.u8(flags(MOVE, false));
                w.raw(&object.0);
                put_link(&mut w, from);
                put_link(&mut w, to);
            }
            Op::Update {
                object,
                base,
                revision,
                restored_from,
                ..
            } => {
                w.u8(flags(UPDATE, restored_from.is_some()));
                w.raw(&object.0);
                w.raw(&base.0);
                w.raw(&revision.0);
                if let Some(r) = restored_from {
                    w.raw(&r.0);
                }
            }
            Op::Note { object, text, also } => {
                w.u8(flags(NOTE, !also.is_empty()));
                w.raw(&object.0);
                match texts.get(text.as_str()) {
                    Some(n) => w.uv(*n),
                    None => {
                        w.uv(0);
                        w.bytes(text.as_bytes());
                        let n = texts.len() as u64 + 1;
                        texts.insert(text, n);
                    }
                }
                if !also.is_empty() {
                    w.uv(also.len() as u64);
                    for o in also {
                        w.raw(&o.0);
                    }
                }
            }
        }
        if here {
            put_body(
                &mut w,
                op.body()
                    .expect("only an op with a body is written with one"),
            );
        }
    }
    w.0
}

/// A commit's operations as `encode_ops` wrote them, and the index of each whose body is in
/// `revisions`: that one holds an empty folder's body until the caller puts its own in.
pub struct Decoded {
    pub ops: Vec<Op>,
    pub bodies_wanted: Vec<usize>,
}

/// The body of an op whose body is in `revisions`, until it is read from there.
fn wanted() -> RevisionBody {
    RevisionBody {
        content: cairn_protocol::Content::Dir,
        metadata: Default::default(),
    }
}

pub fn decode_ops(b: &[u8]) -> Result<Decoded> {
    let bad = || anyhow!("a commit's operations in the database do not parse");
    let mut r = R(b);
    if r.u8().ok_or_else(bad)? != FORMAT {
        return Err(bad());
    }
    let decoded = (|| {
        // Each at least its kind and an object.
        let n = r.count(17)?;
        let mut ops = Vec::with_capacity(n);
        let mut bodies_wanted = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        for i in 0..n {
            let tag = r.u8()?;
            if tag & !(7 | HAS_OPTION | BODY_HERE) != 0 {
                return None;
            }
            let (kind, option, here) = (tag & 7, tag & HAS_OPTION != 0, tag & BODY_HERE != 0);
            let object = ObjectId(r.array::<16>()?);
            let op = match kind {
                CREATE => {
                    let revision = RevisionId(r.array::<16>()?);
                    let link = get_link(&mut r)?;
                    let conflict_of = match option {
                        true => Some(ConflictOf {
                            object: ObjectId(r.array::<16>()?),
                            competing_revision: RevisionId(r.array::<16>()?),
                        }),
                        false => None,
                    };
                    let body = match here {
                        true => get_body(&mut r)?,
                        false => {
                            bodies_wanted.push(i);
                            wanted()
                        }
                    };
                    Op::Create {
                        object,
                        revision,
                        body,
                        link,
                        conflict_of,
                    }
                }
                UPDATE => {
                    let base = RevisionId(r.array::<16>()?);
                    let revision = RevisionId(r.array::<16>()?);
                    let restored_from = match option {
                        true => Some(RevisionId(r.array::<16>()?)),
                        false => None,
                    };
                    let body = match here {
                        true => get_body(&mut r)?,
                        false => {
                            bodies_wanted.push(i);
                            wanted()
                        }
                    };
                    Op::Update {
                        object,
                        base,
                        revision,
                        body,
                        restored_from,
                    }
                }
                _ if here => return None,
                LINK if !option => Op::Link {
                    object,
                    link: get_link(&mut r)?,
                },
                UNLINK => {
                    let link = get_link(&mut r)?;
                    let base = match option {
                        true => Some(RevisionId(r.array::<16>()?)),
                        false => None,
                    };
                    Op::Unlink { object, link, base }
                }
                MOVE if !option => Op::Move {
                    object,
                    from: get_link(&mut r)?,
                    to: get_link(&mut r)?,
                },
                NOTE => {
                    let text = match r.uv()? {
                        0 => {
                            let t = String::from_utf8(r.bytes()?.to_vec()).ok()?;
                            texts.push(t.clone());
                            t
                        }
                        k => texts.get(usize::try_from(k - 1).ok()?)?.clone(),
                    };
                    let also = match option {
                        true => {
                            let n = r.count(16)?;
                            if n == 0 {
                                return None;
                            }
                            let mut also = Vec::with_capacity(n);
                            for _ in 0..n {
                                also.push(ObjectId(r.array::<16>()?));
                            }
                            also
                        }
                        false => Vec::new(),
                    };
                    Op::Note { object, text, also }
                }
                _ => return None,
            };
            ops.push(op);
        }
        Some(Decoded { ops, bodies_wanted })
    })()
    .ok_or_else(bad)?;
    if !r.0.is_empty() {
        return Err(bad());
    }
    Ok(decoded)
}

/// The length of `ops` as JSON, as the server sends them: what a page of the journal is measured by.
pub fn json_len(ops: &[Op]) -> Result<u64> {
    struct Count(u64);
    impl std::io::Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len() as u64;
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut n = Count(0);
    serde_json::to_writer(&mut n, ops)?;
    Ok(n.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_protocol::{
        Bytes, Content, Hash, Link, Metadata, Name, NamedStream, Owner, RecordKey, Timestamp,
    };

    fn id(b: u8) -> [u8; 16] {
        [b; 16]
    }

    fn link(p: u8, name: &[u8]) -> Link {
        Link {
            parent: ObjectId(id(p)),
            name: Name::new(name.to_vec()).unwrap(),
        }
    }

    fn rich() -> Metadata {
        let mut m = Metadata {
            mode: 0o4755,
            owner: Some(Owner {
                uid: 1000,
                gid: 1001,
                user: Some(Bytes(b"user".to_vec())),
                group: None,
            }),
            mtime: Timestamp {
                sec: -5,
                nsec: 999_999_999,
            },
            btime: Some(Timestamp { sec: 1, nsec: 0 }),
            time_precision_ns: 100,
            streams: vec![NamedStream {
                ns: "a.namespace.of.a.newer.build".into(),
                name: Bytes(vec![0, 255]),
                manifest: Hash::of(b"s"),
            }],
            ..Default::default()
        };
        m.records.insert(
            RecordKey {
                ns: cairn_protocol::NS_LINUX_XATTR.into(),
                name: Bytes(b"user.x".to_vec()),
            },
            Bytes(vec![1, 2, 3]),
        );
        m.unknown.insert(
            "from_the_future".into(),
            serde_json::json!({"x": [1, 2.5, "three", null, {"deep": true}]}),
        );
        m
    }

    fn bodies() -> Vec<RevisionBody> {
        vec![
            RevisionBody {
                content: Content::Dir,
                metadata: Metadata::default(),
            },
            RevisionBody {
                content: Content::File {
                    manifest: Hash::of(b"m"),
                    size: u64::MAX,
                },
                metadata: rich(),
            },
            RevisionBody {
                content: Content::Symlink {
                    target: Bytes(b"../\xff/target".to_vec()),
                },
                metadata: Metadata {
                    mode: 0o777,
                    ..Default::default()
                },
            },
        ]
    }

    fn every_op() -> Vec<Op> {
        let b = bodies();
        vec![
            Op::Create {
                object: ObjectId(id(1)),
                revision: RevisionId(id(2)),
                body: b[1].clone(),
                link: link(0, b"f\xffile"),
                conflict_of: None,
            },
            Op::Create {
                object: ObjectId(id(3)),
                revision: RevisionId(id(4)),
                body: b[0].clone(),
                link: link(0, b"copy"),
                conflict_of: Some(ConflictOf {
                    object: ObjectId(id(1)),
                    competing_revision: RevisionId(id(2)),
                }),
            },
            Op::Link {
                object: ObjectId(id(1)),
                link: link(3, b"hard"),
            },
            Op::Unlink {
                object: ObjectId(id(1)),
                link: link(3, b"hard"),
                base: None,
            },
            Op::Unlink {
                object: ObjectId(id(1)),
                link: link(3, b"hard"),
                base: Some(RevisionId(id(2))),
            },
            Op::Move {
                object: ObjectId(id(3)),
                from: link(0, b"copy"),
                to: link(1, b"moved"),
            },
            Op::Update {
                object: ObjectId(id(1)),
                base: RevisionId(id(2)),
                revision: RevisionId(id(5)),
                body: b[2].clone(),
                restored_from: None,
            },
            Op::Update {
                object: ObjectId(id(1)),
                base: RevisionId(id(5)),
                revision: RevisionId(id(6)),
                body: b[1].clone(),
                restored_from: Some(RevisionId(id(2))),
            },
            Op::note(
                ObjectId(id(1)),
                "mode: kept 664 from the server; this device had 644".into(),
            ),
            Op::note(
                ObjectId(id(3)),
                "mode: kept 664 from the server; this device had 644".into(),
            ),
            Op::note(ObjectId(id(3)), String::new()),
            Op::note(
                ObjectId(id(1)),
                "mode: kept 664 from the server; this device had 644".into(),
            ),
            Op::Note {
                object: ObjectId(id(5)),
                text: "mode: kept 664 from the server; this device had 644".into(),
                also: vec![ObjectId(id(1)), ObjectId(id(3))],
            },
            Op::Note {
                object: ObjectId(id(6)),
                text: "mode: kept 775 from the server; this device had 755".into(),
                also: vec![ObjectId(id(7))],
            },
        ]
    }

    #[test]
    fn a_body_decodes_to_what_was_encoded_unknown_fields_and_all() {
        for b in bodies() {
            let bytes = encode_body(&b);
            assert_eq!(decode_body(&bytes).unwrap(), b);
            assert_eq!(
                serde_json::to_string(&decode_body(&bytes).unwrap()).unwrap(),
                serde_json::to_string(&b).unwrap()
            );
        }
        let json = serde_json::to_string(&bodies()[1]).unwrap();
        assert!(json.contains("from_the_future"), "{json}");
    }

    /// Every kind of op, each optional field there and not, with bodies in `revisions` and here, and
    /// notes whose texts repeat: each decodes to itself, and its JSON to the same bytes.
    #[test]
    fn ops_decode_to_what_was_encoded_with_bodies_here_or_wanted() {
        let ops = every_op();
        for here in [false, true] {
            let bytes = encode_ops(&ops, |_| here);
            let mut d = decode_ops(&bytes).unwrap();
            let with_bodies: Vec<usize> = (0..ops.len())
                .filter(|i| ops[*i].body().is_some())
                .collect();
            match here {
                true => assert!(d.bodies_wanted.is_empty()),
                false => assert_eq!(d.bodies_wanted, with_bodies),
            }
            for i in &d.bodies_wanted {
                let body = ops[*i].body().unwrap().clone();
                match &mut d.ops[*i] {
                    Op::Create { body: b, .. } | Op::Update { body: b, .. } => *b = body,
                    _ => unreachable!(),
                }
            }
            assert_eq!(d.ops, ops);
            assert_eq!(
                serde_json::to_string(&d.ops).unwrap(),
                serde_json::to_string(&ops).unwrap()
            );
        }
        // Only the one op asked for has its body here.
        let d = decode_ops(&encode_ops(&ops, |i| i == 6)).unwrap();
        assert_eq!(d.bodies_wanted, vec![0, 1, 7]);
        assert_eq!(d.ops[6], ops[6]);
        assert_eq!(decode_ops(&encode_ops(&[], |_| false)).unwrap().ops, vec![]);
    }

    #[test]
    fn a_repeated_note_is_a_few_bytes() {
        let note = |o: u8| {
            Op::note(
                ObjectId(id(o)),
                "mode: kept 664 from the server; this device had 644".into(),
            )
        };
        let one = encode_ops(&[note(1)], |_| false).len();
        let many = encode_ops(&(0..100).map(note).collect::<Vec<_>>(), |_| false).len();
        assert!(many < one + 99 * 19, "{one} for one, {many} for a hundred");
        // Grouped, sixteen bytes to each object after the first.
        let grouped = cairn_protocol::group_notes(&(0..100).map(note).collect::<Vec<_>>());
        assert_eq!(grouped.len(), 1);
        let bytes = encode_ops(&grouped, |_| false).len();
        assert!(
            bytes <= one + 2 + 99 * 16,
            "{one} for one, {bytes} for a hundred grouped"
        );
    }

    /// A note as the store wrote it before notes named more than one object, byte by byte: it reads
    /// as it did, and writes the same bytes again.
    #[test]
    fn a_note_written_before_groups_reads_as_it_did() {
        let text = "mode: kept 664 from the server; this device had 644";
        let mut w = W(vec![FORMAT, 2, NOTE]);
        w.raw(&id(1));
        w.uv(0);
        w.bytes(text.as_bytes());
        w.u8(NOTE);
        w.raw(&id(2));
        w.uv(1);
        let d = decode_ops(&w.0).unwrap();
        let want = vec![
            Op::note(ObjectId(id(1)), text.into()),
            Op::note(ObjectId(id(2)), text.into()),
        ];
        assert_eq!(d.ops, want);
        assert_eq!(encode_ops(&want, |_| false), w.0);
        assert_eq!(
            serde_json::to_string(&d.ops[0]).unwrap(),
            format!(
                r#"{{"op":"note","object":"{}","text":"{text}"}}"#,
                ObjectId(id(1))
            )
        );
    }

    #[test]
    fn a_damaged_record_is_an_error_not_another_record() {
        let ops = every_op();
        let bytes = encode_ops(&ops, |i| i % 2 == 0);
        for cut in 0..bytes.len() {
            assert!(decode_ops(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode_ops(&longer).is_err());
        let mut other = bytes.clone();
        other[0] = FORMAT + 1;
        assert!(decode_ops(&other).is_err());
        let body = encode_body(&bodies()[1]);
        for cut in 0..body.len() {
            assert!(decode_body(&body[..cut]).is_err(), "cut at {cut}");
        }
        // A note naming a text not yet written.
        let mut w = W(vec![FORMAT, 1, NOTE]);
        w.raw(&id(1));
        w.uv(1);
        assert!(decode_ops(&w.0).is_err());
        // A note said to name more objects, naming none: not how one is written.
        let mut w = W(vec![FORMAT, 1, NOTE | HAS_OPTION]);
        w.raw(&id(1));
        w.uv(0);
        w.bytes(b"t");
        w.uv(0);
        assert!(decode_ops(&w.0).is_err());
        // A kind with no body, said to carry one.
        let mut w = W(vec![FORMAT, 1, LINK | BODY_HERE]);
        w.raw(&id(1));
        put_link(&mut w, &link(0, b"x"));
        put_body(&mut w, &bodies()[0]);
        assert!(decode_ops(&w.0).is_err());
    }

    #[test]
    fn json_len_is_the_length_of_the_json() {
        let ops = every_op();
        assert_eq!(
            json_len(&ops).unwrap(),
            serde_json::to_string(&ops).unwrap().len() as u64
        );
    }
}
