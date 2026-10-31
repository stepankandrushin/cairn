//! Directory entry names: bytes, validated so that no name received from a peer can address
//! anything but a single entry in its parent.

use crate::metadata::Bytes;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

const MAX_NAME: usize = 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("empty name")]
    Empty,
    #[error("`.` and `..` are not entry names")]
    Dot,
    #[error("name contains `/` or NUL")]
    Separator,
    #[error("name longer than {MAX_NAME} bytes")]
    TooLong,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(Vec<u8>);

impl Name {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, NameError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(NameError::Empty);
        }
        if bytes == b"." || bytes == b".." {
            return Err(NameError::Dot);
        }
        if bytes.iter().any(|&b| b == b'/' || b == 0) {
            return Err(NameError::Separator);
        }
        if bytes.len() > MAX_NAME {
            return Err(NameError::TooLong);
        }
        Ok(Self(bytes))
    }

    /// Sorts before every valid name: the start of a range scan, never an entry.
    pub(crate) fn lowest() -> Self {
        Self(Vec::new())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Printable form: UTF-8 stays as it is, any other byte becomes `\xNN`.
pub fn display_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&display_bytes(&self.0))
    }
}
impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", display_bytes(&self.0))
    }
}
impl Serialize for Name {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        Bytes(self.0.clone()).serialize(s)
    }
}
impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Name::new(Bytes::deserialize(d)?.0).map_err(de::Error::custom)
    }
}

/// How a folder translates names for a destination filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FilenamePolicy {
    /// NFC for valid Unicode; collisions and names invalid on Windows detected before applying.
    Portable,
    /// Original bytes, including names that are not valid UTF-8.
    #[default]
    Faithful,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_every_name_that_could_escape_its_parent() {
        assert_eq!(Name::new(*b""), Err(NameError::Empty));
        assert_eq!(Name::new(*b"."), Err(NameError::Dot));
        assert_eq!(Name::new(*b".."), Err(NameError::Dot));
        assert_eq!(Name::new(*b"a/b"), Err(NameError::Separator));
        assert_eq!(Name::new(*b"a\0b"), Err(NameError::Separator));
        assert!(Name::new(vec![b'x'; 2000]).is_err());
    }

    #[test]
    fn a_hostile_name_does_not_deserialize() {
        // base64 of "../x"
        assert!(serde_json::from_str::<Name>("\"Li4veA==\"").is_err());
    }

    #[test]
    fn non_utf8_round_trips_and_displays() {
        let n = Name::new(vec![0xff, b'a', 0xfe]).unwrap();
        let back: Name = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
        assert_eq!(back, n);
        assert_eq!(n.to_string(), "\\xffa\\xfe");
    }
}
