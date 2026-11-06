#!/usr/bin/env bash
# Sign dist/update.json, what every installed Cairn checks before it takes the release, from the release's
# files in dist/ for this version, as release.sh, release-macos.sh and release-windows.ps1 name them:
#   cairn-<v>-linux-x86_64, cairn-<v>-windows-x86_64.exe, cairn-tray-<v>-windows-x86_64.exe,
#   Cairn-Sync-<v>-macos-<arch>.app.tar.gz and Cairn-<v>-macos-<arch>.app.tar.gz, arch universal, aarch64
#   or x86_64.
# Needs CAIRN_UPDATE_SIGNING_KEY, the secret of a key the release was built to trust, which is checked
# against CAIRN_UPDATE_KEYS here, and CAIRN_RELEASE_URL, where the release's files are downloaded from,
# each by its name: for a GitHub release https://github.com/<owner>/<repo>/releases/download/v<version>.
# CAIRN_RELEASE_NOTES, a URL, is shown to people.
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
: "${CAIRN_UPDATE_SIGNING_KEY:?the signing key, base64 PKCS#8}"
: "${CAIRN_UPDATE_KEYS:?the public keys the release trusts}"
: "${CAIRN_RELEASE_URL:?where the files of the release are downloaded from}"
files=()
add() {
    if [[ -f dist/$2 ]]; then
        files+=("$1:dist/$2")
    fi
}
add cairn:linux-x86_64:binary "cairn-$version-linux-x86_64"
add cairn:windows-x86_64:binary "cairn-$version-windows-x86_64.exe"
add cairn-tray:windows-x86_64:binary "cairn-tray-$version-windows-x86_64.exe"
for arch in universal aarch64 x86_64; do
    add "cairn:macos-$arch:app" "Cairn-Sync-$version-macos-$arch.app.tar.gz"
    add "cairn-tray:macos-$arch:app" "Cairn-$version-macos-$arch.app.tar.gz"
done
if [[ ${#files[@]} -eq 0 ]]; then
    echo "no file of $version in dist/" >&2
    exit 1
fi
cargo build --release --locked -p cairn-release
target/release/cairn-release sign --version "$version" --base-url "$CAIRN_RELEASE_URL" \
    --notes "${CAIRN_RELEASE_NOTES-}" --out dist/update.json "${files[@]}"
keys=()
IFS=, read -ra trusted <<<"$CAIRN_UPDATE_KEYS"
for k in "${trusted[@]}"; do
    keys+=(--key "$k")
done
target/release/cairn-release verify "${keys[@]}" dist/update.json >/dev/null
printf '%s\n' "${files[@]}" | sed 's/^/  /'
