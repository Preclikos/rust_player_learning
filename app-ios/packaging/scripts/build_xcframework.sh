#!/usr/bin/env bash
#
# Build RustPlayerFFI.xcframework — the prebuilt static lib (Rust player +
# vendored FFmpeg) for iOS device + simulator, with the C headers. The SwiftPM
# `RustPlayer` package wraps it so consumers never compile Rust.
#
# Produces (next to Package.swift):
#   RustPlayerFFI.xcframework        — for the local `path:` binaryTarget
#   RustPlayerFFI.xcframework.zip    — for the published `url:`+`checksum:` target
#       (checksum: `swift package compute-checksum RustPlayerFFI.xcframework.zip`)
#
# REQUIRES macOS + full Xcode.app. NOT runnable on Windows/Linux — this is the
# one Phase-1 piece that must be verified on a Mac (see the Phase-1 handoff).
#
# Usage:  app-ios/packaging/scripts/build_xcframework.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"          # app-ios/packaging
REPO_ROOT="$(cd "$PKG_DIR/../.." && pwd)"
INCLUDE_DIR="$PKG_DIR/include"
OUT_DIR="$PKG_DIR/build"
XCF="$PKG_DIR/RustPlayerFFI.xcframework"
MIN_IOS="15.0"

command -v xcrun >/dev/null || { echo "error: macOS + Xcode required" >&2; exit 1; }
xcode-select -p | grep -q "Xcode.app" || {
    echo "error: xcode-select points at CLT, not Xcode.app" >&2; exit 1; }

mkdir -p "$OUT_DIR"

# Build one slice: cross-build FFmpeg + the app-ios staticlib, then merge them
# (an xcframework slice holds exactly ONE library) into one fat .a.
#   $1 rust target   $2 ffmpeg platform   $3 sdk   $4 arch   $5 min-version flag
build_lib() {
    local rust_target="$1" ff_platform="$2" sdk="$3" arch="$4" min_flag="$5"
    local ffprefix="$REPO_ROOT/player/vendor/$ff_platform"
    local sdk_path; sdk_path="$(xcrun --sdk "$sdk" --show-sdk-path)"

    # All diagnostics + sub-builds go to STDERR; the ONLY thing this function
    # writes to stdout is the merged-lib path (its captured return value).
    echo "==> [$rust_target] FFmpeg ($ff_platform)" >&2
    bash "$REPO_ROOT/player/scripts/build-ffmpeg.sh" "$ff_platform" >&2

    echo "==> [$rust_target] cargo build -p app-ios --release" >&2
    rustup target add "$rust_target" >/dev/null 2>&1
    PKG_CONFIG_PATH="$ffprefix/lib/pkgconfig" \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_ALL_STATIC=1 \
    BINDGEN_EXTRA_CLANG_ARGS="-isysroot $sdk_path -arch $arch $min_flag" \
        cargo build -p app-ios --target "$rust_target" --release \
        --manifest-path "$REPO_ROOT/Cargo.toml" >&2

    local staticlib="$REPO_ROOT/target/$rust_target/release/libapp_ios.a"
    [ -f "$staticlib" ] || { echo "missing $staticlib" >&2; exit 1; }

    # Merge bridge + all FFmpeg static libs into one library for this arch.
    local merged="$OUT_DIR/librustplayer-$rust_target.a"
    libtool -static -o "$merged" "$staticlib" "$ffprefix"/lib/*.a >&2
    echo "$merged"
}

DEVICE_LIB="$(build_lib aarch64-apple-ios       ios-arm64     iphoneos        arm64  "-miphoneos-version-min=$MIN_IOS")"
SIM_ARM_LIB="$(build_lib aarch64-apple-ios-sim  ios-sim-arm64 iphonesimulator arm64  "-mios-simulator-version-min=$MIN_IOS")"
SIM_X64_LIB="$(build_lib x86_64-apple-ios       ios-sim-x64   iphonesimulator x86_64 "-mios-simulator-version-min=$MIN_IOS")"

# One simulator slice carrying both sim arches.
SIM_FAT="$OUT_DIR/librustplayer-sim.a"
lipo -create "$SIM_ARM_LIB" "$SIM_X64_LIB" -output "$SIM_FAT"

echo "==> create-xcframework"
rm -rf "$XCF"
xcodebuild -create-xcframework \
    -library "$DEVICE_LIB" -headers "$INCLUDE_DIR" \
    -library "$SIM_FAT"    -headers "$INCLUDE_DIR" \
    -output "$XCF"

echo "==> zip + checksum"
( cd "$PKG_DIR" && rm -f RustPlayerFFI.xcframework.zip \
    && zip -ryq RustPlayerFFI.xcframework.zip RustPlayerFFI.xcframework )
if command -v swift >/dev/null; then
    echo -n "checksum: "
    ( cd "$PKG_DIR" && swift package compute-checksum RustPlayerFFI.xcframework.zip )
fi

echo "==> done: $XCF"
