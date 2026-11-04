# Changelog

## Unreleased

**New:**

- `cairn daemon` keeps a root in sync: an inotify watcher, a long-poll change feed that passes through a
  reverse proxy, and a full sync every hour as the safety net. It reports through its output, `cairn status`
  and `.cairn/daemon.json`. A kill at any moment is an ordinary crash, and it pauses and resumes around a
  missing mount without holding the disk busy. This has not yet run on real trees for any length of time.
- `GET /changes` takes `wait`. A client of this version works with an older server, which only makes it poll.
- Ignore rules: `.cairn/ignore`, in `.gitignore` syntax, one device's own. There are still no ignored names by
  default, and a project's `.gitignore` means nothing.
- Selective sync: `cairn exclude <path>` and `cairn include <path>`. An excluded folder stays excluded through a
  rename, and `--remove-local` also removes the copy here, except what differs from the server.
  What a rule covers stays out when a sync moves its folder to where the rule would not match; your own `mv` is
  your choice, as in git.
- The local API: each daemon answers on a socket only its user can open, for `cairn status` and a tray. It
  pushes its status, now with its last 100 changes, takes look now, pause and resume (`cairn pause`, `cairn
  resume`; a pause outlives a restart), and lists open conflicts with their paths here. `cairn status --all`
  shows every root of this user on this machine, from a list `cairn init` and each daemon add to, a root that
  has gone included, until `cairn forget` takes it off; `--json` gives either as JSON, and `cairn ask` sends
  one request.

**Fixed:**

- A device stopped partway through receiving something, by a kill or a full disk, read what it had received as
  its own change. A later change from elsewhere could then be undone with no report (a revert, a chmod back), a
  file deleted elsewhere could come back, and a restored file could get a conflict copy. History kept every
  version. What is on its way is now noted before the first step, and the next scan checks it against the disk.
- A conflict on a hard-linked file, killed before the second name arrived, deleted that name for everyone.
- An edit or an editor's save in the moment before a replacement or removal, followed by a kill at that moment, was
  left under a temporary name and removed as a leftover: the only loss in this list, with an edit made while an
  interrupted hard-link update was being finished, which was overwritten. Both now stay, under a conflict name. A
  leftover from a version before this one still goes, as the name did not say what it held.
- A folder's timestamp moved by an interrupted sync went up as a change, for everyone, in a commit nobody made. It now
  goes back to what it was.
- The first sync of a tree of very many small files could time out on a slow disk, as one request held every chunk
  and the server syncs each to disk before it answers. Nothing was lost, and the next sync went on. A request now
  holds at most 2,000 chunks.
- A swap killed halfway left one of the two files under a "recovered from interrupted sync" name. It now goes where
  it was going.
- A false "mode kept" note in everyone's log after a kill while a new folder arrived, and an unchanged file brought
  back after a kill while its folder was being detached from one that moved out of view.

- A file deleted on one device and restored before another had finished applying the delete could be deleted again
  by that other device, which had never deleted it. History kept it.
- A hard-linked file being replaced by a sync could overwrite a save made to its second name in the moment before
  the replacement, and that text was lost. The version here now stays.
- A sync interrupted after making a conflict copy could stop finishing for a while, ending in "the tree kept changing
  for 12 rounds", if the copy was then deleted and a symlink or folder took its inode number. Nothing was lost.

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
