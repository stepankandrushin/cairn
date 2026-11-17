#!/usr/bin/env bash
# The Android app: crates/mobile built for phones (arm64-v8a) and the emulator (x86_64) into
# android/app/src/generated/jniLibs, with its Kotlin bindings in android/app/src/generated/kotlin, and
# then one of
#
#   --apk         the app built for release and signed: build/android/Cairn-<version>.apk, which installs
#                 on any phone from Android 11 on, and the same file as dist/cairn-<version>-android.apk, the
#                 name a release publishes it under beside the other platforms' files (release-update.sh
#                 puts it in the release's SHA-256 list). Signed by the key that CAIRN_ANDROID_KEYSTORE names, its
#                 password in CAIRN_ANDROID_KEYSTORE_PASSWORD and its alias in CAIRN_ANDROID_KEY_ALIAS
#                 (cairn by default), or by those in the file CAIRN_ANDROID_ENV names. Every build an
#                 installed copy takes as an update must be signed by the same key;
#   --emulator    that APK installed on the emulator adb sees, and started;
#   --debug       the app built for debugging, signed by this machine's debug key: build/android/Cairn-debug.apk;
#   --bindings    the libraries and bindings alone, for Android Studio (android/ is its project).
#
# Needs the Android SDK (ANDROID_HOME, ~/Android/Sdk by default) with an NDK, a JDK 17 or later,
# `rustup target add aarch64-linux-android x86_64-linux-android` and `cargo install cargo-ndk --locked`.
set -euo pipefail
cd "$(dirname "$0")/.."
mode=${1:---apk}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

export ANDROID_HOME=${ANDROID_HOME:-$HOME/Android/Sdk}
if [[ -z ${ANDROID_NDK_HOME-} ]]; then
    ANDROID_NDK_HOME=$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -1)
    test -n "$ANDROID_NDK_HOME" || { echo "no NDK in $ANDROID_HOME/ndk: sdkmanager 'ndk;<version>'" >&2; exit 1; }
    export ANDROID_NDK_HOME
fi

cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
export RUSTFLAGS="${RUSTFLAGS-} --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup --remap-path-prefix=$PWD=/cairn"
gen=android/app/src/generated
rm -rf "$gen"
mkdir -p "$gen/kotlin"
declare -A triple=([arm64-v8a]=aarch64-linux-android [x86_64]=x86_64-linux-android)
for abi in arm64-v8a x86_64; do
    t=${triple[$abi]}
    # -p, never --workspace: the harness turns failpoints on for whatever is built with it. Android 11
    # (API 30) is the app's oldest.
    cargo ndk --platform 30 -t "$abi" build --locked --profile mobile -p cairn-mobile --lib
    if strings -a "target/$t/mobile/libcairn_mobile.so" | grep -q 'CAIRN_FP_'; then
        echo "cairn-mobile was built with failpoints" >&2
        exit 1
    fi
    mkdir -p "$gen/jniLibs/$abi"
    cp "target/$t/mobile/libcairn_mobile.so" "$gen/jniLibs/$abi/"
done
# The bindings from the static library: the shared one is stripped of uniffi's metadata.
cargo run --locked -q -p cairn-mobile --bin uniffi-bindgen -- generate --no-format \
    --library target/aarch64-linux-android/mobile/libcairn_mobile.a --language kotlin --out-dir "$gen/kotlin"
echo "$gen: libcairn_mobile.so for arm64-v8a and x86_64 and its Kotlin bindings, $version"

gradle=(sh ./gradlew --no-daemon -q -Pcairn.version="$version")
[[ -n ${CAIRN_ANDROID_BUILD-} ]] && gradle+=(-Pcairn.build="$CAIRN_ANDROID_BUILD")
mkdir -p build/android
case $mode in
    --bindings) ;;
    --debug)
        (cd android && "${gradle[@]}" assembleDebug)
        cp android/app/build/outputs/apk/debug/app-debug.apk build/android/Cairn-debug.apk
        echo build/android/Cairn-debug.apk
        ;;
    --apk | --emulator)
        if [[ -z ${CAIRN_ANDROID_KEYSTORE-} && -n ${CAIRN_ANDROID_ENV-} ]]; then
            set -a
            # shellcheck disable=SC1090
            . "$CAIRN_ANDROID_ENV"
            set +a
        fi
        test -n "${CAIRN_ANDROID_KEYSTORE-}" || { echo "no signing key: set CAIRN_ANDROID_KEYSTORE or CAIRN_ANDROID_ENV" >&2; exit 1; }
        (cd android && "${gradle[@]}" assembleRelease)
        apk=build/android/Cairn-$version.apk
        cp android/app/build/outputs/apk/release/app-release.apk "$apk"
        tools=$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)
        "$tools/apksigner" verify --print-certs "$apk" | grep -m1 'SHA-256'
        mkdir -p dist
        cp "$apk" "dist/cairn-$version-android.apk"
        echo "$apk"
        echo "dist/cairn-$version-android.apk"
        if [[ $mode == --emulator ]]; then
            adb=$ANDROID_HOME/platform-tools/adb
            "$adb" -e install -r "$apk"
            "$adb" -e shell am start -n storage.cairn.android/.MainActivity
        fi
        ;;
    *)
        echo "usage: $0 [--apk | --emulator | --debug | --bindings]" >&2
        exit 2
        ;;
esac
