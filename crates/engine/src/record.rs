//! The state's records in binary (docs/format.md, "Client state"): a base node, a row and a manifest,
//! each in as few bytes as it holds. Integers are LEB128, signed ones zigzagged first, IDs and hashes
//! their raw bytes, and anything of variable length is preceded by its length. A row is written against
//! the base node stored beside it in the same SQL row: a part equal to the node's says so in a flag bit
//! instead of being written again, so each per-file detail is stored once. Decoding is exact: what is
//! decoded equals what was encoded, field by field, unknown metadata fields included.

use crate::state::{Fingerprint, Row, Synced};
use anyhow::{anyhow, Result};
use cairn_protocol::{
    Bytes, ChunkRef, Content, Hash, Link, Manifest, Metadata, Name, NamedStream, Node, ObjectId,
    OpId, Owner, RecordKey, RevisionBody, RevisionId, Segment, Timestamp,
};
use std::collections::{BTreeMap, BTreeSet};

/// The first byte of a node or a row: moved whenever the layout below changes.
pub const FORMAT: u8 = 1;

pub(crate) struct W(pub Vec<u8>);

impl W {
    pub fn new() -> W {
        W(Vec::with_capacity(128))
    }
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn uv(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    pub fn iv(&mut self, v: i64) {
        self.uv(((v << 1) ^ (v >> 63)) as u64);
    }
    pub fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.uv(b.len() as u64);
        self.raw(b);
    }
    pub fn time(&mut self, t: Timestamp) {
        self.iv(t.sec);
        self.uv(u64::from(t.nsec));
    }
}

pub(crate) struct R<'a>(pub &'a [u8]);

impl<'a> R<'a> {
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub fn uv(&mut self) -> Option<u64> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let b = self.u8()?;
            let part = u64::from(b & 0x7f);
            if i == 9 && part > 1 {
                return None;
            }
            v |= part << (7 * i);
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    pub fn u32v(&mut self) -> Option<u32> {
        u32::try_from(self.uv()?).ok()
    }
    pub fn iv(&mut self) -> Option<i64> {
        let u = self.uv()?;
        Some(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
    pub fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.uv()?).ok()?;
        self.take(n)
    }
    pub fn time(&mut self) -> Option<Timestamp> {
        Some(Timestamp {
            sec: self.iv()?,
            nsec: self.u32v()?,
        })
    }
    /// A count of things each at least `min` bytes long: no more than what is left could hold, so that a
    /// damaged count cannot ask for a huge allocation.
    pub fn count(&mut self, min: usize) -> Option<usize> {
        let n = usize::try_from(self.uv()?).ok()?;
        (n <= self.0.len() / min.max(1)).then_some(n)
    }
}

/// The record namespaces this build knows, by a byte each; 0 is one written out.
const NAMESPACES: [&str; 7] = [
    cairn_protocol::NS_LINUX_XATTR,
    cairn_protocol::NS_MACOS_XATTR,
    cairn_protocol::NS_MACOS_FINDER_INFO,
    cairn_protocol::NS_MACOS_RESOURCE_FORK,
    cairn_protocol::NS_WINDOWS_ACL,
    cairn_protocol::NS_WINDOWS_ATTRIBUTES,
    cairn_protocol::NS_WINDOWS_ADS,
];

fn put_ns(w: &mut W, ns: &str) {
    match NAMESPACES.iter().position(|k| *k == ns) {
        Some(i) => w.u8(i as u8 + 1),
        None => {
            w.u8(0);
            w.bytes(ns.as_bytes());
        }
    }
}

fn get_ns(r: &mut R<'_>) -> Option<String> {
    match r.u8()? {
        0 => String::from_utf8(r.bytes()?.to_vec()).ok(),
        i => NAMESPACES.get(usize::from(i) - 1).map(|s| s.to_string()),
    }
}

const M_OWNER: u8 = 1;
const M_USER: u8 = 1 << 1;
const M_GROUP: u8 = 1 << 2;
const M_BTIME: u8 = 1 << 3;
const M_PRECISION: u8 = 1 << 4;
const M_RECORDS: u8 = 1 << 5;
const M_STREAMS: u8 = 1 << 6;
const M_UNKNOWN: u8 = 1 << 7;

pub(crate) fn put_metadata(w: &mut W, m: &Metadata) {
    let owner = m.owner.as_ref();
    let mut flags = 0;
    for (on, bit) in [
        (owner.is_some(), M_OWNER),
        (owner.is_some_and(|o| o.user.is_some()), M_USER),
        (owner.is_some_and(|o| o.group.is_some()), M_GROUP),
        (m.btime.is_some(), M_BTIME),
        (m.time_precision_ns != 1, M_PRECISION),
        (!m.records.is_empty(), M_RECORDS),
        (!m.streams.is_empty(), M_STREAMS),
        (!m.unknown.is_empty(), M_UNKNOWN),
    ] {
        if on {
            flags |= bit;
        }
    }
    w.u8(flags);
    w.uv(u64::from(m.mode));
    if let Some(o) = owner {
        w.uv(u64::from(o.uid));
        w.uv(u64::from(o.gid));
        if let Some(u) = &o.user {
            w.bytes(&u.0);
        }
        if let Some(g) = &o.group {
            w.bytes(&g.0);
        }
    }
    w.time(m.mtime);
    if let Some(t) = m.btime {
        w.time(t);
    }
    if m.time_precision_ns != 1 {
        w.uv(u64::from(m.time_precision_ns));
    }
    if !m.records.is_empty() {
        w.uv(m.records.len() as u64);
        for (k, v) in &m.records {
            put_ns(w, &k.ns);
            w.bytes(&k.name.0);
            w.bytes(&v.0);
        }
    }
    if !m.streams.is_empty() {
        w.uv(m.streams.len() as u64);
        for s in &m.streams {
            put_ns(w, &s.ns);
            w.bytes(&s.name.0);
            w.raw(&s.manifest.0);
        }
    }
    if !m.unknown.is_empty() {
        // Fields from a newer build, kept as the JSON they came in: they go back out as they were.
        w.bytes(&serde_json::to_vec(&m.unknown).expect("JSON values always encode"));
    }
}

pub(crate) fn get_metadata(r: &mut R<'_>) -> Option<Metadata> {
    let flags = r.u8()?;
    let mode = r.u32v()?;
    let owner = if flags & M_OWNER != 0 {
        let uid = r.u32v()?;
        let gid = r.u32v()?;
        let user = (flags & M_USER != 0)
            .then(|| r.bytes().map(|b| Bytes(b.to_vec())))
            .map_or(Some(None), |b| b.map(Some))?;
        let group = (flags & M_GROUP != 0)
            .then(|| r.bytes().map(|b| Bytes(b.to_vec())))
            .map_or(Some(None), |b| b.map(Some))?;
        Some(Owner {
            uid,
            gid,
            user,
            group,
        })
    } else if flags & (M_USER | M_GROUP) != 0 {
        return None;
    } else {
        None
    };
    let mtime = r.time()?;
    let btime = match flags & M_BTIME != 0 {
        true => Some(r.time()?),
        false => None,
    };
    let time_precision_ns = match flags & M_PRECISION != 0 {
        true => r.u32v()?,
        false => 1,
    };
    let mut records = BTreeMap::new();
    if flags & M_RECORDS != 0 {
        for _ in 0..r.count(3)? {
            let ns = get_ns(r)?;
            let name = Bytes(r.bytes()?.to_vec());
            let value = Bytes(r.bytes()?.to_vec());
            records.insert(RecordKey { ns, name }, value);
        }
    }
    let mut streams = Vec::new();
    if flags & M_STREAMS != 0 {
        for _ in 0..r.count(34)? {
            let ns = get_ns(r)?;
            let name = Bytes(r.bytes()?.to_vec());
            let manifest = Hash(r.array::<32>()?);
            streams.push(NamedStream { ns, name, manifest });
        }
    }
    let unknown = match flags & M_UNKNOWN != 0 {
        true => serde_json::from_slice(r.bytes()?).ok()?,
        false => BTreeMap::new(),
    };
    Some(Metadata {
        mode,
        owner,
        mtime,
        btime,
        time_precision_ns,
        records,
        streams,
        unknown,
    })
}

pub(crate) fn put_content(w: &mut W, c: &Content) {
    match c {
        Content::File { manifest, size } => {
            w.u8(0);
            w.raw(&manifest.0);
            w.uv(*size);
        }
        Content::Dir => w.u8(1),
        Content::Symlink { target } => {
            w.u8(2);
            w.bytes(&target.0);
        }
    }
}

pub(crate) fn get_content(r: &mut R<'_>) -> Option<Content> {
    Some(match r.u8()? {
        0 => Content::File {
            manifest: Hash(r.array::<32>()?),
            size: r.uv()?,
        },
        1 => Content::Dir,
        2 => Content::Symlink {
            target: Bytes(r.bytes()?.to_vec()),
        },
        _ => return None,
    })
}

pub(crate) fn put_links<'a>(w: &mut W, links: impl ExactSizeIterator<Item = &'a Link>) {
    w.uv(links.len() as u64);
    for l in links {
        w.raw(&l.parent.0);
        w.bytes(l.name.as_bytes());
    }
}

pub(crate) fn get_link(r: &mut R<'_>) -> Option<Link> {
    let parent = ObjectId(r.array::<16>()?);
    let name = Name::new(r.bytes()?.to_vec()).ok()?;
    Some(Link { parent, name })
}

fn get_links(r: &mut R<'_>) -> Option<BTreeSet<Link>> {
    let n = r.count(18)?;
    let mut out = BTreeSet::new();
    for _ in 0..n {
        // Each once, as a set writes them: anything else was not written by this.
        if !out.insert(get_link(r)?) {
            return None;
        }
    }
    (out.len() == n).then_some(out)
}

/// A base node: its format, revision, content, metadata and entries.
pub fn encode_node(n: &Node) -> Vec<u8> {
    let mut w = W::new();
    w.u8(FORMAT);
    w.raw(&n.rev.0);
    put_content(&mut w, &n.body.content);
    put_metadata(&mut w, &n.body.metadata);
    put_links(&mut w, n.links.iter());
    w.0
}

pub fn decode_node(b: &[u8]) -> Result<Node> {
    let bad = || anyhow!("a base node in the state does not parse");
    let mut r = R(b);
    if r.u8().ok_or_else(bad)? != FORMAT {
        return Err(bad());
    }
    let node = (|| {
        let rev = RevisionId(r.array::<16>()?);
        let content = get_content(&mut r)?;
        let metadata = get_metadata(&mut r)?;
        let links = get_links(&mut r)?;
        Some(Node {
            rev,
            body: RevisionBody { content, metadata },
            links,
        })
    })()
    .ok_or_else(bad)?;
    if !r.0.is_empty() {
        return Err(bad());
    }
    Ok(node)
}

const R_RACY: u32 = 1;
const R_INCOMPLETE: u32 = 1 << 1;
/// `content` is the node's.
const R_CONTENT_NODE: u32 = 1 << 2;
/// `scan` is the node's metadata.
const R_SCAN_NODE: u32 = 1 << 3;
/// `applied`, two bits: none, the same as `scan`, the node's metadata, or written out.
const R_APPLIED: u32 = 4;
const R_SYNCED: u32 = 1 << 6;
/// `held`, two bits: none, the node's entries, or written out.
const R_HELD: u32 = 7;
/// The fingerprint's size is the content's.
const R_FP_SIZE: u32 = 1 << 9;
/// Its mtime is `scan`'s.
const R_FP_MTIME: u32 = 1 << 10;
/// Its ctime is its mtime.
const R_FP_CTIME: u32 = 1 << 11;
/// Its birth time, two bits: none, `scan`'s (which has one), or written out.
const R_FP_BTIME: u32 = 12;

const SYNCED_COMMIT: u8 = 1;
const SYNCED_UNAPPLIED: u8 = 1 << 1;
const SYNCED_FOLDER_TIME: u8 = 1 << 2;

fn content_size(c: &Content) -> Option<u64> {
    match c {
        Content::File { size, .. } => Some(*size),
        _ => None,
    }
}

/// A row, written against `node`, the base node stored beside it, if there is one: whatever the row
/// has that equals the node's is a flag bit, and only what differs is written.
pub fn encode_row(row: &Row, node: Option<&Node>) -> Vec<u8> {
    let mut flags = 0u32;
    if row.racy {
        flags |= R_RACY;
    }
    if row.incomplete {
        flags |= R_INCOMPLETE;
    }
    let content_node = node.is_some_and(|n| n.body.content == row.content);
    if content_node {
        flags |= R_CONTENT_NODE;
    }
    let scan_node = node.is_some_and(|n| n.body.metadata == row.scan);
    if scan_node {
        flags |= R_SCAN_NODE;
    }
    let applied = match &row.applied {
        None => 0,
        Some(a) if *a == row.scan => 1,
        Some(a) if node.is_some_and(|n| n.body.metadata == *a) => 2,
        Some(_) => 3,
    };
    flags |= applied << R_APPLIED;
    if row.synced.is_some() {
        flags |= R_SYNCED;
    }
    let held = match &row.held {
        None => 0,
        Some(h) if node.is_some_and(|n| n.links == *h) => 1,
        Some(_) => 2,
    };
    flags |= held << R_HELD;
    let fp = &row.fp;
    let size_same = content_size(&row.content) == Some(fp.size);
    if size_same {
        flags |= R_FP_SIZE;
    }
    let mtime_same = fp.mtime == row.scan.mtime;
    if mtime_same {
        flags |= R_FP_MTIME;
    }
    let ctime_same = fp.ctime == fp.mtime;
    if ctime_same {
        flags |= R_FP_CTIME;
    }
    let btime = match fp.btime {
        None => 0,
        Some(t) if row.scan.btime == Some(t) => 1,
        Some(_) => 2,
    };
    flags |= btime << R_FP_BTIME;

    let mut w = W::new();
    w.u8(FORMAT);
    w.uv(u64::from(flags));
    if !content_node {
        put_content(&mut w, &row.content);
    }
    if !scan_node {
        put_metadata(&mut w, &row.scan);
    }
    if let (3, Some(a)) = (applied, &row.applied) {
        put_metadata(&mut w, a);
    }
    w.uv(fp.dev);
    w.uv(fp.ino);
    if !size_same {
        w.uv(fp.size);
    }
    if !mtime_same {
        w.time(fp.mtime);
    }
    if !ctime_same {
        w.time(fp.ctime);
    }
    if let (2, Some(t)) = (btime, fp.btime) {
        w.time(t);
    }
    if let Some(s) = &row.synced {
        put_content(&mut w, &s.body.content);
        put_metadata(&mut w, &s.body.metadata);
        put_links(&mut w, s.links.iter());
        put_metadata(&mut w, &s.applied);
        let mut sf = 0;
        if s.commit.is_some() {
            sf |= SYNCED_COMMIT;
        }
        if s.unapplied {
            sf |= SYNCED_UNAPPLIED;
        }
        if s.folder_time.is_some() {
            sf |= SYNCED_FOLDER_TIME;
        }
        w.u8(sf);
        if let Some(c) = s.commit {
            w.raw(&c.0);
        }
        if let Some(t) = s.folder_time {
            w.time(t);
        }
    }
    if let (2, Some(h)) = (held, &row.held) {
        put_links(&mut w, h.iter());
    }
    w.0
}

/// A row, read against `node`, the base node stored beside it, if there is one.
pub fn decode_row(b: &[u8], node: Option<&Node>) -> Result<Row> {
    let bad = || anyhow!("a row in the state does not parse");
    let mut r = R(b);
    if r.u8().ok_or_else(bad)? != FORMAT {
        return Err(bad());
    }
    let row = (|| {
        let flags = u32::try_from(r.uv()?).ok()?;
        if flags >> (R_FP_BTIME + 2) != 0 {
            return None;
        }
        let content = match flags & R_CONTENT_NODE != 0 {
            true => node?.body.content.clone(),
            false => get_content(&mut r)?,
        };
        let scan = match flags & R_SCAN_NODE != 0 {
            true => node?.body.metadata.clone(),
            false => get_metadata(&mut r)?,
        };
        let applied = match (flags >> R_APPLIED) & 3 {
            0 => None,
            1 => Some(scan.clone()),
            2 => Some(node?.body.metadata.clone()),
            _ => Some(get_metadata(&mut r)?),
        };
        let dev = r.uv()?;
        let ino = r.uv()?;
        let size = match flags & R_FP_SIZE != 0 {
            true => content_size(&content)?,
            false => r.uv()?,
        };
        let mtime = match flags & R_FP_MTIME != 0 {
            true => scan.mtime,
            false => r.time()?,
        };
        let ctime = match flags & R_FP_CTIME != 0 {
            true => mtime,
            false => r.time()?,
        };
        let btime = match (flags >> R_FP_BTIME) & 3 {
            0 => None,
            1 => Some(scan.btime?),
            2 => Some(r.time()?),
            _ => return None,
        };
        let synced = match flags & R_SYNCED != 0 {
            false => None,
            true => {
                let content = get_content(&mut r)?;
                let metadata = get_metadata(&mut r)?;
                let links = get_links(&mut r)?;
                let applied = get_metadata(&mut r)?;
                let sf = r.u8()?;
                if sf >> 3 != 0 {
                    return None;
                }
                let commit = match sf & SYNCED_COMMIT != 0 {
                    true => Some(OpId(r.array::<16>()?)),
                    false => None,
                };
                let folder_time = match sf & SYNCED_FOLDER_TIME != 0 {
                    true => Some(r.time()?),
                    false => None,
                };
                Some(Synced {
                    body: RevisionBody { content, metadata },
                    links,
                    applied,
                    commit,
                    unapplied: sf & SYNCED_UNAPPLIED != 0,
                    folder_time,
                })
            }
        };
        let held = match (flags >> R_HELD) & 3 {
            0 => None,
            1 => Some(node?.links.clone()),
            2 => Some(get_links(&mut r)?),
            _ => return None,
        };
        Some(Row {
            fp: Fingerprint {
                dev,
                ino,
                size,
                mtime,
                ctime,
                btime,
            },
            racy: flags & R_RACY != 0,
            content,
            scan,
            applied,
            synced,
            incomplete: flags & R_INCOMPLETE != 0,
            held,
        })
    })()
    .ok_or_else(bad)?;
    if !r.0.is_empty() {
        return Err(bad());
    }
    Ok(row)
}

/// A manifest as stored: `1` and its fields when its canonical encoding is what it was given, which
/// is then made again to be read; `0` and the bytes as given otherwise, which this build never makes
/// but would keep whole.
pub fn encode_manifest(bytes: &[u8]) -> Vec<u8> {
    let m = match Manifest::decode(bytes) {
        Ok(m) if m.encode() == bytes => m,
        _ => {
            let mut w = W(Vec::with_capacity(bytes.len() + 1));
            w.u8(0);
            w.raw(bytes);
            return w.0;
        }
    };
    let mut w = W::new();
    w.u8(1);
    w.uv(m.size);
    w.uv(m.segments.len() as u64);
    for s in &m.segments {
        w.uv(s.offset);
        w.uv(s.chunks.len() as u64);
        for c in &s.chunks {
            w.raw(&c.hash.0);
            w.uv(u64::from(c.len));
        }
    }
    w.0
}

/// The manifest's canonical bytes again, checked against the hash it is stored under.
pub fn decode_manifest(stored: &[u8], hash: &Hash) -> Result<Vec<u8>> {
    let bad = || anyhow!("manifest {hash} in the state does not parse");
    let mut r = R(stored);
    let bytes = match r.u8().ok_or_else(bad)? {
        0 => r.0.to_vec(),
        1 => {
            let m = (|| {
                let size = r.uv()?;
                let mut segments = Vec::new();
                for _ in 0..r.count(2)? {
                    let offset = r.uv()?;
                    let mut chunks = Vec::new();
                    for _ in 0..r.count(33)? {
                        let hash = Hash(r.array::<32>()?);
                        let len = r.u32v()?;
                        chunks.push(ChunkRef { hash, len });
                    }
                    segments.push(Segment { offset, chunks });
                }
                r.0.is_empty().then(|| Manifest::new(size, segments))
            })()
            .ok_or_else(bad)?;
            m.encode()
        }
        _ => return Err(bad()),
    };
    if Hash::of(&bytes) != *hash {
        return Err(anyhow!(
            "manifest {hash} in the state is not the manifest of that hash"
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(p: u8, name: &[u8]) -> Link {
        Link {
            parent: ObjectId([p; 16]),
            name: Name::new(name.to_vec()).unwrap(),
        }
    }

    fn rich_metadata() -> Metadata {
        let mut m = Metadata {
            mode: 0o7755,
            owner: Some(Owner {
                uid: u32::MAX,
                gid: 0,
                user: Some(Bytes(vec![0xff, b's'])),
                group: None,
            }),
            mtime: Timestamp {
                sec: -1,
                nsec: 999_999_999,
            },
            btime: Some(Timestamp {
                sec: i64::MIN,
                nsec: 0,
            }),
            time_precision_ns: 100,
            records: BTreeMap::new(),
            streams: vec![
                NamedStream {
                    ns: cairn_protocol::NS_MACOS_RESOURCE_FORK.into(),
                    name: Bytes(b"rsrc".to_vec()),
                    manifest: Hash::of(b"r"),
                },
                NamedStream {
                    ns: "some.future.ns".into(),
                    name: Bytes(vec![]),
                    manifest: Hash::of(b"s"),
                },
            ],
            unknown: serde_json::from_str(r#"{"from_the_future":{"x":[1,2.5,"y",null]}}"#).unwrap(),
        };
        for (ns, name) in [
            (cairn_protocol::NS_LINUX_XATTR, &b"user.x"[..]),
            (cairn_protocol::NS_WINDOWS_ACL, b"sddl"),
            ("a.namespace.of.later", b"\0\xff"),
        ] {
            m.records.insert(
                RecordKey {
                    ns: ns.into(),
                    name: Bytes(name.to_vec()),
                },
                Bytes(vec![0, 1, 255]),
            );
        }
        m
    }

    fn nodes() -> Vec<Node> {
        let plain = Metadata {
            mode: 0o644,
            owner: Some(Owner {
                uid: 1000,
                gid: 1000,
                user: Some(Bytes(b"s".to_vec())),
                group: Some(Bytes(b"s".to_vec())),
            }),
            mtime: Timestamp {
                sec: 1_794_046_484,
                nsec: 73_936_147,
            },
            btime: None,
            ..Default::default()
        };
        vec![
            Node {
                rev: RevisionId([7; 16]),
                body: RevisionBody {
                    content: Content::File {
                        manifest: Hash::of(b"m"),
                        size: u64::MAX,
                    },
                    metadata: plain.clone(),
                },
                links: [link(1, b"a"), link(2, b"\xffb")].into(),
            },
            Node {
                rev: RevisionId::ZERO,
                body: RevisionBody {
                    content: Content::Dir,
                    metadata: Metadata::default(),
                },
                links: BTreeSet::new(),
            },
            Node {
                rev: RevisionId([9; 16]),
                body: RevisionBody {
                    content: Content::Symlink {
                        target: Bytes(b"../\xfe/x".to_vec()),
                    },
                    metadata: rich_metadata(),
                },
                links: [link(3, b"l")].into(),
            },
        ]
    }

    #[test]
    fn a_node_decodes_to_what_was_encoded() {
        for n in nodes() {
            let b = encode_node(&n);
            assert_eq!(decode_node(&b).unwrap(), n);
            // Its JSON's meaning, the same.
            let json: Node = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
            assert_eq!(decode_node(&b).unwrap(), json);
        }
    }

    fn rows_for(node: &Node) -> Vec<Row> {
        let fp = Fingerprint {
            dev: 38_654_705_664,
            ino: 87_571_869,
            size: content_size(&node.body.content).unwrap_or(4096),
            mtime: node.body.metadata.mtime,
            ctime: node.body.metadata.mtime,
            btime: node.body.metadata.btime,
        };
        let same = Row {
            fp: fp.clone(),
            racy: false,
            content: node.body.content.clone(),
            scan: node.body.metadata.clone(),
            applied: Some(node.body.metadata.clone()),
            synced: None,
            incomplete: false,
            held: Some(node.links.clone()),
        };
        let mut other_meta = node.body.metadata.clone();
        other_meta.mode ^= 0o111;
        let all_different = Row {
            fp: Fingerprint {
                dev: 0,
                ino: u64::MAX,
                size: 3,
                mtime: Timestamp { sec: 5, nsec: 6 },
                ctime: Timestamp { sec: 7, nsec: 8 },
                btime: Some(Timestamp { sec: 9, nsec: 10 }),
            },
            racy: true,
            content: Content::Symlink {
                target: Bytes(b"t".to_vec()),
            },
            scan: other_meta.clone(),
            applied: Some(rich_metadata()),
            synced: Some(Synced {
                body: RevisionBody {
                    content: Content::File {
                        manifest: Hash::of(b"n"),
                        size: 0,
                    },
                    metadata: rich_metadata(),
                },
                links: [link(4, b"s"), link(4, b"t")].into(),
                applied: Metadata::default(),
                commit: Some(OpId([3; 16])),
                unapplied: true,
                folder_time: Some(Timestamp { sec: -3, nsec: 1 }),
            }),
            incomplete: true,
            held: Some([link(8, b"h")].into()),
        };
        let applied_is_node = Row {
            scan: other_meta,
            applied: Some(node.body.metadata.clone()),
            synced: Some(Synced {
                body: node.body.clone(),
                links: BTreeSet::new(),
                applied: node.body.metadata.clone(),
                commit: None,
                unapplied: false,
                folder_time: None,
            }),
            ..same.clone()
        };
        let nothing = Row {
            applied: None,
            held: None,
            fp: Fingerprint {
                btime: None,
                ..fp.clone()
            },
            ..same.clone()
        };
        let empty_held = Row {
            held: Some(BTreeSet::new()),
            ..same.clone()
        };
        vec![same, all_different, applied_is_node, nothing, empty_held]
    }

    #[test]
    fn a_row_decodes_to_what_was_encoded_beside_any_node_or_none() {
        for n in nodes() {
            for row in rows_for(&n) {
                for beside in [Some(&n), None] {
                    let b = encode_row(&row, beside);
                    assert_eq!(
                        decode_row(&b, beside).unwrap(),
                        row,
                        "{row:?} beside {beside:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_row_like_its_node_is_a_few_bytes() {
        let n = &nodes()[0];
        let row = &rows_for(n)[0];
        let b = encode_row(row, Some(n));
        // The format, the flags, the device and the inode.
        assert!(b.len() <= 1 + 2 + 6 + 4, "{} bytes", b.len());
        assert!(encode_node(n).len() < 120, "{}", encode_node(n).len());
    }

    #[test]
    fn a_damaged_record_is_an_error_not_another_record() {
        let n = &nodes()[2];
        let row = &rows_for(n)[1];
        let node = encode_node(n);
        let b = encode_row(row, Some(n));
        for cut in 0..node.len() {
            assert!(decode_node(&node[..cut]).is_err(), "node cut at {cut}");
        }
        for cut in 0..b.len() {
            assert!(decode_row(&b[..cut], Some(n)).is_err(), "row cut at {cut}");
        }
        let mut longer = b.clone();
        longer.push(0);
        assert!(decode_row(&longer, Some(n)).is_err());
        // A row written beside a node, read without it, cannot fill in the node's parts.
        let same = &rows_for(n)[0];
        assert!(decode_row(&encode_row(same, Some(n)), None).is_err());
        let mut other = node.clone();
        other[0] = FORMAT + 1;
        assert!(decode_node(&other).is_err());
    }

    #[test]
    fn a_manifest_is_stored_small_and_read_back_as_its_canonical_bytes() {
        let one = Manifest::new(
            523,
            vec![Segment {
                offset: 0,
                chunks: vec![ChunkRef {
                    hash: Hash::of(b"c"),
                    len: 523,
                }],
            }],
        );
        let sparse = Manifest::new(
            1 << 40,
            vec![
                Segment {
                    offset: 1 << 20,
                    chunks: vec![
                        ChunkRef {
                            hash: Hash::of(b"d"),
                            len: 4096,
                        },
                        ChunkRef {
                            hash: Hash::of(b"e"),
                            len: 1,
                        },
                    ],
                },
                Segment {
                    offset: 1 << 30,
                    chunks: vec![ChunkRef {
                        hash: Hash::of(b"f"),
                        len: 8 << 20,
                    }],
                },
            ],
        );
        let empty = Manifest::new(0, vec![]);
        for m in [one, sparse, empty] {
            let bytes = m.encode();
            let stored = encode_manifest(&bytes);
            assert_eq!(stored[0], 1);
            assert!(
                stored.len() * 2 < bytes.len(),
                "{} of {}",
                stored.len(),
                bytes.len()
            );
            assert_eq!(decode_manifest(&stored, &m.hash()).unwrap(), bytes);
            assert!(decode_manifest(&stored, &Hash::of(b"other")).is_err());
        }
        // Bytes that are not a canonical manifest are kept as they are.
        let odd = br#"{"size":0,"v":1,"segments":[]}"#;
        let stored = encode_manifest(odd);
        assert_eq!(stored[0], 0);
        assert_eq!(decode_manifest(&stored, &Hash::of(odd)).unwrap(), odd);
    }

    #[test]
    fn varints_round_trip_at_their_edges() {
        let mut w = W::new();
        let us = [
            0,
            1,
            127,
            128,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ];
        let is = [0, -1, 1, i64::MIN, i64::MAX, -64, 64];
        for u in us {
            w.uv(u);
        }
        for i in is {
            w.iv(i);
        }
        let mut r = R(&w.0);
        for u in us {
            assert_eq!(r.uv(), Some(u));
        }
        for i in is {
            assert_eq!(r.iv(), Some(i));
        }
        assert!(r.0.is_empty());
        // Eleven bytes, or a tenth past 64 bits: not a varint this writes.
        assert_eq!(R(&[0xff; 11]).uv(), None);
        assert_eq!(
            R(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]).uv(),
            None
        );
    }
}
