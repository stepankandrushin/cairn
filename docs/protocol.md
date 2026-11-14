# Cairn protocol, version 1

HTTP with JSON bodies, except chunk batches and file contents. Every path in Endpoints but `/v1/me` is under
`/v1/ns/{namespace}`, and every request needs `Authorization: Bearer <token>`, or the session cookie the web
interface signs in with ([Web](#web)). The types are defined once, in `crates/protocol`; this page explains
them. `Info.protocol` is the version: a client refuses a server that speaks another one. What answers without
a token is the web interface's own files, and a public link's view of what it shares.

The server in this release speaks plain HTTP on loopback; beyond loopback it refuses to start without
`--behind-tls-proxy`, and the client refuses `http://` beyond loopback without an explicit opt-in.

## Encodings

| Type | On the wire |
|---|---|
| Object, revision, operation and device IDs | 32 hex digits, 128 random bits chosen by the client |
| Root directory | `ObjectId` of all zeros; its first revision is `RevisionId` of all zeros |
| Hash | 64 hex digits, BLAKE3 |
| Names, symlink targets, xattr names and values | base64 of the raw bytes |
| Time | `{"sec": i64, "nsec": u32}` |

A name is 1 to 1024 bytes without `/` or NUL, and is not `.` or `..`. A name that breaks this does not
deserialize, so nothing a peer sends can address anything but one entry in its parent.

## Endpoints

| Method and path | Body | Answer |
|---|---|---|
| `GET /v1/me` | | `{user, folders: [{folder, access, name?}]}`: whose the token is, and every namespace it reaches; `name` is the user's own name for it, if they have one ([Names](#names)) |
| `POST /v1/folders` | `{folder, name?}` | 201 `{folder, access, name?}`: a new namespace, which the caller manages, with `name` as their own name for it, all at once ([Names](#names)). 400: no such name. 409: the name is or was another namespace's, of which the answer says nothing more |
| `GET /info` | | `{protocol, server_version, head, max_batch_bytes, access, note_groups, folder}`: `access` is the token's level here; a server before accounts sends none, and its one token may do everything. `note_groups`, absent before them, says that a note may name more objects than one. `folder`, absent before renaming, is the namespace's name now, which a client that asked by a former one takes up ([Names](#names)) |
| `POST /chunks/missing` | `{hashes}` | `{hashes}`: those not stored intact in this namespace. A damaged chunk counts as missing, so a good copy replaces it. Needs `write` |
| `POST /chunks/upload` | chunk batch | 204: all durable, made so together. 400: damaged batch, of which nothing is kept. Needs `write` |
| `POST /chunks/download` | `{hashes}` | chunk batch, in the order asked. 404: one is missing |
| `POST /commit` | `CommitRequest` | `CommitResponse`. Needs `write` |
| `GET /changes?after=N&limit=M&wait=S&ops=false` | | `{changes, head}`: journal entries after sequence N, in order: at most M (1000 if not given, at most 10,000), and fewer once about 16 MiB of operations are in the answer, but always one if there is one. With `wait`, see [Waiting](#waiting). With `ops=false`, each without its operations: where a client stands, for the price of one entry however large its commit |
| `GET /conflicts` | | The conflict inbox |
| `GET /access` | | `[{user, access}]`: who has access here, at which level |
| `PUT /access/{user}` | `{access}` | 204: that user has this level here now, in place of any other. Needs `manage`. 404: no such user. 409: it would leave the namespace with no manager |
| `DELETE /access/{user}` | | 204: that user has no access here now. Needs `manage`. 404: no such user, or it had none. 409: as for `PUT` |
| `PUT /name` | `{name, if_none?}` | `{name}`: the caller's own name for the namespace now ([Names](#names)). With `if_none`, set only if they have none. 400: no such name |
| `DELETE /name` | | 204: the caller has no name of their own for it now |
| `POST /rename` | `{folder}` | `{folder}`: the namespace's name now, for everyone who has it ([Names](#names)). Needs `manage`. 400: no such name. 409: as for `POST /v1/folders`; its own former name it may take back |

401 means a missing or wrong token, 404 an unknown namespace or one the token has no access to, alike, 403
a level too low for the request, and 507 a full disk on the server; in each case nothing changed.

## Access

A token belongs to one user, and the server keeps only its hash, BLAKE3 of its text. A user has a level in
each namespace given it, each level holding the one before:

| `access` | May |
|---|---|
| `read` | Read the tree, its chunks, the journal and the conflict inbox, and who has access |
| `write` | Also upload chunks and commit |
| `manage` | Also give other users a level here, or take it away, and rename the namespace |

No level is no access: the namespace answers 404, word for word as one that does not exist. What a user may
do is looked up on every request, so a change holds from the next one, and a device whose access went keeps
what it holds. A client that learns from `Info.access` that it may only read sends nothing up (PLAN.md,
"Sharing and access"); one from before accounts tries, and is answered 403 with the reason.

## Names

A namespace's name is what every path, device and link uses: 1 to 64 of ASCII letters, digits, `.`, `_`
and `-`, not starting with `.` or `-`. A server's first is `cloud`; one made before this release has
`default`. Any user adds one, `POST /v1/folders`, which they manage and nobody else reaches until given
access; its name is no other's, and the answer to one that is says only that. Its managers rename it while the
server serves, `POST /rename`, and its administrator with the server stopped, `cairn-server folder rename`;
nothing of its tree or chunks moves (format.md). The former name goes on reaching it, for paths of every
kind, so every device, link and address made with it goes on working, and no other namespace may take it;
a request begun by it as the rename is made goes on in the same namespace. `Info.folder` says the name now,
and a client keeps the one it is told from then on.

Each user may also have their own name for a namespace, which only they see, in place of its name: 1 to 64
characters of any script, no control character, and no space at either end. A device offers the name of the
directory it syncs the namespace into, `PUT /name` with `if_none`, as `cairn init` joins and once each time
its daemon starts, so that it holds only until they choose one; the web sets it and takes it away. It
changes nothing else: what a path, a device or another user says is the namespace's name.

A **chunk batch** is any number of frames: 32 bytes of hash, a big-endian `u32` length, the bytes.
Every frame is verified against its hash by whoever receives it.

## Committing

```
CommitRequest  { op_id, device, device_name, ops: [Op] }
CommitResponse { result: "committed", seq } | { result: "rejected", reject }
```

A commit is all or nothing, and durable when `committed` is answered. Retrying an `op_id` that committed
returns the first `seq` and changes nothing, so a lost response is harmless. A rejected commit changed
nothing; the client merges again and sends a new one.

Before the operations are looked at, the server checks that every manifest a revision names, and every
chunk those manifests name, is already stored with the right length. So a chunk is durable before
any commit can refer to it.

Operations apply in order, each validated against the state the previous one left, by the same code
that later replays them (`check_and_apply` in `crates/protocol/src/ops.rs`):

| `op` | Fields | Refused when |
|---|---|---|
| `create` | `object, revision, body, link, conflict_of?` | The object exists; the parent is not a live directory; the name is taken |
| `link` | `object, link` | The object does not exist; a directory that still has an entry; as `create` for the target; it would form a cycle |
| `unlink` | `object, link, base?` | The entry is not this object's; a directory that is not empty; `base`, the revision the device saw when it decided to delete, is not the head (`stale_base`): the delete has not seen an edit |
| `move` | `object, from, to` | As `unlink` for `from` without the emptiness rule, and `link` for `to` |
| `update` | `object, base, revision, body, restored_from?` | `base` is not the head (`stale_base`): another device changed it first; the kind would change; the object is deleted (`deleted`): the edit has not seen the delete. Bringing an object back links it first, in the same commit |
| `note` | `object, text, also?` | The object, or one in `also`, does not exist. Otherwise it has no effect; it is the activity log's entry for the loser of a metadata conflict, the same text for `object` and each of `also`. A client puts a commit's notes of one text in one, at the place of the last, only for a server whose `/info` says `note_groups`: a server before them keeps the first object alone. A client before them reads one as a note of its first object |

The root cannot be linked, moved or removed, and `.cairn` directly in the root is refused: that one
name holds each client's state. An object with no entries is deleted: it keeps its last state for
history, and a `link` brings it back as itself.

So neither an edit nor a delete can land on top of the other unseen: the second device is refused, merges, and the edit survives.

A batch that swaps names or nests a directory under its former child moves one party through a name
starting `.cairn-stage-` inside the same commit. Such a name is never visible after the commit.

## Revisions

```
RevisionBody { content, metadata }
content  = {type:"dir"} | {type:"symlink", target} | {type:"file", manifest, size}
metadata = { mode, owner?, mtime, btime?, time_precision_ns, records, streams, ...any other field }
owner    = { uid, gid, user?, group? }
records  = [ {ns, name, value} ]        xattrs, ACLs, Finder info, by platform namespace
streams  = [ {ns, name, manifest} ]     resource forks and alternate data streams
```

A field this version does not know is kept in place and sent back unchanged, so an older client cannot
erase what a newer one wrote.

Platform namespaces: `linux.xattr` (POSIX ACLs and capabilities are xattrs here), `macos.xattr`,
`macos.finder_info`, `macos.resource_fork`, `windows.acl`, `windows.attributes`, `windows.ads`.
Only `linux.xattr` and `macos.xattr` are produced by this release, and each platform leaves the other's untouched; a Finder info or resource fork travels as a `macos.xattr` for now. The others have their place in the record.

## Changes

```
Change { seq, op_id, device, device_name, time, ops, user? }
```

`user` is whose token made the commit, as named then; absent for what was committed before accounts.

`seq` is per namespace, starts at 1 and has no gaps. Replaying every change after `N` onto the tree as
of `N` gives exactly the tree at the head; that is how clients learn what happened, how `export` reads
the past, and what `fsck` checks the tables against. This release keeps the whole journal, so a device
can come back after any time away. A client reads it by asking again after the last entry it got until
that is the head: an answer holds as much as its limits allow, so a new device's first reading of a big
journal comes in several answers. Every client so far asks that way, so a server that answers less at a
time needs no client upgraded first.

## Waiting

`wait=S` holds the answer for up to S seconds, at most 120, until the head is no longer `after`, and then
answers as usual. That is the change feed: an ordinary request and response, so it passes through anything
that passes HTTP, and carries nothing a later `GET /changes` would not. `limit=0` asks for the head alone.
The head differing either way answers at once, since a head behind `after` means a server restored from a
backup. A commit is durable before it wakes anyone. The answer says `Cache-Control: no-store`.

A server that ignores `wait`, or a proxy that answers for it, only makes a client poll: the daemon backs off
when it gets no news without having waited. A reverse proxy's read timeout has to be longer than the wait
to be of use; the daemon waits 25 seconds, under nginx's default 60, and when something cuts its requests off
it waits for less from then on.

## Web

The web interface is a thin client of the endpoints below, which any client may use: the page and its
scripts are in the server binary, at `GET /` and `/web/…`, and the same page at `GET /s/{link}` opens a
public link. They name no version, user or folder, and answer without a token. In `cairn_protocol::web`.

**Signing in.** `POST /v1/session` with `{token}`, or `{ticket}`, answers 204 and sets the cookie `cairn`,
`HttpOnly; Secure; SameSite=Strict`, for 30 days; a wrong token or a ticket used or late is 401. The cookie is a
session, `<token hash>.<end>.<mac>`: the BLAKE3 of the token that signed in, the second it ends, and a keyed
BLAKE3 of both under the server's key, `web/session.key`. It never holds the token, it outlives a restart, and
removing the token signs out every browser it signed in. `DELETE /v1/session` clears it. A ticket comes from
`POST /v1/session/ticket` with a token, `{ticket, expires_in}`: 32 random bytes as base64url, in memory only,
taken once within 120 s. A program with a token (the tray's Open in browser, `cairn web`) opens
`<server>/#/signin/<ticket>/f/<folder>/root`; the ticket is in the fragment, which no browser sends, and the page
takes it out of the address before it signs in with it, so nobody types a token. A request
with no `Authorization` header is taken as the session's token's, and one that is not `GET` or `HEAD` must then
also carry `X-Cairn-Web: 1`, or it is answered 403: no other site's page can send that header, so nothing it
makes a browser send can change anything. Every answer says `Referrer-Policy: no-referrer`, so a link's token
never leaves in a `Referer`.

**Three views of a tree**, each a base path `B` with the same endpoints below:

| `B` | Who | Reaches | Level |
|---|---|---|---|
| `/v1/ns/{ns}` | a user given the folder | the whole namespace | the grant's |
| `/v1/shared/{id}` | the user a share names | the shared object and, for a folder, what is in it | the share's, `read` or `write` |
| `/v1/links/{link}` | anyone holding the link | the same, by a public link | `read` |

`{object}` is an `ObjectId`, or `root` for the view's own root: the namespace's root directory, or the shared
object. An object outside the view, deleted, or not there answers 404 alike. A share or link answers 404 too
once it expires, its object is deleted, or the user who made it no longer manages the folder; it works again if
the object is brought back or that user given `manage` again.

| Method and path | Body | Answer |
|---|---|---|
| `GET B/item/{object}` | | `{item, path, access}` |
| `GET B/dir/{object}` | | `{dir, path, entries, access}`: `entries` are the directory's items in name-byte order |
| `GET B/content/{object}?revision=R&download=1` | | The file's bytes, at `R` (a revision of that object) or its head; one `Range: bytes=` range is answered 206. `download=1` asks for `Content-Disposition: attachment` |
| `GET B/zip/{object}` | | A folder and everything in it as a zip, stored uncompressed, streamed. Symbolic links are left out |
| `GET B/history/{object}` | | `{revisions}`, newest first, each `{revision, seq, time, user?, device_name, kind, size?, mode, mtime}`. Not by link |
| `POST B/mkdir` | `{parent, name}` | `{object, seq}`. Needs `write` |
| `POST B/move` | `{object, from, to}` | `{seq}`: `from` and `to` are links `{parent, name}`; a rename is a move in one folder. Needs `write` |
| `POST B/delete` | `{object, link, base?}` | `{seq, objects}`: a folder goes with everything in it, deepest first, in one commit. With `base`, refused unless the object's head is still that revision. Needs `write` |
| `POST B/restore` | `{object, revision}` | `{seq, revision}`: a new revision with that one's content and metadata (invariant 7). Needs `write` |
| `POST B/uploads` | `{parent, name, size, mtime?, replace?}` | `{id}`: an upload begun. `replace` is the revision of the file at that name the client saw, which becomes the base of an update; without it the name must be free. Needs `write` |
| `PUT B/uploads/{id}?offset=N` | bytes | `{received}`. `N` must be what was received so far, or 409 answers `{received}` to go on from |
| `POST B/uploads/{id}/commit` | | 202 `{state}`: once `received` is `size`, the server stores it |
| `GET B/uploads/{id}` | | `{id, size, received, state, stored, done?, error?}`, `state` one of `receiving`, `storing`, `done` and `failed`; `done` is `{object, revision, seq}` |
| `DELETE B/uploads/{id}` | | 204: given up, and its bytes removed |

Every view of a share or link stops at its root: `path` runs from the view's root, as `path[0]`, to the item
itself, each step `{object, display}`, and no further up. An `item` is `{object, revision, kind, name?,
display, size?, mtime, mode, target?, target_display?}`: `name` is the bytes, as base64, absent for a
namespace's root; `display` the name as text, invalid UTF-8 as `\xNN`, and for a namespace's root its name.
`access` is what this caller may do in the view.

A web commit is like any device's: its device is `web`, its ID the first half of BLAKE3 of `cairn web device
<user>`, and the journal names the user. An upload is received into a file of its own under the data directory's `web/uploads/` (emptied at
startup, and of an upload left alone for 6 hours), then cut and hashed as a client reads a file without holes,
so its manifest is the one a client makes of the same bytes; its chunks are made durable before the commit
that names them, which is checked against what the client saw: a name taken since, or a file changed since,
is refused and nothing is replaced. A new file has mode 0644 and the `mtime` given, or now; a replaced one keeps
everything of its metadata but its `mtime`, owners, extended attributes and streams included (invariant 2). A
new folder has mode 0755. Errors answer as elsewhere, the reason as text: 400 a malformed request, 403 a level
too low, 404 as above, 409 a commit refused because the tree is not as the request saw it, 413 an upload part
over 64 MiB, 507 a full disk.

**Deleted.** `GET /v1/ns/{ns}/deleted?days=D` lists what is still deleted of what was deleted in the last `D`
days (30 if not given): `{items, complete}`, newest first, each `{object, kind, display, path, seq, time, user?,
device_name, size, files}`, where `path` is the folder it was in, as it is now, and a folder deleted in one
commit with what was in it is one item, `files` and `size` counting what it held. The server answers it from an
index of the whole journal kept in memory, so `complete` is true. `POST
/v1/ns/{ns}/undelete` with `{objects}` brings each back in one commit, as `cairn undelete` does: to the entry it
last had, a deleted folder above it first, what was deleted with a folder with it, and to `name (conflict
restored seq N).ext` where the name is taken. It answers `{seq, items}`, each `{object, display, path,
renamed}`. Needs `write`.

**Sharing.** A folder's managers share any object in it, the folder's root too: with a user of the server at
`read` or `write`, or as a public link, which reads only. Each share is `{id, object, kind, path?, user?,
access, by, created, expires?, link?, url?}`: `path` where the object is now, absent while it is deleted; `user`
the user it is shared with, absent for a link; `link` a link's token, which `/s/{link}` opens; `url` where it
opens, when the server was started with `--link-url`, which is then followed by `/s/{link}`: a proxy's door that
passes only `/s/`, `/web/` and `/v1/links/`, open to anyone while the server's own address is not. The token is 32
random bytes as base64url, and kept as it is, so that a manager can copy the link again: whoever reads the
database reads the files anyway.

| Method and path | Body | Answer |
|---|---|---|
| `GET /v1/ns/{ns}/shares?object=O` | | `{shares}`: every share in the folder, or those of `O`. Needs `manage` |
| `POST /v1/ns/{ns}/shares` | `{object, user?, access?, expires?}` | The share. `access` is `read` if not given, and only `read` for a link; `expires` is a time in seconds. Needs `manage` |
| `DELETE /v1/ns/{ns}/shares/{id}` | | 204. Needs `manage` |
| `GET /v1/shared` | | `{shares}`: what is shared with the caller and works now, each `{id, item, by, access, expires?}` |
| `GET /v1/links/{link}` | | `{item, expires?}`, without a token |
| `GET /v1/users` | | `{users}`: every user's name, to share with |
| `POST /v1/session/ticket` | | `{ticket, expires_in}`: signs a browser in once as the caller's token (Signing in, above) |

A user's share reaches no further than what it names: the sync protocol's endpoints answer 404 to a user who
has only shares in a folder, as they reveal the whole tree. Syncing a share as a root of its own waits for
mounting one into another root's tree (PLAN.md, "Sharing and access").

## Local API

Not on the wire to the server: what a device's daemons tell the programs that show them (`cairn status`,
the tray), in `cairn_protocol::local`. Each daemon serves it on a socket of its own, one JSON object per
line each way: a request `{"op": "..."}` in, replies `{"reply": "...", ...}` out.

| Platform | Address | Who may connect |
|---|---|---|
| Linux | `cairn-daemon-<root-id>` in the abstract namespace: the daemon's lock, so an answer means it runs | the daemon's user, by `SO_PEERCRED`; a client checks the server the same way |
| macOS | `<user dir>/<first 16 of root-id>.sock`, a path being at most 104 bytes | the socket 0600, the directory 0700, and the peer's uid |
| Windows | `\\.\pipe\cairn-daemon-<root-id>`, first instance, no remote clients | a DACL naming the daemon's user only, in any session, and owned by that user; a client (the CLI, the tray) believes a pipe its user owns, or else one whose serving process runs as its user, since a desktop session may open a daemon's pipe in session 0 but not its process. A daemon older than that owner, whose pipe Administrators own, is refused where its process cannot be opened |

The user dir is `$XDG_STATE_HOME/cairn` or `~/.local/state/cairn` on Linux, `~/Library/Application
Support/Cairn` on macOS, `%LOCALAPPDATA%\Cairn` on Windows, or `CAIRN_USER_DIR`, for tests. It holds
`roots.json`, the registry of this user's roots (`{ roots: [{ path, root_id, server, added }] }`), which
`cairn init`, once its server has answered, and a starting daemon add to under `roots.lock`, writing a new file over the old. A root
whose folder no longer holds its ID is listed as gone, not dropped, until `cairn forget` takes it off, by
its path or ID, under the same lock: it refuses a root still there unless given `--even-if-present`, and
that root's daemon lists it again when it starts. The tray's Forget, offered for a root it shows as gone, does
the same without the flag, writing the file itself under the same lock. A file that does not parse is an error, never written
over.

| Request | Replies |
|---|---|
| `status` | one `status`, the same object as `.cairn/daemon.json`, which the daemon writes after it answers with it: the file is never newer than the answer |
| `subscribe` | a `status` now, on every change, and every 30 s regardless, until the client closes. A client whose queue of 16 is full, or that takes more than 5 s over a write, is dropped, and the daemon's log says so |
| `sync_now` | `done`: an event like the watcher's, so it brings the next sync forward; an `error` while paused |
| `pause`, `resume` | `done` once kept in `.cairn/paused`, so a pause outlives a kill; while paused no sync starts, the state is `paused` and `paused_since` is set, and on resume a full sync runs at once |
| `conflicts` | `conflicts { open }`, each with its copy and the version it competes with as paths on this device when they are here. The daemon reads it from its base, the server's tree at its cursor, brought up to the head by the journal since, and for each copy still there the commit that made it, never the whole journal; still, it is asked when a person looks, not polled |
| `update_now` | `done` at once: the daemon's updater asks the feed and installs what it offers whatever the policy (Updates, below), and the status's `update` says how it went |
| `deleted { days }` | `deleted { head, items }`: what was deleted in the last `days` (30 if not given) and is still deleted, newest first, each `{ id, seq, time, device_name, path, kind, bytes, files, folder_gone }`. A folder is listed once, with `files` and `bytes` of what went with it, and what it held is not listed apart. The daemon replays the journal into an index of every object's kind, size and entries, with no bodies, keeps it for 10 minutes after the last request and takes only what is new to answer the next |
| `undelete { ids }` | `undeleted { seqs, items, already }`, then a sync: each item `{ id, path, was, renamed, objects }`. `already` names what was asked for but is not deleted now. `error` if nothing came back |
| `sign_in_link` | `sign_in_link { url }`: the server's page at this root's folder, signed in as its token once within two minutes (Web, "Signing in"), which the tray's Open in browser opens; an `error` if the server cannot say, and the tray then opens the page as it is |
| `badges { folder }` | `badges { folder, badge, entries }`: what a file manager shows on the folder, `folder` relative to the root with `/` between names (`""` the root), and on each entry of it, `[{ name, badge }]`. A badge is `synced`, `syncing`, `warning` or `error` (Badges, below); an entry this device does not sync is left out, and `badge` is `null` for a folder it does not sync or that is not there |
| `watch_badges` | `badges_changed { all, paths }` now with `all`, then whenever badges change, once they have been quiet for 250 ms or at least every second while they go on, and an empty one every 30 s regardless, until the client closes: the badges of each path, of all under it and of each folder above it may differ, or with `all` every badge. More than 4,096 paths are sent as `all`. A client whose queue of 16 is full is dropped, and its next connection begins with `all` |

**Badges.** What the daemon knows of each entry, with nothing looked up but the folder asked for. `syncing`:
the watcher has named it since the last sync that agreed with the server, or it is a file still being written,
or, on a folder, something under it is; and everything is until the daemon's first agreement after it starts,
or after the watcher lost track of the tree, as it does not know until then. `error`: the last sync skipped it
(`cairn status` says why), or, on the root, the last sync failed. `warning`: the server keeps metadata this
filesystem refused for it, or, on a folder, something under it is not synced, or, on the root, the daemon is
paused. `synced` otherwise. An error outranks syncing, which outranks a warning. Left out, with no badge: the
state directory, what the rules exclude, the paths of folders excluded by identity and of the copies they
left here, as the last sync read them. Explorer shows them through cairn-explorer's DLL (`cairn explorer on`),
which asks for a folder when it is first shown and again when told it changed.

**Deleted, and brought back.** A delete is the unlink that leaves an object with no entry. The server keeps its
last state, and `link` brings it back as itself, so `undelete` sends only `link`s: each object goes back
to the entry it last had, with what it last held, and nothing is reverted or replaced. A folder brings back
what one device deleted in it within 10 minutes before deleting it, as one sync's commits do however many
they take. A deleted folder above it comes back holding only what was asked for. A name taken since gets
the name a restore gives, `name (conflict restored seq N).ext`, beside what has it now. One commit holds
it all, so a kill leaves nothing or all of it; a restore over 100,000 operations or 48 MiB goes in several,
each leaving a valid tree with every folder before what is in it. A commit refused because something
changed is planned again from the journal once, and then reported. `cairn deleted` and `cairn undelete`
do the same without a daemon, and `cairn ask` sends any request as JSON:
`cairn ask '{"op":"undelete","ids":["..."]}'`.

A malformed request gets an `error { text }`. The status adds only fields with defaults, so a reader of
an older file keeps working: `recent` lists the last 100 changes the daemon sent (`up`) or applied
(`down`) as `added`, `changed`, `metadata` (a file's or a link's alone; a folder's goes unlisted), `moved`
with `to`, `removed` (a folder once) or `conflict`, newest first, and outlives a restart; `syncing` is true while a sync runs, on the socket only, as
the file is written after each sync, and with it `progress`, sent again every 2 s: the sync's `phase`
(`loading`, `journal`, `scanning`, `deciding`, `applying`, `uploading`, `committing`, `recording`; a reader
shows one it does not know as `other`), its `round`, when it began and came to that phase, the entries its
scan has looked at of about how many, the files and bytes it has read to hash them and the file being read,
the phase's own `done` of `total`, the bytes downloaded and uploaded of how many, and `moved`, when any of
it last went up. `cairn status` and the tray say it in words, and the daemon's log every 5 minutes once a
sync has run for one, and once when nothing in it has moved for 10 minutes. A daemon from before this API holds the Linux socket
without answering, so `cairn status` waits 2 s for an answer and then reads its lock and file. `update` is what
this user's updater on this device knows, the same in every daemon's status, and `version` the daemon's own.

## Updates

In `cairn_protocol::update`. A release is published with `update.json`, which a feed URL names: a site's folder,
for this project's `https://cairn.storage/download/update.json`, or a GitHub release's latest download,
`https://github.com/<owner>/<repo>/releases/latest/download/update.json`, where this project mirrors it:

```json
{ "release": "<the release, as JSON text>",
  "signatures": [{ "key": "<Ed25519 public key, base64>", "signature": "<base64>" }] }
```

Each signature is over the bytes `cairn release v1\n` followed by `release` as it stands, so no parse and print
can change what is checked, and a signature made with the key for anything else is never one of a release. The
release, `{ version, protocol, published, notes, files: [{ program, platform, form, url, size, sha256 }] }`, names
each program (`cairn`, `cairn-tray`) for each platform (`linux-x86_64`, `linux-aarch64`, `windows-x86_64`, `macos-aarch64`,
`macos-x86_64` or `macos-universal`, which serves either Mac) as `binary`, the executable itself, or `app`, a
gzipped tar holding one macOS bundle; a `url` that is not absolute is beside `update.json`, wherever that was
fetched from, so one signed file serves from every feed that has the files beside it, as this project's releases
do. A version is numbers with dots and perhaps a `-` tag, which comes before the same numbers without one.

A build trusts the keys it was built with (`CAIRN_UPDATE_KEYS`) and asks the feeds it was built with
(`CAIRN_UPDATE_FEED`, comma-separated), in order: the first that answers with a release a trusted key signed is
the one used, its files downloaded beside it, and what each one before it said instead is kept and logged, so a
fallback never hides a feed that is down; when none answers, the error says what each did. This project's builds
ask cairn.storage, then GitHub. `update-settings.json` in the user dir can name other feeds and their keys, and
holds the `policy`: `auto` (the default), `notify` or `off`. A file that does not parse is an error, never written over. One
updater runs at a time, under `update.lock` in the user dir: a daemon's, about once an hour, give or take a
tenth, and a person's `cairn update now`, which waits for another. It installs a release only if one of the trusted
keys signed it, its version is above what is installed, it was not taken back on this device (unless a person
asks), and its `protocol` is this build's or the one every server of this user's roots here answers `/info` with
(unless `--any-protocol`). Then, for each program installed here, `cairn` and the tray beside it (in the same
folder, or `Cairn Tray.app` beside the bundle `cairn` is in on macOS):

1. download into `.cairn-update` beside it, checking the size and SHA-256 the release gives;
2. for a bundle, unpack it, and on macOS check its signature, which must be by the team of the installed bundle
   when that one has a team;
3. run the new program with `--version`, which must say the release's version;
4. keep the installed one as `<program>-previous` in `.cairn-update`, and put the new one in its place by
   renaming: on Linux and macOS one rename over it, or a bundle swapped whole (`RENAME_SWAP`); on Windows the
   running program renamed aside first, then the new one renamed into place, as a running program there can be
   renamed but not replaced: a kill between the two leaves no program at the path and the old one whole as the
   previous, which whatever runs the daemon can put back.

`update-state.json` in the user dir holds what it found (`checked`, `latest`, the feed that `answered` and what
those before it said, `unanswered`, `available` and why it `waits`,
`installed` and when, the last `error` and since when, the versions `rejected` here), which is the status's
`update`, and the version on probation. A daemon whose program has been replaced, by an update or a person,
restarts into the new one between syncs: on Unix in the same process, by exec, and on Windows by exiting with 75
for the loop that runs it to start it again. A daemon that starts more than three times for one root on a version
just installed without getting through a sync, or running an hour, takes the version back as `cairn update
rollback` does: the previous program put back, the version added to `rejected`. The tray, too, restarts once its
program has been replaced, but never by exec on macOS, whose menu bar gives a program exec'd in another's process
no icon: under its LaunchAgent it exits with 75 for launchd to start it again, and otherwise starts a new process
and exits. A tray that an older one exec'd there (one before 0.1.9) does the same as it starts.
