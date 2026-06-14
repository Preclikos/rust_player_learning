# Player Integration Reference

> **Audience:** engineers embedding the `player` crate into a host
> application (TUI remote-control shells, Android/iOS apps, TV boxes).
> **Status:** describes the **implemented** API surface. The original
> request spec this file grew from has been fully delivered; sections
> below document behaviour as it exists in code today.
>
> Provider-agnostic rule still applies: the player exposes only generic
> primitives (interceptor, license resolver, event stream, sinks).
> All provider-specific behaviour (URL rewrites, auth headers, license
> body shapes) lives in downstream consumer crates.

---

## 1. Big picture

```
host app
  │  open_url / prepare / tracks / play / seek / events()
  ▼
Player<VideoSink, AudioSink>          one tokio runtime, host-provided
  │            │
  │            ├── download tasks (HttpClient + RequestInterceptor)
  │            ├── CENC ClearKey (set_clearkey / LicenseResolver)
  │            ├── per-platform HW decoder (HwVideoDecoder)
  │            └── A/V sync loops (wall clock + audio device clock)
  ▼
VideoRenderer / AudioRenderer         platform render + output paths
```

Key properties:

- **One `Player` per playback surface.** `Player` is `Clone` (cheap —
  shared `Arc` state); clones control the same playback.
- **The host owns windows/surfaces and the process model.** The player
  never creates windows.
- **Everything is event-driven**: subscribe via
  `player.events() -> broadcast::Receiver<PlayerEvent>` (64-deep ring,
  laggy subscribers get `Lagged` and continue from newest).

## 2. Lifecycle

```rust
let player = /* platform constructor, see §3 */;

player.set_request_interceptor(my_interceptor);   // optional
player.set_clearkey(kid_to_key_hex_map)?;          // and/or set_license_resolver
player.open_url(manifest_url).await?;              // MPD fetch + parse
player.prepare().await?;                           // init segments, codec probing
let tracks = player.get_tracks()?;
player.set_video_track(&adaptation, &representation);
player.set_audio_track(&adaptation, &representation);
player.set_subtitle_font(font_bytes)?;             // optional, enables subtitles
player.set_subtitle_track(&text_representation);   // optional
let handle = player.play()?;                       // spawns the pipeline
// ... events drive the UI; seek()/pause()/resume()/stop() any time
```

`play()` returns a `JoinHandle` that completes when playback truly ends
(EndOfStream, stop, or exhausted error retries). Internally, `seek()`
and track changes restart the pipeline without the handle completing.

### Resume semantics (important for retry UX)

When the video pipeline fails mid-stream (network death, decoder
fault), the player **retries internally from the current position**
(3 attempts with 1/2/3 s backoff; the budget refills after >10 s of
successful playback). The consumer sees `Buffering { Stall }` during
recovery. Only after exhausting the budget does it emit
`Error { kind: Decoder, .. }` and end playback — **parking the playback
position**. The next `play()` call resumes from that parked position
(priority: explicit `seek()` target > parked resume > start of
content). A consumer "Retry" button is therefore just `play()`.

## 3. Platform embedding

### 3.1 Desktop (Windows / Linux / macOS) — raw window handle

```rust
let player = Player::new_from_raw_handle(raw_window_handle, raw_display_handle, w, h);
player.resize(PhysicalSize::new(w, h));   // forward layout changes
```

The host keeps the window alive for the player's lifetime. Backends:
DX12 (Windows), Vulkan (Linux), Metal (macOS). HDR10 (P010) decodes via
D3D11VA / VAAPI / VideoToolbox and tonemaps in the player's wgpu shader
(see `player/HDR_TONEMAP.md`).

### 3.2 Android — embed model with TWO surfaces

The reference shell is `app-android` (`MainActivity.kt` + `lib.rs`).
A real host replicates this shape:

```
FrameLayout
 ├── videoView   : SurfaceView          ← MediaCodec renders here (direct mode)
 │     (wrapped in an aspect-ratio FrameLayout; see onVideoSize below)
 └── overlayView : SurfaceView (top)    ← wgpu/GLES — subtitles/UI, or video
       setZOrderMediaOverlay(true)         itself in the non-direct mode
       holder.setFormat(TRANSLUCENT)
```

```rust
// JNI side (both Surfaces acquired via ANativeWindow_fromSurface):
let player = Player::new_from_android_surface(overlay_window, w, h);
player.set_display_hdr_types(mask);             // Display.getHdrCapabilities bitmask
player.set_video_output_window(video_window);   // enables DIRECT mode
```

**Direct mode** (`set_video_output_window`) is the production path for
TV boxes: the decoder renders straight into the video `Surface`, frames
ride a hardware video plane, and **HDR10 / HDR10+ / Dolby Vision —
including dynamic metadata — reach the display exactly as the OS video
pipeline delivers them** (verified: a Google TV Streamer switches the
TV into HDR10+ mode; DV plays through the platform `video/dolby-vision`
decoder with RPUs intact). The overlay surface then only presents when
the active subtitle cue changes.

Without a video window the player renders video itself on the overlay
surface via GLES (OES external textures) with its own HDR→SDR tonemap —
the right mode for SDR displays and for hosts that can't provide a
second surface.

Host responsibilities in direct mode:

- **Aspect ratio + centering**: MediaCodec stretches to fill the surface.
  Listen to `PlayerEvent::Stats { current_resolution }` and resize the
  video SurfaceView to the content aspect — **and center it** (e.g.
  `Gravity.CENTER`; see `MainActivity.onVideoSize`). The overlay surface
  the player draws subtitles on is full-screen and has no way to know
  where you placed the video plane, so it anchors cues at the *full
  surface's* bottom-center. If the resized video plane is not centered
  (a `FrameLayout` defaults to top-left), letter-/pillar-boxed video and
  the subtitles drift apart and cues land off the picture / half into the
  black bar.
- **Display HDR caps**: pass `Display.getHdrCapabilities` types as a
  bitmask (bit 0 = Dolby Vision, 1 = HDR10, 2 = HLG, 3 = HDR10+).
- **Window lifetime**: both `ANativeWindow` refs must outlive the
  player; release them after dropping it.

The non-direct GLES path additionally supports **HDR passthrough**
(surface dataspace BT2020_PQ + SMPTE 2086/CTA-861.3 metadata) when the
display reports HDR10 — but HWCs on HDMI boxes typically refuse to
switch the HDMI output for GPU-composited layers (measured on MT8696),
so treat GL passthrough as phone-panel-only and prefer direct mode.

### 3.3 iOS / macOS — CAMetalLayer

```rust
let player = Player::new_from_metal_layer(layer_ptr, w, h);
```

Video decodes via `VTDecompressionSession`; HDR10 (PQ/HLG) requests a
10-bit (`x420`) destination and runs the same wgpu tonemap + scene
detection as desktop, falling back to VideoToolbox's internal 8-bit
conversion when the 10-bit destination is refused. Audio decodes via
FFmpeg (AudioToolbox's AAC decoder mishandles packetised access units).

Full Dolby Vision / HDR10+ passthrough on tvOS will use
`AVSampleBufferDisplayLayer` (the direct-mode analog); not implemented
yet.

## 4. Network injection

Implemented exactly as originally specced — summary:

- `HttpClient` is the single HTTP entry point; the host injects an
  `Arc<dyn RequestInterceptor>` via `player.set_request_interceptor`.
  `intercept(url, RequestKind) -> PreparedRequest` may rewrite the URL,
  add headers, override method/body. `RequestKind` distinguishes
  `Manifest` / `InitSegment` / `Segment` / `License`.
- `LicenseResolver::resolve(kid: [u8;16]) -> [u8;16]` is consulted on
  cache miss; `set_clearkey(HashMap)` pre-populates the cache so the
  resolver is never called for known keys.
- Both callbacks are time-boxed (~10 s, `set_callback_timeout`);
  failures surface as `Error { Interceptor | LicenseResolver }`.
- Retry policy for transient HTTP/transport errors:
  `set_retry_policy(RetryPolicy)` — defaults 3 attempts, 250 ms initial,
  ×2 backoff, ±20 % jitter; retryable statuses 408/425/429/5xx.

## 5. Events

`player.events()` — `tokio::sync::broadcast::Receiver<PlayerEvent>`:

| Event | When | Payload highlights |
|---|---|---|
| `Idle` | construction | |
| `ManifestLoaded` | `open_url` ok | duration, track counts |
| `Prepared` | `prepare()` ok | |
| `Buffering { reason }` | initial / stall / seek / track switch | |
| `Playing` | first frame after any buffering | |
| `Paused` | `pause()` | |
| `Position` | ≤ 4 Hz | `position`, `duration`, `buffered_ahead_secs`, `bandwidth_bps` |
| `TrackChanged` | selection or ABR switch | `TrackKind`, `TrackInfo` |
| `GlitchRecovered` | recovered hiccup | detail |
| `Stats` | ≤ 1 Hz | see below |
| `EndOfStream` | natural end only (never on errors) | |
| `Error { kind, detail }` | fatal after internal retries | `PlayerErrorKind` |

`Stats` fields: `video_frames_decoded`, `video_frames_dropped`,
`audio_underruns`, `net_stall_ms` (blocked-on-network ms in the last
second), `decoder_name` (e.g. `"MediaCodec"`, `"D3D11VA HEVC"`),
`current_resolution` (post-ABR, drives Android aspect),
`audio_peak_db: Option<[f32; 2]>` (VU meter),
`av_drift_ms: Option<i64>` — measured video-wall-clock minus
audio-device-clock drift since pipeline start. Expect a slow linear
trend from crystal mismatch (10–100 ppm); jumps indicate sync bugs.
The player logs a warning above |100 ms|.

`buffered_ahead_secs` = min(video, audio) decoded high-water PTS minus
current playback PTS — media that survives a network drop right now.

## 6. Tracks and metadata

`get_tracks() -> Tracks { video, audio, text }` — adaptations with
representations. Useful accessors:

```rust
// VideoRepresenation
r.label()                  // "1080p HEVC 10-bit · 8.5 Mbps · HDR10"
r.codec_short()            // "HEVC" / "H.264" / "AV1" / "Dolby Vision"
r.is_hdr10() / r.is_dolby_vision() / r.is_10bit()
r.dv_profile()             // Some(8) for dvh1.08.06
r.dv_base_layer_playable() // false for profile 5 (needs a platform DV decoder)
```

Colorimetry note: the player does **not** trust MPD colour signalling
(real streams mis-declare BT.709 on PQ content). The decode pipeline
parses the SPS VUI from the init segment — `transfer_characteristics`
16 (PQ) / 18 (HLG), BT.2020 primaries, bit depth — and that drives the
HDR path selection per representation, including mid-stream ABR
SDR↔HDR swaps. The manifest flags above are for UI/ABR filtering only.

Dolby Vision representations commonly live in their **own adaptation
set** — enumerate all of `tracks.video`, not just the first.

## 7. ABR

```rust
player.set_abr_strategy(AbrStrategy::BandwidthEwma { safety_factor: 1.25 });
player.set_abr_video_profile(AbrVideoProfile::HdrPreferred); // or SdrOnly / LockedDepth(8|10) / Adaptive
```

Manual `change_video_track()` wins over ABR (resets strategy to
`Manual`). DV representations whose base layer can't play (profile 5
without a platform DV decoder) are never auto-selected.

## 8. HDR / Dolby Vision

Per-platform behaviour (details in `player/HDR_TONEMAP.md`):

| Platform | HDR10 | HDR10+ dynamic | Dolby Vision | Output |
|---|---|---|---|---|
| Windows / Linux | ✅ tonemap (wgpu mobius + scene detection) | metadata parsed, detection preferred | profile 7/8 base layer | SDR |
| macOS / iOS | ✅ tonemap (same shader, 10-bit VT dest) | as above | profile 7/8 base layer | SDR |
| Android, GL path | ✅ tonemap (GLES port + GL scene detection) or GL passthrough on capable panels | SEI parsed → tonemap peak | profile 7/8 base layer | SDR / panel HDR |
| **Android, direct mode** | ✅ **native** | ✅ **native (ST 2094-40 reaches the display)** | ✅ **native (platform `video/dolby-vision` decoder, RPUs intact; profile 5 included)** | display-negotiated |

Host controls:

- `capabilities()` / `probe_capabilities()` — what this build can play.
- `set_display_hdr_types(mask)` — display capability hint (Android).
- `set_video_output_window(ptr)` — enable direct mode (Android).
- `set_hdr_tonemap(HdrTonemapParams)` — tonemap tuning where
  `hdr_tonemap_tunable` is true; default reproduces the SDR ladder's
  reference transcode. Irrelevant in direct mode (display tonemaps).

## 9. Subtitles

```rust
player.set_subtitle_font(ttf_bytes)?;            // required before cues render
player.set_subtitle_track(&text_representation); // WebVTT (single file or segmented)
player.clear_subtitle_track();
```

Rendering is built in on every platform: wgpu overlay pass on
desktop/Apple, GLES quad on Android (including direct mode, where the
translucent overlay surface presents only when the active cue changes).
Styling is fixed phase-1 (white, drop shadow, bottom-center, 7 % safe
area). The rasterizer feeds plain RGBA bitmaps into the renderers — a
future libass backend slots in at that same boundary.

On Android a system font works fine:
`std::fs::read("/system/fonts/Roboto-Regular.ttf")`.

## 10. Error semantics

| Situation | Behaviour | Surfaced as |
|---|---|---|
| Manifest non-2xx after retries | error from `open_url` | `Http { status }` |
| Segment transient (408/425/429/5xx, transport) | retried per `RetryPolicy` | `GlitchRecovered` on success |
| Segment 401/403/404 | not retried | `Http { status }` |
| Interceptor / resolver `Err` or timeout | not retried | `Interceptor` / `LicenseResolver` |
| Multi-period MPD | rejected in `open_url` | `ManifestParse` |
| Video pipeline death mid-play | internal retry from current position (3×, backoff, budget refills with progress) | `Buffering { Stall }` while retrying; `Error { Decoder }` + parked resume position when exhausted |
| DV profile 5 without platform DV decoder | rejected at pipeline start | `Decoder` (Android: clear message) |
| Natural end | — | `EndOfStream` (guaranteed NOT emitted for error stops) |

Known gap (tracked): the **audio** pipeline has no internal retry yet —
an audio-side death parks playback in `Buffering { Stall }`.

## 11. Backward compatibility

The original guarantees hold: `NoopInterceptor` default,
`set_clearkey` pre-populates the key cache, `events()` always valid,
and the bundled test shells (`app/`, `app-android/`, `app-ios/`) play
`https://preclikos.cz/examples/encrypted/manifest.mpd` end-to-end with
hardcoded keys. Additions since the original spec are strictly
additive; the only signature-level changes were new optional fields on
`PlayerEvent::Stats` (consumers matching with `..` are unaffected).
