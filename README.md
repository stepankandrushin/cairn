# Cairn

Self-hosted file sync with versioned history that keeps everything about your files: symlinks, modes,
owners, xattrs, ACLs, capabilities, hard links, sparse files, and filename bytes. A central server keeps
every version; clients sync a real filesystem. Files only: no groupware, no app platform.

**Status: 0.1.0, early.** Linux only, sync on demand, one user. It has been through a harsh failure
harness but has no long history on real data, so keep another copy of anything you put through it. See
[CHANGELOG.md](CHANGELOG.md) for what is and is not there, and [PLAN.md](PLAN.md) for where it is going.

## Why

| Commonly a pain | Here |
|---|---|
| A built-in list of names is never synced | No ignored names: `.git`, `node_modules` and `.venv` sync whole |
| Symlinks skipped, modes and owners lost | Round-tripped exactly; `git status` is clean on the other machine |
| One byte changed re-uploads the file | Content-defined chunks: only what changed moves |
| Versions are full copies | Versions share chunks |
| Conflicts surprise you | Three-way merge: both versions stay, with an inbox |
| An unmounted disk reads as "delete everything" | Sync pauses and asks |

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
cairn conflicts
```

Beyond one machine, put a TLS-terminating reverse proxy in front, start the server with
`--behind-tls-proxy`, and use an `https://` URL.

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
