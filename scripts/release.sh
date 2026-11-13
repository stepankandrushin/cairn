#!/usr/bin/env bash
# Build the Linux release binaries, prove them, and put them in dist/, each program a file of its own, as a
# client and a server are installed on different machines: the client (the CLI and daemon), which installed
# copies also update to, for each platform, cairn-<v>-linux-x86_64 and cairn-<v>-linux-aarch64, and the server,
# cairn-<v>-linux-x86_64-server. release-update.sh signs the clients' with the macOS and Windows halves
# (release-macos.sh, release-windows.ps1) and lists them all in the release's one checksum file.
# Every binary is static (musl). aarch64's is built on x86_64 by cargo-zigbuild, with zig as its C compiler and
# linker (`cargo install cargo-zigbuild --locked`, zig from ziglang.org on the PATH), and proven under
# qemu-aarch64 (Debian's and Ubuntu's qemu-user), the smoke test included, against the x86_64 server: no
# aarch64 machine is needed. On an aarch64 machine it runs as it is.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/update-env.sh

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
x86=x86_64-unknown-linux-musl
arm=aarch64-unknown-linux-musl

missing=()
command -v cargo-zigbuild >/dev/null || missing+=("cargo-zigbuild (cargo install cargo-zigbuild --locked)")
command -v zig >/dev/null || missing+=("zig (ziglang.org, on the PATH)")
rustup target list --installed | grep -qx "$arm" || missing+=("Rust's $arm (rustup target add $arm)")
if [[ $(uname -m) != aarch64 ]] && ! command -v qemu-aarch64 >/dev/null; then
    missing+=("qemu-aarch64 (apt install qemu-user)")
fi
if ((${#missing[@]})); then
    printf 'the aarch64 build needs %s\n' "${missing[@]}" >&2
    exit 1
fi

# The tray is a workspace of its own, which nothing here builds: give it this version, for the commit.
python3 scripts/tray-version.py

# The binaries are published, and panic messages carry source paths: keep the build machine's out.
cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup --remap-path-prefix=$PWD=/cairn"
export CFLAGS="${CFLAGS:-} -ffile-prefix-map=$cargo_home=/cargo -ffile-prefix-map=$PWD=/cairn"

# -p, never --workspace: the harness turns failpoints on for whatever is built together with it.
cargo build --release --locked --target "$x86" -p cairn-cli -p cairn-server
cargo zigbuild --release --locked --target "$arm" -p cairn-cli

# Run an aarch64 program here.
on_arm() { if [[ $(uname -m) == aarch64 ]]; then "$@"; else qemu-aarch64 "$@"; fi; }

prove() { # <binary> [runner]: static, no failpoints, no build path, this version
    local bin=$1
    shift
    # No interpreter and no library: what ldd says, for any architecture.
    if readelf -lW "$bin" | grep -q 'Requesting program interpreter' || readelf -dW "$bin" | grep -q '(NEEDED)'; then
        echo "$bin is not statically linked" >&2
        exit 1
    fi
    if grep -q 'CAIRN_FP_' "$bin"; then
        echo "$bin was built with failpoints" >&2
        exit 1
    fi
    if grep -a -q -e "$HOME" -e "$PWD" "$bin"; then
        echo "$bin names a path of the build machine" >&2
        exit 1
    fi
    test "$("$@" "$bin" --version | awk '{print $2}')" = "$version"
}
prove "target/$x86/release/cairn"
prove "target/$x86/release/cairn-server"
prove "target/$arm/release/cairn" on_arm
for client in "target/$x86/release/cairn" "target/$arm/release/cairn"; do
    if [[ -n ${CAIRN_UPDATE_FEED-} ]] && ! grep -a -q -F "$CAIRN_UPDATE_FEED" "$client"; then
        echo "$client does not name its feed, $CAIRN_UPDATE_FEED" >&2
        exit 1
    fi
done

bash scripts/smoke.sh "target/$x86/release"
# aarch64's client against the x86_64 server, each of its runs through on_arm.
arm_bin=$(mktemp -d)
trap 'rm -rf -- "$arm_bin"' EXIT
if [[ $(uname -m) == aarch64 ]]; then
    ln -s "$PWD/target/$arm/release/cairn" "$arm_bin/cairn"
else
    printf '#!/bin/sh\nexec qemu-aarch64 %q "$@"\n' "$PWD/target/$arm/release/cairn" >"$arm_bin/cairn"
    chmod 755 "$arm_bin/cairn"
fi
ln -s "$PWD/target/$x86/release/cairn-server" "$arm_bin/cairn-server"
bash scripts/smoke.sh "$arm_bin"

# Each file is cairn-<v>-<platform>, then -tray for cairn-tray's and -server for cairn-server's, so a release's
# files sort by platform; cairn's, the main program's, names no program.
x86_name="cairn-$version-linux-x86_64"
arm_name="cairn-$version-linux-aarch64"
mkdir -p dist
cp "target/$x86/release/cairn" "dist/$x86_name"
cp "target/$x86/release/cairn-server" "dist/$x86_name-server"
cp "target/$arm/release/cairn" "dist/$arm_name"
ls -l "dist/$x86_name" "dist/$x86_name-server" "dist/$arm_name"
(cd dist && sha256sum "$x86_name" "$x86_name-server" "$arm_name")
