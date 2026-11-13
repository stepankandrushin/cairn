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
| Fragile upgrades, app bloat | One binary, embedded migrations, clients that install signed updates themselves, no plugin system |

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
| Server metadata | SQLite by default, PostgreSQL for multi-user scale; each revision's body kept once, as a binary record, and named by the journal's operation that made it (docs/format.md) | One binary for a home user, real concurrency for an organisation; both behind one narrow store trait and run through the same test suite. What is sent is made from the records, the same JSON as before; a real store in about a quarter of the pages its JSON took, and an older schema moved in one transaction |
| Client state | SQLite (WAL) per sync root, one SQL row per object holding its base node and its row as binary records, the row written against the node so each detail is stored once (docs/format.md); an older layout is moved to it in one transaction when a build first opens it | Holds the base state for three-way merge, the durable queue, resumable transfers; nodes, rows and manifests in about a seventh of the pages their JSON text took, and a kill or a full disk during the move leaves one layout or the other whole |
| Content store | Immutable content-addressed chunks on local disk, S3-compatible optional | Trivial to back up and replicate |
| Chunking | FastCDC (average ~1-4 MB), BLAKE3, optional Zstd | Inserting bytes mid-file changes only nearby chunks |
| Transport | HTTPS: batched idempotent operations plus a persistent change feed | Passes through any reverse proxy and still gives instant push; QUIC only if measurements later ask for it |
| Linux watcher | inotify as the baseline, fanotify with `FAN_REPORT_FID` when privileged | The daemon runs without root, and fanotify's mount and filesystem marks need `CAP_SYS_ADMIN` |
| Other watchers | FSEvents on macOS, ReadDirectoryChangesW on Windows | All plus periodic full reconcile: a sync lists where events were, so a lost event waits for the reconcile's scan of the whole tree, the safety net |
| Web UI | Plain HTML, CSS and JavaScript modules, embedded in the server binary, with no build step | Thin client of the same API; stays out of the server's hot path. Building the server needs cargo alone, and no npm package runs in a page that holds a session |
| Desktop UI | A tray, `cairn-tray` (Tauri), a separate program nobody needs: it shows each daemon through the local API and opens what a person deals with. On macOS the tray and the daemon are each a signed app bundle, Cairn Tray.app and Cairn Sync.app; each program has one name everywhere, `cairn-sync` (Cairn Sync, the CLI and daemon) and `cairn-tray` (Cairn Tray), its bundle on macOS and what Windows shows for it named after it | The daemon runs where no desktop does, the OS starts it, and a tray that crashes or was never installed stops nothing; native integration only where needed. A bundle is how macOS names a background program and shows its icon: in System Settings, and in Activity Monitor once the daemon has checked itself in with LaunchServices, as a background-only app, which nothing can bring to the front. Windows takes the name and icon from the program's file, so `cairn-sync.exe` carries them as resources. One signature across builds keeps its permissions through upgrades |
| Updates | Each daemon asks its release's feeds about once an hour, the project's own site first and its GitHub mirror only when that does not answer, and installs a newer release itself, `cairn-sync` and the tray beside it, restarting into it between syncs; a person can also ask, by `cairn-sync update now` or in the tray, or set it to only notify, or off. What it installs is an Ed25519-signed list of every file's size and SHA-256, and on macOS also a bundle signed by the team of the one it replaces. The server is not among them | A fix reaches every device within the hour, and nobody stays on an old version by forgetting. No permission is asked again: macOS keeps them for the signing team, which Developer ID gives every release, and Windows by path. A key of the project's own rather than an OS certificate is what is trusted, so Linux is covered alike, and a build from source updates from nobody's releases but its builder's. Because the key is what is trusted, where the file comes from is not: a release's files are named beside `update.json`, so the one signed file serves from the site and from GitHub alike, a release waits for no one else's service, and either being down delays no update. A release is tested before it is published by the full suite on the machine that builds it, with nothing skipped, rather than by waiting for a hosted CI, which still runs on every push as a second check. The server goes first by hand, because a client takes a release on a new protocol only once its server speaks it |
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
| Symlinks | Target verbatim, never followed. Links leaving the root sync as links, optionally flagged. Without platform support: a placeholder, with the real data on the server. Until then, on Windows, an arriving symlink is out of view like a name it cannot hold, and a link or junction made there is reported, like a FIFO, and not synced: no reason to retry, or to hold anything else back |
| Hard links | Relationship preserved inside a sync root where supported |
| Mode | All 12 bits stored; setuid, setgid and Linux capabilities applied only by explicit policy. Windows holds only a file's write bits, as its read-only attribute, and reads a fixed mode otherwise, so what is stored stays |
| Owner, group | Numbers and names stored, with a per-client mapping, since numeric IDs differ between machines. The daemon runs unprivileged: exact restore needs a narrowly scoped helper or an explicit restore command. Windows reads none, so what is made there has none stored until an owner changes on a device that has them: arriving on one is no change, or each device it reached would make a revision |
| mtime, btime | Nanoseconds, with the recorded precision kept. On Windows a file renamed or made where another has just gone takes that one's creation time (NTFS tunneling, which a swap of two names shows), so there a birth time is stored as first read, and is neither a change nor a sign of which file is which: a file ID is, as NTFS never reuses one |
| xattrs, POSIX ACLs, Windows ACLs | Stored in per-platform namespaces; applied by an allow/deny list (`security.selinux` not applied by default) |
| macOS resource forks, Finder metadata | Preserved explicitly. For now as `macos.xattr` records, up to 1 MiB each, past which one is reported and not stored. What is about the machine rather than the file (provenance, quarantine, access grants, last opened) is neither stored nor applied |
| Sparse files | Holes recorded, recreated where supported, never inflated on download. Where the holes are is no change while the bytes are the same: filesystems move them on their own (APFS fills small ones and lays data runs out its own way), and a device that read that as an edit would send the file back to everyone, inflated |
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
at it. Until those policies exist, a name the filesystem refuses, or one it holds as the same name as
another here, is out of view on that device, as if excluded: reported on every sync, never retried, so it
holds nothing else back, and it comes down once the server's entry is renamed or the twin here is gone, or
renamed to it. The other way round, something new here at a name the server has in that folder in
another form this filesystem takes for the same (decomposed on APFS, or in another case where case
folds) waits, reported with both names, until it is renamed to the server's form: it would go up as a
second name, folders whole, for every device that keeps names as bytes to hold both.

## Sync

**Committing a change.** Wait for a stable candidate (a file's ctime 250 ms old, for the daemon as for the
one-shot CLI, since a watcher's close_write says nothing about a program that writes again; 2 s if another
program has it open for writing, which Linux tells by refusing a read lease, or if it has just been emptied; and
an emptied file still open for writing waits for its writer however long, since a program that truncates and
then writes, stalled on a busy machine, leaves exactly that) → chunk it → ask the server which chunks are missing → upload those → commit a revision naming
its base revision and a unique operation ID → durable acknowledgement. A retried operation ID returns
the first result, so a lost response is harmless. Small files take a simpler batched path, and the server
makes a request's chunks durable together before it answers. A change too big for one commit goes in several
from one scan, the server checking each against what the ones before it left.

**Applying a change.** Download to a temp file in the same directory → verify hashes → set metadata →
fsync, once for both, as a flush empties the disk's cache → check the destination has not changed locally →
atomic `rename()`. New files take the first four steps a batch at a time, up to 64 or 16 MiB from a run of
creations whose folders are there: all written, then fsynced, one after another until one has waited on the
disk and from then on all at once, a thread each, since a journal commits fsyncs that wait together in one
flush where one after another each waits for its own (a first sync of 50,000 small files on an NVMe mirror
took 261 s at 5 ms a file, and 32 s so), and where no fsync waits, threads would only wait to run on a busy
machine; then each, in its own turn and the order it had, is checked and renamed once its own fsync has
returned. One that fails is reported in
its turn, those before it placed; a full disk stops the run, and what it had not renamed it removes, or after
a kill the next scan does. Anything done to one while it waits for its fsync means the next scan reads it. A
replacement goes one at a time. Chunks come through a cache a
slice at a time, each gone once the files that take it are written, so a first download needs the tree's size
free and not twice that, and one that would not fit is refused before anything moves. On a local Linux
filesystem a placed file is watched (inotify) from its last write until a later write would move its
timestamps, so its record vouches for it with no read back unless the watch saw it written to or its metadata
changed. A file whose metadata alone a sync sets, as a join does to every file whose mode differs, is watched
too, from before its check against the scan: a write, an open, which any new writer makes, a writer's close,
and once the sync's own change is made any change of metadata, mean a read, and so does a read lease refused
when asked for a moment, which shows a writer that had it open all along, mapped perhaps, whose writes no watch
sees. Anywhere, one the apply left as the scan found it keeps its record's word, and one that moved besides
what the sync set is no step done: an edit in that moment is the next round's, its mtime not taken for the pair
of the server's. The mtime is set last, and set again after any record written, as writing a resource fork,
the file's other stream on macOS, moves it. A watch holds nobody back: a program that saves a file the moment it arrives waits for
nothing, whatever the sync is doing, and its save is read by the next scan. A lease would make that save wait
until the sync gave it up, which a sync blocked in an fsync or stopped cannot.

**Three-way merge per object** (base, local, remote). It is state-based: each round works from what is on
disk and what the journal says, so a crash only means the next run finds some of the work done. A round that
wrote here is confirmed by a fresh scan in one more, which reads no new commits: so a sync agrees as of the
sequence it applied, a device whose rounds take longer than another device takes to commit still comes to agree,
and what came after is the next sync's. What a person changes here once that round's scan has begun is no part
of the agreement, and the next scan finds it. What was
on its way is noted before the first step, in one transaction however much it is, and claims nothing: the
next scan compares it with the disk, field by field and name by name, and what got there counts as shared
with the server. New content arrived whole, and what its fields should read is asked of the filesystem, on a
scratch entry beside it and under the policy the run applied by, so that what it cannot hold, and what its
folder gives a new file (an inherited ACL, a set-group-ID folder's group), is no change of anyone's. A field
that did not fit for want of room is a full disk, not a limit: the file is not placed. So what a device
received does not read as its own change, kill or full disk, and a later change from elsewhere stands as it
would have. With no room left to ask, the sync stops and says the disk is full. An edit caught in the moment
before a file is replaced or removed, or in another name of a hard-linked file as it moves to the new version,
is kept under a conflict name, as a conflict copy in the inbox, and the names stay one file. The limits: in a
folder this device cannot write to, the scratch entry is made in `.cairn`, as it is for a dry run, so there
what the folder gives a new file reads as a change made here. A person who acts on a file in the moment
between the kill and the next scan acts on something the scan cannot tell from what arrived, as a new inode has
no record yet, and telling them apart would cost a durable write for each file before its rename. So a removal
of a just-arrived file can be undone by the server's copy, a rename can leave both, an edit makes a conflict
copy, a rename of what the run had just moved loses to the server's move, reported, and a folder's chmod or
time set then goes back. A folder a rule hides from the next scan while it is still private to the run that
made it logs "this device had 700" once the rule goes.

| Changes | Result |
|---|---|
| One side only | Apply it |
| Metadata on both, different fields | Merge field by field |
| Metadata on both, same field | Server order wins; the loser is in the activity log, a commit's losers of one text one entry naming each object |
| Content on one, metadata or rename on the other | Apply both |
| Content on both | Keep both: a competing revision, a deterministic copy `name (conflict hostname 2026-10-31).ext`, an entry in the conflict inbox, open until the copy is deleted, renamed or moved, which is how a person deals with one. Inside a git directory the copy is `name.conflict-hostname-2026-10-31`, a name git takes: a copy of a branch is a branch to merge, where a space in `refs/` breaks the repository |
| Edit against delete | The edit survives, and it is reported. This includes each side removing a different hard link of a file someone edited |
| Same new name, different objects | Identical content or two directories become one object. With no base its metadata still merges field by field, each side's own: what only one has stays, and where both have a value the server's stands and the other is in the activity log (times and owners, which differ between any two copies, without an entry). A file's execute bits count as fields of their own, so a bit only one side has stays: what kept the copies alike before may have carried no mode, and a program without its bit no longer runs. A file the server holds under several names takes separate equal copies at them as its links, the copy with the most of its own staying, unless another has something the file would not hold. Otherwise keep both under a conflict name, in the conflict inbox like any other |
| Concurrent moves that would form a cycle | The server rejects the second; that client rebases |

**A sync with nothing to do** does not merge. One that ran in full, had nothing to do and wrote nothing records
what it read: the state, by a generation that every write to it moves; the entry at the cursor and the head; the
build, the configuration and the rules; and the tree, by a digest of its scan. A later sync that reads all of that
again, by a scan that stops before it would read a file, would decide the same, so it stops there, before the base
is loaded; anything else, and it runs in full. A harness build runs both and compares them.

**A sync that carries a change** costs what changed, and in the daemon its scan does too. An agreement decided nothing about any
object, and the merge looks at little besides the object itself, so an object whose own state and whose
neighbours' are as they were at an agreement is decided about as it was then: not at all. The state keeps a
ledger of the last agreement, per live object of the base: its kind and entries, and what the scan saw, the
inode and fingerprint, the content, and for anything but a file a digest of the rest it read (a file is its
row, which no scan reads while its fingerprint holds). Triggers in the file mark every object whose base node
or row any build writes, so the ledger speaks for an object only until something touches it. A round's
change set is:

- from the journal since the cursor, every object an operation names and every folder an entry it gives or
  takes is in, but not what a note names: a note changes nothing in the tree, however many objects it
  names, as a join's names nearly every one, and the round's replay of the journal passes over it, the
  server having checked its objects as it took the commit;
- from the scan, every object the ledger has that the scan did not find at the same entries, by its folders'
  inodes, with the same fingerprint and, for a folder or link, the same digest; every inode the ledger does
  not account for; everything marked, everything this filesystem cannot hold, and the whole of what the scan
  could not see;

closed under what the merge reads besides the object: its ancestors on every side (cycles, git folders, rules
by path, paths), every name in each folder any of its entries is or was in, on every side (collisions, twins,
conflict names, adoption by name and content, what is out of view at that name), every name of a hard-linked
file (the node and the scanned inode whole), everything in a folder among them (what is kept inside one
deleted, what goes with one moved or excluded), with rules the whole of a folder the journal moves, and what
was out of view at the last agreement. An object rewritten in place, at the one entry, inode and kind it had,
takes no name, so its folders are not opened for it while no conflict copy can come of it: while the journal
names it in updates alone if at all, and then only while it is as it was here or as this sync pushed or
brought it. The round runs the full sync's own identification, merge, apply,
push and finalize on those alone: the trees, rows and scan it gives them hold nothing else, and what they
leave out is the same on every side. Any decision about an object loaded only as a neighbour means the set
was too small: it is widened and decided again, and nothing is written before that settles. An agreeing
round writes the ledger for what it held, a full one writes it whole. With no ledger from a sync of this
build, configuration, rules and selection, or with a mark or queued commit from a run that did not finish,
the ledger is rebuilt from the state instead, said in the sync's output and the daemon's log: a scan given
every row, then one pass over the base and the rows in object order, puts for each object the entry a full
finalize that decided nothing would write, where a full sync would find it settled (its row read whole, not
racy, nothing on its way, the metadata the base was agreed at, held at the base's entries, and the scan
finding it as the object its inode says, at those entries, as its row has it; or with no row, out of view
here), and takes everything else as written since. The round then runs as on a ledger kept, and its finalize
writes the ledger whole. A round on a ledger kept that finds one inode at two places in the scan, as when
something moves while it walks, or that identification takes a folder for another object than its inode
says, goes again on a fresh scan and the ledger rebuilt. The full sync runs instead, said too, on a first
sync, with no base to rebuild from; on a dry run; with copies kept by `cairn-sync exclude`, found by inode; with
a leftover of a run that did not finish; when the journal's operations besides notes, the set or what a
rebuild finds unsettled comes to more than a quarter of the tree, the set counting only what the state holds something of, so not the
ID a join's adoption leaves behind for each file it took in; when the journal does not replay on the part
loaded; when a round on a
rebuilt ledger cannot go on either, or a rebuild finds records going out of view while the journal moves things;
or after four widenings. A round's walk sorts each folder's entries against the ledger as it leaves the
folder, on several threads, and keeps whole only what the ledger does not account for; of the rest it keeps
the object each is and its folder, some 30 bytes an entry, and what a scope takes from the ledger under a
folder it does not list the same way. The ledger is held as the state stores it, about 150 bytes an entry, and
the round decodes the base and rows of the set alone, making again what it takes in of the rest from its
ledger entry and row. A rebuild walks the tree against every row's glance and the base the same way, keeping
of what they account for a witness of what was read, which the judging of each row checks; then the round
walks again, sorted by the ledger rebuilt. The daemon's check keeps such a witness of what its walk takes as
the ledger has it. So a sync after a device's first holds a few hundred bytes an entry, about 300 MB at
842,000 entries. A harness build decides every incremental round the full way as well, on the same scan,
journal and state, and fails the sync unless every decision is the same: the rows the scan was given,
identification, the rows, the mass-delete check, twins, the plan, the operations, the local steps, what is out
of view, and nothing at all outside the set; the walk keeps every node whole beside what it sorted, and the
rows it was given, the check and a rebuild must be the same from either; after a
finalize, the base, rows and ledger must be what a full one leaves; and on a ledger kept, the ledger rebuilt
from the state must be it for everything the round leaves alone. A release build has no such oracle, so the
daemon checks the bookkeeping instead, a day (`--full-sync-secs`) after the last sync whose every decision was
the full sync's (one that took the whole tree or a rebuilt ledger, or the last check): the ledger kept against
the ledger rebuilt from the state, object by object. An object it vouched for whose entry is not the state's,
or that it has no word of though the state has it, is a defect, said in the log and taken as written since,
and the round goes again on a fresh scan, so that it decides about that object now as the full sync would.
What it does not compare is decisions: a neighbour the closure leaves out, with both ledgers agreeing, is the
harness's oracle to find, on its trees, and no longer the daemon's on every device's.

**Deletes** are tombstones. A device back after the supported offline interval reconciles afresh
before submitting old state, comparing against history, so deleted files do not come back.

**The daemon** decides when to sync and nothing else: it runs the same state-based sync a person runs. An
event from the change feed only says "look". One from the watcher also marks the folder it was in, stamped when
read, and a folder made or moved in whole; a round on what changed alone then lists the folders marked since the
scan the last agreement in this process rests on began, the folders of whatever the state marks as written
since, the journal or a mark names, and what holds a hard link, and the folders above them, gone through
without listing where each is as the ledger has it; every other folder's entries it takes from the ledger, and
a folder found otherwise than the ledger has it is listed after all. Nothing a round decides about or loads
beside it is the ledger's word alone, and every folder whose every name it looks at was listed: otherwise it
goes again listing those too, up to four times, then lists the whole tree. Whatever the watcher cannot place or
may have missed, an overflowed queue, dropped events, FSEvents' must-scan, a root moved, a rules file saved, a
watch that died with its folder still there, is a loss of track: no narrowing stands on anything before it, and
the next sync lists the whole tree. So does every sync while a folder is not watched or the watcher has stopped,
the daemon's first, the reconcile, and the check. A lost event the kernel says nothing of, as for a write
through a mapping, delays a change and cannot lose one: the reconcile, a sync an hour whatever wakes it, scans
the whole tree, and a sync a day checks the ledger the syncs that carry a change rest on against the state and
takes up whatever it vouched for wrongly, so whatever both miss, and whatever a ledger gone wrong would have
hidden, is taken within a day; and an apply checks what it replaces on disk, not in the scan, so what nobody
saw change is kept beside what arrives. What a sync that listed part of the tree did not see again of what
was skipped elsewhere, the daemon keeps showing. A watch is always placed before the sync it leads to; the walk that places them
holds one folder open for each level down, the daemon takes its hard limit on open files, and a folder it cannot
watch is named. A wake for a change from before the scan the last sync agreed on is none, as that scan saw it, the
sync's own writes included, and so is the feed's news of commits that sync took in. Change feed: a long-poll
of `GET /changes`, not a stream, since a plain response survives any proxy; when one cuts it off, the
client waits for less. It holds no file open in a root while idle, so a disk can be unmounted under it, nor
the state in memory, so each sync first reads the state's file through once, in the file's order and in pieces
of a MiB, unless it is over a quarter of the machine's memory: SQLite reads a page at a time in the order of a
table's keys, which in a state syncs have rewritten is all over the file, a seek a page on a hard disk whose
cache has dropped it. A sync that leaves over half of that file free pages, which SQLite reuses but never gives
back, as a join or the move from an older layout does, has it rewritten once it is reported (VACUUM, every table in key order,
the format as it was, its temporary copy in a file rather than in memory), under the root's lock as a sync: said
in the log alone, put off while a disk lacks the room, and a kill or a full disk in it leaves the state as it was.
And it handles no signal: a kill is the crash a sync already recovers from. inotify reports a filesystem being
unmounted, but says nothing when only a bind mount goes away, or when anything is mounted back: the
reconcile finds the first, and retrying while paused the second. On Windows a watched folder says nothing
when it is itself renamed, so a second watch, on the folder above, notices the root renamed away or another
folder put at its path, and the root's watch moves there; a folder further up renamed waits for the
reconcile, as on Linux. After a failure it backs off, up to a
minute, and holds every change back until then. A file still being written is no failure: the daemon comes
back for it on the same backoff and holds nothing else back, and the file's own writes wake nothing, though
its close does. Nor is one the system refuses: it is come back for alike. Between syncs it rests for twice what the last one did that every sync repeats, loading the
state, the scan and the merge, so a tree that never rests keeps it busy at most a third of the time, but for 30 s at
the most, so that a change written as a sync ends waits at most about 30 s and one sync, however big the tree; the
data a big download moved is no reason to wait, as the next sync does not move it again. `cairn-sync status` says when changes wait, until when, and why, and while a sync runs what it is doing,
how far it has got and when anything in it last moved, which the log says too once it has run a minute, so a
long scan is told from a stuck one. It serves the local API
(`cairn_protocol::local`) on a socket only its user can open: its status, pushed on each change, the open
conflicts, and three requests that change no tree: look now, pause, resume. A pause outlives a restart
and shows as `paused`. Each user's roots on a device are listed in a registry the CLI and the daemon add
to, for `cairn-sync status --all` and the tray. A root that has gone stays listed as gone until a person takes it
off with `cairn-sync forget`, which takes off one still there only when asked, or with the tray's Forget, which never does.

**Selection.** Rules in `.gitignore` syntax in `.cairn/ignore`, and folders excluded by identity with
`cairn-sync exclude`, which stay excluded through a rename. No ignored names by default, ever, and no in-tree
rules file: both are one device's choice, as in Syncthing, so they cannot conflict, a rule cannot arrive from
another device and stop syncing something here, and a new device's rules are in place before anything comes
down. A project's own `.gitignore` means nothing to Cairn.

The merge works on a *view*, the base and the server's tree with the excluded entries taken out. The merge's
base also holds only the entries this device holds, per entry and recorded before any removal, because a delete
needs proof that the thing was here: whatever a scan finds is held again, so a killed removal costs nothing.
This holds with no rules too, for a delete elsewhere that is undone before this device has finished. So:

| Situation | Result |
|---|---|
| Excluded here, on the server or not | Neither side touches it, in either direction; deleting the copy here means nothing to the server |
| A rule lifted, or a folder included | What this device does not hold comes down, and never reads as deleted. What is here merges as new: what changed keeps both versions |
| A copy left by `cairn-sync exclude` | Left alone, by inode wherever it moves here, and by path for anything new this device has no record of, so a folder generated again stays out too, whatever the server's folder does, even if that is deleted. It joins in only with `cairn-sync include --merge`, which has to be asked for since an old copy can bring back what was deleted since and leave conflict copies for everyone. If the server gets something else at its path, sync reports what is in the way and waits for a person |
| `mv` into an excluded path here | As in git, a delete of what moved, for everyone: the copy here is out of sight, and the server's history keeps it |
| `cairn-sync exclude` when what is here is not what the server has at that path, or anything under it differs | Refused: leaving the wrong thing out would read the right one as deleted or as new. A rule needs no such agreement, and a folder this device holds nothing of can always be excluded |
| A rule by path, when a sync is about to move the folder and the rule would not match there | Stays out by inode, as a copy left by `cairn-sync exclude` does, reported, and listed by `cairn-sync status`: nobody here chose that. So does what this device held there, put there by its own `mv` that a cycle undone takes back, say: its record does not bring it into view. Not a file with another name here, which would take that name out of view with it. `cairn-sync include --merge` brings it in. A person's own `mv` here is a choice: the rule is about paths, so it goes up, as in git |
| Moved out of view elsewhere | Goes from here like anything deleted there, if unchanged. Changed here: it stays as a new object, and the server's stays where it moved to |
| A folder deleted elsewhere, holding what is excluded here | What is excluded stays, and so does the folder, here only: it does not come back for anyone unless something real is put in it, or it is emptied, since an empty folder is real |
| A folder deleted here, holding on the server what is excluded here | The folder comes back here, empty of that, since the server's copy is not this device's to delete |
| A name here taken on the server by something out of view | Keep both under a conflict name |
| `cairn-sync exclude --remove-local` | Syncs first, then goes the way a delete from elsewhere goes: unchanged files only, and nothing excluded. Without the flag, nothing is synced first: what is new in the copy was not meant to go up |

On-demand files are an addition: ordinary fully downloaded folders stay, above all for
projects and anything needing faithful metadata.

**Failure handling, where most of the test effort goes:** watchers plus reconcile, durable queues,
files changing during a read, disk full, interrupted upgrades, and a missing mount (each root carries
an ID marker; if it has gone, or an unusual share of the tree vanishes, sync pauses and asks). A path
the system refuses this device, to read or to write (an access list, another program holding it, a program
that guards files, a failing disk: told by the kind of error, never by which program, Windows' codes for a
file blocked or removed as a threat among them), is held as it was: no delete, no revision, nothing of it
reverted elsewhere. It is named with the system's own words, and on Windows the hint that another program or
security software may block it, in a sync's report, `cairn-sync status`, the daemon's status, log and tray, and
tried again on the backoff, holding nothing else back, until it clears by itself once let in. A tree most of
which is refused pauses as one most of which vanished does, and `--allow-mass-delete` does not lift that,
since nothing is deleted. What is gone, whatever took it, is a delete as before, which `cairn-sync deleted`
lists to bring back. A request
the network fails in a way that says nothing about the request (no socket buffers, a connection reset,
refused or closed before the whole answer, a name not found, a proxy's 502 to 504) is tried again within
it: over about a minute in a sync, a few seconds in a command a person waits on, about three minutes at
the most however slow each try. The daemon's log says a request's first new try and its outcome; past
the budget the sync fails, and the next picks up from the durable queue. The UI
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

Deleted files are where a person looks for what is gone, whatever took it: another device, a script, or an
antivirus that removed a file on one device, which to every sync tool is a delete like any other and reaches
every device. So the tray's "Recently deleted", `cairn-sync deleted` and `cairn-sync undelete` list what is still
deleted and bring it back, with no code for any antivirus. A folder is listed once, with what one device
deleted in it within 10 minutes before. Bringing it back links each object to the entry it last had, as
itself with what it last held: nothing is reverted, nothing replaced, and a name taken since gets a restored
copy's name beside what has it. One commit, so a kill leaves nothing or all of it. Until the server keeps
a trash of its own (milestone 4), the client reads it from the journal, into an index of every object's
kind, size and entries without their bodies, a fraction of a replayed tree's memory; a daemon keeps it
ten minutes after the last request, taking only what is new to answer the next. The server keeps a smaller
one for the web in memory, replayed when it starts and kept up after each commit.

Retention is configurable (for example hourly for a day, daily for a month, weekly forever).
Mark-and-sweep GC removes a chunk only after accounting for retained revisions and in-progress uploads.
Before it lands, the server must look a commit's operation ID up before it checks the commit's chunks, which
today it does after: a retry of a commit it took, whose chunks were collected since, would be refused rather
than answered.

## Sharing and access

Each folder is its own namespace, so permissions are checked once per namespace and moving a share
rewrites nothing: a user's own folder and a shared one differ only in who has access. A user has tokens,
one per device, kept as their hashes, and a level in each folder given it: `read`, `write`, or `manage`,
which is write and also saying who else has access and renaming the folder, and never leaves a folder without
a manager. No level is
no access, and the folder answers 404 as one that does not exist, so that nobody learns what is there by
asking; too low a level answers 403. The server's administrator adds users, tokens and folders with
`cairn-server` on the data directory, while it serves too; any user adds a folder of their own through the
API, which they manage, and a folder's managers change who has access
through it. A folder's name is what paths, devices, links and commands use, and a server's first is
`cloud`, which `cairn-sync init` joins as the only one its token reaches. Names are one space for the server, so a
user asking for one that is taken learns that it is, and nothing more. A folder's managers rename it through
the API while the server serves, and the administrator with it stopped; its chunks stay where they are, so a
rename moves and copies nothing; the former name goes on reaching it and no other folder may take it, so
nothing made with it breaks, what a request began by it goes on in the same folder, and devices take the new
name up as they sync.
Each user may also have their own name for a folder, which only they see and which changes nothing else:
the name of the directory their first device syncs it into, until they choose another. The journal says whose token made each commit. A data directory's one token from before
accounts becomes the user `admin`'s, managing every folder there is, when the schema gains accounts.

A shared folder is synced as a root of its own; mounting one into another root's tree, so that a recipient
sees it inside their home, waits for moves between namespaces (Open). A folder a device's user may only read
takes nothing from it and loses nothing on it: the merge is given a local tree in which nothing shows as a
change that could only go up, as it is never shown what is excluded. What is new there is held back; what
changed there while the server's stayed as it was is carried, standing for the server's state as something
unreadable does, with its row keeping its baseline, so that the next sync sees it again; what was deleted
there is given up, so that it comes back; where both sides changed something the merge keeps both, the
server's at the name. A sync completes and records its agreement as any other, so a reader costs no more,
and every held-back change is listed until it is moved out of the root or the user may write, when it goes
up.

A folder's managers also share any object in it, its root too: with a user at `read` or `write`, or by a
public link, which only reads and may expire. Either reaches the object and what is in it and nothing above or
beside it, by the object's identity, so a move keeps it; it stops while the object is deleted, and for good
once its maker no longer manages the folder. A user's share is reached only through the web's endpoints,
since the sync protocol's reveal the whole tree; syncing one waits with mounting a shared folder above. A
browser signs in with a token, or by a one-time ticket a device asks for with its own token, which the tray and
`cairn-sync web` open it with; either sets a session the server signed, naming the token and never holding it, in a
cookie no script can read, and a request signed in by it changes nothing without a header no other site's page
can send. Auth: local accounts, then OIDC.

Dedup is scoped to an account or a shared namespace, never global: the "which chunks are missing"
question would otherwise tell one user what another stores.

## Encryption

TLS and optional encryption at rest come first, with server-readable storage. End-to-end encryption is
a separate later decision, opt-in per namespace: per-file keys wrapped by folder keys wrapped by user
keys. It costs web previews, browser access, server-side search and easy key recovery, so it is never
the default.

## Operations

- One static binary, embedded migrations. Clients update themselves (docs/protocol.md, "Updates"): the new program
  checked and run once, put in place by a rename with the one before kept, and a version on which a daemon keeps
  dying before a sync taken back by itself; a kill at any step leaves a whole program. The server is upgraded by
  a person replacing its binary and restarting, before any client takes a release on a new protocol.
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
| 3 | Daemon: watcher, reconcile, persistent change feed, ignore rules and selective sync, the acceptance scenario across two machines | Runs for weeks on real trees with no silent loss. The code and its failure tests are in; the real deployment started with one tree, and its weeks are running |
| 4 | History and trash API, retention, GC. Recently deleted came early, at the owner's asking, read by the client from the journal until this API holds it | GC under load never removes a reachable chunk |
| 5 | Namespaces, sharing, multi-user, OIDC, PostgreSQL | Both stores pass the same suite. Accounts came early, at the owner's asking: users, their tokens, folders as namespaces with read, write and manage access, sharing by a folder's managers, folders added by any user and renamed by their managers while the server serves, and read-only folders on the client, with their failure tests; OIDC, PostgreSQL and mounting a share into another root wait here |
| 6 | Web UI: browse, share, activity, history, conflict inbox, previews | Everything in it works through the public API. Most of it came early, at the owner's asking: browsing, uploads, downloads and folders as zips, previews, history and restore, recently deleted, a folder's members, and shares with users and public links, each through endpoints any client may use, with failure tests for uploads killed or starved of disk; activity and the conflict inbox wait here |
| 7 | macOS and Windows clients, Tauri, installers | A file edited on each platform keeps its Linux metadata; filename policies pass. The macOS and Windows CLIs and daemons came early, to join milestone 3's run: a name a filesystem cannot hold is skipped and reported, not yet mapped. On Windows that includes what Windows programs cannot open (`:`, `CON`, a trailing dot), refused before NTFS is asked, and every path operation goes by handle with links never followed, as `openat` does on Unix. NTFS cannot swap two names, so a replacement holds the old file under a third, a hard link, and looks at it there, as a swap lets it on Unix. The failure harness runs on Windows too, outside CI, for what Windows holds. The tray came early as well, at the owner's asking, on macOS and Windows; logging in from it waits for milestone 5's accounts. Updates came early too, at the owner's asking, so that a fix reaches milestone 3's devices as soon as it is published: the feed, the updater in each daemon, the release built and signed for each platform. Installers wait here |
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
- **Name.** "cairn", "cairn-cli" and "cairn-sync" are other projects' crates on crates.io, so publishing
  crates there needs other names. GitHub has many unrelated "cairn" repos, none of them a file sync tool
  when checked at the first release. No trademark register has been searched yet.
- **Moves between namespaces.** Two journals cannot share one atomic entry: decide whether object
  identity and history survive, or a move is a copy plus a delete.
- **Offline interval.** How long tombstones live, against how long a device may stay away. No limit so
  far, since the journal is kept whole; retention in milestone 4 has to set one.
