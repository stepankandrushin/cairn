# Cairn

An open-source, self-hosted alternative to Nextcloud's file sync: a central server that keeps
versioned history, and clients that sync a real filesystem without losing anything about it (symlinks,
modes, owners, xattrs, hard links, sparse files, filename bytes). Files only: no groupware, no app
platform.

**Status: 0.1.29 released on cairn.storage and GitHub, milestones 0 to 2 done, and the harness runs in CI.**
Linux, on x86_64 and since 0.1.26 aarch64 a one-shot CLI and a daemon; the same on macOS and Windows with a tray, ahead of milestone 7 and not in
CI; each installed copy updates itself from signed releases, from cairn.storage, or GitHub when it does not answer. Users, tokens and per-folder read, write and manage
access, ahead of milestone 5 came in 0.1.12 the web interface, ahead of milestone 6 (browsing, uploads,
history, recently deleted, members, shares with users and public links, signing in from a device), came in
0.1.14 Explorer's badges, each entry's state on its icon on Windows, in 0.1.25 an iPhone app, ahead of
milestone 8 (photo and video backup, browsing with downloads on demand, the Files app read and add only), built
from source, its core crates/mobile in CI and its Swift not; and, since 0.1.29 an Android app, the same on
the same core, its APK among each release's files, its Kotlin outside CI. The architecture, the decisions and their reasons,
and the build order are in PLAN.md - read before writing code, and when work changes a decision,
rewrite that line of the plan rather than appending to it. Milestone 3 code and failure tests are in; what
it still needs is its exit criterion, weeks on real trees with no silent loss, a run begun in November 2026
on the owner's own trees and machines.

## Rules

- **The repo is public**, so code, PLAN.md this file, test data and commit subjects name no person,
  host, address or deployment: those would be served to anyone. Only agents/ and history/ stay off
  GitHub and may name them, and sources/ (copies of other people's pages) stays off it too.
- **Never lose data or metadata, and never fail silently**: PLAN.md invariants outrank features and
  speed, because a sync tool that is wrong once is not trusted again. A change that touches them comes
  with a failure test (crash, full disk, lost event, missing mount).
- **A test on a real user's account sets its own `CAIRN_USER_DIR`** for every `cairn` and tray it runs:
  `cairn init` and a starting daemon add their root to that user's list of roots, which the tray and
  `cairn status --all` show, so a scratch root made without it shows up to the person whose machine it is.
- **The filesystem is the hard part, so it comes first**: follow PLAN.md milestone order, and do not
  start the UI or another platform while the Linux engine's exit criteria are unmet. macOS and
  Windows, the tray on them, Explorer's badges, updates, accounts, the web interface and the phone apps are
  the exceptions, at the owner's asking.
- **All of it speaks one protocol**: the server, the CLI, the daemon and the web UI share the Rust
  types in the protocol crate, so a client can never quietly strip a field it does not understand.
- **The status lines say what is true now**: README.md status, this file's, and, in the maintainer's
  checkout, the top of agents/deployment/README.md (the root, its devices and trees, what runs where, the
  checker). A commit that makes one false (a release, a platform, CI, a root or device added or retired, a
  service renamed or stopped) rewrites it in that same commit; before ending any task, read them against
  what the task changed. Rewrite what went stale rather than adding a newer line beside it.

## Docs

- PLAN.md — the design and build order; before any design or code work.
- docs/protocol.md docs/format.md — the wire and on-disk formats; before changing either.
- docs/android.md — the Android app's manual, its screenshots in docs/android/ also published as
  cairn.storage/android/ - when the app's screens change, retake them as that site's shots/README.md says
- scripts/ci.sh — the checks and every test, on the real disk in /var/tmp since tmpfs hid a bug, under
  cargo-nextest `cargo install cargo-nextest --locked` the long sweeps split into parts by `shards!`
  crates/harness/tests/common `--quick` must pass before a commit: all but the 61 tests and sweeps over
  20 s .config/nextest.toml about a minute. The full run, with `CAIRN_SEEDS=200` and `CAIRN_NO_SKIP=1`
  which makes a test that cannot run fail instead of skipping `skip` in the harness's common/ is what a
  release passes first, in its own tree on the machine that builds it (about 11 minutes there, as long as its
  disk takes over the suite's fsyncs), and waits for no hosted CI; GitHub runs it too on every push, a second
  check that nothing waits for, whose failure is work for the next release. scripts/release.sh — a release,
  with release-macos.sh and release-windows.ps1 for those platforms, and release-update.sh for the signed
  update.json every installed copy checks before it updates itself docs/protocol.md "Updates".
  Run them with `bash` - git does not keep them executable. Rust comes from rustup
- crates/harness/tests/ — the failure tests. A failing seed reproduces with `CAIRN_SEED=n` and its log
  and kept directory say what happened: trust the oracle until a trace proves it wrong. They run on
  Windows too, outside CI, by `cargo test -p cairn-harness` what Windows does not hold is split out with
  `#[cfg(unix)]` each with its reason.
- tray/ — the tray, its own Cargo workspace outside CI, as it needs a desktop's webview; built on macOS
  and Windows.
- ios/ — the iPhone app and its Files and Photos upload extensions, outside CI, on crates/mobile (uniffi);
  `bash scripts/release-ios.sh --sim` on a Mac builds it and, given a server, runs its UI test in the simulator.
  The team that signs and the server Photos uploads to are in the untracked ios/Config/Local.xcconfig
- android/ — the Android app (Kotlin, Compose) and its Files picker provider, outside CI, on crates/mobile
  (uniffi); `bash scripts/release-android.sh` builds a signed APK on Linux, `--emulator` installs it there. The
  key that signs it is the maintainer's agents/maintainer.md - every APK an installed copy updates to needs it.
- agents/maintainer.md — remotes, publishing and releases; before any of those. It and history/ exist only
  in the maintainer's checkout.
