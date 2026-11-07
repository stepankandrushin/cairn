# Changelog

## 0.1.6

**Added:**

- Updates. Each daemon asks its release's feed about once an hour and installs a newer release itself, `cairn` and
  the tray beside it, and each restarts into it, a daemon between syncs, so a release reaches every device within
  the hour with no permission asked again. `cairn update` says what is installed and what the feed offers; `cairn
  update now` installs at once and waits for the daemons; `cairn update auto|notify|off` sets how (the tray's
  Settings tab too, with an Update now button and the menu saying when one is available); `cairn update rollback`
  goes back. Only a release signed with the project's own Ed25519 key is installed, each file checked by its size
  and SHA-256, and on macOS a bundle signed by the team of the one it replaces. The new program must run and say
  its version before it is put in place, by a rename, with the one before kept; a kill or a full disk at any step
  leaves a whole program, and a version on which a daemon keeps dying before it syncs is taken back by itself. A
  release on a newer protocol waits until the server speaks it; the server is never updated by itself. A build of
  one's own updates from nobody's releases unless it names a feed and keys of its own. `cairn-release` makes the
  key and signs a release's `update.json`; scripts/release-macos.sh, release-windows.ps1 and release-update.sh
  build, sign and describe the macOS and Windows halves. This release is the last to be installed by hand.
- On macOS the daemon can run from its own app bundle, Cairn Sync.app, which `tray/macos/bundle.sh` makes around the
  `cairn` binary, so that System Settings shows it as Cairn Sync with its icon, in Login Items and in the privacy
  lists, where it was a bare `cairn` with the generic executable's tile; the Local Network prompt names it and says
  why it asks. README.md has the launchd agent for it. The tray's agent now names Cairn.app
  (`AssociatedBundleIdentifiers`), so Login Items shows Cairn rather than `cairn-tray`.
- New icons for both, a cairn of three stones, the daemon's inside two sync arrows, drawn on macOS's icon grid and
  compiled into an asset catalog when Xcode is installed, as macOS 26 and later draw a plain icon on a gray tile at
  small sizes. `bundle.sh` signs with `CAIRN_SIGN`, an Apple-issued certificate, with which Login Items ties each
  agent to its app and macOS keeps the Local Network answer from one build to the next.

**Fixed:**

- A device joining a big tree wrote its state many times over: a Windows device on an HDD joining 468k entries
  wrote 70.7 GB in the first round for a state of 3 GB, for 15 MB of files it lacked. Each statement on a table
  with triggers kept its journal in a temporary file, and rows went in at random keys through a page cache of
  2 MiB, which sent each page to the WAL again each time it was dirtied again. The journals are now in memory,
  every write goes in the order of the table's keys, the page cache is 32 MiB, and a note in a commit no longer
  rewrites the row of what it is about. A join of 40,000 files on Linux writes 1.2 GB where it wrote 7.6 GB.
- A join then read every file again: the sync set the server's mode or mtime on each, which counted as a change
  that only a read could vouch for, and Windows reports a fixed mode, so there every file looked changed, though
  most were not touched at all: 190 GB read again on that device, an hour. A file the sync left as the scan found
  it now keeps its record's word, and on Linux one whose metadata it set is watched from before its check against
  the scan, for any write, open, writer's close or later change of metadata, with a writer that had it open all
  along found by the lease the kernel refuses: none of the 40,000 is read again, and the join writes 0.84 GB. What
  a scan or an upload reads of a file is the first the cache gives up (Windows), or leaves it at once (Linux), so a
  scan of a tree bigger than memory no longer pushes the state out, whose every step then waited on the disk.
- An edit that landed on a file in the moment a sync set its metadata, between the check and the record, was
  taken, mtime and all, for what the sync had set: the content went up, but its mtime read as the pair of the
  server's and never did, and the devices kept different times. It is now the next round's to take up.
- A device whose sync rounds take longer than another device takes to commit never came to agree: a round that
  applied anything had to be confirmed by another, which found a newer commit to apply, until after 12 rounds the
  sync gave up, "the tree kept changing", exit 3, having agreed on nothing, so the next sync looked at the whole
  tree again. A Windows device on an HDD joining 468k entries beside one committing every 30 s or so ended so
  twice, and a Mac's daemon went round for an hour. The round that confirms an apply now reads no new commits,
  so the sync agrees as of the sequence it applied and leaves what came after to the next: such a join beside a
  device committing every 0.6 s now agrees in 3 rounds where it ran 12 and failed.
- The check at the end of every walk, which catches an entry moved from a folder not yet listed into one already
  listed, reopened each folder from the root, a level at a time. On Linux it now opens each in one `openat2` that
  refuses any symlink on the way, and compares the inode as well as the times: on a tree of 27,550 folders a walk's
  opens, dups and closes fall from 325,476 to 165,374.
- A daemon's every sync walked the whole tree, the ones that take what changed alone too: a stat of every entry
  and a listing of every folder, then a second pass that reopened each folder from the root, 15 to 30 s a round
  on a Mac with 472k entries and 46 to 83 s on a Windows HDD, so rounds rarely fitted between another device's
  commits. The watcher now marks the folder each event was in, and a daemon's round on what changed alone lists
  those, the folders of whatever the state marks as written since or the journal names, and the folders above
  them, and takes everything else from the last agreement. Beside a device committing every 2 s, a daemon on a
  tree of 227,551 entries now lists 3 folders in each sync's 2 rounds, some 260 stats where it made 620,000, and
  keeps up, 28 syncs for 33 commits at 1.1 s of CPU each where it managed 16 at 2.3 s; slowed to 1.5 s a round
  beside a commit a second, it agrees every time, where it gave up after 12 rounds. Whatever the watcher may have
  missed, an overflowed queue, dropped events, a watch that ended, a rules file saved, sends the next sync to the
  whole tree, and so does every sync while a folder is not watched or the watcher has stopped, the daemon's first,
  the hourly reconcile and the daily check; an event the kernel never sends, as for a write through a mapping,
  waits for those. The daemon's "synced to" line now says how many rounds the sync took, how long its scans took, and
  how many folders it listed.
- The tray's window showed times on the webview's locale clock, so `04:52 PM` in Recent activity on a Mac set to
  24 hours whose locale has 12, beside `synced at 16:52` from the tray itself. Every time the tray shows, in its
  window, its menu and `--probe`, is now written in one place, on a 24-hour clock unless a person chooses 12 in the
  window's new Settings tab (also the menu's Settings…, or `--open settings`). The choice is kept in `tray.json`
  beside the registry of roots.

## 0.1.5

**Fixed:**

- On macOS the daemon woke and scanned for every write under a folder the rules leave out, such as a build
  folder: its watcher asked the rules about the written path itself, which a rule naming only the folder does not
  match. It now asks about each folder above it too, as on Windows; Linux never watches an excluded folder.
- A sync stopped at the first error the network gave, however passing: a first upload of 842k entries from macOS
  failed after 98 minutes on "No buffer space available (os error 55)", a moment's shortage of socket memory, and a
  second run finished it. A request the network fails in a way that says nothing about the request (no socket
  buffers or memory, a connection reset, aborted, refused, timed out or closed before the whole answer came, as a
  server restarting closes it, a name that could not be looked up, a network or host out of reach, or 502, 503 or
  504 from a proxy) is now tried again: after 0.5, 1, 2, 4, 8, 16 and 30 s in a sync, after 0.5, 1 and 2 s in any
  other command, and for about three minutes at most however slow each try. The command says each try on its error
  output, the daemon's log says a request's first new try and how it ended, and the sync's summary counts them;
  past that the sync fails as before, and the next one delivers what it queued. A refusal (4xx), the server's disk
  full (507), an answer that makes no sense, or a server that has not answered in ten minutes is never tried again.
- On macOS, and on Linux in a build against glibc, the daemon held what a sync had used, up to that sync's peak, for
  the pause after it and 30 s more: gigabytes for about a minute after a big tree's sync. It now gives it back as the
  sync ends. The Linux release binaries did not hold it: musl gives freed memory back by itself.
- The daemon's daily check held the tree twice, deciding every round the full way beside the round itself: on
  842,000 entries 17.4 GB and over a minute. It now checks the bookkeeping instead: the record that syncs carrying
  a change rest on, rebuilt from the state, against the one kept, object by object. An object the record vouched
  for that the state does not is said in the log as a defect and taken up in the same sync, as the full sync
  would. The check now takes 1.15 GB and 7 s there. Whether a round decides as the full sync would is still
  checked on every sync of the test suite, and no longer on devices.
- The first sync after an upgrade, after a change of rules or configuration, or after a run that did not finish,
  went the whole way: 7.8 GB and a minute on 842,000 entries. It now rebuilds the record from the state, its base,
  its rows and a scan, in one pass, and then takes what changed alone, and says so and why: 1.2 GB and 8 s there.
  So does a sync whose scan saw something move under it, which went the whole way too. A first sync, a dry run,
  copies kept by `cairn exclude` and a change to more than a quarter of the tree still take the whole tree.
- A file rewritten in place had a sync look at every other name in its folder too, so that 1,000 rewrites in big
  folders looked at 137,000 entries and held 3.4 GB. A rewrite takes no name, so a sync now looks at the file and
  the folders above it, unless the file changed on both sides: those 1,000 now look at 3,300 entries and hold
  1.1 GB, and one edit 83 entries where it looked at 9,159.

**Known limits:**

- A first sync holds the whole tree in memory: at about 840,000 entries, 9.2 GB for a first upload and 9.1 GB for
  a first download. A later release bounds it.
- Every other sync, the daily check and the first sync after an upgrade included, peaks at about 0.9 to 1.2 GB at
  that size. A later release brings it down further. Not measured on Windows.
- While a copy kept out by `cairn exclude` exists, every sync goes the whole way.
- A first sync of a big tree still takes hours per device over a slow path, most of it placing files one at a time.
  Off Linux every placed file is still read back once.

## 0.1.4

**Fixed:**

- On Windows, a conflict on a hard-linked file, killed while its conflict copy was taking its names, brought back a
  name deleted on another device, holding this device's version: the next sync never finished moving the copy's
  names, as NTFS gives no file a creation time that marks it. Linux and macOS were not affected.
- On Windows the daemon did not notice its root renamed away, or another folder put at its path: it said
  `synced` until its hourly full sync, and once the root was back in another folder, as a disk mounted back on its
  folder is, changes in it waited for that full sync too, as the watch stayed on the folder that had gone. A second
  watch, on the folder above the root, now notices at once, and the root's watch moves to what is at its path.
- A file that came down was flushed to disk twice, after its data and again after its metadata, and each flush
  has the disk empty its cache. It is now flushed once, after both, still before the rename that puts it in place.
  A first download places a small file on an HDD under Windows in about 20 ms where it took 29, and on a busy
  Linux disk array in about 18 ms where it took 31: most of what is left is that one flush.
- A sync that carried a change still went the whole way, twice, at a cost that grew with the tree and not with
  the change: on 840,000 entries about a minute and up to 8.6 GB a round. A round now decides only about what
  changed, here or on the server, and what is next to it: the other names in its folders, the folders above it,
  what is in a folder that changes, every name of a hard-linked file. Everything else is as the last agreement
  left it, which the state now keeps a compact record of, with triggers that mark whatever any version writes
  since. Its cost is the scan and what changed. Where the record cannot vouch for that, after an upgrade, a
  change of rules or configuration, a run that did not finish, or a change to a quarter of the tree, the sync
  goes the whole way and says why. Once a day the daemon decides every round the full way as well and takes
  that answer, and anything the incremental round would have decided otherwise is said in its log as a
  defect; `--full-sync-secs` sets how often.
- A folder that a rule in `.cairn/ignore` left out could leave this device's disk while the sync said it stayed
  out. It took one this device had synced, put where the rule matches by a move here, and a sync that undid that
  move, as one does when another device's move would make a cycle with it. Out from under its rule, the folder was
  brought back into view by this device's own record of it, and went the way the server's had, deleted by that
  move as anything moved into an excluded path is, but for what had been edited in it, which went up. What was
  kept out was recorded for an inode that no longer existed, and `cairn status` listed it at a path that had gone.
  It now stays here, out of view and listed where it is, until `cairn include --merge`, and the same for a file.
  Where another device has added to the folder meanwhile, each sync says the copy here is in the way of the
  server's and to move it or `cairn include --merge` it, which merges it as new. A file with another name here
  cannot stay out without taking that name with it, and comes back in.

**Known limits:**

- The daemon checks the incremental path once a day by deciding a sync the whole way as well, and holds both
  decisions at once: at about 840,000 entries, measured after the release, about 82 s and a peak of 17.4 GB,
  once a day. Not for a tree that size on a machine with less memory; a later release bounds it.
- A first sync, and the first sync after an upgrade, hold the whole tree in memory: at about 840,000 entries,
  9.2 GB for a first upload and 7.5 GB after an upgrade. On macOS one such upload failed once with "No buffer
  space available" and finished when run again.
- While a copy kept out by `cairn exclude` or a rule exists, every sync goes the whole way.
- A first sync of a big tree still takes hours per device over a slow path, most of it placing files one at a time.
  Off Linux every placed file is still read back once.

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
