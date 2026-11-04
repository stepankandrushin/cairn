# Cairn protocol, version 1

HTTP with JSON bodies, except chunk batches. Every path below is under `/v1/ns/{namespace}` and needs
`Authorization: Bearer <token>`. The types are defined once, in `crates/protocol`; this page explains
them. `Info.protocol` is the version: a client refuses a server that speaks another one.

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
| `GET /info` | | `{protocol, server_version, head, max_batch_bytes}` |
| `POST /chunks/missing` | `{hashes}` | `{hashes}`: those not stored intact in this namespace. A damaged chunk counts as missing, so a good copy replaces it |
| `POST /chunks/upload` | chunk batch | 204: all durable. 400: damaged batch |
| `POST /chunks/download` | `{hashes}` | chunk batch, in the order asked. 404: one is missing |
| `POST /commit` | `CommitRequest` | `CommitResponse` |
| `GET /changes?after=N&limit=M&wait=S` | | `{changes, head}`: journal entries after sequence N. With `wait`, see [Waiting](#waiting) |
| `GET /conflicts` | | The conflict inbox |

401 means a missing or wrong token, 404 an unknown namespace, and 507 a full disk on the server, in
which case nothing changed.

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
| `note` | `object, text` | The object does not exist. Otherwise it has no effect; it is the activity log's entry for the loser of a metadata conflict |

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
Change { seq, op_id, device, device_name, time, ops }
```

`seq` is per namespace, starts at 1 and has no gaps. Replaying every change after `N` onto the tree as
of `N` gives exactly the tree at the head; that is how clients learn what happened, how `export` reads
the past, and what `fsck` checks the tables against. This release keeps the whole journal, so a device
can come back after any time away.

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

## Local API

Not on the wire to the server: what a device's daemons tell the programs that show them (`cairn status`,
the tray), in `cairn_protocol::local`. Each daemon serves it on a socket of its own, one JSON object per
line each way: a request `{"op": "..."}` in, replies `{"reply": "...", ...}` out.

| Platform | Address | Who may connect |
|---|---|---|
| Linux | `cairn-daemon-<root-id>` in the abstract namespace: the daemon's lock, so an answer means it runs | the daemon's user, by `SO_PEERCRED`; a client checks the server the same way |
| macOS | `<user dir>/<first 16 of root-id>.sock`, a path being at most 104 bytes | the socket 0600, the directory 0700, and the peer's uid |
| Windows | `\\.\pipe\cairn-daemon-<root-id>`, first instance, no remote clients | a DACL naming the daemon's user only, in any session; owned by that user, which the tray checks, or else the user of the process serving it, as a desktop session may not open a daemon's in session 0 |

The user dir is `$XDG_STATE_HOME/cairn` or `~/.local/state/cairn` on Linux, `~/Library/Application
Support/Cairn` on macOS, `%LOCALAPPDATA%\Cairn` on Windows, or `CAIRN_USER_DIR`, for tests. It holds
`roots.json`, the registry of this user's roots (`{ roots: [{ path, root_id, server, added }] }`), which
`cairn init` and a starting daemon add to under `roots.lock`, writing a new file over the old. A root
whose folder no longer holds its ID is listed as gone, not dropped, until `cairn forget` takes it off, by
its path or ID, under the same lock: it refuses a root still there unless given `--even-if-present`, and
that root's daemon lists it again when it starts. A file that does not parse is an error, never written
over.

| Request | Replies |
|---|---|
| `status` | one `status`, the same object as `.cairn/daemon.json` |
| `subscribe` | a `status` now, on every change, and every 30 s regardless, until the client closes. A client whose queue of 16 is full, or that takes more than 5 s over a write, is dropped, and the daemon's log says so |
| `sync_now` | `done`: an event like the watcher's, so it brings the next sync forward; an `error` while paused |
| `pause`, `resume` | `done` once kept in `.cairn/paused`, so a pause outlives a kill; while paused no sync starts, the state is `paused` and `paused_since` is set, and on resume a full sync runs at once |
| `conflicts` | `conflicts { open }`, each with its copy and the version it competes with as paths on this device when they are here. It replays the journal, so it is asked when a person looks, not polled |

A malformed request gets an `error { text }`. The status adds only fields with defaults, so a reader of
an older file keeps working: `recent` lists the last 100 changes the daemon sent (`up`) or applied
(`down`) as `added`, `changed`, `metadata` (a file's or a link's alone; a folder's goes unlisted), `moved`
with `to`, `removed` (a folder once) or `conflict`, newest first, and outlives a restart; `syncing` is true while a sync runs, on the socket only, as
the file is written after each sync. A daemon from before this API holds the Linux socket
without answering, so `cairn status` waits 2 s for an answer and then reads its lock and file.
