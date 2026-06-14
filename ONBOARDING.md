# rust_player_learning — Onboarding Guide

A cross-platform encrypted DASH video player written in Rust.
Targets: Windows, Linux, macOS, Android (armeabi-v7a + arm64-v8a + x86_64), iOS.

Feature highlights: ClearKey CENC, hardware decode everywhere, ABR with
make-before-break representation swaps, HDR10 / HDR10+ / Dolby Vision
(native passthrough on Android direct mode, faithful tonemap elsewhere),
WebVTT subtitles, pipeline retry-with-resume, A/V drift measurement.

---

## Workspace layout

| Crate | Path | Role |
|---|---|---|
| **player** | `player/` | Core library: DASH/MPD, decoders, renderers, DRM, events |
| **app** | `app/` | Desktop shell (Windows / Linux / macOS) + stdin console (`abr on/off`, track switching) |
| **app-shared** | `app-shared/` | Test fixture shared by all shells (stream URL, keys, track pick) |
| **app-android** | `app-android/` | Android embed shell: host Activity + two SurfaceViews + JNI |
| **app-ios** | `app-ios/` | iOS shell (UIView + CAMetalLayer; simulator build script) |
| **app-web** | `app-web/` | experimental |

---

## player crate — source map

```
player/src/
  player.rs              Player struct + public API + A/V sync loops + pipeline supervisor
  events.rs              PlayerEvent / PlayerErrorKind / TrackInfo / Fps
  capabilities.rs        Static + probed PlayerCapabilities (hdr10, dolby_vision, tunable)
  abr.rs                 AbrStrategy (bandwidth EWMA) + AbrVideoProfile filters
  crypto.rs              AES-128-CTR ClearKey CENC + hvcC/dvcC/senc/tenc box parsing
  hdr_tonemap.rs         HdrTonemapParams (tonemap_opencl mobius mirror)
  manifest.rs            DASH MPD download + quick-xml parsing
  net.rs                 HttpClient, RequestInterceptor, LicenseResolver, RetryPolicy
  parsers/mp4.rs         ISO BMFF helpers + length-prefixed NALU → Annex-B
  parsers/hevc.rs        HEVC bitstream: SPS/VUI colour info, HDR SEI (mastering, CLL, HDR10+ ST 2094-40)
  parsers/vtt.rs         WebVTT cues (single-file + segmented)
  tracks.rs (+ tracks/)  Tracks, Video/Audio/Text adaptations, segment indexing, HDR/DV detection
  decoders/
    mod.rs               HwVideoDecoder/AudioDecoder traits, VideoColorInfo, HdrFrameMeta, frame types
    ffmpeg_hw.rs         Desktop video: FFmpeg D3D11VA / VAAPI (shared hw-device across ABR swaps)
    ffmpeg_audio.rs      Desktop + Apple audio: FFmpeg AAC/AC-3/EAC-3
    mediacodec.rs        Android video: ImageReader path + DIRECT-to-Surface path (incl. video/dolby-vision)
    mediacodec_audio.rs  Android audio: MediaCodec AAC
    videotoolbox.rs      Apple video: VTDecompressionSession (NV12 or 10-bit x420 destination)
  renderers/
    audio.rs             cpal output + resampler + played-samples clock (A/V drift reference)
    subtitle.rs          Cue store + fontdue rasterizer; wgpu overlay pass + CPU bitmaps for GLES
    video.rs             VideoRenderer: backend pick, wgpu pipelines, HDR detection, Android dispatch
    video/video_directx.rs    Windows D3D11→DX12 shared-handle import
    video/video_vaapi.rs      Linux VAAPI DMA-BUF import
    video/video_vulkan.rs     Vulkan image→wgpu wrap (AHB path, currently not default)
    video/video_mediacodec.rs Android AHB→VkImage helper (Vulkan path)
    video/video_gles_egl.rs   Android GLES present hook: OES video, HDR tonemap + GL scene
                              detection, SDR→PQ, subtitle quad, HDR metadata, dataspace
    video/video_metal.rs      Apple CVPixelBuffer→Metal plane import (8-bit + 10-bit)
    video/video_frame.rs      Desktop frame wrapper (native import / upload)
  renderers/shader.wgsl           SDR NV12 → RGB (exact limited-range BT.709)
  renderers/shader_hdr.wgsl       HDR10 P010 → SDR (tonemap_opencl mobius port)
  renderers/shader_hdr_detect.wgsl  Scene peak/average compute passes (desktop/Apple)
```

---

## Public API (player::Player)

See `PLAYER_INTEGRATION.md` for the full integration contract. Sketch:

```rust
// constructors (per platform)
Player::new_from_raw_handle(window, display, w, h)     // desktop
Player::new_from_android_surface(native_window, w, h)  // Android overlay surface
Player::new_from_metal_layer(layer, w, h)              // iOS/macOS

// lifecycle
open_url(url).await / prepare().await / get_tracks()
set_video_track / set_audio_track / set_subtitle_track / clear_subtitle_track
play() -> JoinHandle / seek / seek_relative / pause / resume / stop
events() -> broadcast::Receiver<PlayerEvent> / position()

// injection + policy
set_request_interceptor / set_license_resolver / set_clearkey
set_retry_policy / set_callback_timeout
set_abr_strategy / set_abr_video_profile

// rendering / platform
resize / volume / set_volume
set_subtitle_font(ttf_bytes)
set_hdr_tonemap(HdrTonemapParams)          // see player/HDR_TONEMAP.md
set_display_hdr_types(mask)                // Android: Display.getHdrCapabilities bitmask
set_video_output_window(ptr)               // Android: enable DIRECT mode (HW video plane)
capabilities() / probe_capabilities()
```

---

## Decoder pipeline

Segments download ahead (default 8 s target), get CENC-decrypted and
mp4-parsed on a blocking thread **overlapped with the previous segment's
feed** (software AES on 32-bit Android costs ~0.5 s per 4K segment —
inline it starved the codec at every boundary), then feed the platform
decoder sample-by-sample. Decoded frames flow through a small reorder
window into the vsync loop, which paces them against the wall clock and
drops late frames proportionally. ABR swaps are make-before-break (new
representation prefetches while the old decoder keeps playing, then
splices forward at the last rendered PTS). Pipeline failures retry from
the current position with backoff before surfacing an error.

### Desktop (Windows / Linux / macOS)
- **Video**: FFmpeg `ffmpeg-next 8.1` → D3D11VA (Windows) / VAAPI (Linux);
  macOS decodes via native VTDecompressionSession instead.
- **Audio**: FFmpeg → PCM → cpal (all desktop + Apple targets — AudioToolbox's
  AAC decoder mishandles packetised access units).

See [`README.md`](README.md) for the vendored FFmpeg build + smoke tests.

### Android
- **Video, direct mode (production for TV)**: NDK `AMediaCodec` renders
  straight into the host's video `Surface` — frames ride a HW video
  plane, HDR10/HDR10+/DV reach the display natively. Frame pacing via
  `releaseOutputBufferAtTime` ~100 ms ahead (the queue bridges segment
  boundaries). DV uses the platform `video/dolby-vision` decoder
  (resolved by NAME via Java `MediaCodecList` over JNI — by-type lookup
  returns the wrong profile's codec) with RPU NALs kept.
- **Video, GL path (SDR displays / single-surface hosts)**: MediaCodec →
  ImageReader → AHardwareBuffer → EGLImage → `GL_TEXTURE_EXTERNAL_OES`,
  drawn in a wgpu present hook directly to FBO 0. 10-bit streams use a
  PRIVATE-format ImageReader. HDR tonemaps in GLSL (see
  `player/HDR_TONEMAP.md`); HDR-capable panels can take BT2020_PQ
  passthrough.
- **Audio**: NDK MediaCodec (AAC) → PCM → cpal/AAudio.

### iOS / macOS
- **Video**: VTDecompressionSession → CVPixelBuffer → CVMetalTextureCache →
  wgpu Metal textures (R8/RG8 for NV12, R16/RG16 for the 10-bit HDR path).

---

## Device quirk encyclopedia (hard-won)

**Google TV Streamer (kirkwood / MT8696, 32-bit userspace, PowerVR GE9215):**
- Vulkan driver aborts in `BILParseStream` → GL-only backend.
- wgpu renderbuffer presents are silently discarded → all GL drawing
  happens in a present hook directly on FBO 0 (custom wgpu fork hook).
- `YCBCR_P010` ImageReader allocates but never delivers images →
  10-bit pool order is PRIVATE → P010 → YUV; the acquire spin is
  time-boxed so a bad combo errors instead of hanging.
- HWC refuses HDMI HDR for GPU-composited layers (even with BT2020_PQ
  dataspace + SMPTE 2086 metadata) — video-plane (direct mode) only.
- Runs the **armeabi-v7a** build → software AES (~20 MB/s); see the
  overlapped segment prep above.
- HEVC decoder pads aggressively (e.g. 2560×1440 content in a
  4096×1472 buffer) — content size must come from the `crop` rect
  (`AMediaFormat_getRect`, the Java-style `crop-left` int keys don't
  exist at the NDK level), and GL-path crops inset by 1 texel
  (AOSP SurfaceTexture rule) or bilinear bleeds green padding.
- Codec quirk: `AMediaCodec_createDecoderByType("video/dolby-vision")`
  returns the AVC-based `dvav.ser` decoder — silent black for HEVC DV.
- max AImageReader images = 32 (64 SIGABRTs).

**Samsung Galaxy S21 (Mali-G78):** GL path default; AImage lifetime
matters — hold the AImage (not just the AHB ref) until the frame leaves
the renderer, or MediaCodec overwrites queued frames (scene-cut
flicker).

**Android emulator:** `vulkan.ranchu` rejects `vkCreateInstance` →
GLES; ES 3.0 only (no compute).

---

## Key dependencies

| Crate | Version | Note |
|---|---|---|
| `wgpu` | git fork `Preclikos/wgpu` | adds the GLES present hook (FBO 0 drawing) + P010 |
| `ffmpeg-next` | 8.1 | desktop video + desktop/Apple audio; workspace-patched for 8.1.1+ headers |
| `ndk` / `ndk-sys` | 0.9 / 0.6 | MediaCodec, ImageReader, ANativeWindow (API 29 features) |
| `ndk-context` | 0.1 | JavaVM handle for the few Java-only APIs (MediaCodecList) |
| `jni` | 0.21 | Android Java interop |
| `glow` | 0.17 | raw GLES for the OES present hook |
| `cpal` | 0.17 | audio output (+ played-samples clock) |
| `fontdue` | — | subtitle rasterization |
| `reqwest` | 0.12 | HTTP (rustls on Android/iOS) |
| `quick-xml` / `re_mp4` | 0.37 / 0.3 | MPD + MP4 parsing |
| `aes` + `ctr` | — | ClearKey CENC (`opt-level = 3` even in dev — see Cargo.toml) |

---

## Android build

### Prerequisites
- Rust targets: `aarch64-linux-android`, `armv7-linux-androideabi`, `x86_64-linux-android`
- `cargo-ndk`; Android NDK per `ndkVersion` in `app-android/android/app/build.gradle`
- JDK 17+ (`JAVA_HOME = C:\Program Files\Android\Android Studio\jbr` works)

### Build + install (PowerShell)
```powershell
# one-shot helper: build → install → launch → filtered logcat
.\test_android.ps1            # add -Release for release profile

# or manually:
cargo ndk -t arm64-v8a -o app-android\android\app\src\main\jniLibs build -p app-android
cd app-android\android; .\gradlew.bat assembleDebug   # builds ALL ABIs via cargo-ndk
adb install -r app\build\outputs\apk\debug\app-debug.apk
```

### Useful logcat filter
```powershell
adb logcat | Select-String 'app_android|app_shared|player::|stall|LATE|BACKWARD'
```
Tags are Rust module paths (truncated to 23 chars): `player::decoders::med..`,
`player::renderers::vi..`, `player  ` (player.rs), `app_shared`, `app_android`.

### Test-shell knobs (files in `/sdcard/Android/data/cz.preclikos.rust_player/files/`)
| File | Values | Meaning |
|---|---|---|
| `video_pref.txt` | `hdr` / `dv` / rep index | representation pick (default: index 5 = 720p SDR) |
| `direct.txt` | `0` | disable direct mode (default ON) |
| `hdr_passthrough.txt` | `1` | enable GL-path HDR passthrough experiment |

---

## Test content

Encrypted DASH stream used for development:
```
https://preclikos.cz/examples/encrypted/manifest.mpd
```

ClearKey keys live in `app-shared/src/lib.rs` (single source for all
shells). Adaptation set 0 = HEVC ladder (480p–4K; ≤1080p truly SDR,
1440p/4K are PQ **despite the MPD claiming BT.709** — the player trusts
the SPS VUI, not the manifest). Adaptation set 1 = Dolby Vision profile
8 (`dvh1.08`, 1440p + 4K). One forced-English WebVTT subtitle track.

---

## Known gaps / next steps

- Audio pipeline has no internal retry (video does) — audio death parks
  playback in `Buffering { Stall }`.
- A/V drift is measured (`Stats::av_drift_ms`) but not yet servo-corrected.
- HLG renders through the SDR shader (washed) on tonemap paths.
- GL surface is RGBA8888 — GL-path HDR passthrough would band; 1010102
  needs a wgpu-fork surface-config change (moot once direct mode is used).
- tvOS direct-mode analog (`AVSampleBufferDisplayLayer`) not implemented.
- Apple targets not compile-verified from the Windows dev machine (ring
  can't cross-build) — first Mac build will tell.
