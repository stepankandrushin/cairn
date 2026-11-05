# Changelog

## 0.1.3

**Fixed:**

- A folder removed on one device while something in it was edited on another comes back to the first. Stopped
  before its last pass, by a kill or a full disk, it stayed private (mode 0700) there for good, with no change
  shown: that device took 0700 for a mode it keeps. The server and the other devices had the right mode. It now
  gets it on the next sync.
- A folder that arrived and was stopped before its last pass, by a kill or a full disk, also stayed at 0700 for
  good if it could not be read when it next moved, renamed on another device or here: what could not be read was
  taken for what the folder held. Only what was read in full is now recorded as what a folder holds.
- `cairn init` with a server it could not reach, or one that refused its token, still made the folder a root and
  listed it, so the same init run again called it a root already. The server is now asked first, and an init it
  does not take leaves the folder as it was and lists nothing.
- Every sync, with something to do or nothing, loaded, merged and scheduled the whole tree in memory: on a tree
  of 840,000 entries about a minute of one core and 10.6 GB, of which reading the disk was 6 seconds. A sync that
  finds what the last sync with nothing to do found, the same state, server, rules and tree, now stops after its
  scan, which reads no file. The first sync after a change still goes the whole way, and writes down what it
  agreed on for the next. The state file gets triggers that count every write to it, which an older version
  opens and moves too.
- A sync that does go the whole way holds less at once: no copy of the server's tree when nothing came from it,
  and neither the scan nor what the merge was given once it is done with them. The daemon gives that memory back
  to the system once it has been idle half a minute past its pause, instead of keeping it until the next sync.
- The daemon's watcher held every subfolder waiting its turn open, and in a folder of thousands of them ran out
  of open files at the usual soft limit of 1,024: the rest went unwatched, and the daemon fell back to a full sync
  every 5 minutes. It now holds one folder for each level down, the daemon takes the hard limit on open files at
  start, and it names the first folders it could not watch, and why.
- A device that applied a change synced once more for its own writes, and one whose sync took in commits after
  its own synced once more for the feed's news of them. A wake from before the scan a sync agreed on now wakes
  nothing: that scan saw it.
- The pause after a sync was twice its scan alone, a fraction of a sync of a big tree, so the daemon was busy most
  of the time while changes kept coming. It is now twice what every sync repeats, loading the state, the scan and
  the merge, not the data it moved, and 30 seconds at the most, so that a change written as a sync ends waits at
  most about that and one sync.
- Every sync downloaded the whole commit at its cursor to check that the server still has its history, megabytes
  after a big one. `GET /changes` takes `ops=false`, which answers an entry without its operations; an older server
  sends them anyway.
- A device's first sync of a big tree took a night or more: on a SATA disk, for a tree of 842,000 entries and
  53 GB, about 5.6 hours to upload and 6 hours to download, 11 over a 15 ms path, with twice the tree's size free.
  It now takes 45 minutes and 3 h 43 min there, with the tree's size free. The server made each chunk durable on
  its own, two fsyncs one after the other; it now makes an upload request's chunks durable together, their fsyncs
  overlapping, and still answers only once all of them are.
- A big upload scanned the whole tree again for each commit of 20,000 operations. Its commits now go one after
  another from one scan, and one the server refuses stops the rest, so that none after it records a tree the
  server never had.
- A download asked for each file's manifest in a request of its own and stored it, and each file's row, in a
  transaction of its own: a round trip and two fsyncs a file. Manifests now come 2,000 to a request, and they and
  the rows thousands to a transaction.
- A download brought every chunk into its cache before placing anything, and read every file back afterwards. A
  chunk now leaves the cache once the files that take it are written, chunks come a slice of about 1 GiB at a time,
  and a download that would not fit is refused before anything moves, saying what it needs and what is free. On
  Linux a placed file is watched until a later write would show, and its record needs no read unless the watch saw
  something done to it; nothing waits on the watch, so a save of a file that has just arrived is never held up.
- A new device read the whole journal in one answer of `GET /changes`, hundreds of megabytes and twice that of the
  server's memory. An answer now stops at about 16 MiB, always with one entry; every client asks again from where
  it ended.

**Known limits:**

- A sync that carries a change still loads, merges and schedules the whole tree: at about 840,000 entries, about a
  minute of one core and 8.5 GB, twice for each device a change reaches, so a change takes one to two and a half
  minutes to arrive. An idle tree is cheap: 8 seconds and under 1 GB. The next release works on a sync that costs
  what changed.
- A first sync of such a tree still takes hours per device over a slow path, most of it placing files one at a
  time, each with its fsyncs. Off Linux every placed file is still read back once.

## 0.1.2

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
- A macOS client: FSEvents for the daemon, xattrs and holes on APFS. A name APFS takes for another (a decomposed
  name and its composed form, or two cases) waits until it is renamed, as below. macOS is not in CI.
- A Windows client: NTFS by handle, no link followed, names Windows refuses left out and reported. Windows holds no
  symlinks, owners or xattrs: what it does not hold is kept on the server as it was, and a file's read-only
  attribute is its write bits. The failure harness runs there too, though not in CI.
- A tray for macOS and Windows, in tray/, a workspace of its own: each daemon's state, its problems and conflicts,
  pause and resume, and Forget for a root that is gone. It reads the local API and believes only a daemon of its
  own user.

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
- A conflict on a hard-linked file, killed before its copy had all its names: a name deleted on another device came
  back holding this device's version, and the others could read as deleted here. The copy now takes every name.
- An edit caught by a killed sync replacing a hard-linked file split it in two: a name went on holding the edit as a
  separate file, for everyone, and a second kill could add a copy of the other device's version and a false
  "deleted here". The names now stay one file, with the edit kept once beside them.
- A kept edit was said only in the output of the sync that kept it, which a kill could cut short. It is now a conflict
  copy of the file that caught it, listed by `cairn conflicts` on every device and counted in the sync's conflicts.
- A rule made between a kill and the next sync could hide an edit under a temporary name until the rule went, or
  have a download left by the kill taken for an edit and copied once it went. Both are looked at before the rule
  hides them.
- The losing side of a same-field metadata conflict was missing from the log when the sync that applied the winner
  was killed. It is noted before anything is applied, and logged once.
- A disk that filled while a file arrived could leave it in place without an xattr, and the next sync then removed
  that xattr for everyone, the device it came from included. A field that does not fit for want of room now stops
  the run before the file is placed, and a full disk is said as such wherever it shows, SQLite's own error included.
- After a kill, what a folder gives a new file (an access ACL from a default ACL, a set-group-ID folder's group)
  went up as a change, for everyone, and a policy changed before the next sync, `setid` turned on, took a setuid bit
  off the server's record. The filesystem is now asked beside the file, under the policy the run applied by.
- After a kill, a chmod made in that moment never went up if the disk had no room for one more entry, and a dry run,
  so `cairn status`, said there was nothing to do. The sync now stops and says the disk is full, and the dry run
  asks as the sync does.
- An edit to another name of a hard-linked file, in the moment a sync moved that name to the new version, split
  the file in two for everyone: that name became a new file holding the edit, with no conflict copy. The names now
  stay one file, and the edit is a conflict copy in the inbox.
- A file arriving in a folder where another filesystem is mounted inside the tree was written onto that
  filesystem, which the scan does not cross, and every sync after stopped at it. It now waits, reported.

- A file deleted on one device and restored before another had finished applying the delete could be deleted again
  by that other device, which had never deleted it. History kept it.
- A hard-linked file being replaced by a sync could overwrite a save made to its second name in the moment before
  the replacement, and that text was lost. The version here now stays, as a conflict copy.
- A sync interrupted after making a conflict copy could stop finishing for a while, ending in "the tree kept changing
  for 12 rounds", if the copy was then deleted and a symlink or folder took its inode number. Nothing was lost.
- On a filesystem that cannot swap two names in one step, a replacement renamed the new version over the old one,
  and took an edit made in the moment since its last look. The old file is now held under a third name, a hard
  link, and looked at there.
- A device joining a tree the server already had, with the same bytes and metadata of its own, lost the xattrs the
  server lacked: they were removed from its disk and never sent (on a Mac, Finder tags and FinderInfo). What only
  one side has now stays, sent up from here or applied here. Where both have a value, the server's stands and the
  other is in the log, as for any conflict over one field. A join with more such notes than one commit holds lost
  the rest; they now go in the next.
- A device joining with separate equal copies of a file the server holds under several names made a conflict copy
  of each extra name, for everyone. They are now links of the one file here.
- A conflict copy made because a new file here had a name the server already used, as happens at a join, was missing
  from `cairn conflicts`. It is listed now, like any other.
- On a filesystem that takes one name for another (APFS: a decomposed name and its composed form, or two cases),
  something new at a name the server had in the other form went up as a second name, a folder with everything in
  it, and every device that keeps names as bytes then held both. It now waits, reported with both names, until it
  is renamed here to the server's form, and then the two merge.

**Known limits:**

- A big tree costs whole-tree work on every sync: at about 840,000 entries, a sync with nothing to do takes about a
  minute of one core and 10 GB of memory, and a change takes one to three minutes to reach another device. A first
  sync of such a tree takes hours per device. The next release works on this.
- On Linux, at the usual limit of 1,024 open files, the daemon's watcher can leave folders unwatched in a tree with a
  folder of thousands of subfolders, without saying so. Changes there wait for the hourly full sync.

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
