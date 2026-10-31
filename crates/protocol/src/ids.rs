//! Identifiers: random 128-bit IDs chosen by clients, and BLAKE3 content hashes.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

fn hex_de<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[u8; N], D::Error> {
    let s = String::deserialize(d)?;
    let mut out = [0u8; N];
    hex::decode_to_slice(&s, &mut out).map_err(de::Error::custom)?;
    Ok(out)
}

macro_rules! hex_id {
    ($(#[$doc:meta])* $name:ident, $n:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(pub [u8; $n]);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", &hex::encode(self.0)[..8])
            }
        }
        impl FromStr for $name {
            type Err = hex::FromHexError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let mut out = [0u8; $n];
                hex::decode_to_slice(s, &mut out)?;
                Ok(Self(out))
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&hex::encode(self.0))
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                hex_de::<D, $n>(d).map(Self)
            }
        }
    };
}

macro_rules! random_id {
    ($name:ident) => {
        impl $name {
            pub fn random() -> Self {
                Self(rand::random())
            }
        }
    };
}

hex_id!(
    /// Stable identity of a file, directory or symlink, independent of its path.
    ObjectId,
    16
);
hex_id!(
    /// One revision of an object.
    RevisionId,
    16
);
hex_id!(
    /// Idempotency key of a commit: a retried ID returns the first result.
    OpId,
    16
);
hex_id!(
    /// A client installation.
    DeviceId,
    16
);
hex_id!(
    /// BLAKE3 of a chunk or of an encoded manifest.
    Hash,
    32
);
random_id!(ObjectId);
random_id!(RevisionId);
random_id!(OpId);
random_id!(DeviceId);

impl ObjectId {
    /// The root directory of every namespace.
    pub const ROOT: ObjectId = ObjectId([0; 16]);
}

impl RevisionId {
    /// The root directory's revision before any client has described it.
    pub const ZERO: RevisionId = RevisionId([0; 16]);
}

impl Hash {
    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }
}
