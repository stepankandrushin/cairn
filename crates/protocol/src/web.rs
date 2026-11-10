//! The endpoints the web interface uses, which any client may (docs/protocol.md, "Web"): a tree's items,
//! their contents and history, uploads, what was deleted, shares and public links.

use crate::ids::{ObjectId, RevisionId};
use crate::metadata::{Bytes, Timestamp};
use crate::name::Name;
use crate::ops::{Link, ObjectKind};
use crate::wire::Access;
use serde::{Deserialize, Serialize};

/// The cookie a browser is signed in by: the token itself, which no script on the page can read.
pub const SESSION_COOKIE: &str = "cairn";

/// What a request signed in by the cookie must carry unless it only reads: no other site's page can
/// make a browser send it.
pub const WEB_HEADER: &str = "x-cairn-web";

/// The most one `PUT` of an upload carries.
pub const MAX_UPLOAD_PART: usize = 64 * 1024 * 1024;

/// The device name every web commit has.
pub const WEB_DEVICE: &str = "web";

/// `POST /v1/session`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignIn {
    pub token: String,
}

/// An object as a person sees it in a listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub object: ObjectId,
    pub revision: RevisionId,
    pub kind: ObjectKind,
    /// The name it has where it was reached, absent for a namespace's root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Name>,
    /// The name as text, invalid UTF-8 as `\xNN`; for a namespace's root, the namespace's name.
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub mtime: Timestamp,
    pub mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Bytes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_display: Option<String>,
}

/// One folder on the way from a view's root to an item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub object: ObjectId,
    pub display: String,
}

/// `GET B/item/{object}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemView {
    pub item: Item,
    /// From the view's root, `path[0]`, to the item itself.
    pub path: Vec<Step>,
    pub access: Access,
}

/// `GET B/dir/{object}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listing {
    pub dir: Item,
    pub path: Vec<Step>,
    /// In name-byte order.
    pub entries: Vec<Item>,
    pub access: Access,
}

/// One revision of an object, with the commit that made it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub revision: RevisionId,
    pub seq: u64,
    pub time: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub device_name: String,
    pub kind: ObjectKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub mode: u32,
    pub mtime: Timestamp,
}

/// `GET B/history/{object}`: newest first.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct History {
    pub revisions: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mkdir {
    pub parent: ObjectId,
    pub name: Name,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Made {
    pub object: ObjectId,
    pub seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MoveRequest {
    pub object: ObjectId,
    pub from: Link,
    pub to: Link,
}

/// What a commit that names nothing new answers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Done {
    pub seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeleteRequest {
    pub object: ObjectId,
    pub link: Link,
    /// The revision the client saw: the delete is refused if the object has changed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<RevisionId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeleteDone {
    pub seq: u64,
    /// How many objects the commit unlinked: a folder's own and everything in it.
    pub objects: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub object: ObjectId,
    pub revision: RevisionId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Restored {
    pub seq: u64,
    /// The new revision, which holds the old one's content and metadata.
    pub revision: RevisionId,
}

/// `POST B/uploads`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadStart {
    pub parent: ObjectId,
    pub name: Name,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<Timestamp>,
    /// The revision of the file at that name the client saw, which the upload replaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<RevisionId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadId {
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Received {
    pub received: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadState {
    Receiving,
    Storing,
    Done,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadDone {
    pub object: ObjectId,
    pub revision: RevisionId,
    pub seq: u64,
}

/// `GET B/uploads/{id}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadStatus {
    pub id: String,
    pub size: u64,
    pub received: u64,
    pub state: UploadState,
    /// Bytes cut, hashed and stored so far while `storing`.
    pub stored: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<UploadDone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One thing deleted lately and still deleted: a file, or a folder with what went with it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeletedEntry {
    pub object: ObjectId,
    pub kind: ObjectKind,
    pub display: String,
    /// The folder it was in, as that folder's path is now or was when it went too; empty for the root.
    pub path: String,
    pub seq: u64,
    pub time: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub device_name: String,
    /// A file's size; for a folder, the bytes of the files deleted with it.
    pub size: u64,
    /// For a folder, the files deleted with it.
    pub files: u64,
}

/// `GET /v1/ns/{ns}/deleted`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeletedList {
    pub items: Vec<DeletedEntry>,
    /// False when the journal was read back only part of the way to the time asked for.
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Undelete {
    pub objects: Vec<ObjectId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UndeletedEntry {
    pub object: ObjectId,
    pub display: String,
    /// Where it is now, folders and name.
    pub path: String,
    /// It, or a folder above it that came back, has a name other than its last.
    pub renamed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UndeleteDone {
    pub seq: u64,
    pub items: Vec<UndeletedEntry>,
}

/// An object shared with a user, or by a public link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub id: String,
    pub object: ObjectId,
    pub kind: ObjectKind,
    /// Where the object is now in its folder; absent while it is deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whom it is shared with; absent for a public link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub access: Access,
    /// The manager who made it.
    pub by: String,
    pub created: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<i64>,
    /// A public link's token, which `/s/{link}` opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Where a public link is opened, when the server was told an address for links: that, then
    /// `/s/{link}`. Absent, a link opens at the server's own address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Shares {
    pub shares: Vec<Share>,
}

/// `POST /v1/ns/{ns}/shares`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewShare {
    pub object: ObjectId,
    /// Absent for a public link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<i64>,
}

/// One thing shared with the caller, as `GET /v1/shared` lists it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedItem {
    pub id: String,
    pub item: Item,
    pub by: String,
    pub access: Access,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedWithMe {
    pub shares: Vec<SharedItem>,
}

/// `GET /v1/links/{link}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkInfo {
    pub item: Item,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<i64>,
}

/// `GET /v1/users`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Users {
    pub users: Vec<String>,
}
