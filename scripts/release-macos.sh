#!/usr/bin/env bash
# The macOS half of a release, on a Mac: cairn and the tray built for both kinds of Mac and joined with lipo,
# each in its app bundle (tray/macos/bundle.sh), Cairn Sync.app and Cairn.app, signed with CAIRN_SIGN for
# notarization, notarized and stapled, then packed for updates as
#   dist/Cairn-Sync-<version>-macos-universal.app.tar.gz and dist/Cairn-<version>-macos-universal.app.tar.gz.
# CAIRN_MACOS_ARCH=aarch64 (or x86_64) builds for that kind alone, named so in place of universal; both
# need `rustup target add aarch64-apple-darwin x86_64-apple-darwin`.
#
# CAIRN_SIGN is a Developer ID Application identity: the only kind another Mac lets run, and the one whose
# team every later release keeps, as installed copies refuse an update by another team and macOS keeps
# their permissions by it. Notarization takes either an App Store Connect API key (CAIRN_NOTARY_KEY, the
# .p8 file, with CAIRN_NOTARY_KEY_ID and CAIRN_NOTARY_ISSUER) or a notarytool keychain profile
# (CAIRN_NOTARY_PROFILE); CAIRN_NOTARIZE=off skips it, for a build that is only tried, never published,
# as a Mac that downloads it from a browser refuses it. Over SSH, unlock the login keychain first.
# Also needs what release.sh does (update-env.sh), and Xcode's command-line tools.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/update-env.sh
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
: "${CAIRN_SIGN:?a Developer ID Application identity}"
notarize=${CAIRN_NOTARIZE-on}

python3 scripts/tray-version.py
cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
export RUSTFLAGS="${RUSTFLAGS-} --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup --remap-path-prefix=$PWD=/cairn"
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
arch=${CAIRN_MACOS_ARCH:-universal}
case $arch in
    universal) targets=(aarch64-apple-darwin x86_64-apple-darwin) ;;
    aarch64 | x86_64) targets=("$arch-apple-darwin") ;;
    *) echo "CAIRN_MACOS_ARCH is universal, aarch64 or x86_64" >&2; exit 2 ;;
esac
for t in "${targets[@]}"; do
    # -p, never --workspace: the harness turns failpoints on for whatever is built with it.
    cargo build --release --locked --target "$t" -p cairn-cli
    (cd tray && cargo build --release --locked --target "$t" --bin cairn-tray)
done
universal=target/macos-$arch
mkdir -p "$universal"
cairns=() trays=()
for t in "${targets[@]}"; do
    cairns+=("target/$t/release/cairn")
    trays+=("tray/target/$t/release/cairn-tray")
done
lipo -create -output "$universal/cairn" "${cairns[@]}"
lipo -create -output "$universal/cairn-tray" "${trays[@]}"
for bin in cairn cairn-tray; do
    if strings -a "$universal/$bin" | grep -q 'CAIRN_FP_'; then
        echo "$bin was built with failpoints" >&2
        exit 1
    fi
    test "$("$universal/$bin" --version | awk '{print $2}')" = "$version"
done
if [[ -n ${CAIRN_UPDATE_FEED-} ]] && ! grep -a -q -F "$CAIRN_UPDATE_FEED" "$universal/cairn"; then
    echo "cairn does not name its feed, $CAIRN_UPDATE_FEED" >&2
    exit 1
fi

stage=$(mktemp -d)
export CAIRN_SIGN
if [[ $notarize != off ]]; then
    export CAIRN_NOTARIZE=1
fi
bash tray/macos/bundle.sh "$universal/cairn" "$stage/Cairn Sync.app"
bash tray/macos/bundle.sh "$universal/cairn-tray" "$stage/Cairn.app"
mkdir -p dist
for pair in "Cairn Sync:Cairn-Sync" "Cairn:Cairn"; do
    app="$stage/${pair%%:*}.app"
    name="${pair#*:}-$version-macos-$arch.app.tar.gz"
    codesign --verify --strict --deep "$app"
    if [[ $notarize != off ]]; then
        zip="$stage/${pair#*:}.zip"
        ditto -c -k --keepParent "$app" "$zip"
        if [[ -n ${CAIRN_NOTARY_KEY-} ]]; then
            xcrun notarytool submit "$zip" --key "$CAIRN_NOTARY_KEY" --key-id "${CAIRN_NOTARY_KEY_ID:?}" \
                --issuer "${CAIRN_NOTARY_ISSUER:?}" --wait
        else
            xcrun notarytool submit "$zip" --keychain-profile "${CAIRN_NOTARY_PROFILE:?a notarytool profile, or CAIRN_NOTARY_KEY}" --wait
        fi
        xcrun stapler staple "$app"
        # What a Mac that downloaded it decides.
        spctl --assess --type execute --verbose=2 "$app"
        rm -f "$zip"
    fi
    # No ._ files for extended attributes: the signature covers the files alone.
    COPYFILE_DISABLE=1 tar -C "$stage" -czf "dist/$name" "${pair%%:*}.app"
    (cd dist && shasum -a 256 "$name" >"$name.sha256")
    ls -l "dist/$name"
done
rm -rf "$stage"
