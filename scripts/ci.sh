#!/usr/bin/env bash
# What must hold before a change goes in. Run by .github/workflows/ci.yml, and by hand:
#   bash scripts/ci.sh           the full suite, about 20 minutes: GitHub runs it on every push, and a release
#                                waits for it there
#   bash scripts/ci.sh --quick   before a commit, about a minute: the same but for the 70 tests over 20 s, most
#                                of them sweeping every step of a sync (.config/nextest.toml, "quick")
# CAIRN_NO_SKIP=1, which the workflow sets, turns a test that cannot run here into a failure.
set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in
    "") profile=default ;;
    --quick) profile=quick ;;
    *)
        echo "usage: bash scripts/ci.sh [--quick]" >&2
        exit 2
        ;;
esac

# The quick run is cargo-nextest's: each test a process of its own and the tests of every file in one pool,
# so it takes about as long as its share of the machine, where cargo test takes the sum of its files. The
# full run stays cargo test's: under nextest all the sweeps ran at once, each with its own pool of workers,
# took three times as long each, and two tests with a deadline failed.
if [ "$profile" = quick ] && ! cargo nextest --version >/dev/null 2>&1; then
    echo "the quick run is cargo-nextest's, which is not installed: cargo install cargo-nextest --locked" >&2
    exit 1
fi

# On a real disk: /tmp is often tmpfs, which never reuses an inode number, and ext4 reuses them at once.
# A failure leaves this directory behind, since it holds the kept directories of failing seeds.
TMPDIR=$(mktemp -d /var/tmp/cairn-ci.XXXXXX)
export TMPDIR

cargo fmt --all --check
python3 scripts/tray-version.py --test
cargo clippy --workspace --all-targets -- -D warnings
if [ "$profile" = quick ]; then
    cargo nextest run --workspace --profile quick
else
    cargo test --workspace
fi

# As root the round trip also covers owners, setuid, capabilities and trusted.* xattrs.
if sudo -n true 2>/dev/null; then
    test_binary=$(cargo test -p cairn-harness --test roundtrip --no-run --message-format=json 2>/dev/null \
        | python3 -c 'import json,sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m["target"]["name"] == "roundtrip" and m.get("executable"):
        print(m["executable"])')
    sudo -n env CAIRN_NO_SKIP="${CAIRN_NO_SKIP:-}" "$test_binary" every_metadata_type second_device what_cannot
elif [ -n "${CAIRN_NO_SKIP:-}" ] && [ "$CAIRN_NO_SKIP" != 0 ]; then
    echo "no passwordless sudo, and CAIRN_NO_SKIP says every test must run here: the root half of the round trip did not" >&2
    exit 1
else
    echo "no passwordless sudo: the root half of the round trip did not run" >&2
fi

chmod -R u+rwx "$TMPDIR"
rm -rf "$TMPDIR"
