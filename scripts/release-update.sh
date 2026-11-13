#!/usr/bin/env bash
# Sign dist/update.json, what every installed Cairn checks before it takes the release, from the release's
# files in dist/ for this version, as release.sh, release-macos.sh and release-windows.ps1 name them:
#   cairn-<v>-linux-x86_64-sync, cairn-<v>-windows-x86_64-sync.exe, cairn-<v>-windows-x86_64-tray.exe,
#   cairn-<v>-macos-<arch>-sync.app.tar.gz and cairn-<v>-macos-<arch>-tray.app.tar.gz, arch universal,
#   aarch64 or x86_64.
# An installed copy finds its file by program and platform, never by name, and installs it under its own.
# Then write dist/cairn-<v>.sha256, the SHA-256 of every file of the version in dist/, the tarball too, for
# people to check what they downloaded by: `sha256sum -c --ignore-missing cairn-<v>.sha256`.
# Needs CAIRN_UPDATE_SIGNING_KEY, the secret of a key the release was built to trust, which is checked
# against CAIRN_UPDATE_KEYS here. Each file is named beside update.json, wherever that is fetched from, so
# the one signed update.json serves from every feed a build asks: a site's folder, or a GitHub release's
# latest download, https://github.com/<owner>/<repo>/releases/latest/download/update.json. CAIRN_RELEASE_URL,
# if given, names one place the files are downloaded from instead, each by its name under it.
# CAIRN_RELEASE_NOTES, a URL, is shown to people.
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
: "${CAIRN_UPDATE_SIGNING_KEY:?the signing key, base64 PKCS#8}"
: "${CAIRN_UPDATE_KEYS:?the public keys the release trusts}"
files=()
add() {
    if [[ -f dist/$2 ]]; then
        files+=("$1:dist/$2")
    fi
}
add cairn-sync:linux-x86_64:binary "cairn-$version-linux-x86_64-sync"
add cairn-sync:windows-x86_64:binary "cairn-$version-windows-x86_64-sync.exe"
add cairn-tray:windows-x86_64:binary "cairn-$version-windows-x86_64-tray.exe"
for arch in universal aarch64 x86_64; do
    add "cairn-sync:macos-$arch:app" "cairn-$version-macos-$arch-sync.app.tar.gz"
    add "cairn-tray:macos-$arch:app" "cairn-$version-macos-$arch-tray.app.tar.gz"
done
if [[ ${#files[@]} -eq 0 ]]; then
    echo "no file of $version in dist/" >&2
    exit 1
fi
cargo build --release --locked -p cairn-release
base=()
if [[ -n ${CAIRN_RELEASE_URL-} ]]; then
    base=(--base-url "$CAIRN_RELEASE_URL")
fi
target/release/cairn-release sign --version "$version" "${base[@]}" \
    --notes "${CAIRN_RELEASE_NOTES-}" --out dist/update.json "${files[@]}"
keys=()
IFS=, read -ra trusted <<<"$CAIRN_UPDATE_KEYS"
for k in "${trusted[@]}"; do
    keys+=(--key "$k")
done
target/release/cairn-release verify "${keys[@]}" dist/update.json >/dev/null
# Every file of the version, whose names all go on with a `-` after it; update.json, the latest release's
# wherever it is served, has its signature instead.
(cd dist && sha256sum -- "cairn-$version"-* >"cairn-$version.sha256")
cat "dist/cairn-$version.sha256"
printf '%s\n' "${files[@]}" | sed 's/^/  /'
