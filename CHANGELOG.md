# Changelog

## 0.1.1

**Fixed:**

- A device could stop finishing syncs for good on a filesystem that reuses inode numbers, such as ext4.
  If a new file took the inode number of a file deleted in the same sync, every `cairn sync` ended with
  "the tree kept changing for 12 rounds" and exit code 3, even on an idle tree. Nothing was lost, and
  updating is enough: the next sync completes.
- With the same reuse, a file that moved and then became unreadable could fail to be held back, and read
  as deleted elsewhere. History kept it.
- The release binaries no longer carry paths from the machine that built them.

## 0.1.0

The first release: milestones 0 to 2 of [PLAN.md](PLAN.md), for Linux, with a one-shot CLI. **This is
early software.** It has been tested hard but has run on no one's real data for any length of time, so
keep another copy of anything you put through it. Formats may change before 1.0.

**There now:**

- `cairn-server`: one static binary with SQLite and a local chunk store; `init`, `serve`, `fsck`.
- `cairn`: `init`, `sync`, `status`, `log`, `history`, `restore`, `export`, `conflicts`.
- Full Linux metadata: all 12 mode bits, owners by number and name, nanosecond mtime, btime stored,
  xattrs, POSIX ACLs, capabilities, symlinks verbatim, hard links, sparse files, and names as bytes.
- Content-defined chunks, so only what changed moves. Versions share chunks.
- Three-way merge per object, with conflict copies and an inbox.
- Every restore is a new revision. A directory can go back to any sequence number; deleted files too.
- Failure harness: a kill at every step on client and server, full disks both injected and real,
  randomized operations on three devices against a no-version-lost oracle, rename storms, files changing
  during sync, missing mounts, and long-offline devices.

**Not there yet:**

- Sync runs when you run it. The daemon, watcher and change feed are milestone 3.
- One user: a single token and namespace. Sharing and OIDC are milestone 5.
- No TLS in the server: put a reverse proxy in front of it beyond loopback.
- No retention or garbage collection: the server only grows. Milestone 4.
- Linux only. No Zstd, no S3, no PostgreSQL, no ignore rules, no selective sync, no web UI.

**Known limits:**

- A program holding a file open and writing to it across a replacement writes to the old copy.
- A file moved into a directory that then becomes unreadable reads as a delete elsewhere until the
  directory can be read again. History keeps it, and it comes back.
- A killed sync at the exact step between placing a file and recording it, followed by another device
  restoring the file's previous content, can undo that restore. Both versions stay in history.
- When a conflict is resolved after a crash or a refused commit, some edits made on top of a pushed
  version become conflict copies rather than plain edits. Both versions survive; it is noise, not loss.
- A device rejoining with existing files keeps the server's metadata for identical content. What it
  replaces is written to the activity log (`cairn log`), values included.
- Files changed in the last 250 ms wait before being read, to avoid catching a save halfway. A writer
  that pauses longer than that in the middle of a file can still be captured mid-save. A later sync
  picks up the finished file, and the partial version stays in history.
- A kill was tested at every step, but power loss was not: fsync ordering is by design and by review.
- A download needs room for its chunks twice over for a moment: the cache, then the file.
- Every file a sync downloads is read back once by the next scan.
- A full tree is held in memory during a sync, and `export` and `restore` replay the whole journal.
- macOS and Windows names and metadata have a place in the format but no implementation.
