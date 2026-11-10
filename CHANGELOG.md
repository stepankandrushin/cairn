# Changelog

## 0.1.15 (2026-11-11)

**Changed:**

- The server keeps each revision's body once, as a binary record, where it kept it twice as JSON: in the
  commit and again in the table of revisions. Its database is schema 4, about a quarter of the size (a real
  one, 1.3 GB, became 361 MB), and reading it is a little faster. What it sends is the same, byte for byte.
  A server of this release moves a database of schema 3 when it starts, in one transaction, after checking
  for room, and gives the freed space back; `cairn-server downgrade` moves it back to schema 3, for a server
  from before.
- `cairn-server fsck` also checks that each revision is named by the commit that made it.
- A commit's notes of one text, the losers of a merge, go as one note naming each object, where they went
  as one to each: a device joining on another umask wrote the same "mode: kept 664 from the server; this
  device had 644" for every file. `cairn log` says how many each names. A client sends them so only to a
  server of this release, which says it takes them (`/info`'s `note_groups`); to an older one, a note to
  each object as before. A client from before reads one as a note of its first object.

**Fixed:**

- The web's Recently deleted opens at once and lists all of the last 30 days, from an index the server
  keeps in memory, rather than reading the journal back on each request, which took minutes on a busy disk.

## 0.1.14 (2026-11-10)

**Added:**

- A web interface, at the server's address. Sign in with a token, then browse each folder; upload files and
  folders, in parts any proxy passes; download a file, or a folder as a zip; preview images, video, audio,
  PDFs and text; see a file's history and restore any revision; bring back what was deleted lately; make
  folders, rename, move and delete. A change made there is a commit like any device's: refused, with the
  reason, when the file or folder changed since it was looked at, never forced over it. A file replaced from a
  browser keeps its mode, owners, extended attributes and streams.
- Sharing from the web. A folder's members are listed, and its managers change them. A manager shares any
  file or folder in it with another user, to read or to write, or by a public link that needs no account,
  reads only and may expire: each reaches what it names and nothing above it, follows it when it moves, and
  stops when it is deleted or its maker no longer manages the folder. "Shared with me" lists what others
  shared with you.
- Signing in from a device, with nothing to type: the tray's Open in browser, and `cairn web` in a synced
  folder, open the web interface signed in as that device, by a ticket that works once within two minutes.
  The browser keeps a session the server signed, never the token, and removing the token signs it out.
- Endpoints for all of it, which any client may use (docs/protocol.md, "Web"), and schema 3 on the server,
  which adds the table of shares.

## 0.1.13 (2026-11-09)

**Fixed:**

- The tray's "Open in browser" opened a page saying only `missing or wrong token`, which read as a client
  that had lost its token. The server's address now answers a browser with a page saying it is a Cairn
  server, that there is no web interface yet, and where the files and their history are; it names no
  version, user or folder. Everything else on the server still needs a token.

## 0.1.12 (2026-11-09)

**Added:**

- Users. A server holds users, each with tokens of its own, one per device at best, and folders, each its own
  tree, history and set of chunks. A user reaches only the folders given to it, at one of three levels:
  `read`, `write`, or `manage`, which is write and also saying who else has access. A folder a user has no
  access to answers as one that does not exist. The server's administrator manages all of it with
  `cairn-server user`, `token`, `folder`, `grant` and `revoke`, while the server runs or not; a folder's
  managers share it with `cairn access grant` and `revoke`, never leaving it without a manager. `cairn
  folders` lists what a token reaches, `cairn init --folder` makes a root of one, and `cairn log` says which
  user made each commit.
- Read-only folders. Everything in one comes down, and nothing made on the reader's devices goes up or is lost
  there: an edit, a new file or a rename stays as it is, listed by every sync as `read-only`, until it is
  moved out of the root or the user may write, when it goes up like any change; something deleted there comes
  back from the server; where the server changes something also changed there, both stay, the server's at the
  name. Such a sync completes as any other and costs no more, and the daemon does not try it again for
  nothing. A commit queued before a user lost write access is taken back, and what it held stays.

**Changed:**

- The server keeps tokens as their hashes, in its database, and no longer reads the data directory's `token`
  file once it has been upgraded: the first start of this release makes that token the user `admin`'s,
  managing every folder there is, so every device using it goes on as before. Clients from before this
  release work with it unchanged, and are refused uploads to a folder they may only read, with the reason.
- `cairn-server init` makes the user `admin` with the folder `default`, and refuses a directory that already
  holds a server's data.

## 0.1.11 (2026-11-09)

**Changed:**

- The tray's menu keeps what is used most next to its icon on both systems: Settings…, then Recent activity,
  Problems, Conflicts and Recently deleted…, then each root's actions, then what each root is doing, with Quit
  last. macOS drops the menu from the top of the screen and Windows raises it from the bottom, so the order
  runs down on macOS and up on Windows; before, it was the same list on both, and on macOS the status lines
  came first and Settings near the bottom.
- Recent activity in the menu is a submenu listing the changes themselves, every root's, newest first, up to
  100. A click shows the file selected in Finder or Explorer, or its folder once it has gone; a right click
  copies its full path (on macOS a Control- or Option-click as well). The window's Recent activity does the
  same, and says what it copied.
- Start at login moved from the menu to the window's Settings tab.
- Escape closes the tray's window, as its × does. In Recently deleted's search box, with something typed, it
  clears the search first.

**Fixed:**

- The tray's menu vanished while the pointer was on it, most often during a sync. The menu was made anew
  whenever what it shows changed, which during a sync is every second or two, as its line counts the
  seconds; on Windows that destroyed the menu on screen, which then shrank to an empty 6 by 6 pixel box. The
  menu is no longer touched while open, and is brought up to date as it closes.

## 0.1.10 (2026-11-08)

**Fixed:**

- On macOS 26 and later an idle daemon held much of what its biggest sync had used, for as long as it ran: 0.8 to
  1.9 GB on a tree of 568k entries, shown as Cairn Sync in Activity Monitor, with about 1 MB of it in use. The
  system's allocator keeps the blocks of 64 KiB and more that a program frees, for later, and the call the daemon
  made at each sync's end to give memory back frees none of them there. The daemon now starts itself again, in the
  same process, with that cache off (`MallocLargeCache=0`), and holds about 15 MB between syncs on that tree. A
  daemon started with the variable set already, to any value, is left as it is.
- On Windows an idle daemon could hold a few hundred megabytes it no longer used, shown as Cairn Sync in Task
  Manager: 441 MB between syncs of a file or two on a tree of 568k entries. Once a big sync (the first after a
  start, or one that looks at the whole tree) has grown the heap, what each later sync frees stays the process's,
  and the daemon gave back nothing on Windows. It now compacts the heap and empties its caches as each sync ends:
  on a tree of 200k files, 13 MB between syncs where it held 24 to 132 MB, and in about 5 ms.
- On Linux an idle daemon held several times what it used: 102 MB between syncs on a tree of 570k entries. All
  but 1 MB of what it kept in use was its watcher's record of each folder it watches, by path, as a vector of
  names: on 65k folders half a million small blocks, made again by each walk of the watches, which every
  reconcile does among a sync's own blocks. Spread over the heap like that, they kept it from giving back what
  the syncs freed, and the allocator of the Linux build has no call to make it. Each path is now one run of bytes
  in one buffer, and a walk that finds a path as it was makes nothing; the watcher's marks of where something
  happened are one block each too. On a tree of that one's 65k folders, with a reconcile every 2 minutes: 25 MB
  between syncs where it held 54 to 60 MB, with 16 MB in use where it had 24.
- On Linux each update a daemon restarted into, in the same process, kept the version before's inotify watches:
  a whole tree's more at each update, counted against the kernel's limit on watches until the daemon stopped,
  and enough updates would reach it, after which every sync lists the whole tree. Every descriptor the daemon
  holds is now closed when it restarts.

## 0.1.9 (2026-11-08)

**Added:**

- Recently deleted, in the tray and as `cairn deleted` and `cairn undelete`: what is deleted and still deleted, from
  the server's history, newest first, with when, by which device, its size, and for a folder how many files went
  with it, listed once. Restore brings it back on every device as it was when deleted, a folder with what was
  deleted in it, replacing nothing: where its name has been taken since, it comes back beside it under a restored
  copy's name. It is one commit, so a kill leaves nothing or all of it. The tray's tab has a search, a choice of
  7, 30 or 90 days, ticks and Restore selected, and draws 200 at a time. It is where a file an antivirus removed
  is found too, as that is a delete to every device, and it needs nothing of any antivirus. On a tree of 2.66
  million journal operations, the list takes 8 s and 234 MB, where `cairn history` takes 20 s and 1.7 GB; a
  daemon keeps its index for 10 minutes, so what follows takes only what is new.

**Fixed:**

- On Windows a file with a long name, 77 bytes or more, could stay at an old version for good, and could not be
  deleted there either, failing every sync with "destination cannot represent this (The filename, directory
  name, or volume label syntax is incorrect. (os error 123))". The temporary name it is moved aside under carries
  the start of its own name, cut to fit, and where the cut fell just after a dot or a space Windows refused it.
  The cut now never ends in one. Each attempt also left its download beside the file, a leftover that sends the
  next sync through the whole tree to settle it rather than only what changed: on a large tree on a hard disk,
  18 minutes to over an hour and a half a sync. A download refused there is now removed at once.
- A file the system refuses this device is held as it was, said by name, and never sent as a delete or a change,
  to read on one device or to write on another: an access list, another program holding it, a program that guards
  files, a failing disk. Reads were held already; now a refused write says "waiting for permission" with the
  system's own words instead of "destination cannot represent this", Windows' codes for a file blocked or removed
  as a threat (225, 226) and for a policy, a lock or a failing disk are told by kind, with the hint on Windows
  that another program or security software may block it, and `cairn status` lists what the daemon's last sync
  skipped. A refused file no longer holds every other change back for up to a minute in the daemon: it is tried
  again on the backoff, and clears by itself once let in. A tree most of which is refused pauses, as one most of
  which vanished does, and nothing is sent until it can be read.
- On macOS the tray's icon went from the menu bar once an update replaced the tray, and stayed gone while it ran:
  the tray started its new program in its own process, by exec, and the menu bar of macOS 26 and later gives a
  program exec'd in another's process no icon. It now starts again as a new process, under its LaunchAgent by
  exiting for launchd to start it. A tray that an older one exec'd does the same as it starts, so the update to
  this version brings the icon back by itself.
- A sync on a hard disk sat for up to a quarter of an hour in "loading what this device knows", and as long again
  in its first round. SQLite reads the state a page at a time, in the order of each table's keys, which in a state
  that syncs have rewritten is all over the file; the daemon holds nothing between syncs, so once the system's
  cache had dropped the file, every page was a seek: a Windows PC whose tree is on a hard disk read it 4 KiB at a
  time, 166 reads a second. A sync now reads the state's file through once as it starts, in the file's order and
  in pieces of a MiB, so the disk goes at its sequential rate and what follows finds it cached. On Linux, with a
  state of 40,000 files joined and rewritten four times (256 MB) dropped from the cache, a sync after a rule changed read it
  in 244 reads in one sequential run before loading, where loading had taken 5,311 reads at 4,329 places and its
  first round another 14,541 at 7,461; a sync with nothing to do, 10,616 at 6,605. A state over a quarter of the
  machine's memory, which the cache would not keep, is read as before.
- The state of a sync root never shrank. A join, or any sync that rewrites or deletes much, leaves its file
  mostly free pages, which SQLite reuses but never gives back: on the devices of a tree of 558,000 entries the
  state stood at 1.6 to 2.7 GB, over half of it free, and each sync's read of it went through all of it. Once a
  sync that leaves over half of the file free has been reported, the state is now rewritten without them,
  every table in key order, which only the daemon's log mentions. On copies of two of those states, 2.34
  GB came to 0.98 GB in 3.5 s (Linux, NVMe) and 2.65 GB to 1.08 GB in 7.7 s (an M1's SSD), with 70 to 85 MB more
  memory and about twice what the state holds of room for the moment. Where a disk lacks that room it is put
  off, and the log says so once; a kill or a full disk partway through leaves the state as it was.
- The state held each file's details two and three times over, as JSON text: the base node and the row each in
  a table of its own, the row repeating the node's content, metadata and entries, and repeating its own metadata
  as `scan` and `applied`, with every hash and ID in hex and every name in base64. A row and its node now share
  one SQL row as binary records, the row written against the node, so it holds only what differs from it
  (usually the device and inode alone), and manifests are stored as their fields, their canonical JSON made
  again when one is read and checked against its hash. The first build of this layout to open a state moves it,
  in one transaction, after checking the disk has room; the next sync rebuilds the ledger and compacts the file.
  On copies of two states of a tree of 568,000 entries, what their nodes, rows and manifests took went from 1.0
  GiB to 144 MiB in a move of 3.4 s (Linux, NVMe), and from 1.2 GiB to 188 MiB in 7.7 s (an M1's SSD), every
  record decoding to what it was; compacted, the files came to 131 and 174 MiB before the ledger, from 2.34 and
  2.65 GB. A kill or a full disk during the move leaves the state as it was, and a disk short of room stops the
  sync until there is room. An older build refuses a state this one has moved.
- On Windows, Task Manager showed the daemon as a bare `cairn.exe` with the generic program icon and no
  description, beside the tray's "Cairn": the gap macOS had until 0.1.8. Windows takes a program's name and icon
  from its file, and `cairn.exe` had neither. It now carries the daemon's icon, the cairn inside two sync arrows
  that Cairn Sync.app has, and version information naming it "Cairn Sync", product "Cairn", at the release's
  version, so Task Manager and the file's Properties show it so. The commands are the same program and show the
  same way.

## 0.1.8

**Fixed:**

- A join took away the execute bits of programs. When a device joined a tree that a tool carrying no modes had
  synced until then, and its copy of a script or a program had the execute bit where the server's lacked it, the
  join took the server's mode whole: the program no longer ran there, and `git status` called it changed. The only word of it was one entry in
  the activity log among one for every other file whose mode differed. A file's execute bits now count as fields
  of their own at a join, as an extended attribute only one side has does: a bit only one side has stays, and goes
  to the others, where the mode reads the file. A folder's mode, which says who can enter it, is the server's as
  before.
- A Mac kept the wrong time on a file with a resource fork, and said it was synced. When a sync gave a file only
  its resource fork, at a join or as a change on another device, writing the fork moved the file's modification
  time to the present, and the time was set only where it had differed before: the moved time stayed, taken for
  the server's, and never went up. The time is now set after any extended attribute written.
- Taking in a big folder that was already on the disk took about four times the memory it needed. When a rule
  came off a folder a device held a copy of, the sync took it in on the part of the tree that changed, then counted
  each file twice in the round that confirms it, under the ID the scan had given it and the server's, and took the
  whole tree there instead: a Mac taking in 82,000 photos in a tree of 557,000 entries peaked at 8.9 GB, where the
  rest of the sync held about 2.4 GB, and read every file whose mode it had set a third time. That round now counts
  only what the device holds: on Linux the same join of 16,000 files in a tree of 124,000 peaks at 0.56 GB where
  it peaked at 1.36 GB.
- On a Mac, Activity Monitor showed the daemon as "cairn", with no icon, beside the tray's "Cairn": the two
  differed by a capital letter. Activity Monitor gives a process its bundle's name and icon only once
  LaunchServices knows it as an app, and launchd starts the daemon without AppKit, which is what tells it. The
  daemon now checks itself in when it starts on a desktop, so it shows as "Cairn Sync" with its icon, and
  Cairn Sync.app is now background-only rather than a UI element: still no Dock icon, menu or window, and
  opening the bundle can no longer bring to the front a process with no event loop. Opening it while the daemon
  runs starts nothing, where it used to start a `cairn` that printed its usage. The bundle's identifier and
  signature are unchanged, so no permission is asked again. It adds about 2 MB to the daemon's memory, and
  nothing to any other command; started over SSH or through the `~/.local/bin/cairn` link, the daemon shows as
  before.

## 0.1.7

**Fixed:**

- A long sync looked like a hung one. A Mac's daemon took in a folder its rules had left out until then, 73,498
  files and about 700 GB already there, and read each to hash it: one thread busy for 54 minutes, nothing in the
  log from the line before to the one after, and `cairn status` saying "synced", the last sync's outcome, with no
  word that one was running. While a sync runs, `cairn status`, `cairn status --all` and the tray now say so and
  what it is doing: its phase (loading the state, the server's journal, looking at the tree, working out what to
  do, applying, uploading, committing, recording), how far it has got there (entries looked at of about how many,
  steps applied, chunks uploaded, commits sent), the files and bytes it has read to hash them and the file it is
  reading, the bytes downloaded and uploaded of how many, how long it has run and when anything in it last moved.
  The local API's status carries the same as `progress`, sent again every 2 s. A sync still running a minute after
  it began says it in the daemon's log, and again every 5 minutes; one in which nothing has moved for 10 minutes
  says so there, once, and again when it moves; and the line at a sync's end says what it read to hash and how
  long it took, now also for a sync that changed nothing but took a minute.
- GitHub's CI fails a test that cannot run there, where it passed with its skip hidden in the test's output. It
  found one that had never run, which now does.

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
