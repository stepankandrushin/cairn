# Cairn: plan

A file sync engine with a versioned storage server and a small web interface. Closer to how Dropbox
and Seafile work than to Nextcloud. Most of the engineering goes into preserving files correctly
across operating systems; the language helps, but lossless metadata, durable revisions and
deterministic recovery are what make it better.

**The promise:** preserve everything centrally, restore everything where the destination supports it,
and clearly report whatever it cannot represent. "Every platform behaves the same" is not achievable:
Windows, Linux and macOS have different filesystem semantics.

## What it fixes, what it keeps

| Nextcloud's pain | Cairn's answer |
|---|---|
| PHP per request, slow with many small files | Long-running Rust server; small files batched |
| `oc_filecache` drifts from disk, `occ files:scan` | The journal and manifests *are* the state; nothing to rescan |
| WebDAV as the sync protocol: chatty, polled | Own protocol: batches and a persistent change feed |
| A 2 GB file with one byte changed is re-uploaded | Content-defined chunks; only missing chunks move |
| Versions are full copies | Versions share chunks: 100 small edits cost 100 small diffs |
| Symlinks skipped; modes, owners and xattrs lost | First-class metadata, round-tripped by every client |
| A built-in list of names is never synced | No built-in list of names: a project tree syncs whole |
| Conflicted copies that surprise | Three-way merge, explicit conflict inbox |
| Fragile upgrades, app bloat | One binary, embedded migrations, no plugin system |

Kept from Nextcloud: self-hosting, a server as the source of truth, version history and trash, users
and groups, shared folders, expiring public links, selective sync, on-demand files, previews, a web
UI, clients on every platform, file-manager integration.

Not in scope: calendar, contacts, office, chat, an app store. Extensions use the API, or a webhook or
event stream.

## Invariants

These outrank features and speed; each has failure tests.

1. A commit acknowledged by the server is durable, and its chunks were durable first.
2. A device never erases what it cannot represent: **stored metadata and locally applied metadata are
   separate**. Linux uploads `script.sh` at 0755, Windows edits it, the new revision keeps 0755, Linux
   receives the edit at 0755.
3. Arrival order never decides which content survives: competing edits both remain.
4. A crash never leaves a half-written file at a synced path, and never loses a queued operation.
5. An empty folder is not a mass delete until proven so: a missing mount pauses sync.
6. Sync never traverses a symlink, and path operations cannot be redirected through one.
7. Every restore creates a new revision, so a restore can be undone too.
8. Anything skipped is shown with its reason and a recovery action, not dropped silently.

## Stack

| Layer | Choice | Why |
|---|---|---|
| Sync engine, server | Rust; Tokio + Axum on the server | One data model and engine everywhere; the hard part is client filesystem work: races, partial writes, platform APIs, crash recovery |
| Server metadata | SQLite by default, PostgreSQL for multi-user scale | One binary for a home user, real concurrency for an organisation; both behind one narrow store trait and run through the same test suite |
| Client state | SQLite (WAL) per sync root | Holds the base state for three-way merge, the durable queue, resumable transfers |
| Content store | Immutable content-addressed chunks on local disk, S3-compatible optional | Trivial to back up and replicate |
| Chunking | FastCDC (average ~1-4 MB), BLAKE3, optional Zstd | Inserting bytes mid-file changes only nearby chunks |
| Transport | HTTPS: batched idempotent operations plus a persistent change feed | Passes through any reverse proxy and still gives instant push; QUIC only if measurements later ask for it |
| Linux watcher | inotify as the baseline, fanotify with `FAN_REPORT_FID` when privileged | The daemon runs without root, and fanotify's mount and filesystem marks need `CAP_SYS_ADMIN` |
| Other watchers | FSEvents on macOS, ReadDirectoryChangesW on Windows | All plus periodic full reconcile: events can be lost, the scan is the safety net |
| Web UI | TypeScript + Svelte, static build embedded in the binary | Thin client of the same API; stays out of the server's hot path |
| Desktop UI | Tauri tray and settings around the Rust daemon | Small; native integration only where needed |
| Mobile | Swift/SwiftUI and Kotlin, Rust core through uniffi | Native file-provider and background-task integration |
| Deployment | One binary + storage (+ PostgreSQL at scale) | No fleet of supporting services |

## Data model

A file has an identity independent of its path:

| Record | Holds |
|---|---|
| Namespace | A user's home or a shared folder: its own journal, permissions, chunk scope |
| Object | Stable ID; file, directory or symlink |
| Directory entry | Parent object + name (bytes) → object. Two entries to one file object are a hard link; a directory has exactly one |
| Revision | Object, predecessor revisions, manifest (ordered chunk hashes) or link target, metadata record |
| Journal entry | Namespace, monotonic sequence, device, operation ID, affected objects |

What follows from it:

- Renaming a 100 GB directory changes one directory entry. A move keeps history and share links.
- `chmod +x` is a metadata revision, with no content uploaded.
- A symlink stores its target verbatim, broken ones included.
- Clients hold a cursor and ask for everything after sequence N: resumable, cheap, exact.
- History is old revisions, and a sequence number is a point in time. Delete and trash are journal
  entries; restoring points back at an old manifest.
- Local inode and file IDs help detect moves but are only hints: they get reused, and editors save by
  replacing the file.

## Metadata

| Item | Behaviour |
|---|---|
| Symlinks | Target verbatim, never followed. Links leaving the root sync as links, optionally flagged. Without platform support: a placeholder, with the real data on the server |
| Hard links | Relationship preserved inside a sync root where supported |
| Mode | All 12 bits stored; setuid, setgid and Linux capabilities applied only by explicit policy |
| Owner, group | Numbers and names stored, with a per-client mapping, since numeric IDs differ between machines. The daemon runs unprivileged: exact restore needs a narrowly scoped helper or an explicit restore command |
| mtime, btime | Nanoseconds, with the recorded precision kept |
| xattrs, POSIX ACLs, Windows ACLs | Stored in per-platform namespaces; applied by an allow/deny list (`security.selinux` not applied by default) |
| macOS resource forks, Finder metadata | Preserved explicitly |
| Sparse files | Holes recorded, recreated where supported, never inflated on download |
| FIFOs, sockets, devices | Excluded from ordinary sync, with the reason shown |

## Filenames

Names are stored as bytes, and translated per destination filesystem, not per OS: macOS does not
universally force NFD. Each folder picks a policy:

- **Portable:** NFC for valid Unicode; case and normalization collisions and names invalid on Windows
  (`:`, `CON`) are detected before changes apply.
- **Faithful:** original bytes, including Linux names that are not valid UTF-8, with reversible local
  mappings where necessary.

`File.txt` and `file.txt` both stay on the server; a case-insensitive destination shows a clear
mapping or a conflict. A rename made for compatibility must not break a relative symlink that points
at it.

## Sync

**Committing a change.** Wait for a stable candidate (close_write, or unchanged for a short window; the
one-shot CLI waits until a file's ctime is 250 ms old) → chunk it → ask the server which chunks are missing → upload those → commit a revision naming
its base revision and a unique operation ID → durable acknowledgement. A retried operation ID returns
the first result, so a lost response is harmless. Small files take a simpler batched path.

**Applying a change.** Download to a temp file in the same directory → verify hashes → fsync → set
metadata → check the destination has not changed locally → atomic `rename()`.

**Three-way merge per object** (base, local, remote). It is state-based: each round works from what is on
disk and what the journal says, so a crash only means the next run finds some of the work done, which
merges as agreement.

| Changes | Result |
|---|---|
| One side only | Apply it |
| Metadata on both, different fields | Merge field by field |
| Metadata on both, same field | Server order wins; the loser is in the activity log |
| Content on one, metadata or rename on the other | Apply both |
| Content on both | Keep both: a competing revision, a deterministic copy `name (conflict hostname 2026-10-31).ext`, an entry in the conflict inbox |
| Edit against delete | The edit survives, and it is reported. This includes each side removing a different hard link of a file someone edited |
| Same new name, different objects | Identical content or two directories become one object; otherwise keep both under a conflict name |
| Concurrent moves that would form a cycle | The server rejects the second; that client rebases |

**Deletes** are tombstones. A device back after the supported offline interval reconciles afresh
before submitting old state, comparing against history, so deleted files do not come back.

**Selection.** Ignore rules in `.gitignore` syntax, selective sync per folder. No ignored names by
default. On-demand files are an addition: ordinary fully downloaded folders stay, above all for
projects and anything needing faithful metadata.

**Failure handling, where most of the test effort goes:** watchers plus reconcile, durable queues,
files changing during a read, disk full, interrupted upgrades, and a missing mount (each root carries
an ID marker; if it has gone, or an unusual share of the tree vanishes, sync pauses and asks). The UI
names the state and the action: "waiting for permission", "destination cannot represent this name",
"file keeps changing".

**A limit to state plainly:** a watcher cannot capture every intermediate save, so history covers what
was captured and committed. Consistent snapshots of live databases or VMs need filesystem snapshots or
application coordination.

**Acceptance scenario:** a working tree with `.git`, `node_modules` and `.venv`, used from Linux and
macOS, one machine at a time. Modes and symlinks survive, nothing vanishes for its name, no conflict
copies turn up inside `.git`, and `git status` on the other machine is clean.

## History and retention

Restore a file revision; a directory as of a sequence number; deleted files; into a separate folder
for inspection; undo a batch from one device; review permission and rename history.

Retention is configurable (for example hourly for a day, daily for a month, weekly forever).
Mark-and-sweep GC removes a chunk only after accounting for retained revisions and in-progress uploads.

## Sharing and access

Each shared folder is its own namespace, mounted into recipients' trees, so permissions are checked
once per namespace and moving a share rewrites nothing. Public links are expiring tokens to a namespace
or a single manifest. Auth: local accounts, then OIDC.

Dedup is scoped to an account or a shared namespace, never global: the "which chunks are missing"
question would otherwise tell one user what another stores.

## Encryption

TLS and optional encryption at rest come first, with server-readable storage. End-to-end encryption is
a separate later decision, opt-in per namespace: per-file keys wrapped by folder keys wrapped by user
keys. It costs web previews, browser access, server-side search and easy key recovery, so it is never
the default.

## Operations

- One static binary, embedded migrations: upgrade by replacing the binary and restarting.
- Backup: snapshot the metadata DB, rsync the append-only chunk directory.
- A built-in `fsck` checks every chunk hash and every manifest reference.
- WebDAV is an optional gateway module for legacy tools, never the sync protocol.

## Build order

Each milestone ends at its exit criterion, and the next does not start before it.

| # | Work | Done when |
|---|---|---|
| 0 | Protocol and on-disk format written down; Cargo workspace: protocol, store, engine, server, CLI crates | The types cover every row of the metadata table; a license is chosen |
| 1 | Chunk store, manifests, journal; a one-shot CLI that syncs one Linux folder with full metadata and three-way merge, and restores | A tree with every metadata type round-trips byte-exact and metadata-exact through the server |
| 2 | Failure harness: `kill -9` at every step, disk full, rename storms, lost events, missing mounts, long-offline clients; randomized operations checked against the invariants, and `fsck`, which the harness uses as an oracle | The harness runs in CI and finds no violation |
| 3 | Daemon: watcher, reconcile, persistent change feed, ignore rules and selective sync, the acceptance scenario across two machines | Runs for weeks on real trees with no silent loss |
| 4 | History and trash API, retention, GC | GC under load never removes a reachable chunk |
| 5 | Namespaces, sharing, multi-user, OIDC, PostgreSQL | Both stores pass the same suite |
| 6 | Web UI: browse, share, activity, history, conflict inbox, previews | Everything in it works through the public API |
| 7 | macOS and Windows clients, Tauri, installers | A file edited on each platform keeps its Linux metadata; filename policies pass |
| 8 | On-demand files (FUSE, File Provider, Cloud Files API), mobile | |
| 9 | WebDAV gateway, Nextcloud importer (files, versions, trash), E2EE, LAN/P2P transfers | |

## Prior art to study first

Seafile (content-defined chunking and git-like history, but weak on Unix metadata) and Syncthing
(excellent care with conflicts and metadata, but peer-to-peer with no central history): borrow the
first's storage model and the second's care with edge cases. What this plan says about them and about
Nextcloud comes from design notes and has not been checked against their documentation or code: check
it when a decision leans on it, and save the page in sources/.

## Open

- **License.** AGPL-3.0-only for the server and storage, Apache-2.0 for the protocol, engine and client,
  so any client can embed them. Applied to meet milestone 0; the owner confirms it before the first
  outside contribution, since changing it afterwards needs every contributor's consent.
- **Name.** Check "cairn" on crates.io, GitHub and for trademarks before publishing.
- **Moves between namespaces.** Two journals cannot share one atomic entry: decide whether object
  identity and history survive, or a move is a copy plus a delete.
- **Offline interval.** How long tombstones live, against how long a device may stay away. No limit so
  far, since the journal is kept whole; retention in milestone 4 has to set one.
