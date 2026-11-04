#!/usr/bin/env bash
# Makes Cairn.app around a built cairn-tray: `bash macos/bundle.sh target/release/cairn-tray out/Cairn.app`,
# from tray/. LSUIElement keeps it out of the Dock. Signed ad hoc, so the bundle's seal matches what is in it.
# Install it by moving the new bundle into place, never by copying over the binary of a running one:
# macOS kills what starts from a signed binary changed in place.
set -euo pipefail
cd "$(dirname "$0")/.."
binary=$1
app=$2
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
sed "s/VERSION/$version/g" macos/Info.plist > "$app/Contents/Info.plist"
cp icons/icon.icns "$app/Contents/Resources/icon.icns"
cp "$binary" "$app/Contents/MacOS/cairn-tray"
codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict "$app"
echo "$app: cairn-tray $version"
