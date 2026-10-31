# Cairn on-disk formats, version 1

## Server data directory

```
data/
  token                  access token, 0600
  lock                   flock held while a server has the directory
  meta.sqlite            metadata: WAL, synchronous=FULL
  ns/<hex of namespace name>/
    chunks/ab/abcdef...  one file per chunk or manifest, named by its full hash
    incoming/            uploads in progress; emptied at startup
```

**Backup:** snapshot `meta.sqlite` (the SQLite backup API, or stop the server), then rsync `ns/`.
Chunks are immutable and named by content, and this release never removes one. After a restore, each
device notices that the server no longer has what it last saw and pauses. Removing that root's `.cairn`
and running `cairn init` again merges by name and content and sends the server what it lost.

**Chunks** are the raw bytes, with no compression in this format version. A chunk reaches its final
path only by: write to `incoming/`, fsync, rename, fsync the directory. Reading verifies the hash, so
bit rot is an error rather than data. Each namespace has its own set: dedup never crosses a namespace,
so "which chunks are missing" reveals nothing about another namespace.

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
| `revisions` | Every revision of every object, body as JSON |
| `journal` | Sequence, operation ID, device, time, operations as JSON; the source of truth |
| `conflicts` | The inbox: which copy holds whose competing version |

`cairn-server fsck` reads every chunk, follows every manifest reference, and checks `entries` and
`objects` against a replay of `journal`. Chunks nothing refers to are reported and are not damage: they
are what an interrupted upload leaves behind.

## Client state

`.cairn/` at the top of a sync root, 0700. It is the one name a root cannot sync, and only directly in
the root.

```
.cairn/
  root-id        random ID, also in state.sqlite: the marker that proves this is the root
  config.json    server, token, device, apply policy; 0600
  lock           flock held while a sync runs
  state.sqlite   WAL, synchronous=FULL
  stage/         entries lifted out for a moment, for swaps and cycles
  chunks/        downloads in progress, by hash; resumes after an interruption
```

| Table | Holds |
|---|---|
| `kv` | Root ID, journal cursor, and the operation at the cursor, which shows whether the server still has this history |
| `base` | The server's tree as of the cursor: the common ancestor for merging |
| `rows` | Per object: the fingerprint and content last read; `applied`, what the filesystem held when the base last matched, from which local changes are measured; `synced`, state shared ahead of the base, naming the commit that carries it: it counts as the common ancestor only once that commit is seen in the journal |
| `manifests` | Manifests of local content, needed to upload it |
| `outbox` | Commits not yet acknowledged, sent again with the same ID on the next run |
| `intents` | Marks made before a step that a crash could interrupt: a conflict copy's identity, a staged entry's origin, a directory's real mode, new content on its way to every hard link of a file |

`rows.applied` next to `base` is what keeps stored and applied metadata separate. A local change is
"the filesystem differs from `applied`", never "the filesystem differs from what is stored", so a field
this client cannot hold is not a change.

Temporary files are `.cairn-tmp-<device id>-<random>` in the destination's own directory. A scan removes
its own device's, which can only be left by a killed run. They never hold anything at a synced path.
