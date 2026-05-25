# rust_player_learning — Onboarding Guide

A cross-platform encrypted DASH video player written in Rust.
Targets: Windows, Linux, Android (armeabi-v7a + arm64-v8a + x86_64), iOS (stub).

---

## Workspace layout

| Crate | Path | Role |
|---|---|---|
| **player** | `player/` | Core library: DASH/MPD, decoders, renderers, DRM |
| **app** | `app/` | Desktop entry point (Windows / Linux) |
| **app-android** | `app-android/` | Android NativeActivity entry point |
| **app-ios** | `app-ios/` | iOS entry point (stub — VideoToolbox not wired) |

---

## player crate — source map

```
player/src/
  player.rs              Main Player struct + public API
  crypto.rs              AES-128-CTR ClearKey DRM decryption
  manifest.rs            DASH MPD download + quick-xml parsing
  networking.rs          HTTP client (reqwest), bandwidth measurement
  parsers/mp4.rs         ISO BMFF box parsing (ftyp/moov/sidx)
  tracks.rs              Tracks struct, VideoAdaptation, AudioAdaptation
  tracks/segment.rs      Segment (URL, byte range, PTS, duration)
  utils/time.rs          ISO 8601 duration → std::time::Duration
  decoders/
    mod.rs               HwVideoDecoder trait + DecodedVideoFrame types
    ffmpeg_hw.rs         Desktop: FFmpeg D3D11VA / VAAPI
    ffmpeg_audio.rs      Desktop: FFmpeg AAC
    mediacodec.rs        Android: NDK MediaCodec video
    mediacodec_audio.rs  Android: NDK MediaCodec AAC
    videotoolbox.rs      iOS: VideoToolbox (stub)
  renderers/
    audio.rs             cpal audio output pipeline
    video.rs             VideoRenderer — selects backend, dispatches frames
    video/video_directx.rs    Windows D3D11 (AHB import)
    video/video_vulkan.rs     Vulkan AHB → VkImage (Android Vulkan path)
    video/video_mediacodec.rs Android AHB → VkImage helper
    video/video_vaapi.rs      Linux VAAPI
    video/video_gles_egl.rs   Android GLES: EGLImageKHR + GL_TEXTURE_EXTERNAL_OES
    video/video_frame.rs      Desktop VideoFrame (CPU → GPU upload)
    video/shader.wgsl         NV12 / P010 → RGB wgpu shader
```

---

## Public API (player::Player)

```rust
Player::new(window: Arc<Window>) -> Self
player.open_url(url: &str) -> Result<()>          // fetch + parse MPD
player.set_clearkey(keys: HashMap<String,String>)  // register DRM keys
player.prepare() -> Result<()>                     // fetch init segments
player.get_tracks() -> Result<Tracks>
player.set_video_track(adaptation, repr)
player.set_audio_track(adaptation, repr)
player.play() -> Result<JoinHandle<()>>            // starts background loop
player.seek(target: Duration)
player.seek_relative(delta_ms: i64)
player.position() -> Duration
player.resize(size: PhysicalSize<u32>)
player.stop()
player.volume(diff: f32)
```

---

## Decoder pipeline

### Desktop (Windows / Linux)
- **Video**: FFmpeg `ffmpeg-next 8.1` → D3D11VA (Windows) or VAAPI (Linux) → `VideoFrame` → wgpu NV12 texture → `shader.wgsl`
- **Audio**: FFmpeg → PCM → cpal

### Android
- **Video**: NDK `MediaCodec` → `AHardwareBuffer` → one of two render paths (see below)
- **Audio**: NDK `MediaCodec` (mp4a-latm AAC) → PCM → cpal/AAudio

---

## Android render paths

### Path A — Vulkan + NV12 (Samsung Galaxy S etc.)
Chosen when adapter exposes `TEXTURE_FORMAT_NV12`.

```
AHardwareBuffer
  → vkImportAndroidHardwareBufferANDROID (video_mediacodec.rs)
  → VkImage (NV12)
  → wgpu Texture (NV12)
  → shader.wgsl (Y+UV planes → RGB)
  → wgpu surface present
```

### Path B — GLES + OES (Google TV MT8696, emulator, any non-Vulkan device)
Chosen when Vulkan is unavailable or broken. **Zero-copy**: no CPU read-back.

```
AHardwareBuffer
  → eglGetNativeClientBufferANDROID()  → EGLClientBuffer
  → eglCreateImageKHR(EGL_NATIVE_BUFFER_ANDROID) → EGLImageKHR
  → glEGLImageTargetTexture2DOES(GL_TEXTURE_EXTERNAL_OES)
  → OES sampler (hardware YCbCr→RGB)
  → wgpu swapchain renderbuffer (via temporary FBO)
  → wgpu present() blits renderbuffer → EGL window surface
```

Implemented in `video_gles_egl.rs`. The OES renderer is initialised once in
`VideoRenderer::new()` by locking the EGL context (`AdapterContext::lock()`).

---

## MT8696 (Google TV Chromecast HD) quirks

Device: `kirkwood`, SoC `MT8696`, ABI `armeabi-v7a` (32-bit), GPU `PowerVR Rogue`.

**Vulkan driver** (`vulkan.mt8696.so`) calls `abort()` in `BILParseStream` on ANY
Vulkan API call. Detection: check `/vendor/lib/hw/vulkan.mt8696.so` on disk before
touching any Vulkan API. If found → `Backends::GL` only → GLES path.

**wgpu limits**: PowerVR Rogue max texture dimension = 4096, not 8192.
Fix: `max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d`.

---

## Key dependencies

| Crate | Version | Note |
|---|---|---|
| `wgpu` | git fork | `https://github.com/Preclikos/wgpu.git` (custom patches) |
| `winit` | 0.30.9 | window + event loop |
| `ffmpeg-next` | 8.1.0 | desktop only |
| `ndk` | 0.9 | Android MediaCodec, API 29+ |
| `glow` | 0.17 | Android only — raw GLES calls for OES path |
| `ash` | 0.38 | Vulkan bindings |
| `cpal` | 0.17 | audio output |
| `reqwest` | 0.12 | HTTP (native-tls on desktop, rustls on Android) |
| `quick-xml` | 0.37 | DASH MPD parsing |
| `re_mp4` | 0.3 | MP4 box parsing |
| `aes` + `ctr` | 0.8 / 0.9 | AES-128-CTR ClearKey |

---

## Android build

### Prerequisites
- Rust targets: `aarch64-linux-android`, `armv7-linux-androideabi`, `x86_64-linux-android`
- `cargo-ndk`
- Android NDK 29 (`ndkVersion = "29.0.13113456"` in build.gradle)
- JDK 22 (`JAVA_HOME = C:/Java/jdk-22.0.2` on dev machine)
- `ANDROID_SDK_ROOT` / `ANDROID_NDK_HOME` set

### Build commands (PowerShell)
```powershell
# Rust: build all three ABIs
$env:JAVA_HOME = "C:/Java/jdk-22.0.2"
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 build -p app-android

# Android: assemble APK (runs cargo ndk internally via Gradle tasks)
cd app-android\android
.\gradlew assembleDebug
```

APK output: `app-android/android/app/build/outputs/apk/debug/app-debug.apk`

Gradle tasks `buildRustDebug` / `buildRustRelease` invoke cargo-ndk and copy
`libapp_android.so` + `libc++_shared.so` into `src/main/jniLibs/<abi>/`.

### Install + logcat
```powershell
adb install -r app-debug.apk
adb logcat -s RustStdoutStderr:V player:V gles_oes:V
```

Key logcat tags: `RustStdoutStderr` (all `log::*` output from Rust).

### SSH / git with PPK key
```powershell
$env:GIT_SSH_COMMAND = '"C:\Program Files\PuTTY\plink.exe" -batch -i "C:\Users\husak\Documents\GitKeys\private.ppk"'
git fetch origin
git pull
```

---

## Git branches

| Branch | Purpose |
|---|---|
| `master` | main / stable |
| `feature/renderer_split` | current — renderer refactor + GLES OES path |
| `audio` | audio pipeline work |

---

## Test content

Encrypted DASH stream used for development:
```
https://preclikos.cz/examples/encrypted/manifest.mpd
```

ClearKey DRM keys (hardcoded in both `app/src/main.rs` and `app-android/src/lib.rs`):
```
0fd37dac41c0e987e68d43b801b1210c → fd8d9f408c2bd702970afcd3b219e791
519af81ab2d284f52aa8257d96b5e4bd → 627ef72b42d98770dec20ecab46cd1f4
```

Default track selection: video index 5 (720p HEVC), first `mp4a` audio track.

---

## Known issues / TODO

- **iOS**: `app-ios` entry point doesn't construct `Player` yet — VideoToolbox decoder is a stub
- **Google TV MT8696**: GLES OES path initialised; rendering in progress (black screen being diagnosed — `video_gles_egl.rs`)
- **Android emulator**: Vulkan `vulkan.ranchu` rejects `vkCreateInstance` → falls back to GLES path automatically
