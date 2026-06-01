# Building FFmpeg locally for player development

The player crate links against FFmpeg's `libav*` family on Windows,
Linux, and macOS. System packages (`libavcodec-dev`, BtbN releases,
Homebrew bottles) move at different rates per distro and Homebrew tap,
which has bitten us before — see the IAMF / DYNAMIC_HDR_SMPTE_2094_APP5
non-exhaustive enum break when Linux CI's system FFmpeg jumped past
ffmpeg-next's match arms.

To avoid that, build a known-good FFmpeg locally with the same minimal
config the BlackZone Console release pipeline ships. The smoke tests
in `player/tests/ffmpeg_smoke.rs` then verify that build still contains
everything the player calls into — so regressions are caught here, not
two repos downstream during a release.

The script at `player/scripts/build-ffmpeg.sh` mirrors
`BlackZoneConsole/vendor/build-ffmpeg.sh`. Keep them in sync if you
change codec or hwaccel selection.

## What the player needs from FFmpeg

| Surface       | What it's for                                  |
|---------------|------------------------------------------------|
| **Decoders**  | `h264`, `hevc` — software video fallback (HW paths exist via VideoToolbox/MediaCodec/D3D11VA/VAAPI). `aac`, `ac3`, `eac3` — audio on Win/Linux + macOS (macOS native AudioToolbox AAC mis-behaves on packetized input). |
| **Parsers**   | `h264`, `hevc`, `aac`, `ac3` — drive the NALU/AU split for the decode loop. |
| **Hwaccels**  | `d3d11va` + `dxva2` (Windows), `vaapi` (Linux), `videotoolbox` (macOS). |
| **Other libs**| `libswresample` for audio resample; `libavformat`/`libavfilter` are linked (ffmpeg-next defaults) but the player has its own MP4/DASH demuxer and runs no filter graphs. |

External codec libs (x264/x265/dav1d/aom/vpx) are NOT enabled — only
FFmpeg's built-in decoders. Keeps the build self-contained and the
bundle ~5–10 MB per platform.

## Pre-requisites

```text
linux:    gcc, make, nasm, pkg-config, curl, xz-utils, libva-dev, clang/libclang-dev
windows:  msys64 with mingw-w64-x86_64-toolchain + mingw-w64-x86_64-nasm
          + pkg-config + make; rustup gnu toolchain (cargo +stable-x86_64-pc-windows-gnu)
macos:    Xcode CLI tools (clang, make), brew install nasm pkg-config
```

Set `LIBCLANG_PATH` to wherever `libclang` lives on your machine — on
Windows that's typically `C:\msys64\mingw64\bin`; on Linux something
like `/usr/lib/llvm-18/lib`.

## Build

```bash
# from player/ — pick your platform:
./scripts/build-ffmpeg.sh linux
./scripts/build-ffmpeg.sh windows       # via C:\msys64\usr\bin\bash.exe -lc '...'
./scripts/build-ffmpeg.sh macos-arm64
./scripts/build-ffmpeg.sh macos-x64
```

The script is idempotent: it bails fast if
`player/vendor/<platform>/lib/pkgconfig/libavcodec.pc` already exists.
To force a rebuild, `rm -rf player/vendor/<platform>/` first.

## Point cargo at the vendored build

### Linux

```bash
export FFMPEG_PREFIX="$PWD/player/vendor/linux-x64"
export PKG_CONFIG_PATH="$FFMPEG_PREFIX/lib/pkgconfig"
export LD_LIBRARY_PATH="$FFMPEG_PREFIX/lib"
export RUSTFLAGS="-L native=$FFMPEG_PREFIX/lib -C link-arg=-Wl,-rpath,\$ORIGIN/lib -C link-arg=-Wl,--disable-new-dtags"
cargo build -p player
```

### Windows (PowerShell, GNU toolchain)

```powershell
$prefix = "$PWD\player\vendor\windows-x64"
$env:LIBCLANG_PATH      = "C:\msys64\mingw64\bin"
$env:PKG_CONFIG_PATH    = "$prefix\lib\pkgconfig"
$env:LIBRARY_PATH       = "$prefix\lib"
$env:PATH               = "C:\msys64\mingw64\bin;$prefix\bin;$prefix\lib;$env:PATH"
$env:PKG_CONFIG_ALLOW_CROSS = "1"
cargo +stable-x86_64-pc-windows-gnu build -p player --target x86_64-pc-windows-gnu
```

### macOS

```bash
export PREFIX="$PWD/player/vendor/macos-arm64"      # or macos-x64
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
export RUSTFLAGS="-C link-arg=-Wl,-headerpad_max_install_names -C link-arg=-Wl,-rpath,@executable_path/lib"
cargo build -p player --target aarch64-apple-darwin
```

## Run the smoke test

Same env vars as the build, then:

```bash
cargo test -p player --test ffmpeg_smoke
```

The test exercises `avcodec_find_decoder_by_name`, `av_parser_init`,
and `av_hwdevice_iterate_types` for the codecs/parsers/hwaccels
above. If any go missing — e.g. a future FFmpeg drops `eac3` from
defaults, or you forget `--enable-vaapi` on the rebuild — the test
fails with a pointer back to `scripts/build-ffmpeg.sh`.

`player/vendor/` is gitignored — local artefacts only.
