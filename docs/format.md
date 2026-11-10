# Cairn on-disk formats, version 1

## Server data directory

```
data/
  token                  the first user's token, 0600: made by `init`, or one from before accounts
  lock                   flock held while a server has the directory
  meta.sqlite            metadata: WAL, synchronous=FULL
  ns/<hex of namespace name>/
    chunks/ab/abcdef...  one file per chunk or manifest, named by its full hash
    incoming/            uploads in progress; emptied at startup
  web/uploads/<id>       a web upload's bytes as they arrive, until stored; emptied at startup
  web/session.key        what browsers' sessions are signed with, 0600, made at the first start: removed, a
                         new one is made at the next, and every browser signs in again
```

Nothing is renamed or linked between `ns/` and the rest, so `ns/` may be a mount of another filesystem:
the chunks on large, slow disks and the database on a fast one.

**Backup:** snapshot `meta.sqlite` (the SQLite backup API, or stop the server), then rsync `ns/`.
Chunks are immutable and named by content, and this release never removes one. After a restore, each
device notices that the server no longer has what it last saw and pauses. Removing that root's `.cairn`
and running `cairn init` again merges by name and content and sends the server what it lost.

**Chunks** are the raw bytes, with no compression in this format version. A chunk reaches its final
path only by: write to `incoming/`, fsync, rename, fsync the directory. An upload request's chunks go
together, so that their fsyncs overlap: every one written and synced, then each renamed, then each
directory synced once, and only then the answer. Reading verifies the hash, so bit rot is an error
rather than data. Each namespace has its own set: dedup never crosses a namespace, so "which chunks are
missing" reveals nothing about another namespace.

**Manifests** are stored like chunks. The hash of a file's content is the hash of its manifest's
canonical encoding: compact JSON with fields in the order below.

```json
{"v":1,"size":1099511627776,"segments":[{"offset":1048576,"chunks":[{"hash":"…","len":4096}]}]}
```

`segments` are the data runs, in order and not overlapping; bytes between them and after the last are a
hole. Chunks are cut within each run by FastCDC (`v2020`, level 1) with minimum 512 KiB, average 2 MiB,
maximum 8 MiB. A run of at most the minimum is one chunk. Two clients reading the same content produce
the same bytes, so they produce the same hash.

**Metadata** is in `meta.sqlite`, schema in `crates/store/src/sqlite.rs` with its version in
`PRAGMA user_version`. Migrations are embedded and run at startup; a database newer than the binary is
refused.

| Table | Holds |
|---|---|
| `namespaces` | Name and journal head |
| `objects` | Identity, kind, head revision |
| `entries` | Parent object + name bytes → object |
| `revisions` | Every revision of every object: its object, base, the commit that made it, and its body, a binary record, kept here alone |
| `journal` | Sequence, operation ID, device, time, user, the operations as a binary record, and their length as JSON; the source of truth, with the bodies its creates and updates name |
| `conflicts` | The inbox: which copy holds whose competing version |
| `users` | Name and when made |
| `tokens` | BLAKE3 of each token, never the token, with its user, label and when made |
| `grants` | Namespace, user, and its level there: `read`, `write` or `manage` |
| `shares` | An object of a namespace shared with a user at `read` or `write`, or by a public link's token, which reads only; who made it, when, and when it expires |

The `journal` also says, from schema 2 on, the user whose token made each commit. Schema 2 brought accounts:
the migration to it, in its one transaction, makes the `token` file's token, if there is one, the user
`admin`'s, managing every namespace there is, so that a server upgraded in place keeps every device it had.
After that the server never reads the file. An administrator's command (`cairn-server user`, `token`,
`folder`, `grant`, `revoke`) takes the data directory's lock when it is free; when a server holds it, the
command opens the database beside it, only at this binary's schema, and writes only accounts and namespaces,
each in a transaction the server's own wait for.

Schema 3 added `shares` alone, so a binary before it refuses the database only for its number: dropping
the table and setting `user_version` back to 2 gives that binary the store as it was. A share or link goes
with the user who made it and with the user it names. A link's token is kept as it is, not hashed as a
user's tokens are, so that a manager can copy the link again: it reads only what it names, which whoever
reads the database can read anyway.

Schema 4 keeps each revision's body once. Up to schema 3 a create's or update's body was JSON twice, in
the commit's operations and in `revisions`; from 4 on it is a binary record in `revisions` alone, and the
operation in `journal` names its revision. Reading a commit puts each body back from there, where the
revision must be that object's and made by that commit, or the read is an error, never another body; so
what the server sends, the journal, a folder, a history, is the JSON it always was, byte for byte, and only
the storage changed. A real store of 2.9 million operations and 950,000 revisions went from 1.3 GB to 361 MB.

The records are the client state's encoding (the Records paragraph below, in `cairn_protocol::record`). A
body is a format byte (1), the content and the metadata, unknown metadata fields kept as the JSON they came
in. A commit's operations are a format byte (1), their count, and each: a byte whose low three bits are the
kind (create, link, unlink, move, update, note), bit 3 that its one optional field is there (a create's
`conflict_of`, an unlink's `base`, an update's `restored_from`) and bit 4 that its body follows here; then
the object and the rest in the order of `crates/protocol/src/ops.rs`, each link a parent and a name. A
note's text is written out the first time in a commit and is the number of that first time after it, as a
join gives each of a hundred thousand files the same few texts. `json_len` is what a page of the journal
is measured by (docs/protocol.md, `/changes`), so pages end where they always did.

A binary of schema 4 moves a database of schema 3 when it opens it, before it serves, in one transaction:
each table read in key order and written anew, every body and commit decoded again and checked against
what it was read from, a body `revisions` does not hold for that commit kept in the commit itself (bit 4),
then the old tables dropped. It first checks that the folder has room for the new tables, at most what the
whole database holds, in the WAL and again in the file; short of it, or should the disk fill during it, the
database stays at schema 3, the server does not start, and it says so. A kill at any point leaves schema 3
or schema 4, each whole. The pages the old tables held are free after it: at each start, more than half of
the file free and at least 16 MiB of it, the server rewrites it (VACUUM, as any transaction), given room in
its folder and SQLite's temporary folder; short of room, or failing, it goes on as it is and tries at the
next start. A binary before schema 4 refuses it ("newer than this binary"); `cairn-server downgrade --data
<dir>`, with the server stopped, moves it back to schema 3 in one transaction, each body written out again
in its commit and in `revisions` as such a binary wrote them: moved to 4 and back, a real store's every row
was the same, byte for byte.

`cairn-server fsck` reads every chunk, follows every manifest reference, checks `entries` and `objects`
against a replay of `journal`, and that each revision is named by the commit that made it, but the first
of a namespace's root, made with the namespace. Chunks nothing refers to are reported and are not damage:
they are what an interrupted upload leaves behind.

## Client state

`.cairn/` at the top of a sync root, 0700. It is the one name a root cannot sync, and only directly in
the root.

```
.cairn/
  root-id        random ID, also in state.sqlite: the marker that proves this is the root
  config.json    server, token, device, apply policy; 0600
  ignore         this device's rules, optional: never synced, and written only by a person
  lock           flock held while a sync runs
  daemon.json    what a running daemon last reported; for reading only, and never needed
  full-checked   when a sync last ended with every decision the full sync's, in seconds since the
                 epoch: the daemon's check on the incremental path is due a day after; never needed
  state.sqlite   WAL, synchronous=FULL; its layout in `PRAGMA user_version`, now 2; rewritten (VACUUM)
                 after a sync that leaves over half of it free pages
  stage/         entries lifted out for a moment, for swaps and cycles
  chunks/        downloads in progress, by hash, in slices of about 1 GiB, each chunk gone once the
                 files that take it are written; resumes after an interruption
```

| Table | Holds |
|---|---|
| `kv` | Root ID, journal cursor, the operation at the cursor, which shows whether the server still has this history, and what `cairn exclude` left out: the objects, and the copies left here, by inode so that they stay left alone wherever they move, and for one a sync kept out of what this device held, that object, whose record does not bring it back into view; and the entries on the server this filesystem cannot hold, each with why and whether a twin here is in its way. `generation`, which triggers in the file move on every write to any table, whatever build makes it, and `steady`, what the last sync that had nothing to do and wrote nothing read: that generation, the cursor and its operation, a hash of the build, configuration and rules, a digest of its scan, and what it reported |
| `objects` | Per object, its base node and its row, each a binary record or NULL, never both NULL: the node is the server's tree as of the cursor, the common ancestor for merging; the row is what this device knows of it: the entries this device holds, without which an entry missing from disk is not a delete here; the fingerprint and content last read, or last written, or whose metadata alone was set, by a run whose watch on the file saw nothing done to it until a later write would have shown; `applied`, what the filesystem held when the base last matched, from which local changes are measured; `synced`, state shared ahead of the base, naming the commit that carries it: it counts as the common ancestor only once that commit is seen in the journal, whether the filesystem is still to be brought to it, and the time a killed run moved a folder's own timestamp from |
| `manifests` | Manifests of local content, needed to upload it: `1` and the fields of its canonical JSON (size, then each segment's offset and chunks, each chunk's hash and length), which is made again when it is read and checked against the hash it is stored under; or `0` and the bytes as they came, for any that are not a canonical manifest |
| `outbox` | Commits not yet acknowledged, sent again with the same ID on the next run |
| `intents` | Marks made before a step that a crash could interrupt: a conflict copy's identity and the names it moves to, a kept edit's identity as a conflict copy, the losing side of a same-field metadata conflict until the commit that logs it is queued, a staged entry's origin, a directory's real mode, new content on its way to every hard link of a file, and what the server had for each object a run was about to change, with where the plan puts it here, the policy it applied by, and whether it is a folder only something inside changes or one the run makes, private until its last pass, which the next scan compares with the disk to tell what arrived from what this device did |
| `ledger` | Per live object of the base, the last agreement a sync can take what changed alone against: the kind and entries, and what the scan that agreed found, the device, inode and fingerprint, the content (a file's manifest and size, a link's target), and for anything but a file a BLAKE3 digest of the rest it read; no scan part for an entry out of view here. A compact binary record, its first byte the format |
| `dirty` | Every object whose base node or row has been written since the ledger last spoke for it, put there by triggers on `objects` |
| `ledger_anchor` | What the ledger stands on: its format, a hash of the build, configuration, rules and selection it was written under, and the entries on the server out of view then. Without it, or with any of that changed, a sync takes the whole tree and writes the ledger anew |

**Records.** Integers are LEB128, signed ones zigzagged first; IDs and hashes are their raw bytes; names, targets
and other bytes are a length and the bytes. A timestamp is its seconds and nanoseconds. A node is its format
byte (1), revision, content (a tag, then a file's manifest hash and size or a link's target), metadata and
entries (a count, then each parent ID and name). Metadata is a byte of flags for what is present, the mode, the
owner (uid, gid, user and group names), mtime, birth time, the timestamp precision when it is not 1 ns, the
records and named streams, each record's namespace a byte for one this build knows or the name written out, and
any fields of a newer build as the JSON they came in. A row is its format byte (1), a varint of flags, and only
what is not already in the node beside it: its content, `scan` and `applied` unless they are the node's (or, for
`applied`, the same as `scan`), the fingerprint's device and inode, its size, mtime, ctime and birth time unless
they are the content's size, `scan`'s times and its own mtime, `synced` whole, and the entries it holds unless
they are the node's. So a row says nothing about the node but where it agrees with it, and is written again
whenever the node beside it changes; a hand edit of a node must write its row again too. On a real tree's 568,000
objects a node takes about 130 bytes and a row about 30.

**Layouts.** Version 1, up to 0.1.8, kept the node in `base` and the row in `rows`, each as JSON text, and
manifests as their canonical JSON. A build of version 2 moves such a state when it opens it, before a sync, in
one transaction: both tables read in key order and merged, each object written once, manifests the same, the old
tables dropped, the ledger with them, rebuilt by the next sync, and the generation moved. It first checks that
the state's filesystem has room for the new tables in the WAL, and for the file to grow by what its free pages
do not hold; short of it, or should the disk fill during it, the state stays version 1 and no sync runs until
there is room. A kill at any point leaves version 1 or version 2, each whole. The pages the old tables held are
free after it, which the compaction after the next sync gives back. A build older than the layout refuses the
state, saying so ("newer than this binary: upgrade cairn"), so an update taken back past this one needs the
state made again: remove `.cairn` and run `cairn init`, which merges by name and content.

The triggers are not a migration: an older build of the same layout opens the file as before, and its writes
move the generation too. A file found without all of them, or with one changed, gets them all again and loses
its `steady` record, in one transaction. The same for the ledger: an older build's writes mark what they touch, and a file found without
the `dirty_*` triggers, or with one changed, gets them again and loses its ledger.

A row's `applied` next to its node is what keeps stored and applied metadata separate. A local change is
"the filesystem differs from `applied`", never "the filesystem differs from what is stored", so a field
this client cannot hold is not a change.

`ignore` is `.gitignore` syntax relative to the root, UTF-8, with git's meaning, checked against
`git check-ignore`; a rule can match names that are not UTF-8. POSIX classes such as `[:digit:]` are refused, since
the matcher would quietly match nothing with them. In the few other odd patterns where it differs from git (`[a`,
`a[/]b`, `doc\/`) it leaves out more. Anything wrong with the file stops the sync, since "no rules" would upload
what the rules hold back. A symlink or a FIFO there counts as wrong. Save it in place or by rename: while it does
not exist at all, there are no rules.

A daemon holds no file in the root while idle, so a disk can be unmounted under it: its single-instance lock is
the abstract socket `cairn-daemon-<root id>`.

Temporary files are `.cairn-tmp-<device id>-<random>` in the destination's own directory. A scan removes
its own device's, which can only be left by a killed run, and holds nothing at a synced path. What is moved out of
the way of a replacement or a removal is `.cairn-tmp-<device id>-aside-<random>-<the name, as far as it fits>`,
which a half-written file never is. If a killed run leaves one that holds neither what was on record for its inode
nor what was on its way, and no other name has it, someone edited or saved in the moment before: it stays, under a
conflict name of that name, reported, and goes up as a conflict copy of the file it was caught by, so the conflict
inbox lists it. One in a folder a rule made since the kill leaves out is looked for there, in the folders of what
was on its way. After a killed run, the filesystem is asked what it makes of a record on a scratch entry made for
a moment beside the file, under that temporary prefix, or as `.cairn/arriving-probe-<random>` in a folder this
device cannot write to and for a dry run; a run killed in that moment leaves one, which the next removes.
