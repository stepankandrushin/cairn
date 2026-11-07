#!/usr/bin/env bash
# Everything that must hold before a change goes in. Run by .github/workflows/ci.yml, and by hand.
# CAIRN_NO_SKIP=1, which the workflow sets, turns a test that cannot run here into a failure.
set -euo pipefail
cd "$(dirname "$0")/.."

# On a real disk: /tmp is often tmpfs, which never reuses an inode number, and ext4 reuses them at once.
# A failure leaves this directory behind, since it holds the kept directories of failing seeds.
TMPDIR=$(mktemp -d /var/tmp/cairn-ci.XXXXXX)
export TMPDIR

cargo fmt --all --check
python3 scripts/tray-version.py --test
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

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
