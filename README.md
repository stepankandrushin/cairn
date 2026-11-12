# Cairn

Self-hosted file sync with versioned history that keeps everything about your files: symlinks, modes,
owners, xattrs, ACLs, capabilities, hard links, sparse files, and filename bytes. A central server keeps
every version; clients sync a real filesystem. Files only: no groupware, no app platform.

**Status: 0.1.17, early.** Clients for Linux, macOS and Windows, with only Linux in CI; the server on Linux;
users with their own folders and shared ones, read-only or not; since 0.1.14, a web interface in the server,
which a device opens signed in, with shares of any file or folder and public links. It has
been through a harsh failure harness and now syncs real trees, but for days, not yet weeks, so keep another
copy of anything you put through it. See
[CHANGELOG.md](CHANGELOG.md) for what is and is not there, and [PLAN.md](PLAN.md) for where it is going.

## Cairn vs Nextcloud vs Seafile vs Syncthing

Cairn focuses on preserving the whole filesystem and its history. This comparison covers desktop
filesystem sync and the services needed to run it; metadata support depends on the operating system,
filesystem and privileges.

✅ Supported · ⚠️ Partial or conditional support · ❌ Not supported

| Feature | **Cairn**[^cairn] | Nextcloud[^nextcloud] | Seafile[^seafile] | Syncthing[^syncthing] |
|---|---|---|---|---|
| Installed application size (macOS)[^macos-size] | **17.1 MB** — tray + sync daemon, Apple Silicon | **414.4 MB** — desktop client, universal Intel + Apple Silicon | Not measured | Not measured |
| Application download size (Linux x86-64)[^downloads] | **6.3 MiB** — CLI + server archive | 60.3 MiB — desktop AppImage; server separate | 13.7 MiB CLI / 184.3 MiB desktop AppImage; server separate | 13.4 MiB — archive, including web UI |
| RAM use / requirements[^performance] | Chunk-buffered file I/O; tree and chunk metadata also consume RAM; no measured baseline | Server: 128 MB minimum / 512 MB recommended per PHP process, plus other services | CE server stack: 2 GB minimum | Depends on files, peers and concurrency; buffers and Go memory target are tunable |
| Implementation languages | **Rust** — CLI, daemon and server; HTML/JavaScript web UI | C++ desktop; PHP server | C sync engine; C++ desktop; C/Go server; Python/JavaScript web UI | Go sync engine; HTML/JavaScript web UI |
| Run without a separate database server or language runtime | ✅ Native binaries with embedded SQLite | ❌ PHP and a web server; usually a separate database | ❌ Server stack includes a database, cache and web services | ✅ Native binary; no central server required |
| Preserve symlinks | ✅ Preserved as links | ❌ Skipped | ❌ Targets copied instead of preserving links | ⚠️ Preserved, except on Windows |
| Preserve full filesystem metadata | ✅ Modes, owners, xattrs, ACLs, hard links and sparse layout | ❌ No full Unix metadata round-trip | ⚠️ Executable bit on Unix; not full modes or ownership | ⚠️ Modes; owners and xattrs opt-in; hard links not preserved |
| Sync speed: avoid whole-file re-uploads[^performance] | ✅ Incremental scans; only new content-defined chunks transferred | ❌ Whole changed file uploaded again | ✅ Only new content-defined blocks transferred | ✅ Only changed fixed-size blocks transferred; downloads from peers in parallel |
| Versions share chunks | ✅ Shared chunks | ❌ Whole-file versions | ✅ Shared blocks | ❌ Whole-file archives of changes received from peers; off by default |
| Keep both sides of a conflict | ✅ Three-way merge; both versions kept, with an inbox | ⚠️ Local conflict copy; not uploaded by default | ✅ Both versions kept; one renamed as a conflict file | ✅ Both versions kept; one renamed as a conflict file |
| Guard against missing disks / mass deletion | ✅ Mount checks; sync pauses and asks | ⚠️ Confirmation before mass deletion | ⚠️ Confirmation above a deletion-count threshold | ✅ Sync stops if the folder's `.stfolder` marker is missing |

[^macos-size]: Installed bundles measured on an Apple Silicon Mac:
    Cairn 0.1.5 — `Cairn.app` 9.1 MB + `Cairn Sync.app` 8.0 MB; Nextcloud 34.0.5 — `Nextcloud.app` 414.4 MB.
    Sizes sum regular-file bytes inside each bundle, excluding symlinks, with 1 MB = 1,000,000 bytes.
    These are installed application files, not download sizes or allocated disk space; user data and
    server components are excluded. Nextcloud's executable is universal (Intel + Apple Silicon), while
    these Cairn builds are Apple Silicon only. Seafile and Syncthing were not installed for measurement.
[^downloads]: Download sizes: [Cairn 0.1.5](https://github.com/stepankandrushin/cairn/releases/tag/v0.1.5),
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
    [streaming chunker](crates/engine/src/chunker.rs), [chunk uploads](crates/engine/src/sync.rs),
    [incremental sync](crates/engine/src/incr.rs), [SQLite storage](crates/store/src/sqlite.rs).
[^nextcloud]: Nextcloud: [symlinks](https://docs.nextcloud.com/server/latest/user_manual/en/desktop/usage.html),
    [metadata and delta-sync roadmap](https://github.com/nextcloud/desktop/issues/9636),
    [versions](https://docs.nextcloud.com/server/latest/user_manual/en/files/version_control.html),
    [conflicts](https://docs.nextcloud.com/server/latest/user_manual/en/desktop/conflicts.html),
    [deletion confirmation](https://github.com/nextcloud/desktop/blob/master/src/gui/folder.cpp),
    [server requirements](https://docs.nextcloud.com/server/stable/admin_manual/installation/system_requirements.html),
    [desktop source](https://github.com/nextcloud/desktop), [server source](https://github.com/nextcloud/server).
[^seafile]: Seafile: [symlink handling](https://github.com/haiwen/seafile/blob/master/daemon/repo-mgr.c),
    [Unix modes](https://github.com/haiwen/seafile/blob/master/common/index/index.h),
    [blocks and version storage](https://manual.seafile.com/14.0/develop/data_model/),
    [conflicts](https://help.seafile.com/syncing_client/file_conflicts/),
    [deletion confirmation](https://help.seafile.com/faq/#meaning-file-syncing-errors),
    [server requirements](https://manual.seafile.com/14.0/setup/system_requirements/),
    [sync engine source](https://github.com/haiwen/seafile), [desktop source](https://github.com/haiwen/seafile-client),
    [server source](https://github.com/haiwen/seafile-server), [web UI source](https://github.com/haiwen/seahub).
[^syncthing]: Syncthing: [metadata and missing-folder protection](https://docs.syncthing.net/users/faq.html),
    [blocks, conflicts and temporary files](https://docs.syncthing.net/users/syncing.html),
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
cairn deleted                                    # what was deleted in the last 30 days, each with its ID
cairn undelete <ID>                              # bring it back as it was, beside anything now at its name
```

Beyond one machine, put a TLS-terminating reverse proxy in front, start the server with
`--behind-tls-proxy`, and use an `https://` URL. A read timeout above 25 seconds lets changes arrive at once
(nginx's default of 60 is fine); with a shorter one the daemon asks more often.

## Users and folders

A server holds folders, each with its own tree, history and set of chunks, and users, each with tokens of its
own, one per device at best. A user reaches only the folders given to it, at one of three levels: `read`,
`write`, or `manage`, which is write and also saying who else has access. To anyone else a folder answers as
one that does not exist. `cairn-server init` makes the user `admin`, who manages the folder `cloud`, with
its token in the data directory's `token` file; a server from before accounts gives that file's token the
same, managing every folder it has, the first of them `default`. `cairn init` without `--folder` syncs the
one folder its token reaches, and with several asks which.

A folder's name is what devices, links and commands use. Each user may also call it something of their
own, which only they see in the web, `cairn folders` and wherever else it is named for them: the name of the
directory their first device syncs it into, until they choose another in the web's Settings. Any user adds a
folder of their own in the web, which they manage; a folder's managers rename it there, in its settings, while
the server serves, and the administrator on the server with it stopped. Renamed, a folder still answers to its
former name, so no device, link or bookmark made with it breaks, and each device takes the new name up at its
next sync.

```sh
# On the server, while it runs or not, but for a rename
cairn-server user add alice --token-file alice.token --data ~/cairn-data
cairn-server user add bob   --token-file bob.token   --data ~/cairn-data
cairn-server folder add alice --manager alice --data ~/cairn-data      # her own
cairn-server folder add team  --manager alice --data ~/cairn-data      # to share
cairn-server grant team bob read --data ~/cairn-data
cairn-server token add bob --label laptop --token-file bob-laptop.token --data ~/cairn-data
cairn-server user list --data ~/cairn-data
cairn-server revoke team bob --data ~/cairn-data
cairn-server token list --data ~/cairn-data; cairn-server token remove <ID> --data ~/cairn-data
cairn-server folder list --data ~/cairn-data     # each folder, its head, former names and who has access
cairn-server folder rename team crew --data ~/cairn-data   # with the server stopped; or in the web

# On a device: each folder is a root of its own
cairn folders --server https://cairn.example --token-file bob.token
cairn init --server https://cairn.example --token-file bob.token --folder team ~/Team
cairn access                                     # who has access to this root's folder
cairn access grant carol write                   # a manager shares it; nobody else can
```

A folder a user may only read comes down whole, and nothing made on that user's devices goes up or is lost
there: an edit, a new file, a rename stay as they are, listed by every sync as `read-only`; something deleted
there comes back from the server; where the server changes something also changed there, both stay, the
server's at the name. Moved out of the root, a change is kept and the server's version comes back; once the
user may write, what was held back goes up like any change. `cairn log` says which user made each commit.

## The web interface

The server's address opens it in a browser. From a computer that syncs, the tray's Open in browser, or `cairn
web` in a synced folder, opens it signed in as that device, with nothing to type; elsewhere, sign in with a
token, which `cairn-server token add alice --label browser --data ~/cairn-data` makes. Each folder is there to browse, with uploads of files and folders,
downloads of a file or of a folder as a zip, previews, a file's history with any revision restored, what was
deleted brought back, who has access, a page of its settings, and, for a folder's managers, its rename and
shares of any file or folder in it: with
another user of the server, to read or to write, or by a public link that needs no account, reads only and
may expire. A change made there is a commit like any device's, refused rather than forced when something
changed since it was looked at, and a file replaced there keeps its mode, owners and attributes. A share with
a user is seen in the web interface, not synced yet. The + beside Folders adds a folder of one's own. It is
light or dark as the device is, and the sun or moon at the right end of its top bar switches it. Times are on
a 24-hour clock, as `2026-11-11 09:50`; its Settings choose the theme, a 12-hour clock or another order of the
date, kept in that browser, and the user's own name for each folder, kept on the server. The endpoints are in
docs/protocol.md, "Web".

A public link needs only `/s/`, `/web/` and `/v1/links/`. Where the server's address is open to some addresses
alone, a second door in the proxy, open to anyone, can pass just those, and `cairn-server serve --link-url
https://files.example:8443` makes every link name it:

```nginx
server {
    listen 8443 ssl;                         # the same certificate as the server's own door
    client_max_body_size 1k;
    location ~ ^/(s|web|v1/links)/ {
        limit_except GET { deny all; }
        proxy_pass http://127.0.0.1:8733;
        proxy_buffering off;
    }
    location / { return 404; }
}
```

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

## Updates

A release build keeps itself up to date. Each daemon asks its release's feed about once an hour, and when there
is a newer release it downloads it beside the installed program, checks it, puts it in place and restarts into
it between syncs; the tray installed beside `cairn` is updated with it and restarts too. No permission is asked
again: macOS keeps them for a signature by the same team, which every release has, and Windows by the path, which
does not change.

```sh
cairn update                     # what is installed, how updates are set, what the feed offered last
cairn update now                 # check and install now, whatever the setting, then wait for the daemons
cairn update check               # only say what there is
cairn update notify              # only say a new release is there, here and in the tray; `auto` (the default) or `off`
cairn update rollback            # back to the version before, which is then not installed again unless asked
```

The tray's Settings tab has the same choice and an Update now button. What it installs is only ever what the
release's own key signed: every file's size and SHA-256 are in a signed `update.json`, and on macOS the new bundle
must also be signed by the team of the one it replaces. A kill or a full disk at any step leaves the old program
or the new one in place, whole; the one before is kept beside it in `.cairn-update`, and a version on which a
daemon keeps dying before it gets through a sync is taken back by itself. A release on a newer protocol waits until
the server speaks it, so upgrade the server first: it is never updated by itself. Where the program's folder is
not yours to write, as for a `/usr/local/bin` owned by root, `cairn update` says so, and `sudo cairn update now`
installs it; the daemons restart into it.

A build of your own names no feed and is never updated by anyone's releases. To update it from releases of your
own, build with `CAIRN_UPDATE_FEED` (the URL of your `update.json`) and `CAIRN_UPDATE_KEYS` (your public keys), or
name them in `update-settings.json` beside the registry of roots (`feed`, `keys`); `cairn-release` (crates/release)
makes the key and signs the file, and docs/protocol.md has its format.

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
bash scripts/ci.sh --quick      # the same in about a minute, less the tests that sweep every step of a sync
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
