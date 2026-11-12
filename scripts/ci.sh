#!/usr/bin/env bash
# What must hold before a change goes in. Run by .github/workflows/ci.yml, and by hand:
#   bash scripts/ci.sh           the full suite: every release passes it first, with CAIRN_SEEDS=200 and
#                                CAIRN_NO_SKIP=1, in its own tree on the machine that builds it, where its tests
#                                take about 16 minutes; GitHub runs it too on every push, as a second check that
#                                nothing waits for
#   bash scripts/ci.sh --quick   before a commit, about a minute: the same but for the 56 tests and sweeps
#                                over 20 s (.config/nextest.toml, "quick")
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

# Both runs are cargo-nextest's (.config/nextest.toml): each test a process of its own, the tests of every
# file in one pool, the longest first, where cargo test runs one file after another. The full run's tests
# wait on the disk's fsyncs far more than on the CPU, and the more of them wait at once the more fsyncs each
# journal commit takes together: half as many again as there are CPUs, and no fewer than 16, as on a runner
# of 4.
if ! cargo nextest --version >/dev/null 2>&1; then
    echo "the tests run under cargo-nextest, which is not installed: cargo install cargo-nextest --locked" >&2
    exit 1
fi
threads=()
if [ "$profile" = default ]; then
    n=$(( $(nproc) * 3 / 2 ))
    threads=(--test-threads "$(( n > 16 ? n : 16 ))")
fi

# On a real disk: /tmp is often tmpfs, which never reuses an inode number, and ext4 reuses them at once.
# A failure leaves this directory behind, since it holds the kept directories of failing seeds.
TMPDIR=$(mktemp -d /var/tmp/cairn-ci.XXXXXX)
export TMPDIR

cargo fmt --all --check
python3 scripts/tray-version.py --test
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --profile "$profile" ${threads[@]+"${threads[@]}"}

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
