//! Requests and responses of the HTTP API. See docs/protocol.md.

use crate::ids::{DeviceId, Hash, ObjectId, OpId, RevisionId};
use crate::ops::{Op, OpReject};
use serde::{Deserialize, Serialize};

pub const MAX_CHUNK_BATCH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Info {
    pub protocol: u32,
    pub server_version: String,
    pub head: u64,
    #[serde(default)]
    pub max_batch_bytes: u64,
    /// What the token asking may do in this namespace. Absent from a server before accounts, whose one
    /// token may do everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
    /// That a note may name more objects than one (`Op::Note`'s `also`). Absent from a server
    /// before them, which would keep only the first: a client sends such a server a note to each.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub note_groups: bool,
    /// The namespace's name now. A namespace renamed answers to its former names too, so a client that
    /// asked by one goes on working, and takes this one up. Absent from a server before renaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// What this server does beyond what every server of this protocol does, by the names in
    /// [`feature`]: a client asks here before it uses any of it, and does without it otherwise. Absent
    /// from a server before features, which has none of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

impl Info {
    /// Whether the server says it has this feature, one of [`feature`]'s.
    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

/// The names of what a server may do beyond what every server of this protocol does, in
/// `Info::features` and `Me::features`. A client never asks a server for something it does not name
/// here; a name it does not know it passes over.
pub mod feature {
    /// A note may name more objects than one (also `Info::note_groups`, for clients before features).
    pub const NOTE_GROUPS: &str = "note_groups";
    /// A web `Item` carries its file's manifest hash.
    pub const ITEM_HASH: &str = "item_hash";
    /// `PUT /file` changes a file already there, given the revision the client saw (`base`).
    pub const FILE_UPDATE: &str = "file_update";
    /// `GET B/thumb/{object}`, and `PUT` of a client's own thumbnail.
    pub const THUMBS: &str = "thumbs";
    /// A user's own tokens through `/v1/tokens`, and `Me::token`.
    pub const TOKENS: &str = "tokens";
    /// A signed-in device makes a code another device trades for a token of its own: `/v1/pair`.
    pub const PAIRING: &str = "pairing";
    /// A user may sign in to the web with a password: `POST /v1/session` with `{user, password}`.
    pub const PASSWORDS: &str = "passwords";
    /// `PUT /file` is resumable, as the IETF's resumable uploads draft says.
    pub const RESUMABLE: &str = "resumable";
    /// A folder may keep less than its whole history: `B/retention`, and revisions marked pruned.
    pub const RETENTION: &str = "retention";
}

/// What a user may do in a namespace, which a person knows as a folder: each level holds the one
/// before it. No level at all is no access, and the namespace answers as one that does not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Read the tree, its history and its conflicts; change nothing.
    Read,
    /// Also upload and commit.
    Write,
    /// Also say who else has access, and at which level.
    Manage,
}

impl Access {
    pub const ALL: [Access; 3] = [Access::Read, Access::Write, Access::Manage];

    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Manage => "manage",
        }
    }

    pub fn can_write(self) -> bool {
        self >= Access::Write
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Access {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Access::ALL
            .into_iter()
            .find(|a| a.as_str() == s)
            .ok_or_else(|| format!("{s:?} is not an access level: read, write or manage"))
    }
}

/// `GET /v1/me`: who the token is, and every namespace it reaches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    pub user: String,
    pub folders: Vec<FolderAccess>,
    /// As `Info::features`, for a client that has no namespace to ask yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// The token the request came with, as `/v1/tokens` lists it (`feature::TOKENS`): which of the
    /// user's devices or browsers this is. Absent from a server before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<MeToken>,
    /// Whether the user has a password to sign in to the web with (`feature::PASSWORDS`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub password: bool,
}

/// The token a request came with, in `Me`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeToken {
    pub id: String,
    pub label: String,
    pub kind: TokenKind,
}

/// What a token was made for. A server before kinds made every token as a `device`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// Handed to a device by a person: `cairn-server token add`, or `POST /v1/tokens`.
    Device,
    /// Made for one browser when it signed in with a password; its text was never shown to anyone.
    Browser,
    /// Taken by a device with a pairing code another of the user's made: `POST /v1/pair/claim`.
    Paired,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Device => "device",
            TokenKind::Browser => "browser",
            TokenKind::Paired => "paired",
        }
    }
}

impl std::str::FromStr for TokenKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        [TokenKind::Device, TokenKind::Browser, TokenKind::Paired]
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("{s:?} is not a kind of token: device, browser or paired"))
    }
}

/// One of the caller's tokens, as `GET /v1/tokens` lists them: never the token itself, which the
/// server does not keep, but the first digits of its hash, by which it is named.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenView {
    pub id: String,
    pub label: String,
    pub kind: TokenKind,
    /// Seconds since the epoch.
    pub created: i64,
    /// The hour it was last used, in seconds since the epoch; absent if it has not been since the
    /// server began keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<i64>,
    /// The token this request came with.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub current: bool,
}

/// `GET /v1/tokens`: the caller's tokens, oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub tokens: Vec<TokenView>,
}

/// `POST /v1/tokens` and `PATCH /v1/tokens/{id}`: the label a person knows a token by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenLabel {
    pub label: String,
}

/// The answer to `POST /v1/tokens`: the new token, shown this once, and its ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewToken {
    pub id: String,
    pub token: String,
}

/// `PUT /v1/me/password`: a new password, or with `new` absent none; `current` the one the user has now,
/// if they have one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPassword {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

/// A password a user may have: 10 to 1024 bytes, and not only spaces.
pub fn check_password(password: &str) -> Result<(), String> {
    if password.chars().count() < 10 {
        return Err("a password is at least 10 characters".into());
    }
    if password.len() > 1024 {
        return Err("a password is at most 1024 bytes".into());
    }
    if password.trim().is_empty() {
        return Err("a password is more than spaces".into());
    }
    Ok(())
}

/// How long a pairing code waits to be taken, in seconds.
pub const PAIR_SECS: u64 = 600;

/// The letters a pairing code is made of: Crockford's base 32, which leaves out I, L, O and U, so that
/// nothing a person reads off a screen is taken for another.
pub const PAIR_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// `POST /v1/pair`, signed in: a code another device trades for a token of the same user's. `label` names
/// the device to come, until it names itself; `origin` the server's address as the caller reaches it,
/// for the answer's link and QR code.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// The answer to `POST /v1/pair`: the code as a person reads it, in three groups of four, how long it
/// waits, and given an origin, the link a phone's camera opens and its QR code as SVG.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    pub code: String,
    pub expires_in: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qr_svg: Option<String>,
}

/// `POST /v1/pair/claim`, with no token: the code, and the label the new device gives itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairClaim {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The answer to a claim: whose the new token is, its ID, and the token, shown this once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    pub user: String,
    pub id: String,
    pub token: String,
}

/// A pairing code as typed: its twelve letters, upper case, with any dashes and spaces left out and the
/// letters a person takes for digits read as those digits; `None` if it is not one.
pub fn pair_code(typed: &str) -> Option<String> {
    let mut out = String::with_capacity(12);
    for c in typed.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        if !c.is_ascii() || !PAIR_ALPHABET.contains(&(c as u8)) {
            return None;
        }
        out.push(c);
    }
    (out.len() == 12).then_some(out)
}

/// A pairing code as a person reads it: `ABCD-EFGH-JKMN`
pub fn pair_display(code: &str) -> String {
    let b = code.as_bytes();
    let group = |r: std::ops::Range<usize>| String::from_utf8_lossy(&b[r]).into_owned();
    format!("{}-{}-{}", group(0..4), group(4..8), group(8..12))
}

/// The link a phone opens to pair with a server: `cairn://pair?server=<origin>&code=<code>`
pub fn pair_uri(origin: &str, code: &str) -> String {
    let mut server = String::new();
    for b in origin.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            server.push(b as char);
        } else {
            server.push_str(&format!("%{b:02X}"));
        }
    }
    format!("cairn://pair?server={server}&code={code}")
}

/// A server's address as a pairing link names it: `http` or `https`, a host and perhaps a port, and
/// nothing after them but a slash.
pub fn check_origin(origin: &str) -> Result<(), String> {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .ok_or_else(|| format!("{origin:?} is not an http or https address"))?;
    let host = rest.strip_suffix('/').unwrap_or(rest);
    if host.is_empty()
        || host.len() > 253
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b))
    {
        return Err(format!("{origin:?} is not a server's address"));
    }
    Ok(())
}

/// A token's label: what a person knows it by, at most 100 characters and no control characters, and
/// not empty.
pub fn check_token_label(label: &str) -> Result<(), String> {
    if label.trim().is_empty() {
        return Err("a token's label is empty".into());
    }
    if label.chars().count() > 100 {
        return Err("a token's label is longer than 100 characters".into());
    }
    if label.chars().any(char::is_control) {
        return Err("a token's label holds a control character".into());
    }
    Ok(())
}

impl Me {
    /// Whether the server says it has this feature, one of [`feature`]'s.
    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderAccess {
    pub folder: String,
    pub access: Access,
    /// The user's own name for it, which they see in place of `folder`: absent if they have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The body of `PUT /v1/ns/{ns}/name`, and its answer: the caller's own name for the namespace, which
/// is theirs alone and changes nothing else. With `if_none`, it is set only if they have none yet, as
/// a device sends the name of the directory it syncs the namespace into; the answer is the name then.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderName {
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub if_none: bool,
}

/// The body of `POST /v1/folders`: a namespace to make, which the caller manages from then on, and
/// their own name for it, if they give one. The answer is the namespace as `GET /v1/me` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewFolder {
    pub folder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The body of `POST /v1/ns/{ns}/rename`, the namespace's name to be, and its answer, the name now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderRename {
    pub folder: String,
}

/// A user's own name for a folder: 1 to 64 characters, none of them a control character, and no space
/// at either end. Any script, as it is shown, never used as a path or in a URL.
pub fn valid_folder_name(name: &str) -> bool {
    let chars = name.chars().count();
    (1..=64).contains(&chars) && name.trim() == name && !name.chars().any(char::is_control)
}

/// One user's access to a namespace, as `GET /v1/ns/{ns}/access` lists them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub user: String,
    pub access: Access,
}

/// The body of `PUT /v1/ns/{ns}/access/{user}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub access: Access,
}

/// A user or namespace name a server takes: 1 to 64 of ASCII letters, digits, `.`, `_` and `-`, not
/// starting with `.` or `-`, so that it is the same in a URL, a file name and a shell.
pub fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HashList {
    pub hashes: Vec<Hash>,
}

/// `GET /v1/ns/{ns}/retention`, and `PUT` of it with `policy` alone (`feature::RETENTION`): the
/// folder's retention policy as text (`crate::retention::Policy`), `None` for none, which keeps every
/// revision and removes nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    pub policy: Option<String>,
    /// How long a deleted file's last revision is kept, in seconds: the policy's trash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash: Option<u64>,
    /// Who set it, and when, in seconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_at: Option<i64>,
    /// In the answer to a `PUT`: the revisions given up that came back, as no collection had taken
    /// their content yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored: Option<u64>,
}

/// `POST /v1/ns/{ns}/pruned`: which of these revisions the folder's retention gave up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedAsk {
    pub revisions: Vec<RevisionId>,
}

/// One revision retention gave up, and when, in seconds since the epoch: from then on its content may
/// be gone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedRevision {
    pub revision: RevisionId,
    pub at: i64,
}

/// The answer to `POST /v1/ns/{ns}/pruned`: those of the revisions asked about that were given up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedList {
    pub pruned: Vec<PrunedRevision>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitRequest {
    pub op_id: OpId,
    pub device: DeviceId,
    pub device_name: String,
    pub ops: Vec<Op>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CommitResponse {
    /// Durable; `seq` is the journal position. A retried `op_id` gets the first answer.
    Committed { seq: u64 },
    /// Nothing was applied.
    Rejected { reject: RejectReason },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RejectReason {
    Op {
        op_index: usize,
        reason: OpReject,
    },
    /// A manifest or chunk the commit names is not on the server.
    MissingChunk {
        hash: Hash,
    },
    BadManifest {
        hash: Hash,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub seq: u64,
    pub op_id: OpId,
    pub device: DeviceId,
    pub device_name: String,
    /// Seconds since the epoch, server clock.
    pub time: i64,
    pub ops: Vec<Op>,
    /// The user whose token made the commit, as named then. Absent before accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangesResponse {
    pub changes: Vec<Change>,
    pub head: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictEntry {
    pub seq: u64,
    pub copy: ObjectId,
    pub object: ObjectId,
    pub competing_revision: RevisionId,
    pub device_name: String,
    pub time: i64,
}

/// One chunk in a batch body: 32-byte hash, big-endian u32 length, the bytes.
pub fn write_frame(out: &mut Vec<u8>, hash: &Hash, data: &[u8]) {
    out.extend_from_slice(&hash.0);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("batch body is truncated")]
    Truncated,
    #[error("chunk {0} does not match its hash")]
    HashMismatch(Hash),
}

/// Split a batch body into verified chunks.
pub fn read_frames(mut body: &[u8]) -> Result<Vec<(Hash, &[u8])>, FrameError> {
    let mut out = Vec::new();
    while !body.is_empty() {
        if body.len() < 36 {
            return Err(FrameError::Truncated);
        }
        let hash = Hash(body[..32].try_into().expect("slice is 32 bytes"));
        let len = u32::from_be_bytes(body[32..36].try_into().expect("slice is 4 bytes")) as usize;
        body = &body[36..];
        if body.len() < len {
            return Err(FrameError::Truncated);
        }
        let (data, rest) = body.split_at(len);
        if Hash::of(data) != hash {
            return Err(FrameError::HashMismatch(hash));
        }
        out.push((hash, data));
        body = rest;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_hold_the_ones_below_and_parse_as_written() {
        assert!(Access::Manage > Access::Write && Access::Write > Access::Read);
        assert!(!Access::Read.can_write());
        assert!(Access::Write.can_write() && Access::Manage.can_write());
        for a in Access::ALL {
            assert_eq!(a.as_str().parse::<Access>(), Ok(a));
            assert_eq!(serde_json::to_string(&a).unwrap(), format!("\"{a}\""));
        }
        assert!("admin".parse::<Access>().is_err());
    }

    #[test]
    fn an_info_or_change_from_before_accounts_still_reads() {
        let info: Info =
            serde_json::from_str(r#"{"protocol":1,"server_version":"0.1.11","head":3}"#).unwrap();
        assert_eq!(info.access, None);
        let change: Change = serde_json::from_str(
            r#"{"seq":1,"op_id":"00000000000000000000000000000001","device":"00000000000000000000000000000002","device_name":"a","time":0,"ops":[]}"#,
        )
        .unwrap();
        assert_eq!(change.user, None);
        // And neither field is sent where there is none, so an older peer sees what it always did.
        assert!(!serde_json::to_string(&change).unwrap().contains("user"));
    }

    #[test]
    fn account_names_are_plain() {
        for good in ["default", "alice", "team-2026", "a.b_c", "A"] {
            assert!(valid_account_name(good), "{good}");
        }
        let long = "x".repeat(65);
        for bad in ["", ".hidden", "-x", "a b", "a/b", "é", "a:b", long.as_str()] {
            assert!(!valid_account_name(bad), "{bad}");
        }
    }

    #[test]
    fn frames_round_trip_and_catch_damage() {
        let mut body = Vec::new();
        for data in [&b"one"[..], b"", b"three"] {
            write_frame(&mut body, &Hash::of(data), data);
        }
        let frames = read_frames(&body).unwrap();
        assert_eq!(
            frames.iter().map(|f| f.1).collect::<Vec<_>>(),
            [&b"one"[..], b"", b"three"]
        );

        assert_eq!(
            read_frames(&body[..body.len() - 1]),
            Err(FrameError::Truncated)
        );
        let mut bad = body.clone();
        bad[36] ^= 1;
        assert!(matches!(
            read_frames(&bad),
            Err(FrameError::HashMismatch(_))
        ));
    }
}
