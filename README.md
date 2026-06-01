# rust_player_learning

Cross-platform encrypted DASH video player in Rust.
Targets: Windows, Linux, macOS, Android, iOS.

For workspace layout, source map, decoder pipeline and the full
`Player` API see [`ONBOARDING.md`](ONBOARDING.md). For embedding the
player crate from a downstream consumer see
[`PLAYER_INTEGRATION.md`](PLAYER_INTEGRATION.md).

---

## Quick start (desktop: Windows / Linux / macOS)

The player links against FFmpeg's `libav*` family on desktop targets.
Distro packages and prebuilt bundles (apt, Homebrew, BtbN) move at
different rates and have broken our build before — most recently when
FFmpeg 8.1.x backports added enum variants `ffmpeg-next 8.1.0`'s
non-exhaustive match arms didn't cover. The fix is to build a
known-good FFmpeg locally with the script bundled in this repo, then
point cargo at it.

The same script also lives in `BlackZoneConsole/vendor/build-ffmpeg.sh`
(release pipeline). Keep both in sync if you change codec / hwaccel
selection — this one is the canonical version since the requirements
are dictated by what the player crate calls into.

### 1. Pre-requisites

| Platform | Install |
|---|---|
| **Linux** | `gcc make nasm pkg-config curl xz-utils libva-dev clang libclang-dev` |
| **Windows** | msys64 with `mingw-w64-x86_64-toolchain` + `mingw-w64-x86_64-nasm` + `pkg-config` + `make`; rustup gnu toolchain (`rustup toolchain install stable-x86_64-pc-windows-gnu`) |
| **macOS** | Xcode CLI tools (`xcode-select --install`), `brew install nasm pkg-config` |

Set `LIBCLANG_PATH` to wherever `libclang` lives:
- Windows: `C:\msys64\mingw64\bin`
- Linux: `/usr/lib/llvm-18/lib` (adjust to installed version —
  `find /usr/lib -name "libclang.so*"`)
- macOS: usually auto-detected via Xcode

### 2. Build FFmpeg locally

```bash
# from repo root
./player/scripts/build-ffmpeg.sh linux             # or windows / macos-arm64 / macos-x64
```

Idempotent — bails fast if `player/vendor/<platform>/lib/pkgconfig/libavcodec.pc`
already exists. To force a rebuild:

```bash
rm -rf player/vendor/<platform>/
./player/scripts/build-ffmpeg.sh <platform>
```

Windows must run under msys64's MinGW64 bash:

```powershell
$env:MSYSTEM = "MINGW64"
C:\msys64\usr\bin\bash.exe -lc "cd $(cygpath -u $PWD) && ./player/scripts/build-ffmpeg.sh windows"
```

The script downloads FFmpeg 7.1.1 source from ffmpeg.org, runs
`./configure` with the minimal feature set the player uses, builds,
and installs into `player/vendor/<platform>/`. Output is shared libs:

```
player/vendor/linux-x64/
  lib/libavcodec.so.61 libavutil.so.59 libswresample.so.5 …
  lib/pkgconfig/libavcodec.pc …
  include/libavcodec/avcodec.h …
```

### 3. Point cargo at the vendored build

**Linux:**

```bash
export FFMPEG_PREFIX="$PWD/player/vendor/linux-x64"
export PKG_CONFIG_PATH="$FFMPEG_PREFIX/lib/pkgconfig"
export LD_LIBRARY_PATH="$FFMPEG_PREFIX/lib"
export RUSTFLAGS="-L native=$FFMPEG_PREFIX/lib -C link-arg=-Wl,-rpath,\$ORIGIN/lib -C link-arg=-Wl,--disable-new-dtags"
```

The three RUSTFLAGS together guarantee the vendored FFmpeg wins over
anything on the host at both link and run time. `$ORIGIN/lib` rpath
makes the binary load libs from `lib/` next to itself regardless of
`LD_LIBRARY_PATH`.

**Windows (PowerShell, GNU toolchain):**

```powershell
$prefix = "$PWD\player\vendor\windows-x64"
$env:LIBCLANG_PATH        = "C:\msys64\mingw64\bin"
$env:PKG_CONFIG_PATH      = "$prefix\lib\pkgconfig"
$env:LIBRARY_PATH         = "$prefix\lib"
$env:PATH                 = "C:\msys64\mingw64\bin;$prefix\bin;$prefix\lib;$env:PATH"
$env:PKG_CONFIG_ALLOW_CROSS = "1"
$env:PKG_CONFIG_ALLOW_CROSS_x86_64_pc_windows_gnu = "1"
$env:PKG_CONFIG_PATH_x86_64_pc_windows_gnu = $env:PKG_CONFIG_PATH
$env:RUSTFLAGS = "-C link-arg=-static-libgcc -C link-arg=-static-libstdc++ -C link-arg=-Wl,-Bstatic -C link-arg=-lwinpthread -C link-arg=-Wl,-Bdynamic"
```

Use the GNU host toolchain so HOST == TARGET == `x86_64-pc-windows-gnu`
— eliminates cross-compile detection in `ffmpeg-sys-next`'s build.rs.

**macOS:**

```bash
export PREFIX="$PWD/player/vendor/macos-arm64"     # or macos-x64
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
export PKG_CONFIG_ALLOW_CROSS=1                     # only on Intel host → arm64 target
export RUSTFLAGS="-C link-arg=-Wl,-headerpad_max_install_names -C link-arg=-Wl,-rpath,@executable_path/lib"
# Intel host targeting aarch64-apple-darwin needs an explicit -arch for cc shims:
export CFLAGS_aarch64_apple_darwin="-arch arm64"
export CXXFLAGS_aarch64_apple_darwin="-arch arm64"
```

### 4. Build + smoke test

```bash
cargo build -p player

# Verify the linked FFmpeg has every codec / parser / hwaccel the
# player calls into. Each assert failure points back at the line in
# scripts/build-ffmpeg.sh that controls it. Runs in <1 s.
cargo test  -p player --test ffmpeg_smoke
```

The smoke test asserts:

- `av_version_info()` returns a 7.x string (catches accidental system-lib swap)
- `avcodec_find_decoder_by_name` succeeds for `h264`, `hevc`, `aac`, `ac3`, `eac3`
- `av_parser_init` opens parsers for `h264`, `hevc`, `aac`, `ac3`
- `av_hwdevice_iterate_types` exposes the platform's hwaccel (VAAPI on Linux,
  D3D11VA on Windows, VideoToolbox on macOS)

If you ever change `--enable-decoder` / `--enable-parser` / `--enable-hwaccel`
lines in `build-ffmpeg.sh`, also update `ffmpeg_smoke.rs` so the assertions
match the new set.

### 5. Run the desktop app

```bash
cargo run -p app -- <DASH manifest URL>
```

---

## What the player actually uses from FFmpeg

This is what `build-ffmpeg.sh` enables — anything else is `--disable-everything`.

| Surface | Modules | Notes |
|---|---|---|
| Decoders `h264 hevc` | `decoders/ffmpeg_hw.rs` | Software fallback. HW paths use D3D11VA / VAAPI / VTDecompressionSession / MediaCodec instead. |
| Decoders `aac ac3 eac3` | `decoders/ffmpeg_audio.rs` | macOS uses FFmpeg for audio (AudioToolbox AAC mis-handles packetized AUs). iOS/Android use native decoders. |
| Parsers `h264 hevc aac ac3` | `parsers/mp4.rs` + decoder front-ends | NALU/AU split. |
| Hwaccels `d3d11va dxva2` | Windows | DXVA2 bundled because some internal helpers cross-reference it. |
| Hwaccel `vaapi` | Linux | Needs `libva-dev` at FFmpeg configure + `libva.so.2` at runtime. |
| Hwaccels `h264_videotoolbox hevc_videotoolbox` | macOS | Player uses VT directly, but these stay enabled for the software path. |
| `libswresample` | `renderers/audio.rs` | Audio resample to cpal's preferred rate. |
| `libavformat`, `libavfilter` | — | Linked (ffmpeg-next default features) but unused — the player has its own MP4/DASH demuxer and runs no filter graphs. |

No external codec libs (x264/x265/dav1d/aom/vpx). Bundle stays
~5–10 MB of shared objects per platform.

---

## FFmpeg log forwarding

```rust
use player::{LogLevel, set_log_level};

set_log_level(LogLevel::Warning);  // call once at startup, before Player::new
```

Routes libav* diagnostics through the `log` crate with `target: "ffmpeg"`,
so `RUST_LOG=ffmpeg=debug` enables FFmpeg traces independently of the
player's own logs. Idempotent — re-calling only changes the verbosity
threshold. No-op stub on iOS / Android (no ffmpeg-sys-next there).

---

## Mobile builds

iOS and Android don't link FFmpeg. Audio + video both go through
platform-native decoders (`AVFoundation`/`VTDecompressionSession` +
`AudioToolbox` on Apple, `MediaCodec` via the NDK on Android), so
`ffmpeg-sys-next` isn't a dep on those targets and you can skip the
local FFmpeg build entirely.

- Android: `app-android/android/README.md` for the gradle + cargo-ndk flow
- iOS: `app-ios/ios/build_sim.sh` for the simulator xcframework build

---

## When the FFmpeg version moves

If we eventually need FFmpeg 8.x APIs in the player:

1. Bump `FFMPEG_VERSION` in `player/scripts/build-ffmpeg.sh` (and in
   the BlackZoneConsole mirror).
2. Bump `ffmpeg-next` / `ffmpeg-sys-next` versions in
   `player/Cargo.toml` and `app/Cargo.toml`.
3. Bump the version-band assertion in
   `player/tests/ffmpeg_smoke.rs::ffmpeg_version_is_in_7_x_series`.
4. Sanity-check that `ffmpeg-next` for the new major exists on
   crates.io with non-exhaustive match arms covering whatever
   point-release variants the system headers ship — see the
   `vendor/ffmpeg-next` patch history if you need to vendor + patch
   temporarily.
