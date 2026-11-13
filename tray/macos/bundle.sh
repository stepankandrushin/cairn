#!/usr/bin/env bash
# Makes a macOS app bundle around a built binary, from tray/:
#   bash macos/bundle.sh target/release/cairn-tray "out/Cairn Tray.app"   # the tray, storage.cairn.tray
#   bash macos/bundle.sh ../target/release/cairn out/Cairn.app             # the CLI and daemon, storage.cairn
# Each bundle is named as its program is shown, Cairn.app around cairn and Cairn Tray.app around cairn-tray,
# as Windows shows cairn.exe as "Cairn" and cairn-tray.exe as "Cairn Tray".
# The daemon runs from inside its bundle, so that System Settings shows "Cairn" with its icon, in Login
# Items and in the privacy lists, rather than a bare "cairn", and Activity Monitor too, as the daemon checks
# itself in with LaunchServices when it starts on a desktop (crates/cli/src/macos.rs). ~/.local/bin/cairn can
# be a link to it, though a daemon started through the link shows as "cairn". Neither is in the Dock: the tray
# is a UI element (LSUIElement), the daemon background-only (LSBackgroundOnly), so that opening its bundle
# never brings to the front a process with no event loop to answer.
#
# The icons come from icons/ (make.py): an asset catalog compiled by Xcode's actool when there is one, which
# macOS 26 and later draw properly at every size, and a plain .icns either way.
#
# Signed with CAIRN_SIGN, a codesigning identity from `security find-identity -v -p codesigning`, or ad hoc
# when it is unset. Login Items ties a LaunchAgent to its app (AssociatedBundleIdentifiers) only for a real
# signature, and macOS keeps the Local Network answer for the same certificate and identifier across builds,
# where an ad-hoc build is a new program each time. Over SSH, unlock the login keychain in the same
# connection first.
#
# With CAIRN_NOTARIZE=1, as scripts/release-macos.sh sets it, the signature is one Apple notarizes: the
# hardened runtime and a secure timestamp, which needs the network. A Developer ID is what installs on any Mac.
#
# Install a bundle by moving the new one into place, never by copying over the binary of a running one:
# macOS kills what starts from a signed binary changed in place. `cairn update` does just that.
set -euo pipefail
cd "$(dirname "$0")/.."
binary=$1
app=$2
case $(basename "$binary") in
    cairn-tray)
        plist=macos/Info.plist icons=icons/Cairn.xcassets icns=icon exe=cairn-tray
        version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
        ;;
    cairn)
        plist=macos/Sync-Info.plist icons=icons/Sync.xcassets icns=sync exe=cairn
        version=$("$binary" --version | awk '{print $2}')
        ;;
    *)
        echo "bundle.sh: $binary is neither cairn-tray nor cairn" >&2
        exit 2
        ;;
esac
[[ $version =~ ^[0-9A-Za-z.+-]+$ ]] || { echo "bundle.sh: no version from $binary" >&2; exit 1; }
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
sed "s/VERSION/$version/g" "$plist" > "$app/Contents/Info.plist"
cp "icons/$icns.icns" "$app/Contents/Resources/$icns.icns"
xcode=()
if [[ -z ${DEVELOPER_DIR-} ]] && ! xcrun --find actool >/dev/null 2>&1 &&
    [[ -d /Applications/Xcode.app/Contents/Developer ]]; then
    xcode=(DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer)
fi
# ${xcode[@]+...}: macOS's bash 3.2 calls an empty array unbound under set -u.
if env ${xcode[@]+"${xcode[@]}"} xcrun --find actool >/dev/null 2>&1; then
    partial=$(mktemp)
    # actool reads stdin, which would swallow the rest of a script piped into a shell.
    env ${xcode[@]+"${xcode[@]}"} xcrun actool "$icons" --compile "$app/Contents/Resources" --platform macosx \
        --minimum-deployment-target 11.0 --app-icon AppIcon --output-partial-info-plist "$partial" \
        </dev/null >/dev/null
    rm -f "$partial"
    [[ -e $app/Contents/Resources/Assets.car ]] || { echo "bundle.sh: actool made no Assets.car" >&2; exit 1; }
    # actool writes AppIcon.icns from the same images, beside the catalog: name both, as GuiIO does.
    rm "$app/Contents/Resources/$icns.icns"
    /usr/libexec/PlistBuddy -c 'Set :CFBundleIconFile AppIcon' -c 'Add :CFBundleIconName string AppIcon' \
        "$app/Contents/Info.plist"
else
    echo "bundle.sh: no actool (Xcode), so the icon is the plain $icns.icns, on a gray tile at small sizes"
fi
cp "$binary" "$app/Contents/MacOS/$exe"
if [[ ${CAIRN_NOTARIZE-} == 1 ]]; then
    codesign --force --sign "${CAIRN_SIGN:?CAIRN_NOTARIZE needs CAIRN_SIGN, a Developer ID}" --options runtime \
        --timestamp "$app"
else
    codesign --force --sign "${CAIRN_SIGN:--}" --timestamp=none "$app"
fi
codesign --verify --strict "$app"
echo "$app: $exe $version, signed by ${CAIRN_SIGN:-ad hoc}"
