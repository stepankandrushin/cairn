# Cairn

An open-source, self-hosted alternative to Nextcloud's file sync: a central server that keeps
versioned history, and clients that sync a real filesystem without losing anything about it (symlinks,
modes, owners, xattrs, hard links, sparse files, filename bytes). Files only: no groupware, no app
platform.

**Status: 0.1.1 released on GitHub, milestones 0 to 2 done, and the harness runs in CI.** Linux, a one-shot
CLI and, since then, a daemon; the same on macOS, ahead of milestone 7 and not in CI. The architecture, the decisions and their reasons,
and the build order are in PLAN.md: read it before writing code, and when work changes a decision,
rewrite that line of the plan rather than appending to it. Milestone 3's code and failure tests are in; what
it still needs is its exit criterion, weeks on real trees, on trees and machines the owner chooses.

## Rules

- **The repo is public**, so code, PLAN.md, this file, test data and commit subjects name no person,
  host, address or deployment: those would be served to anyone. Only agents/ and history/ stay off
  GitHub and may name them.
- **Never lose data or metadata, and never fail silently**: PLAN.md's invariants outrank features and
  speed, because a sync tool that is wrong once is not trusted again. A change that touches them comes
  with a failure test (crash, full disk, lost event, missing mount).
- **The filesystem is the hard part, so it comes first**: follow PLAN.md's milestone order, and do not
  start the UI or another platform while the Linux engine's exit criteria are unmet. macOS is the
  exception, at the owner's asking, to test in milestone 3's run.
- **All of it speaks one protocol**: the server, the CLI, the daemon and the web UI share the Rust
  types in the protocol crate, so a client can never quietly strip a field it does not understand.

## Docs

- PLAN.md — the design and build order; before any design or code work.
- docs/protocol.md, docs/format.md — the wire and on-disk formats; before changing either.
- scripts/ci.sh — everything that must pass before a commit; on the real disk in /var/tmp, since tmpfs hid
  a bug; about 30 minutes with GitHub's `CAIRN_SEEDS=200`. scripts/release.sh — a release.
  Run them with `bash`: git does not keep them executable. Rust comes from rustup.
- crates/harness/tests/ — the failure tests. A failing seed reproduces with `CAIRN_SEED=n`, and its log
  and kept directory say what happened: trust the oracle until a trace proves it wrong.
- agents/maintainer.md — remotes, publishing and releases; before any of those. It and history/ exist only
  in the maintainer's checkout.
