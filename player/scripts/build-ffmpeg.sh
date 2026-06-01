#!/usr/bin/env bash
# Build FFmpeg 7.1.1 from source with the minimal config the player
# crate relies on. Mirrors BlackZoneConsole/vendor/build-ffmpeg.sh —
# keep both in sync if you tweak codec / hwaccel selection.
#
# What the player actually uses (grep ffmpeg_audio.rs / ffmpeg_hw.rs):
#   - libavcodec   decoders: h264, hevc (video — software fallback);
#                            aac, ac3, eac3 (audio — Win/Linux full
#                            path, macOS audio-only since video is
#                            VTDecompressionSession)
#   - libavcodec   parsers:  h264, hevc, aac, ac3
#   - libavformat  is linked (ffmpeg-next defaults) but the player
#                  has its own MP4/DASH demuxer; nothing else is used
#   - libavutil    frame/packet/channel-layout types
#   - libswresample audio resampling (AVCodec → cpal output)
#   - libavfilter  linked, no filter graphs run
# Disabled: postproc (GPL trigger), avdevice (no system I/O), all
# external codec libs (x264/x265/vpx/aom/dav1d/…). No transitive deps,
# bundle stays ~5–10 MB of shared objects per platform.
#
# Usage: build-ffmpeg.sh <linux | windows | macos-x64 | macos-arm64
#                         | ios-arm64 | ios-sim-arm64 | ios-sim-x64>
#
# Idempotent: if vendor/<platform>/lib/pkgconfig/libavcodec.pc exists,
# bails fast. To force rebuild: `rm -rf player/vendor/<platform>/`.
#
# iOS builds are STATIC + audio-only (the player decodes video with native
# VideoToolbox; FFmpeg on iOS is only the AAC/AC-3/E-AC-3 audio path). Desktop
# builds stay shared + full (video software fallback + hwaccel).
#
# Runner pre-reqs:
#   linux:       gcc, make, nasm, pkg-config, curl, xz-utils, libva-dev
#   windows:     msys64 mingw-w64-x86_64-toolchain + mingw-w64-x86_64-nasm
#                + pkg-config + make; run through msys64 bash:
#                `C:\msys64\usr\bin\bash.exe -lc './scripts/build-ffmpeg.sh windows'`
#   macos / ios: Xcode (full, for the iOS SDKs), brew install nasm pkg-config
set -euo pipefail

PLATFORM="${1:?usage: $0 <linux|windows|macos-x64|macos-arm64|ios-arm64|ios-sim-arm64|ios-sim-x64>}"
FFMPEG_VERSION="7.1.1"
# Script lives at player/scripts/build-ffmpeg.sh — go up to player/.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Defaults (desktop): shared libs, full decoder/parser set. iOS overrides these
# to static + audio-only below.
LINK_KIND=(--enable-shared --disable-static)
DECODERS="h264,hevc,aac,ac3,eac3"
PARSERS="h264,hevc,aac,ac3"

case "$PLATFORM" in
  linux)
    PREFIX="$ROOT/vendor/linux-x64"
    JOBS="$(nproc 2>/dev/null || echo 4)"
    # Player's ffmpeg_hw.rs hits AV_HWDEVICE_TYPE_VAAPI on Linux —
    # requires libva-dev at build time + libva.so.2 at runtime
    # (usually pre-installed alongside any Intel/AMD GPU driver).
    # With --disable-autodetect we have to opt in explicitly.
    EXTRA=(
      --enable-vaapi
      --enable-hwaccel=h264_vaapi,hevc_vaapi
    )
    ;;
  windows)
    PREFIX="$ROOT/vendor/windows-x64"
    JOBS="$(nproc 2>/dev/null || echo 4)"
    # `--enable-w32threads --disable-pthreads` makes FFmpeg use the
    # native Win32 threading API instead of winpthread directly. That
    # alone isn't enough — msys2's mingw GCC is built with the POSIX
    # threading model, so libgcc itself has a transitive
    # libwinpthread-1.dll dep. Static-link winpthread explicitly via
    # `-Wl,-Bstatic -lwinpthread -Wl,-Bdynamic` so libgcc's pthread
    # bits get inlined and no DT_NEEDED winpthread entry lands in the
    # output DLLs.
    # Player crate uses D3D11VA via AV_HWDEVICE_TYPE_D3D11VA. DXVA2 is
    # bundled too because some FFmpeg internal helpers cross-reference
    # DXVA2 paths even when D3D11VA is the decode target.
    EXTRA=(
      --target-os=mingw64
      --arch=x86_64
      --enable-w32threads
      --disable-pthreads
      --enable-d3d11va
      --enable-dxva2
      --enable-hwaccel=h264_d3d11va,hevc_d3d11va,h264_dxva2,hevc_dxva2
      --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb
      "--extra-ldflags=-static-libgcc -static-libstdc++ -Wl,-Bstatic -lwinpthread -Wl,-Bdynamic"
    )
    ;;
  macos-x64)
    PREFIX="$ROOT/vendor/macos-x64"
    JOBS="$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"
    EXTRA=(
      --install-name-dir=@rpath
      --enable-hwaccel=h264_videotoolbox,hevc_videotoolbox
    )
    ;;
  macos-arm64)
    PREFIX="$ROOT/vendor/macos-arm64"
    JOBS="$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"
    # Cross-compile from Intel host to arm64. `-arch arm64` via
    # extra-cflags + extra-ldflags so it propagates to every cc + ld
    # invocation (a multi-token CC='clang -arch arm64' gets stripped
    # through some make rules that parse $(CC) as a single token,
    # producing arm64 .o files but an x86_64 final link — ld then
    # can't resolve the NEON symbols `ff_tx_fft*_float_neon`).
    # --install-name-dir=@rpath bakes @rpath into LC_ID_DYLIB of each
    # produced dylib so the binary's load commands resolve via
    # LC_RPATH @executable_path/lib at runtime.
    # --disable-inline-asm sidesteps a clang/FFmpeg 7.1.1
    # incompatibility in libavcodec/aarch64/cabac.h ("I" constraint
    # immediate of 512 outside the instruction's range). External .S
    # asm stays enabled so codec hot paths still use NEON.
    EXTRA=(
      --install-name-dir=@rpath
      --enable-cross-compile
      --target-os=darwin
      --arch=arm64
      --cc=clang
      "--extra-cflags=-arch arm64"
      "--extra-ldflags=-arch arm64"
      --disable-inline-asm
      --enable-hwaccel=h264_videotoolbox,hevc_videotoolbox
    )
    ;;
  ios-arm64|ios-sim-arm64|ios-sim-x64)
    PREFIX="$ROOT/vendor/$PLATFORM"
    JOBS="$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"
    # iOS: STATIC libs (an app can't load arbitrary dylibs without embedding +
    # signing a framework) and AUDIO-ONLY (video = native VideoToolbox).
    LINK_KIND=(--enable-static --disable-shared)
    DECODERS="aac,ac3,eac3"
    PARSERS="aac,ac3"
    case "$PLATFORM" in
      ios-arm64)     SDK=iphoneos;        ARCH=arm64;  MINVER="-miphoneos-version-min=15.0" ;;
      ios-sim-arm64) SDK=iphonesimulator; ARCH=arm64;  MINVER="-mios-simulator-version-min=15.0" ;;
      ios-sim-x64)   SDK=iphonesimulator; ARCH=x86_64; MINVER="-mios-simulator-version-min=15.0" ;;
    esac
    SYSROOT="$(xcrun --sdk "$SDK" --show-sdk-path)"
    # `--cc=$(xcrun -f clang)` is a single absolute-path token (avoids the
    # multi-token CC stripping noted in the macos-arm64 case); `-arch` +
    # `-isysroot` in the flags do the actual cross-targeting. inline-asm off:
    # arm64 hits the same libavcodec/aarch64/cabac.h clang incompat as macOS
    # arm64, and audio-only decode doesn't need the asm hot paths anyway.
    EXTRA=(
      --enable-cross-compile
      --target-os=darwin
      --arch="$ARCH"
      --cc="$(xcrun -f clang)"
      --sysroot="$SYSROOT"
      "--extra-cflags=-arch $ARCH -isysroot $SYSROOT $MINVER"
      "--extra-ldflags=-arch $ARCH -isysroot $SYSROOT $MINVER"
      --disable-inline-asm
    )
    ;;
  *)
    echo "error: unknown platform '$PLATFORM' (expected linux|windows|macos-x64|macos-arm64|ios-arm64|ios-sim-arm64|ios-sim-x64)" >&2
    exit 1
    ;;
esac

if [ -f "$PREFIX/lib/pkgconfig/libavcodec.pc" ]; then
  echo "FFmpeg already built at $PREFIX — skipping"
  ls "$PREFIX/lib/" | head
  exit 0
fi

SRC_PARENT="$PREFIX/src"
BUILD="$PREFIX/build"
mkdir -p "$SRC_PARENT" "$BUILD"
SRC="$SRC_PARENT/ffmpeg-$FFMPEG_VERSION"

if [ ! -f "$SRC/configure" ]; then
  echo "Downloading FFmpeg $FFMPEG_VERSION source..."
  TARBALL="$SRC_PARENT/ffmpeg.tar.xz"
  curl -fL -o "$TARBALL" \
    "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz"
  tar -xJf "$TARBALL" -C "$SRC_PARENT"
  rm "$TARBALL"
fi

cd "$BUILD"
mkdir -p "$PREFIX"
"$SRC/configure" \
  --prefix="$PREFIX" \
  "${LINK_KIND[@]}" \
  --disable-programs --disable-doc \
  --disable-htmlpages --disable-manpages --disable-podpages --disable-txtpages \
  --disable-postproc \
  --disable-autodetect \
  --disable-everything \
  --enable-decoder="$DECODERS" \
  --enable-parser="$PARSERS" \
  "${EXTRA[@]}" 2>&1 | tee "$PREFIX/configure.log"

echo ""
echo "===== Enabled hwaccels / decoders / parsers ====="
grep -A 3 -E "^Enabled (hwaccels|decoders|parsers|protocols|demuxers|muxers|filters)" "$PREFIX/configure.log" || true
echo "================================================="

echo "Building with $JOBS parallel jobs..."
make -j"$JOBS"
make install

# Tidy up: drop the build dir + extracted sources, we only need
# $PREFIX/lib/, /include/, /bin/ for cargo. Sources can be re-fetched
# from ffmpeg.org on a re-configure.
rm -rf "$BUILD" "$SRC_PARENT"
echo ""
echo "FFmpeg $FFMPEG_VERSION installed to $PREFIX"
echo "Libraries:"
ls "$PREFIX/lib/" | grep -E '\.(so\.|dylib|dll\.a|a)$' | head
echo "pkg-config files:"
ls "$PREFIX/lib/pkgconfig/" | head
