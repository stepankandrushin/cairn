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
Only `linux.xattr` is produced by this release; the others have their place in the record.

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
