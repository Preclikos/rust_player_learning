# Architecture

A DASH video player written in Rust, embeddable on desktop, Android, iOS and
the browser.
The repo is organized in **four layers** — engine → bridge → platform → examples
— plus the distribution packaging. Read top to bottom.

```
player/              ENGINE   — the core library (decode, A/V sync, render, DRM, ABR)
bridge/              BRIDGE   — platform-agnostic embedding layer over the engine
platform/
  android/           PLATFORM — JNI shell  → librustplayer.so → :rustplayer AAR
  ios/               PLATFORM — FFI shell   → librustplayer.a  → RustPlayerFFI.xcframework
  web/               PLATFORM — wasm-bindgen shell → rustplayer.js + rustplayer_bg.wasm
examples/
  desktop/           EXAMPLE  — winit desktop app (reference consumer / smoke test)
docs/handoffs/       working notes between sessions / integration
.github/workflows/   CI       — build + publish the AAR / XCFramework
```

## 1. `player/` — the engine

The actual product: a `Player` that fetches a DASH manifest, decrypts (CENC
ClearKey), decodes (per-platform HW: MediaCodec / VideoToolbox / D3D12 / VAAPI /
WebCodecs), A/V-syncs, and renders (wgpu / GLES / direct HW plane / WebGPU
canvas). HDR10 / HDR10+ / Dolby
Vision, ABR, subtitles. Provider-agnostic: it exposes generic primitives
(`RequestInterceptor`, `LicenseResolver`, `events()`, sinks) and bakes in NO
auth/CDN/DRM-endpoint specifics. See `PLAYER_INTEGRATION.md`.

## 2. `bridge/` — the embedding layer (crate `bridge`)

One reusable core (`bridge::start` → `BridgeHandle`) that wraps the engine with:
- a **unified control surface** (play/pause/seek/volume/tracks/track-switch/…),
- an **event pump** that serializes every `PlayerEvent` to one **JSON** schema,
- generic **provider hooks** (`BridgeHost::intercept` / `resolve_key`),
- the open_url → prepare → tracks → play orchestration + `StartConfig`.

The platform crates are thin shells over this; the desktop example reuses its
`run_test_playback` test fixture. This is the single source of truth for the
event/track JSON contract.

## 3. `platform/` — the bindings (what consumers actually depend on)

Thin shells that expose `bridge` to each OS as a **general-purpose player API**
(ExoPlayer/Shaka-style): the host passes a manifest URL + a provider, no
app-specific concepts inside.

- **`platform/android/`** — crate `bridge-android` (cdylib → `librustplayer.so`,
  `System.loadLibrary("rustplayer")`). JNI symbols
  `Java_cz_preclikos_rustplayer_NativeBridge_*`. Inside `android/` is the Gradle
  project: the **`:rustplayer`** library module (Kotlin `RustPlayer` /
  `RustPlayerProvider` API + the `.so`, published as the **AAR** to GitHub
  Packages) and the **`:app`** smoke-test consumer (`MainActivity`).
- **`platform/ios/`** — crate `bridge-ios` (staticlib → `librustplayer.a`, C FFI
  `bz_player_*`). Inside: `ios/` (an Obj-C smoke-test host) and `packaging/` (the
  SwiftPM **`RustPlayer`** package wrapping `RustPlayerFFI.xcframework`).
- **`platform/web/`** — crate `bridge-web` (cdylib → `rustplayer_bg.wasm` +
  wasm-bindgen glue `rustplayer.js`, the JS class **`RustPlayer`**). The page owns
  a `<canvas>`; provider hooks are JS functions on a host object (`onEvent`,
  `resolveKey`, optional `intercept`), awaited as promises. `www/` is the
  smoke-test page. See `platform/web/README.md`.

A consuming app adds the AAR (`implementation("cz.preclikos:rustplayer:…")`),
the SwiftPM package or the wasm-pack `pkg/` — **and compiles no Rust**.

## 4. `examples/` — reference consumers

- **`examples/desktop/`** — crate `example-desktop`: a winit app that plays the
  bundled test stream + a stdin track-control console. The desktop way to run
  the engine end to end.

## Build & distribution

- Rust: `cargo` workspace (root `Cargo.toml`). `cargo check --workspace` builds
  engine + bridge + desktop on the host; the android/ios/web crates are
  `#[cfg(target_os=…)]` / `#[cfg(target_arch = "wasm32")]`-gated (no-ops
  off-target). The engine never names an executor directly: `player::rt` is
  Tokio on native and wasm-bindgen-futures + `setTimeout` in the browser (the
  engine's `tokio::sync` channels and `select!` are runtime-agnostic and shared).
- Web: `platform/web/build.ps1` (wasm-pack → `platform/web/www/pkg/`); `cargo check
  -p bridge-web --target wasm32-unknown-unknown` needs no FFmpeg. WebCodecs is
  behind web-sys's unstable gate, set in `.cargo/config.toml` for the wasm32
  target. Browser backends inside the engine: `decoders/webcodecs.rs`,
  `renderers/audio/audio_web.rs`, `crypto_web.rs` (WebCrypto AES-CTR), the
  `CpuPlanes` frame path in `renderers/video.rs`.
- Android: `cargo ndk` cross-compiles `bridge-android` → the `:rustplayer`
  module's `jniLibs`; Gradle bundles the AAR. `.github/workflows/publish-android.yml`
  publishes it. Local device run: `test_android.ps1`.
- iOS (macOS only): `platform/ios/packaging/scripts/build_xcframework.sh` builds
  `bridge-ios` + FFmpeg into `RustPlayerFFI.xcframework`;
  `.github/workflows/publish-ios.yml` attaches it to a release. Simulator run:
  `platform/ios/ios/build_sim.sh`.

## Naming conventions

- Rust crates name their role: `player` (engine), `bridge` (core),
  `bridge-android` / `bridge-ios` / `bridge-web` (platform shells), `example-desktop`.
- The shipped native lib is **`librustplayer`** on both platforms; the Android
  library module / Maven artifact is **`rustplayer`**.
- Kotlin/Swift packages live under `cz.preclikos.rustplayer` / `RustPlayer`.
