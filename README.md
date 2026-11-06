# Cairn

Self-hosted file sync with versioned history that keeps everything about your files: symlinks, modes,
owners, xattrs, ACLs, capabilities, hard links, sparse files, and filename bytes. A central server keeps
every version; clients sync a real filesystem. Files only: no groupware, no app platform.

**Status: 0.1.1, early.** Linux only, one user. It has been through a harsh failure harness but has no long
history on real data, so keep another copy of anything you put through it. The daemon is newer than the
release and has run on no real tree yet. See
[CHANGELOG.md](CHANGELOG.md) for what is and is not there, and [PLAN.md](PLAN.md) for where it is going.

## Why

Cairn focuses on preserving the whole filesystem and its history. This comparison covers desktop
filesystem sync and the services needed to run it; metadata support depends on the operating system,
filesystem and privileges.

✅ Supported · ⚠️ Partial or conditional support · ❌ Not supported

| Feature | **Cairn**[^cairn] | Nextcloud[^nextcloud] | Seafile[^seafile] | Syncthing[^syncthing] |
|---|---|---|---|---|
| Installed application size (macOS)[^macos-size] | **17.1 MB** — tray + sync daemon, Apple Silicon | **414.4 MB** — desktop client, universal Intel + Apple Silicon | Not measured | Not measured |
| Application download size (Linux x86-64)[^downloads] | **6.3 MiB** — CLI + server archive | 60.3 MiB — desktop AppImage; server separate | 13.7 MiB CLI / 184.3 MiB desktop AppImage; server separate | 13.4 MiB — archive, including web UI |
| RAM use / requirements[^performance] | Chunk-buffered file I/O; tree and chunk metadata also consume RAM; no measured baseline | Server: 128 MB minimum / 512 MB recommended per PHP process, plus other services | CE server stack: 2 GB minimum | Depends on files, peers and concurrency; buffers and Go memory target are tunable |
| Implementation languages | **Rust** — CLI, daemon and server | C++ desktop; PHP server | C sync engine; C++ desktop; C/Go server; Python/JavaScript web UI | Go sync engine; HTML/JavaScript web UI |
| Run without a separate database server or language runtime | ✅ Native binaries with embedded SQLite | ❌ PHP and a web server; usually a separate database | ❌ Server stack includes a database, cache and web services | ✅ Native binary; no central server required |
| Include `.git` directories by default[^exclusions] | ✅ Included | ✅ Included | ✅ Included | ✅ Included |
| Sync `.DS_Store` and `Thumbs.db` by default[^exclusions] | ✅ Included | ❌ Excluded by default patterns | ❌ Excluded by built-in patterns | ✅ Included |
| Preserve symlinks | ✅ Preserved as links | ❌ Skipped | ❌ Targets copied instead of preserving links | ⚠️ Preserved, except on Windows |
| Preserve full filesystem metadata | ✅ Modes, owners, xattrs, ACLs, hard links and sparse layout | ❌ No full Unix metadata round-trip | ⚠️ Executable bit on Unix; not full modes or ownership | ⚠️ Modes; owners and xattrs opt-in; hard links not preserved |
| Sync speed: avoid whole-file re-uploads[^performance] | ✅ Incremental scans; only new content-defined chunks transferred | ❌ Whole changed file uploaded again | ✅ Only new content-defined blocks transferred | ✅ Only changed fixed-size blocks transferred; downloads from peers in parallel |
| Versions share chunks | ✅ Shared chunks | ❌ Whole-file versions | ✅ Shared blocks | ❌ Whole-file archives of changes received from peers; off by default |
| Keep both sides of a conflict | ✅ Three-way merge; both versions kept, with an inbox | ⚠️ Local conflict copy; not uploaded by default | ✅ Both versions kept; one renamed as a conflict file | ✅ Both versions kept; one renamed as a conflict file |
| Guard against missing disks / mass deletion | ✅ Mount checks; sync pauses and asks | ⚠️ Confirmation before mass deletion | ⚠️ Confirmation above a deletion-count threshold | ✅ Sync stops if the folder's `.stfolder` marker is missing |

[^exclusions]: `.git` is not a default filename exclusion in any of these tools. The filename rows
    describe default rules; user-configured exclusions can change what syncs. Cairn also includes
    `node_modules` and `.venv`, and does not apply a project's `.gitignore`. Its exclusions are opt-in
    through `.cairn/ignore` or `cairn exclude`; its own root `.cairn/` directory and internal temporary
    files are reserved and are not synced.
[^macos-size]: Installed bundles measured on an Apple Silicon Mac on 2026-11-07:
    Cairn 0.1.5 — `Cairn.app` 9.1 MB + `Cairn Sync.app` 8.0 MB; Nextcloud 34.0.5 — `Nextcloud.app` 414.4 MB.
    Sizes sum regular-file bytes inside each bundle, excluding symlinks, with 1 MB = 1,000,000 bytes.
    These are installed application files, not download sizes or allocated disk space; user data and
    server components are excluded. Nextcloud's executable is universal (Intel + Apple Silicon), while
    these Cairn builds are Apple Silicon only. Seafile and Syncthing were not installed for measurement.
[^downloads]: Download sizes checked on 2026-11-07: [Cairn 0.1.5](https://github.com/stepankandrushin/cairn/releases/tag/v0.1.5),
    [Nextcloud Desktop 35.0.3](https://download.nextcloud.com/desktop/releases/Linux/),
    [Seafile Client 9.0.22](https://github.com/haiwen/seafile-client/releases/tag/v9.0.22),
    [Syncthing 2.1.7](https://github.com/syncthing/syncthing/releases/tag/v2.1.7).
    Archives are `.tar.gz`; AppImages bundle desktop or CLI dependencies. These packages contain different
    components, and their download sizes are not installed sizes. Cairn's archive does not include its tray.
[^performance]: RAM entries describe implementation behavior or upstream requirements, not measured
    consumption on the same workload. Cairn has no published RAM or comparative throughput benchmark.
    Sync speed depends on file count, edit patterns, disk, network and settings; the speed row compares
    the work needed to transfer edits, not measured MB/s.
[^cairn]: Cairn: [Rust workspace](Cargo.toml), [release packaging](scripts/release.sh),
    [filename selection rules](crates/engine/src/selection.rs), [filename sync tests](crates/harness/tests/selection.rs),
    [streaming chunker](crates/engine/src/chunker.rs), [chunk uploads](crates/engine/src/sync.rs),
    [incremental sync](crates/engine/src/incr.rs), [SQLite storage](crates/store/src/sqlite.rs).
[^nextcloud]: Nextcloud: [exclusions](https://github.com/nextcloud/desktop/blob/master/sync-exclude.lst),
    [symlinks](https://docs.nextcloud.com/server/latest/user_manual/en/desktop/usage.html),
    [metadata and delta-sync roadmap](https://github.com/nextcloud/desktop/issues/9636),
    [versions](https://docs.nextcloud.com/server/latest/user_manual/en/files/version_control.html),
    [conflicts](https://docs.nextcloud.com/server/latest/user_manual/en/desktop/conflicts.html),
    [deletion confirmation](https://github.com/nextcloud/desktop/blob/master/src/gui/folder.cpp),
    [server requirements](https://docs.nextcloud.com/server/stable/admin_manual/installation/system_requirements.html),
    [desktop source](https://github.com/nextcloud/desktop), [server source](https://github.com/nextcloud/server).
[^seafile]: Seafile: [exclusions and symlink handling](https://github.com/haiwen/seafile/blob/master/daemon/repo-mgr.c),
    [Unix modes](https://github.com/haiwen/seafile/blob/master/common/index/index.h),
    [blocks and version storage](https://manual.seafile.com/14.0/develop/data_model/),
    [conflicts](https://help.seafile.com/syncing_client/file_conflicts/),
    [deletion confirmation](https://help.seafile.com/faq/#meaning-file-syncing-errors),
    [server requirements](https://manual.seafile.com/14.0/setup/system_requirements/),
    [sync engine source](https://github.com/haiwen/seafile), [desktop source](https://github.com/haiwen/seafile-client),
    [server source](https://github.com/haiwen/seafile-server), [web UI source](https://github.com/haiwen/seahub).
[^syncthing]: Syncthing: [metadata and missing-folder protection](https://docs.syncthing.net/users/faq.html),
    [blocks, conflicts and temporary files](https://docs.syncthing.net/users/syncing.html),
    [ignore rules](https://docs.syncthing.net/users/ignoring.html),
    [versioning](https://docs.syncthing.net/users/versioning.html),
    [memory and performance tuning](https://docs.syncthing.net/users/tuning.html),
    [source](https://github.com/syncthing/syncthing).

## Why the name Cairn?

A cairn is a lasting stone marker that helps you find your way. The name reflects our aim: reliable
storage and a history you can trace, keeping files safe and helping you find your way back to earlier
versions.

## Promises

These outrank features and speed, and each has failure tests in `crates/harness`:

1. A commit acknowledged by the server is durable, and its chunks were durable first.
2. A device never erases what it cannot represent.
3. Arrival order never decides which content survives: competing edits both remain.
4. A crash never leaves a half-written file at a synced path, and never loses a queued operation.
5. An empty folder is not a mass delete until proven so.
6. Sync never traverses a symlink, and path operations cannot be redirected through one.
7. Every restore creates a new revision, so a restore can be undone too.
8. Anything skipped is shown with its reason and a recovery action.

## Try it

```sh
# The server
cairn-server init --data ~/cairn-data
cairn-server serve --data ~/cairn-data           # 127.0.0.1:8733; plain HTTP, so loopback only

# A client
cairn init --server http://127.0.0.1:8733 --token-file ~/cairn-data/token ~/Sync
cd ~/Sync && echo hello > hello.txt
cairn sync

# What happened, and going back
cairn log
cairn history hello.txt
cairn restore hello.txt --at 1
cairn export --at 1 /tmp/as-it-was               # into a new folder; changes nothing else
cairn conflicts                                  # the copies kept, until each is deleted, renamed or moved
```

Beyond one machine, put a TLS-terminating reverse proxy in front, start the server with
`--behind-tls-proxy`, and use an `https://` URL. A read timeout above 25 seconds lets changes arrive at once
(nginx's default of 60 is fine); with a shorter one the daemon asks more often.

## The daemon

```sh
cairn daemon --root ~/Sync       # stays in the foreground; run one per root under your service manager
cairn status                     # what it last said, and what a sync would do now
cairn status --all               # every root of yours on this machine, and which have gone
cairn forget ~/Old               # take one that has gone off that list (--even-if-present for one still there)
cairn pause                      # no sync starts until `cairn resume`, through restarts too
```

It syncs when something changes here or on the server, and every hour regardless (`--reconcile-secs`), since
events can be lost. A sync takes what changed alone; once a day (`--full-sync-secs`) one decides everything the
full way as well and takes that answer, and says in its output if the two differed, which would be a bug to
report. Its output says what it did and what it could not do, each thing once, and `cairn status`
keeps what this filesystem refused (a mode on a file another user owns, say) until it is put right. A paused or
failing root keeps being retried, and `cairn sync` beside it works as usual; `cairn log`, `history`, `export`
and `conflicts` only ask the server, so they answer during a sync too. It answers `cairn status` and the
tray on a local socket only your user can open (`--json` gives the same objects; docs/protocol.md). As a
systemd user unit:

```ini
[Service]
ExecStart=%h/.local/bin/cairn daemon --root %h/Sync
Restart=always

[Install]
WantedBy=default.target
```

On a big tree, the kernel's limit on watches may need raising (`fs.inotify.max_user_watches`); the daemon says
so if it does, and until then those folders wait for the hourly sync.

On Windows, Microsoft Defender's real-time protection scans the files the client opens, and on a tree of many
small files that can be most of a first sync's time: one on a hard disk was read at under 2 MB/s, with Defender
using about a hundred times the client's CPU. A process exclusion, set once from an administrator's PowerShell,
spares it:

```powershell
Add-MpPreference -ExclusionProcess "$env:USERPROFILE\.local\bin\cairn.exe"   # the path of the cairn.exe you run
```

Its cost: what the client writes into the tree is scanned only when another program opens it. `Get-MpPreference`
lists the exclusions, and `Remove-MpPreference -ExclusionProcess <path>` takes one back.

On macOS, run the daemon from its app bundle, Cairn Sync.app, so that System Settings shows it as Cairn Sync with
its icon, in Login Items and in the privacy lists, rather than as a bare `cairn`. From tray/, `bash macos/bundle.sh
../target/release/cairn "$HOME/Applications/Cairn Sync.app"` makes it around the binary (with Xcode installed,
its icon is compiled for macOS 26 and later, which otherwise draw it on a gray tile at small sizes). Link
`~/.local/bin/cairn` to `Contents/MacOS/cairn` inside it, and start the daemon with a launchd user agent that
names the bundle, one per root, as `~/Library/LaunchAgents/com.cairn.home.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.cairn.home</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/you/Applications/Cairn Sync.app/Contents/MacOS/cairn</string>
    <string>daemon</string><string>--root</string><string>/Users/you/Sync</string>
  </array>
  <key>AssociatedBundleIdentifiers</key><array><string>com.cairn.sync</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/Users/you/Library/Logs/cairn-home.log</string>
  <key>StandardErrorPath</key><string>/Users/you/Library/Logs/cairn-home.log</string>
</dict>
</plist>
```

`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.cairn.home.plist` starts it. The tray's Cairn.app is
made the same way (`bash macos/bundle.sh target/release/cairn-tray "$HOME/Applications/Cairn.app"`), and
`cairn-tray --login on` writes its agent. Sign both with a certificate Apple issued: `CAIRN_SIGN` for bundle.sh,
one of `security find-identity -v -p codesigning`. Login Items ties an agent to its app only for such a
signature, and macOS keeps the Local Network answer below for the same certificate and identifier from one
build to the next, where an ad-hoc build is a new program at each upgrade and asks again. A signed bundle put
where an ad-hoc one was can be left under the certificate's team name in Login Items: load the agent once from
an ad-hoc copy, then from the signed bundle with its plist written anew, and it is filed under its app.

On macOS 15 and later, a daemon that launchd starts as a user agent needs the Local Network permission to reach a
server on the same local network (one started from Terminal or SSH does not, which is why `cairn sync` there
works). macOS asks once, "Allow "Cairn Sync" to find devices on local networks?"; until someone answers, every
connection fails with "No route to host", and nothing syncs, though nothing is lost. A server reached through a
router, not on the Mac's own network, needs no permission.

## Leaving things out

Nothing is left out unless you say so. Rules go in `.cairn/ignore`, in `.gitignore` syntax, for this device
only:

```gitignore
*.log
/build/
node_modules/
```

To keep a server folder off this device, including one that does not exist here yet:

```sh
cairn exclude photos                  # stays excluded if it is renamed; the copy here stays too
cairn exclude photos --remove-local   # removes the copy here, except what differs from the server
cairn include photos                  # brings it back
cairn include photos --merge          # the same, with a copy left here merging in
```

Either way, what this device does not sync stays untouched on the server and on other devices, and deleting
your copy deletes nothing else. A copy merging in counts as new: what changed keeps both versions, and an old
copy can bring back what was deleted since, which is why that has to be asked for. Moving something into a left
out path here is a delete for everyone, as in git. A rule is about paths, so your own `mv` out of it uploads. If a sync moves
the folder instead, because another device renamed it, what the rule covered stays out and is listed. `cairn
status` lists what is left out.

`cairn sync` exits 0 when both sides agree (anything excluded is listed), 2 when it paused and is waiting
for you, 3 when another run is needed, and 1 on an error. In every case nothing is lost.

By default a client applies modes, `user.*` xattrs and POSIX ACLs. Owners (`--owners`), setuid and setgid
(`--setid`) and capabilities (`--capabilities`) need an explicit choice at `cairn init`; whatever a client
does not apply stays stored for those that do. `cairn export --exact` applies all of it, given privileges.

## Build and test

```sh
cargo build --release -p cairn-cli -p cairn-server
bash scripts/ci.sh              # format, lints, every test, the failure matrix; as root too, if sudo works
bash scripts/release.sh         # static binaries and checksums in dist/, after a smoke test over TLS
```

`CAIRN_SEEDS=500 cargo test -p cairn-harness --test random` runs more random seeds, and `CAIRN_SEED=n`
reproduces one.

## Docs

- [PLAN.md](PLAN.md): the design, the decisions and their reasons, the build order.
- [docs/protocol.md](docs/protocol.md): the HTTP API.
- [docs/format.md](docs/format.md): what is on disk, on server and client.

## License

The server and storage (`cairn-server`, `cairn-store`, and the test harness) are
[AGPL-3.0-only](LICENSE-AGPL). The protocol, engine, client and failpoint crates are
[Apache-2.0](LICENSE-APACHE), so any client can embed them.
