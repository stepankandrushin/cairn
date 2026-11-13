#!/usr/bin/env bash
# Build the release binaries, prove them, and put them in dist/: cairn-<v>-linux-x86_64.tar.gz for people, and
# the bare cairn-<v>-linux-x86_64-sync, cairn-sync alone, for updates, which release-update.sh signs with the macOS and Windows
# halves (release-macos.sh, release-windows.ps1) and lists in the release's one checksum file.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/update-env.sh

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
target=x86_64-unknown-linux-musl

# The tray is a workspace of its own, which nothing here builds: give it this version, for the commit.
python3 scripts/tray-version.py

# The binaries are published, and panic messages carry source paths: keep the build machine's out.
cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup --remap-path-prefix=$PWD=/cairn"
export CFLAGS="${CFLAGS:-} -ffile-prefix-map=$cargo_home=/cargo -ffile-prefix-map=$PWD=/cairn"

# -p, never --workspace: the harness turns failpoints on for whatever is built together with it.
cargo build --release --locked --target "$target" -p cairn-cli -p cairn-server
out="target/$target/release"

for bin in cairn-sync cairn-server; do
    if ldd "$out/$bin" 2>&1 | grep -q '=>'; then
        echo "$bin is not statically linked" >&2
        exit 1
    fi
    if grep -q 'CAIRN_FP_' "$out/$bin"; then
        echo "$bin was built with failpoints" >&2
        exit 1
    fi
    if grep -a -q -e "$HOME" -e "$PWD" "$out/$bin"; then
        echo "$bin names a path of the build machine" >&2
        exit 1
    fi
    test "$("$out/$bin" --version | awk '{print $2}')" = "$version"
done
if [[ -n ${CAIRN_UPDATE_FEED-} ]] && ! grep -a -q -F "$CAIRN_UPDATE_FEED" "$out/cairn-sync"; then
    echo "cairn-sync does not name its feed, $CAIRN_UPDATE_FEED" >&2
    exit 1
fi

bash scripts/smoke.sh "$out"

# Each file is cairn-<v>-<platform>[-<program>], the program `sync` for cairn-sync and `tray` for cairn-tray, so
# a release's files sort by platform; the tarball, which holds cairn-sync and cairn-server, names none.
name="cairn-$version-linux-x86_64"
stage=$(mktemp -d)
mkdir "$stage/$name"
cp "$out/cairn-sync" "$out/cairn-server" README.md CHANGELOG.md LICENSE-APACHE LICENSE-AGPL "$stage/$name/"
mkdir -p dist
tar -C "$stage" --owner=0 --group=0 --sort=name -czf "dist/$name.tar.gz" "$name"
rm -rf "$stage"
# What an installed cairn-sync on Linux updates itself to: the program alone.
cp "$out/cairn-sync" "dist/$name-sync"
ls -l "dist/$name.tar.gz" "dist/$name-sync"
(cd dist && sha256sum "$name.tar.gz" "$name-sync")
