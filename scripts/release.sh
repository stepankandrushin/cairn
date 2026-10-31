#!/usr/bin/env bash
# Build the release binaries, prove them, and put them with checksums in dist/.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
target=x86_64-unknown-linux-musl

# The binaries are published, and panic messages carry source paths: keep the build machine's out.
cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup --remap-path-prefix=$PWD=/cairn"
export CFLAGS="${CFLAGS:-} -ffile-prefix-map=$cargo_home=/cargo -ffile-prefix-map=$PWD=/cairn"

# -p, never --workspace: the harness turns failpoints on for whatever is built together with it.
cargo build --release --locked --target "$target" -p cairn-cli -p cairn-server
out="target/$target/release"

for bin in cairn cairn-server; do
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

bash scripts/smoke.sh "$out"

name="cairn-$version-$target"
stage=$(mktemp -d)
mkdir "$stage/$name"
cp "$out/cairn" "$out/cairn-server" README.md CHANGELOG.md LICENSE-APACHE LICENSE-AGPL "$stage/$name/"
mkdir -p dist
tar -C "$stage" --owner=0 --group=0 --sort=name -czf "dist/$name.tar.gz" "$name"
rm -rf "$stage"
(cd dist && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
ls -l "dist/$name.tar.gz"
cat "dist/$name.tar.gz.sha256"
