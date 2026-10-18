#!/usr/bin/env bash
#
# Builds the Android APK for the virtual-display client.
#
#   1. compiles the `vd-client` Rust crate for each configured ABI with cargo-ndk
#      and copies libvd_client.so into android/app/src/main/jniLibs/<abi>/
#   2. packages it with Gradle
#
# Requirements: Rust android targets, cargo-ndk, Android SDK + NDK, a JDK.
#
# Usage:
#   scripts/build-android.sh            # debug APK, arm64-v8a only
#   scripts/build-android.sh --release  # release APK
#   scripts/build-android.sh --all-abis # arm64-v8a + armeabi-v7a + x86_64

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ANDROID_DIR="$ROOT/android"
JNI_LIBS="$ANDROID_DIR/app/src/main/jniLibs"

PROFILE="debug"
CARGO_FLAGS=()
ABIS=(arm64-v8a)

for arg in "$@"; do
    case "$arg" in
        --release) PROFILE="release"; CARGO_FLAGS+=(--release) ;;
        --all-abis) ABIS=(arm64-v8a armeabi-v7a x86_64) ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# --- locate the NDK -----------------------------------------------------------
if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
    SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
    if [[ -z "$SDK" ]]; then
        echo "error: set ANDROID_NDK_HOME (or ANDROID_HOME)" >&2
        exit 1
    fi
    # Pick the newest installed NDK.
    NDK_DIR="$(find "$SDK/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort | tail -1)"
    if [[ -z "$NDK_DIR" ]]; then
        echo "error: no NDK found under $SDK/ndk" >&2
        exit 1
    fi
    export ANDROID_NDK_HOME="$(cygpath -w "$NDK_DIR" 2>/dev/null || echo "$NDK_DIR")"
fi

# --- native library -----------------------------------------------------------
TARGET_FLAGS=()
for abi in "${ABIS[@]}"; do
    TARGET_FLAGS+=(-t "$abi")
done

echo ">> cargo ndk (${ABIS[*]}, $PROFILE)"
cargo ndk "${TARGET_FLAGS[@]}" -o "$JNI_LIBS" build "${CARGO_FLAGS[@]}" -p vd-client --lib

# Gradle needs a Windows-style path on Windows shells.
if [[ -n "${JAVA_HOME:-}" ]]; then
    export JAVA_HOME="$(cygpath -w "$JAVA_HOME" 2>/dev/null || echo "$JAVA_HOME")"
fi

# --- package ------------------------------------------------------------------
GRADLE_TASK="assembleDebug"
[[ "$PROFILE" == "release" ]] && GRADLE_TASK="assembleRelease"

echo ">> gradle :app:$GRADLE_TASK"
cd "$ANDROID_DIR"
./gradlew ":app:$GRADLE_TASK"

echo
echo "APK written to $ANDROID_DIR/app/build/outputs/apk/$PROFILE/"
