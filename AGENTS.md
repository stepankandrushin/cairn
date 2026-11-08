# Cairn

An open-source, self-hosted alternative to Nextcloud's file sync: a central server that keeps
versioned history, and clients that sync a real filesystem without losing anything about it (symlinks,
modes, owners, xattrs, hard links, sparse files, filename bytes). Files only: no groupware, no app
platform.

**Status: 0.1.11 released on GitHub, milestones 0 to 2 done, and the harness runs in CI.** Linux, a one-shot
CLI and a daemon; the same on macOS and Windows with a tray, ahead of milestone 7 and not in CI; each
installed copy updates itself from signed releases. The architecture, the decisions and their reasons,
and the build order are in PLAN.md: read it before writing code, and when work changes a decision,
rewrite that line of the plan rather than appending to it. Milestone 3's code and failure tests are in; what
it still needs is its exit criterion, weeks on real trees with no silent loss, a run begun in November 2026
on the owner's own trees and machines.

## Rules

- **The repo is public**, so code, PLAN.md, this file, test data and commit subjects name no person,
  host, address or deployment: those would be served to anyone. Only agents/ and history/ stay off
  GitHub and may name them.
- **Never lose data or metadata, and never fail silently**: PLAN.md's invariants outrank features and
  speed, because a sync tool that is wrong once is not trusted again. A change that touches them comes
  with a failure test (crash, full disk, lost event, missing mount).
- **A test on a real user's account sets its own `CAIRN_USER_DIR`**, for every `cairn` and tray it runs:
  `cairn init` and a starting daemon add their root to that user's list of roots, which the tray and
  `cairn status --all` show, so a scratch root made without it shows up to the person whose machine it is.
- **The filesystem is the hard part, so it comes first**: follow PLAN.md's milestone order, and do not
  start the UI or another platform while the Linux engine's exit criteria are unmet. macOS and
  Windows, the tray on them, and updates are the exceptions, at the owner's asking, for milestone 3's run.
- **All of it speaks one protocol**: the server, the CLI, the daemon and the web UI share the Rust
  types in the protocol crate, so a client can never quietly strip a field it does not understand.
- **The status lines say what is true now**: README.md's status, this file's, and, in the maintainer's
  checkout, the top of agents/deployment/README.md (the root, its devices and trees, what runs where, the
  checker). A commit that makes one false (a release, a platform, CI, a root or device added or retired, a
  service renamed or stopped) rewrites it in that same commit; before ending any task, read them against
  what the task changed. Rewrite what went stale rather than adding a newer line beside it.

## Docs

- PLAN.md — the design and build order; before any design or code work.
- docs/protocol.md, docs/format.md — the wire and on-disk formats; before changing either.
- scripts/ci.sh — everything that must pass before a commit; on the real disk in /var/tmp, since tmpfs hid
  a bug; about 30 minutes with GitHub's `CAIRN_SEEDS=200`, where `CAIRN_NO_SKIP=1` also makes a test that
  cannot run fail instead of skipping (`skip` in the harness's common/). scripts/release.sh — a release,
  with release-macos.sh and release-windows.ps1 for those platforms, and release-update.sh for the signed
  update.json every installed copy checks before it updates itself (docs/protocol.md, "Updates").
  Run them with `bash`: git does not keep them executable. Rust comes from rustup.
- crates/harness/tests/ — the failure tests. A failing seed reproduces with `CAIRN_SEED=n`, and its log
  and kept directory say what happened: trust the oracle until a trace proves it wrong. They run on
  Windows too, outside CI, by `cargo test -p cairn-harness`; what Windows does not hold is split out with
  `#[cfg(unix)]`, each with its reason.
- tray/ — the tray, its own Cargo workspace outside CI, as it needs a desktop's webview; built on macOS
  and Windows.
- agents/maintainer.md — remotes, publishing and releases; before any of those. It and history/ exist only
  in the maintainer's checkout.
