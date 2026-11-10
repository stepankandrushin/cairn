//! The binary records the client's state and the server's store keep (docs/format.md): integers in
//! LEB128, signed ones zigzagged first, IDs and hashes as their raw bytes, and anything of variable
//! length preceded by its length. Decoding is exact: what is decoded equals what was encoded, field by
//! field, unknown metadata fields included, and anything else is `None`, never another record.

use crate::{
    Bytes, Content, Hash, Link, Metadata, Name, NamedStream, ObjectId, Owner, RecordKey,
    RevisionBody, Timestamp,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct W(pub Vec<u8>);

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

pub struct R<'a>(pub &'a [u8]);

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
    crate::NS_LINUX_XATTR,
    crate::NS_MACOS_XATTR,
    crate::NS_MACOS_FINDER_INFO,
    crate::NS_MACOS_RESOURCE_FORK,
    crate::NS_WINDOWS_ACL,
    crate::NS_WINDOWS_ATTRIBUTES,
    crate::NS_WINDOWS_ADS,
];

pub fn put_ns(w: &mut W, ns: &str) {
    match NAMESPACES.iter().position(|k| *k == ns) {
        Some(i) => w.u8(i as u8 + 1),
        None => {
            w.u8(0);
            w.bytes(ns.as_bytes());
        }
    }
}

pub fn get_ns(r: &mut R<'_>) -> Option<String> {
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

pub fn put_metadata(w: &mut W, m: &Metadata) {
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

pub fn get_metadata(r: &mut R<'_>) -> Option<Metadata> {
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

pub fn put_content(w: &mut W, c: &Content) {
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

pub fn get_content(r: &mut R<'_>) -> Option<Content> {
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

pub fn put_links<'a>(w: &mut W, links: impl ExactSizeIterator<Item = &'a Link>) {
    w.uv(links.len() as u64);
    for l in links {
        put_link(w, l);
    }
}

pub fn put_link(w: &mut W, l: &Link) {
    w.raw(&l.parent.0);
    w.bytes(l.name.as_bytes());
}

pub fn get_link(r: &mut R<'_>) -> Option<Link> {
    let parent = ObjectId(r.array::<16>()?);
    let name = Name::new(r.bytes()?.to_vec()).ok()?;
    Some(Link { parent, name })
}

pub fn get_links(r: &mut R<'_>) -> Option<BTreeSet<Link>> {
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

/// A revision's body: its content, then its metadata.
pub fn put_body(w: &mut W, b: &RevisionBody) {
    put_content(w, &b.content);
    put_metadata(w, &b.metadata);
}

pub fn get_body(r: &mut R<'_>) -> Option<RevisionBody> {
    let content = get_content(r)?;
    let metadata = get_metadata(r)?;
    Some(RevisionBody { content, metadata })
}
