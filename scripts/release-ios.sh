#!/usr/bin/env bash
# The iOS app, on a Mac with Xcode: crates/mobile built for iPhones and their simulator, joined as
# ios/Generated/CairnMobile.xcframework with its Swift bindings in ios/Shared/Generated/, and then one of
#
#   --sim         the app built for the simulator (CAIRN_SIM names the device, an iPhone by default), and
#                 with CAIRN_TEST_SERVER and CAIRN_TEST_TOKEN set, its UI test run against that server,
#                 the photos access granted first; the screenshots are in build/ios-test.xcresult;
#   --device      an archive, exported for the devices registered to the team: build/ios/Cairn.ipa;
#   --testflight  an archive uploaded to App Store Connect, for TestFlight: outward-facing, so only when
#                 the owner asks. It needs the app's record there, which xcodebuild cannot make ("App
#                 record … not found"): the first time, open build/ios/Cairn.xcarchive in Xcode's
#                 Organizer and Distribute App, which offers to make it (its name must be free on the App
#                 Store, and the team's agreements in force);
#   --bindings    the library and bindings alone, for Xcode.
#
# Signing is automatic, by the team in ios/Config/Local.xcconfig (CAIRN_IOS_TEAM), which git leaves out
# as it does the server the Photos extension may send to (CAIRN_UPLOAD_URL_BASE); over SSH, unlock the
# login keychain first. Needs `rustup target add aarch64-apple-ios aarch64-apple-ios-sim`.
set -euo pipefail
cd "$(dirname "$0")/.."
mode=${1:---bindings}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
build=${CAIRN_IOS_BUILD:-$(date -u +%Y%m%d%H%M)}

cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
export RUSTFLAGS="${RUSTFLAGS-} --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup --remap-path-prefix=$PWD=/cairn"
export IPHONEOS_DEPLOYMENT_TARGET=26.4
targets=(aarch64-apple-ios aarch64-apple-ios-sim)
for t in "${targets[@]}"; do
    # -p, never --workspace: the harness turns failpoints on for whatever is built with it.
    cargo build --locked --profile mobile --target "$t" -p cairn-mobile --lib
    if strings -a "target/$t/mobile/libcairn_mobile.a" | grep -q 'CAIRN_FP_'; then
        echo "cairn-mobile was built with failpoints" >&2
        exit 1
    fi
done

gen=$(mktemp -d)
trap 'rm -rf "$gen"' EXIT
cargo run --locked -q -p cairn-mobile --bin uniffi-bindgen -- generate \
    --library target/aarch64-apple-ios/mobile/libcairn_mobile.a --language swift --out-dir "$gen"
mkdir -p "$gen/include"
cp "$gen/cairn_mobileFFI.h" "$gen/include/"
cp "$gen/cairn_mobileFFI.modulemap" "$gen/include/module.modulemap"
rm -rf ios/Generated/CairnMobile.xcframework
mkdir -p ios/Generated ios/Shared/Generated
xcodebuild -create-xcframework \
    -library target/aarch64-apple-ios/mobile/libcairn_mobile.a -headers "$gen/include" \
    -library target/aarch64-apple-ios-sim/mobile/libcairn_mobile.a -headers "$gen/include" \
    -output ios/Generated/CairnMobile.xcframework
cp "$gen/cairn_mobile.swift" ios/Shared/Generated/cairn_mobile.swift
echo "ios/Generated/CairnMobile.xcframework and ios/Shared/Generated/cairn_mobile.swift, $version"

settings=(CAIRN_VERSION="$version" CAIRN_BUILD="$build")
case $mode in
    --bindings) ;;
    --sim)
        name=${CAIRN_SIM:-$(xcrun simctl list devices available | sed -n 's/^ *\(iPhone [^(]*[^ (]\) (.*/\1/p' | head -1)}
        test -n "$name" || { echo "no iPhone simulator: xcodebuild -downloadPlatform iOS" >&2; exit 1; }
        dest="platform=iOS Simulator,name=$name"
        xcodebuild -project ios/Cairn.xcodeproj -scheme Cairn -configuration Debug -destination "$dest" \
            -derivedDataPath build/ios-derived "${settings[@]}" build
        if [[ -n ${CAIRN_TEST_SERVER-} ]]; then
            xcrun simctl boot "$name" 2>/dev/null || true
            xcrun simctl privacy "$name" grant photos storage.cairn.ios || true
            rm -rf build/ios-test.xcresult
            TEST_RUNNER_CAIRN_SERVER=$CAIRN_TEST_SERVER TEST_RUNNER_CAIRN_TOKEN=${CAIRN_TEST_TOKEN:?} \
                TEST_RUNNER_CAIRN_UPLOAD_PATH=${CAIRN_TEST_PATH:-phone/Simulator} \
                xcodebuild -project ios/Cairn.xcodeproj -scheme Cairn -configuration Debug -destination "$dest" \
                -derivedDataPath build/ios-derived -resultBundlePath build/ios-test.xcresult "${settings[@]}" test
        fi
        ;;
    --device | --testflight)
        rm -rf build/ios/Cairn.xcarchive
        xcodebuild -project ios/Cairn.xcodeproj -scheme Cairn -configuration Release -destination generic/platform=iOS \
            -archivePath build/ios/Cairn.xcarchive -allowProvisioningUpdates -allowProvisioningDeviceRegistration \
            "${settings[@]}" archive
        if [[ $mode == --device ]]; then
            method=debugging destination=export
        else
            method=app-store-connect destination=upload
        fi
        cat >build/ios/export.plist <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>method</key><string>$method</string>
<key>destination</key><string>$destination</string>
<key>signingStyle</key><string>automatic</string>
</dict></plist>
EOF
        xcodebuild -exportArchive -archivePath build/ios/Cairn.xcarchive -exportOptionsPlist build/ios/export.plist \
            -exportPath build/ios -allowProvisioningUpdates
        ;;
    *)
        echo "release-ios.sh [--bindings | --sim | --device | --testflight]" >&2
        exit 2
        ;;
esac
