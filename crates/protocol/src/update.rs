//! Updates: what a release holds, as its publisher signed it, and what a device makes of one.
//!
//! A release is described by a [`Release`], which the publisher signs with an Ed25519 key: the
//! [`Signed`] envelope is `update.json`, one file of the release, fetched from a feed URL. A build trusts
//! the public keys it was built with, or those its settings name with another feed, so what it installs
//! is what the holder of one of those keys published, byte for byte: each file's size and SHA-256 are in
//! what was signed. A build from source names no feed unless whoever built it gives one, so it is never
//! updated by anyone else's releases.
//!
//! docs/protocol.md, "Updates", has the rest: how a device checks, installs and takes an update back.

use serde::{Deserialize, Serialize};

use crate::local::Secs;

/// What a signature is over: this, then the release's JSON as it stands in [`Signed::release`]. So a
/// signature made for anything else with the same key can never pass for one of a release.
pub const SIGNED_PREFIX: &[u8] = b"cairn release v1\n";

/// `update.json`: a release, and the signatures over it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed {
    /// The [`Release`] as JSON: the very text signed, so that no reader's parse-and-print can change
    /// what is checked.
    pub release: String,
    /// One per key: a release signed with an old key and its successor is trusted by builds of both.
    pub signatures: Vec<Signature>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// The Ed25519 public key, 32 bytes in standard base64.
    pub key: String,
    /// Over [`SIGNED_PREFIX`] and then [`Signed::release`]: 64 bytes in standard base64.
    pub signature: String,
}

/// One version of every program, for every platform it is built for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// As `cairn-sync --version` prints it after the program's name: `0.1.6`.
    pub version: String,
    /// [`crate::PROTOCOL_VERSION`] of its programs. A client refuses a server on another, so a client
    /// takes a release on another protocol only once every server it syncs with speaks that one.
    pub protocol: u32,
    pub published: Secs,
    /// Where a person reads what is in it.
    #[serde(default)]
    pub notes: String,
    pub files: Vec<ReleaseFile>,
}

/// One program for one platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseFile {
    /// `cairn-sync` (the CLI and daemon) or `cairn-tray`.
    pub program: String,
    /// `linux-x86_64`, `macos-aarch64`, `macos-x86_64`, `macos-universal`, `windows-x86_64`, ...:
    /// [`platform`] of the build that is to take it.
    pub platform: String,
    pub form: Form,
    /// Absolute, or relative to the URL of `update.json`.
    pub url: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Form {
    /// The executable itself, put in place of the one installed.
    Binary,
    /// A macOS app bundle as a gzipped tar holding `<Name>.app` and nothing else, put in place of the
    /// installed bundle of the same name: its signature covers the whole bundle, and the permissions
    /// macOS keeps go by that signature.
    App,
}

/// The platform this build runs on, as [`ReleaseFile::platform`] names it.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Whether a file for `file` serves this platform: its own, or on macOS a universal build.
pub fn serves(file: &str, here: &str) -> bool {
    file == here || (here.starts_with("macos-") && file == "macos-universal")
}

/// A version as releases are numbered: dot-separated numbers, then perhaps `-` and a pre-release tag,
/// which comes before the same numbers without one. `0.1` and `0.1.0` are the same version.
#[derive(Clone, Debug)]
pub struct Version {
    pub numbers: Vec<u64>,
    pub pre: Option<String>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        let (main, pre) = match text.split_once('-') {
            Some((m, p)) if !p.is_empty() => (m, Some(p.to_string())),
            Some(_) => return None,
            None => (text, None),
        };
        let numbers = main
            .split('.')
            .map(|n| n.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        if numbers.is_empty() {
            return None;
        }
        Some(Self { numbers, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let len = self.numbers.len().max(other.numbers.len());
        let at = |v: &Self, i: usize| v.numbers.get(i).copied().unwrap_or(0);
        (0..len)
            .map(|i| at(self, i).cmp(&at(other, i)))
            .find(|o| o.is_ne())
            .unwrap_or_else(|| match (&self.pre, &other.pre) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// How this user's programs on this device take a new release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Download, check and install it, and each program restarts into it: the daemons between syncs.
    #[default]
    Auto,
    /// Say that it is there, in `cairn-sync update` and the tray, and install it only when a person asks.
    Notify,
    /// Never ask the feed. `cairn-sync update now` still installs what a person asks for.
    Off,
}

impl Policy {
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Notify => "notify",
            Self::Off => "off",
        }
    }
}

/// `update-settings.json` in the user dir ([`settings_path`]): absent, `auto` from the feed the build
/// names. The CLI (`cairn-sync update auto|notify|off`) and the tray write it, a new file over the old; a
/// file that does not parse is an error, never written over.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub policy: Policy,
    /// Another feed, the URL of an `update.json`: for a build of one's own, or to try a release before
    /// it is published. Several, comma-separated, are asked in order, as the build's are.
    #[serde(default)]
    pub feed: Option<String>,
    /// The keys that feed is signed with, base64. With a feed, they replace the build's own.
    #[serde(default)]
    pub keys: Vec<String>,
}

/// What the updater knows, in `update-state.json` in the user dir and in each daemon's status. Fields
/// are only ever added, with a default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub policy: Policy,
    /// When the feed last answered, and the newest release it offered then.
    #[serde(default)]
    pub checked: Secs,
    #[serde(default)]
    pub latest: Option<String>,
    /// The feed that answered then, the first of those named that gave a release a trusted key signed,
    /// and what each one asked before it said instead: a later feed is a fallback, so this says the first
    /// is down rather than leaving it unseen.
    #[serde(default)]
    pub answered: Option<String>,
    #[serde(default)]
    pub unanswered: Option<String>,
    /// A release newer than what is installed here, which is not installed: why is in `waits`.
    #[serde(default)]
    pub available: Option<String>,
    #[serde(default)]
    pub waits: Option<String>,
    /// The version the last update put in place, and when: a program still running an older one
    /// restarts into it, a daemon between syncs.
    #[serde(default)]
    pub installed: Option<String>,
    #[serde(default)]
    pub installed_at: Secs,
    /// What went wrong the last time, since when; cleared by the next check that goes through.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_since: Secs,
    /// Versions taken back here, by a person or because a daemon did not get through a sync on them:
    /// never installed again but by a person's `cairn-sync update now`.
    #[serde(default)]
    pub rejected: Vec<String>,
}

/// Where the settings are, in the user dir.
pub fn settings_path() -> Option<std::path::PathBuf> {
    crate::local::user_dir().map(|d| d.join("update-settings.json"))
}

/// Where the updater keeps its [`Status`], and what only it reads, in the user dir: a reader of the
/// status ignores the rest.
pub fn state_path() -> Option<std::path::PathBuf> {
    crate::local::user_dir().map(|d| d.join("update-state.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_as_releases_do() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(v("0.1.10") > v("0.1.9"));
        assert!(v("0.2") > v("0.1.99"));
        assert!(v("0.2.0") > v("0.2.0-rc1"));
        assert!(v("0.2.0-rc2") > v("0.2.0-rc1"));
        assert!(v("v1.0.0") > v("0.9.9"));
        assert_eq!(v("0.1"), v("0.1.0"));
        for bad in ["", "x", "0..1", "0.1-", "-rc1", "0.1.x"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_universal_build_serves_either_mac() {
        assert!(serves("macos-universal", "macos-aarch64"));
        assert!(serves("macos-universal", "macos-x86_64"));
        assert!(!serves("macos-universal", "linux-x86_64"));
        assert!(serves("linux-x86_64", "linux-x86_64"));
        assert!(!serves("linux-aarch64", "linux-x86_64"));
    }

    #[test]
    fn settings_and_status_from_before_their_fields_still_read() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.policy, Policy::Auto);
        let s: Status = serde_json::from_str(r#"{"policy":"notify"}"#).unwrap();
        assert_eq!((s.policy, s.installed), (Policy::Notify, None));
    }
}
