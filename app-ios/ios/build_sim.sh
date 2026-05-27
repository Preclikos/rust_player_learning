#!/usr/bin/env bash
#
# Build + install + launch RustPlayer on the iOS Simulator.
#
# Bypasses Xcode IDE — we drive `cargo`, `xcrun clang`, and `simctl`
# directly. Requires:
#   * full Xcode.app (Command Line Tools alone don't have iphonesimulator SDK)
#   * rustup target installed for the simulator (the script handles that)
#   * the workspace already built once for the host (warms ~/.cargo)
#
# Usage:
#   ./app-ios/ios/build_sim.sh
#
# Optional env vars:
#   SIM_DEVICE   — simctl device type (default: iPhone 15)
#   SIM_RUNTIME  — simctl runtime    (default: latest)

set -euo pipefail

# Resolve repo root regardless of cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUNDLE_NAME="RustPlayer"
BUNDLE_ID="com.rust.player"
APP_DIR="$SCRIPT_DIR/$BUNDLE_NAME"
BUILD_DIR="$SCRIPT_DIR/build"
SIM_DEVICE="${SIM_DEVICE:-iPhone 15}"
SIM_RUNTIME="${SIM_RUNTIME:-}"

# ---------------------------------------------------------------------
# 1. Sanity-check toolchain
# ---------------------------------------------------------------------

if ! xcode-select -p | grep -q "Xcode.app"; then
    echo "error: xcode-select points to Command Line Tools." >&2
    echo "Install Xcode.app and run:" >&2
    echo "  sudo xcode-select -switch /Applications/Xcode.app/Contents/Developer" >&2
    exit 1
fi

if ! xcrun --sdk iphonesimulator --show-sdk-path >/dev/null 2>&1; then
    echo "error: iphonesimulator SDK missing — open Xcode.app once to finish setup." >&2
    exit 1
fi

# ---------------------------------------------------------------------
# 2. Pick the simulator target triple for the host arch
# ---------------------------------------------------------------------

HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
    arm64)  TARGET="aarch64-apple-ios-sim";  CLANG_ARCH="arm64" ;;
    x86_64) TARGET="x86_64-apple-ios";       CLANG_ARCH="x86_64" ;;
    *) echo "unsupported host arch: $HOST_ARCH" >&2; exit 1 ;;
esac

echo "==> Simulator target: $TARGET (arch: $CLANG_ARCH)"
rustup target add "$TARGET" >/dev/null

# ---------------------------------------------------------------------
# 3. Build the Rust staticlib
# ---------------------------------------------------------------------

echo "==> cargo build -p app-ios --target $TARGET --release"
( cd "$REPO_ROOT" && cargo build -p app-ios --target "$TARGET" --release )

STATICLIB="$REPO_ROOT/target/$TARGET/release/libapp_ios.a"
[ -f "$STATICLIB" ] || { echo "missing staticlib: $STATICLIB" >&2; exit 1; }

# ---------------------------------------------------------------------
# 4. Compile the Obj-C launcher and link against the staticlib
# ---------------------------------------------------------------------

mkdir -p "$BUILD_DIR/$BUNDLE_NAME.app"
BIN="$BUILD_DIR/$BUNDLE_NAME.app/$BUNDLE_NAME"

SDK_PATH="$(xcrun --sdk iphonesimulator --show-sdk-path)"

# Frameworks the Rust code transitively links — VideoToolbox + Metal +
# friends come in via `#[link(name = ...)]` attributes on the player crate,
# but the linker needs the search paths primed for the simulator SDK and
# system libobjc / libSystem.
echo "==> Linking $BUNDLE_NAME.app/$BUNDLE_NAME"
xcrun --sdk iphonesimulator clang \
    -arch "$CLANG_ARCH" \
    -mios-simulator-version-min=15.0 \
    -isysroot "$SDK_PATH" \
    -fobjc-arc \
    -framework UIKit \
    -framework Foundation \
    -framework QuartzCore \
    -framework Metal \
    -framework MetalKit \
    -framework CoreFoundation \
    -framework CoreVideo \
    -framework CoreMedia \
    -framework VideoToolbox \
    -framework AudioToolbox \
    -framework CoreAudio \
    -framework Security \
    -framework SystemConfiguration \
    -framework AVFoundation \
    -lc++ -liconv \
    "$SCRIPT_DIR/$BUNDLE_NAME/main.m" \
    "$STATICLIB" \
    -o "$BIN"

cp "$SCRIPT_DIR/$BUNDLE_NAME/Info.plist" "$BUILD_DIR/$BUNDLE_NAME.app/Info.plist"

echo "==> Bundle: $BUILD_DIR/$BUNDLE_NAME.app"

# ---------------------------------------------------------------------
# 5. Boot a simulator (reuse running one if any) + install + launch
# ---------------------------------------------------------------------

# Find a booted device, otherwise create + boot one matching SIM_DEVICE.
DEVICE_ID="$(xcrun simctl list devices booted 2>/dev/null | awk -F'[()]' '/Booted/ {print $2; exit}')"

if [ -z "$DEVICE_ID" ]; then
    echo "==> No booted simulator; creating $SIM_DEVICE"
    # Pick latest iOS runtime (or SIM_RUNTIME override).
    if [ -z "$SIM_RUNTIME" ]; then
        SIM_RUNTIME="$(xcrun simctl list runtimes available | awk '/iOS/ {print $NF}' | tail -1)"
    fi
    DEVICE_TYPE_ID="$(xcrun simctl list devicetypes | awk -F'[()]' -v d="$SIM_DEVICE" 'index($0, d) {print $(NF-1); exit}')"
    [ -n "$DEVICE_TYPE_ID" ] || { echo "device type not found: $SIM_DEVICE" >&2; exit 1; }
    DEVICE_ID="$(xcrun simctl create "RustPlayerTest" "$DEVICE_TYPE_ID" "$SIM_RUNTIME")"
    xcrun simctl boot "$DEVICE_ID"
    # Open Simulator.app so the window is visible.
    open -a Simulator
fi

echo "==> Simulator device: $DEVICE_ID"

echo "==> simctl install"
xcrun simctl install "$DEVICE_ID" "$BUILD_DIR/$BUNDLE_NAME.app"

echo "==> simctl launch (streaming logs)"
xcrun simctl launch --console-pty "$DEVICE_ID" "$BUNDLE_ID"
