//! The metadata record: what is stored, whatever any one client can apply (invariant 2).

use base64::Engine;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

use crate::ids::Hash;

/// All 12 permission bits.
pub const MODE_MASK: u32 = 0o7777;

pub const NS_LINUX_XATTR: &str = "linux.xattr";
pub const NS_MACOS_XATTR: &str = "macos.xattr";
pub const NS_MACOS_FINDER_INFO: &str = "macos.finder_info";
pub const NS_MACOS_RESOURCE_FORK: &str = "macos.resource_fork";
pub const NS_WINDOWS_ACL: &str = "windows.acl";
pub const NS_WINDOWS_ATTRIBUTES: &str = "windows.attributes";
pub const NS_WINDOWS_ADS: &str = "windows.ads";

/// Opaque bytes, base64 on the wire.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}
impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map(Bytes)
            .map_err(de::Error::custom)
    }
}

/// Nanoseconds since the epoch, kept exactly.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug, Serialize, Deserialize,
)]
pub struct Timestamp {
    pub sec: i64,
    pub nsec: u32,
}

/// Numbers and names, since numeric IDs differ between machines.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<Bytes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<Bytes>,
}

/// Addresses a platform record: xattr, ACL, Finder info. Linux POSIX ACLs and capabilities are
/// xattrs (`system.posix_acl_*`, `security.capability`) and travel as such.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordKey {
    pub ns: String,
    pub name: Bytes,
}

#[derive(Serialize, Deserialize)]
struct RecordWire {
    ns: String,
    name: Bytes,
    value: Bytes,
}

mod records {
    use super::*;
    pub fn serialize<S: Serializer>(
        m: &BTreeMap<RecordKey, Bytes>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_seq(m.iter().map(|(k, v)| RecordWire {
            ns: k.ns.clone(),
            name: k.name.clone(),
            value: v.clone(),
        }))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<RecordKey, Bytes>, D::Error> {
        Ok(Vec::<RecordWire>::deserialize(d)?
            .into_iter()
            .map(|r| {
                (
                    RecordKey {
                        ns: r.ns,
                        name: r.name,
                    },
                    r.value,
                )
            })
            .collect())
    }
}

/// A large named side stream (macOS resource fork, NTFS alternate data stream): chunked like a file.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct NamedStream {
    pub ns: String,
    pub name: Bytes,
    pub manifest: Hash,
}

/// The stored metadata of a revision.
///
/// `unknown` keeps every field this build does not know, and it serializes back in place, so an
/// older client passing a record through cannot erase what a newer one wrote.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Metadata {
    pub mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
    pub mtime: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btime: Option<Timestamp>,
    /// Timestamp granularity of the filesystem the times were read from.
    #[serde(default = "one")]
    pub time_precision_ns: u32,
    #[serde(default, with = "records", skip_serializing_if = "BTreeMap::is_empty")]
    pub records: BTreeMap<RecordKey, Bytes>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<NamedStream>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, serde_json::Value>,
}

fn one() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_this_build_does_not_know_survives_a_round_trip() {
        let wire = r#"{"mode":493,"mtime":{"sec":1,"nsec":2},"time_precision_ns":1,"from_the_future":{"x":[1,2]}}"#;
        let m: Metadata = serde_json::from_str(wire).unwrap();
        assert_eq!(m.unknown["from_the_future"]["x"][1], 2);
        let again: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(again["from_the_future"]["x"][1], 2);
    }

    /// Milestone 0: every row of PLAN.md's metadata table has a place in the record.
    #[test]
    fn the_record_covers_the_metadata_table() {
        let mut m = Metadata {
            mode: 0o7755,
            owner: Some(Owner {
                uid: 1000,
                gid: 1000,
                user: Some(Bytes(b"name".to_vec())),
                group: Some(Bytes(b"group".to_vec())),
            }),
            mtime: Timestamp {
                sec: 1,
                nsec: 999_999_999,
            },
            btime: Some(Timestamp { sec: 0, nsec: 1 }),
            time_precision_ns: 1,
            streams: vec![NamedStream {
                ns: NS_MACOS_RESOURCE_FORK.into(),
                name: Bytes(b"rsrc".to_vec()),
                manifest: Hash::of(b""),
            }],
            ..Default::default()
        };
        for (ns, name) in [
            (NS_LINUX_XATTR, "user.x"),
            (NS_LINUX_XATTR, "system.posix_acl_access"),
            (NS_LINUX_XATTR, "security.capability"),
            (NS_MACOS_XATTR, "com.apple.quarantine"),
            (NS_MACOS_FINDER_INFO, "info"),
            (NS_WINDOWS_ACL, "sddl"),
            (NS_WINDOWS_ATTRIBUTES, "attrs"),
        ] {
            m.records.insert(
                RecordKey {
                    ns: ns.into(),
                    name: Bytes(name.into()),
                },
                Bytes(vec![0, 1, 255]),
            );
        }
        let back: Metadata = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }
}
