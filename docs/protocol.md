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
| `POST /chunks/upload` | chunk batch | 204: all durable, made so together. 400: damaged batch, of which nothing is kept |
| `POST /chunks/download` | `{hashes}` | chunk batch, in the order asked. 404: one is missing |
| `POST /commit` | `CommitRequest` | `CommitResponse` |
| `GET /changes?after=N&limit=M&wait=S&ops=false` | | `{changes, head}`: journal entries after sequence N, in order: at most M (1000 if not given, at most 10,000), and fewer once about 16 MiB of operations are in the answer, but always one if there is one. With `wait`, see [Waiting](#waiting). With `ops=false`, each without its operations: where a client stands, for the price of one entry however large its commit |
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
| `status` | one `status`, the same object as `.cairn/daemon.json` |
| `subscribe` | a `status` now, on every change, and every 30 s regardless, until the client closes. A client whose queue of 16 is full, or that takes more than 5 s over a write, is dropped, and the daemon's log says so |
| `sync_now` | `done`: an event like the watcher's, so it brings the next sync forward; an `error` while paused |
| `pause`, `resume` | `done` once kept in `.cairn/paused`, so a pause outlives a kill; while paused no sync starts, the state is `paused` and `paused_since` is set, and on resume a full sync runs at once |
| `conflicts` | `conflicts { open }`, each with its copy and the version it competes with as paths on this device when they are here. It replays the journal, so it is asked when a person looks, not polled |
| `update_now` | `done` at once: the daemon's updater asks the feed and installs what it offers whatever the policy (Updates, below), and the status's `update` says how it went |
| `deleted { days }` | `deleted { head, items }`: what was deleted in the last `days` (30 if not given) and is still deleted, newest first, each `{ id, seq, time, device_name, path, kind, bytes, files, folder_gone }`. A folder is listed once, with `files` and `bytes` of what went with it, and what it held is not listed apart. The daemon replays the journal into an index of every object's kind, size and entries, with no bodies, keeps it for 10 minutes after the last request and takes only what is new to answer the next |
| `undelete { ids }` | `undeleted { seqs, items, already }`, then a sync: each item `{ id, path, was, renamed, objects }`. `already` names what was asked for but is not deleted now. `error` if nothing came back |

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

In `cairn_protocol::update`. A release is published with `update.json`, which a feed URL names, for a GitHub
release `https://github.com/<owner>/<repo>/releases/latest/download/update.json`:

```json
{ "release": "<the release, as JSON text>",
  "signatures": [{ "key": "<Ed25519 public key, base64>", "signature": "<base64>" }] }
```

Each signature is over the bytes `cairn release v1\n` followed by `release` as it stands, so no parse and print
can change what is checked, and a signature made with the key for anything else is never one of a release. The
release, `{ version, protocol, published, notes, files: [{ program, platform, form, url, size, sha256 }] }`, names
each program (`cairn`, `cairn-tray`) for each platform (`linux-x86_64`, `windows-x86_64`, `macos-aarch64`,
`macos-x86_64` or `macos-universal`, which serves either Mac) as `binary`, the executable itself, or `app`, a
gzipped tar holding one macOS bundle; a `url` that is not absolute is beside `update.json`. A version is numbers
with dots and perhaps a `-` tag, which comes before the same numbers without one.

A build trusts the keys it was built with (`CAIRN_UPDATE_KEYS`) and asks the feed it was built with
(`CAIRN_UPDATE_FEED`); `update-settings.json` in the user dir can name another feed and its keys, and holds the
`policy`: `auto` (the default), `notify` or `off`. A file that does not parse is an error, never written over. One
updater runs at a time, under `update.lock` in the user dir: a daemon's, about once an hour, give or take a
tenth, and a person's `cairn update now`, which waits for another. It installs a release only if one of the trusted
keys signed it, its version is above what is installed, it was not taken back on this device (unless a person
asks), and its `protocol` is this build's or the one every server of this user's roots here answers `/info` with
(unless `--any-protocol`). Then, for each program installed here, `cairn` and the tray beside it (in the same
folder, or `Cairn.app` beside the bundle `cairn` is in on macOS):

1. download into `.cairn-update` beside it, checking the size and SHA-256 the release gives;
2. for a bundle, unpack it, and on macOS check its signature, which must be by the team of the installed bundle
   when that one has a team;
3. run the new program with `--version`, which must say the release's version;
4. keep the installed one as `<program>-previous` in `.cairn-update`, and put the new one in its place by
   renaming: on Linux and macOS one rename over it, or a bundle swapped whole (`RENAME_SWAP`); on Windows the
   running program renamed aside first, then the new one renamed into place, as a running program there can be
   renamed but not replaced: a kill between the two leaves no program at the path and the old one whole as the
   previous, which whatever runs the daemon can put back.

`update-state.json` in the user dir holds what it found (`checked`, `latest`, `available` and why it `waits`,
`installed` and when, the last `error` and since when, the versions `rejected` here), which is the status's
`update`, and the version on probation. A daemon whose program has been replaced, by an update or a person,
restarts into the new one between syncs: on Unix in the same process, by exec, and on Windows by exiting with 75
for the loop that runs it to start it again. A daemon that starts more than three times for one root on a version
just installed without getting through a sync, or running an hour, takes the version back as `cairn update
rollback` does: the previous program put back, the version added to `rejected`. The tray, too, restarts once its
program has been replaced.
