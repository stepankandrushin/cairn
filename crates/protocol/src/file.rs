//! `PUT /v1/ns/{ns}/file`: one file's bytes in one request, which the server stores and commits
//! (docs/protocol.md, "One-request uploads"). Everything but the token is in the query, so a request
//! built once and sent later by something else, as a phone's system sends a photo it uploads in the
//! background, carries all of it; the answer is in headers too, for a sender that keeps only those.

use crate::ids::{DeviceId, ObjectId, RevisionId};
use crate::metadata::Timestamp;
use crate::name::Name;
use serde::{Deserialize, Serialize};

/// What to do when the file's name is taken by something else in its folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Taken {
    /// Refuse, and change nothing.
    #[default]
    Fail,
    /// Take the first free `stem (n).ext` from 2 on, so that both stay.
    Rename,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileQuery {
    /// The folder `path` starts in; absent for the namespace's root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ObjectId>,
    /// Names joined by `/`, the last the file's own. Folders missing before it are made.
    pub path: String,
    /// The new file's ID and its revision's, chosen by the client. The commit's `op_id` is the
    /// revision's bytes, so a retry is the same commit, and its answer the first one's.
    pub object: ObjectId,
    pub revision: RevisionId,
    pub device: DeviceId,
    pub device_name: String,
    /// Seconds since the epoch, with up to nine digits after a `.`: as many as it was known to.
    pub mtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btime: Option<String>,
    #[serde(default)]
    pub taken: Taken,
}

/// The answer, 201 for a file made (or a retry of the request that made it), 200 for one that was
/// there already, at that name with the same bytes, which nothing was committed for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAnswer {
    pub object: ObjectId,
    pub revision: RevisionId,
    pub seq: u64,
    /// The name it has: the one asked for, or with `(n)` if that was taken.
    pub name: Name,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub existed: bool,
}

/// The answer's headers, lower case as HTTP/2 and the Photos system give them. The name is base64 of
/// its bytes; the error, on a refusal, is ASCII with any other byte as `%XX`.
pub const HEADER_OBJECT: &str = "x-cairn-object";
pub const HEADER_REVISION: &str = "x-cairn-revision";
pub const HEADER_SEQ: &str = "x-cairn-seq";
pub const HEADER_NAME: &str = "x-cairn-name";
pub const HEADER_EXISTED: &str = "x-cairn-existed";
pub const HEADER_ERROR: &str = "x-cairn-error";

/// A time as the query carries it, to `precision_ns`.
pub fn format_time(t: Timestamp, precision_ns: u32) -> String {
    let mut digits = 9usize;
    let mut p = precision_ns.max(1);
    while p >= 10 && digits > 0 {
        p /= 10;
        digits -= 1;
    }
    if digits == 0 {
        return t.sec.to_string();
    }
    let frac = format!("{:09}", t.nsec.min(999_999_999));
    format!("{}.{}", t.sec, &frac[..digits])
}

/// A time from the query, and the precision its digits give: `None` if it is not one.
pub fn parse_time(s: &str) -> Option<(Timestamp, u32)> {
    let (sec, frac) = match s.split_once('.') {
        Some((_, "")) => return None,
        Some((a, b)) => (a, b),
        None => (s, ""),
    };
    if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if sec.is_empty()
        || !sec
            .trim_start_matches('-')
            .bytes()
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let sec: i64 = sec.parse().ok()?;
    let nsec: u32 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<9}").parse().ok()?
    };
    let precision = 10u32.pow(9 - frac.len() as u32);
    Some((Timestamp { sec, nsec }, precision))
}

/// A query string of these, each value percent-encoded.
pub fn query_string(q: &FileQuery) -> String {
    let mut parts: Vec<(&str, String)> = Vec::new();
    if let Some(p) = q.parent {
        parts.push(("parent", p.to_string()));
    }
    parts.push(("path", q.path.clone()));
    parts.push(("object", q.object.to_string()));
    parts.push(("revision", q.revision.to_string()));
    parts.push(("device", q.device.to_string()));
    parts.push(("device_name", q.device_name.clone()));
    parts.push(("mtime", q.mtime.clone()));
    if let Some(b) = &q.btime {
        parts.push(("btime", b.clone()));
    }
    parts.push((
        "taken",
        match q.taken {
            Taken::Fail => "fail",
            Taken::Rename => "rename",
        }
        .into(),
    ));
    parts
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v.as_bytes())))
        .collect::<Vec<_>>()
        .join("&")
}

/// Every byte but RFC 3986's unreserved ones as `%XX`.
pub fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Text for a header: printable ASCII as it is, `%` and any other byte as `%XX`.
pub fn header_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if (0x20..0x7f).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The text `header_text` made, or `%XX` undone in anything else; invalid UTF-8 as U+FFFD.
pub fn from_header_text(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_digit(b[i + 1]), hex_digit(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Splits `path` into names: `None` if one is empty, `.`, `..` or otherwise not a name.
pub fn path_names(path: &str) -> Option<Vec<Name>> {
    if path.is_empty() {
        return None;
    }
    path.split('/')
        .map(|c| Name::new(c.as_bytes().to_vec()).ok())
        .collect()
}

/// `stem (n).ext`, the extension being what follows the last `.` that is not the first byte, within
/// the 255 bytes filesystems allow.
pub fn numbered(name: &Name, n: u32) -> Name {
    let bytes = name.as_bytes();
    let split = bytes
        .iter()
        .rposition(|&b| b == b'.')
        .filter(|&i| i > 0)
        .unwrap_or(bytes.len());
    let (stem, ext) = bytes.split_at(split);
    let tag = format!(" ({n})").into_bytes();
    let (stem, ext) = if tag.len() + ext.len() + 1 > 255 {
        (bytes, &bytes[0..0])
    } else {
        (stem, ext)
    };
    let room = 255usize.saturating_sub(tag.len() + ext.len());
    let mut out = stem[..stem.len().min(room)].to_vec();
    out.extend_from_slice(&tag);
    out.extend_from_slice(ext);
    Name::new(out).expect("a numbered name is built from a valid name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_keep_their_precision() {
        let t = Timestamp {
            sec: 1_731_650_000,
            nsec: 123_456_789,
        };
        assert_eq!(format_time(t, 1), "1731650000.123456789");
        assert_eq!(format_time(t, 1_000_000), "1731650000.123");
        assert_eq!(format_time(t, 1_000_000_000), "1731650000");
        assert_eq!(
            parse_time("1731650000.123"),
            Some((
                Timestamp {
                    sec: 1_731_650_000,
                    nsec: 123_000_000
                },
                1_000_000
            ))
        );
        assert_eq!(parse_time("1731650000").unwrap().1, 1_000_000_000);
        assert_eq!(parse_time("-5.5").unwrap().0.sec, -5);
        for bad in ["", ".5", "1.", "1.1234567890", "1e5", "x", "1.-2", "-"] {
            assert_eq!(parse_time(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn header_text_round_trips() {
        for s in ["plain", "50% done", "фото (2).jpg", "tab\there"] {
            let h = header_text(s);
            assert!(h.bytes().all(|b| (0x20..0x7f).contains(&b)), "{h}");
            assert_eq!(from_header_text(&h), s);
        }
    }

    #[test]
    fn a_path_of_names_only() {
        assert_eq!(path_names("a/b.jpg").unwrap().len(), 2);
        for bad in ["", "/a", "a/", "a//b", "a/../b", ".", "a/\0"] {
            assert!(path_names(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn numbered_names() {
        let n = |s: &str| Name::new(s.as_bytes().to_vec()).unwrap();
        assert_eq!(numbered(&n("a.jpg"), 2), n("a (2).jpg"));
        assert_eq!(numbered(&n(".hidden"), 3), n(".hidden (3)"));
        assert_eq!(numbered(&n("noext"), 2), n("noext (2)"));
        assert!(numbered(&n(&"x".repeat(300)), 12).as_bytes().len() <= 255);
    }

    #[test]
    fn query_is_what_axum_reads_back() {
        let q = FileQuery {
            parent: None,
            path: "Photos/The phone's photos/26-11-15 10-00-00 0001.heic".into(),
            object: ObjectId([1; 16]),
            revision: RevisionId([2; 16]),
            device: DeviceId([3; 16]),
            device_name: "iPhone & co".into(),
            mtime: "1731650000.123".into(),
            btime: None,
            taken: Taken::Rename,
        };
        let s = query_string(&q);
        assert!(!s.contains(' ') && !s.contains('\''), "{s}");
        assert!(s.contains("path=Photos%2FThe%20phone%27s%20photos%2F"), "{s}");
    }
}
