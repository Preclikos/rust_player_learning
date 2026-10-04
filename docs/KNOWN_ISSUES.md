# Known issues

What is still open, unverified, or depends on the host app doing its part.
Each item was checked against the code; anything clearly fixed is not listed
(see `PITFALLS.md` for those). Remove an item when it is closed.

## Host contracts (the integrator must honour these)

These are not bugs in the player, but breaking them reproduces old bugs.

| Contract | Why | Where |
| --- | --- | --- |
| **Stop before leaving.** Call `Player::stop().await` and await the `play()` handle (Android: `RustPlayer.release()`, iOS: the wrapper's teardown) before starting another player. | Dropping a `JoinHandle` detaches the task; a second `start` without a stop runs two pipelines and plays audio in the background. | `player/src/player/mod.rs` (`stop`), `bridge/src/bridge.rs` (`orchestrate`) |
| **Detach surfaces in `surfaceDestroyed`.** Call `setOverlaySurface(null)` and `setVideoSurface(null)`, hand the new surfaces over in `surfaceCreated`, then `setSize`. | The player holds its own window ref, but it keeps drawing into a destroyed window (picture never returns) and AFR targets it. | `RustPlayer.kt` (`setVideoSurface`, `setOverlaySurface`) |
| **Centre the video plane** (aspect-fit, `Gravity.CENTER`). | Subtitles are placed on the full overlay surface or on a centred aspect-fit picture; the player does not know where the host put the plane. | `player/src/subtitle_style.rs` (`SubtitleAnchor`) |
| **Overlay z-order above any opaque window background.** | A `MediaOverlay` overlay can be covered in the letterbox bar, where cues sit by default. | host layout |
| **Pass the bottom safe inset.** | TV overscan crops the bottom 5–8 %; without an inset the default 8 % padding may still be cut on some panels. | `setSubtitleSafeInsetBottom`, `Player::set_subtitle_safe_insets` |
| **Report display timing** and re-report after an AFR mode switch (Android direct mode). | Release stamps snap to the vsync grid; with unknown timing the lead falls back to 50 ms and stutter can return. | `RustPlayer.kt` (`setDisplayTiming`), `player/src/present_lead.rs` |
| **iOS: call `syncVideoLayerFrame()` from `layoutSubviews`** when the render layer is re-laid out without a `setSize` (rotation, split view). | iOS layers have no autoresizing; the direct-mode video layer stays at the old frame. | `platform/ios/packaging/Sources/RustPlayer/RustPlayer.swift` |
| **Choose languages at start, not after.** Pass `preferredAudioLang` / `preferredSubtitleLang` to `start`. | Selecting audio after playback starts is a seek-rebuild; on a resume it can stall direct-mode decode. | `RustPlayer.start`, `StartConfig` |
| **Provider hooks may block, but must return.** `onRequest`/`resolveKey` run on the blocking pool. | A hook that never returns stalls that request until the segment retry budget runs out. | `platform/android/src/lib.rs` |

## Android request filter has no method or body

- **What:** Android's `RustPlayerProvider.PreparedRequest` carries `url` and
  `headers` only. The core `player::net::PreparedRequest` and the iOS boundary
  also carry `method` and `body`.
- **Impact:** an Android host cannot turn a request into a POST or replace a
  licence request body through the filter. Header and URL rewriting work.
- **Where:** `platform/android/android/rustplayer/.../RustPlayerProvider.kt`,
  the JNI mapping in `platform/android/src/lib.rs` (`..Default::default()`).
- **Next:** extend the Kotlin data class with `method: String?` and
  `body: ByteArray?` (defaulted, source compatible) and map them in JNI.

## iOS request filter: header on the wire not tested

- **What:** the iOS completion carries headers, method and body and builds, but
  no test proves a header set in the Swift provider reaches the HTTP request.
- **Impact:** a regression in the C marshalling (flat header array, string
  lifetimes) would silently drop auth headers again.
- **Where:** `platform/ios/src/lib.rs` (`rustplayer_intercept_complete`),
  `RustPlayer.swift` (`interceptCallback`).
- **Next:** a generic test provider returning `X-Test: 1` against a local echo
  server, asserting the header for the manifest request.

## A/V sync on Android devices not measured independently

- **What:** the flush-boundary / absolute-timeline fix was verified by the
  desktop conformance harness, which measures lip-sync from content. On Android
  (PCM AudioTrack and HDMI passthrough) there is no equivalent measurement;
  only the engine's own gauges and listening.
- **Impact:** a constant offset on device would go unnoticed by the drift
  gauge. An early passthrough build showed a constant ~110 ms (audio behind
  video) on an HDMI AVR; whether the later presented-position clock removed it
  has not been measured.
- **Where:** `player/src/av_sync.rs`, `player/src/player/clock.rs`,
  `renderers/audio/audio_track_pcm.rs`, `renderers/audio/audio_passthrough.rs`.
- **Next:** on a device, check `[audio-pcm] flush boundary … X ms of previous
  audio still queued` and `[vsync gen N] first frame pts=… target=…` after seeks
  and audio switches; longer term, HDMI capture of the conformance asset
  (phase 2 conformance).

## Passthrough edge cases

- **EC-3 fallback when the bitstream sink cannot be built.** If
  `AudioTrackSink::new` fails, the pipeline decodes E-AC-3 to PCM instead. That
  path was flaky in early testing (`queue_input ErrorUnknown` from MediaCodec)
  and has not been re-tested. Where: `player/src/player/mod.rs` (passthrough
  engage), `decoders/mediacodec_audio.rs`. Next: force the sink to fail on a
  device and play an EC-3-only stream.
- **Audible output on a real AVR** was inferred from a moving playback head,
  not confirmed by listening, in the original verification. Next: confirm by
  ear on a receiver when one is at hand.

## Google TV Streamer audio HAL wedge (outside the player)

- **What:** the box's MediaTek / Dolby MS12 output pipeline sometimes stops
  consuming audio box-wide for long periods; every new track is dead on
  arrival, a reboot does not clear it immediately. Trigger not pinned (an idle
  period seems to help; continuous force-stop/start may hold it).
- **Impact:** silent playback. The PCM sink's surrender layer keeps video at
  realtime and retries a fresh track every 30 s, so audio returns on its own.
- **Where:** `renderers/audio/audio_track_pcm.rs` (`check_stall`, surrender).
- **Next:** nothing app-side beyond the existing defence; record the trigger if
  it is ever reproduced on demand.

## Amlogic decoder dies across Home

- **What:** on the Mi TV Stick (Amlogic) the codec sometimes fails after Home
  and return (`dequeueInputBuffer -10000` or an input-buffer stall).
- **Impact:** a short stall; the supervisor rebuilds the pipeline and playback
  recovers. Root cause not investigated.
- **Where:** `decoders/mediacodec.rs`, `player/src/player/supervisor.rs`.
- **Next:** capture an unfiltered logcat around the Home transition to see
  whether the codec is reclaimed by the system or released by the surface
  teardown.

## Blocking MediaCodec calls in async tasks

- **What:** the Android MediaCodec dequeue loops block inline inside async
  tasks; a worker-thread floor of 6 keeps headroom.
- **Impact:** latent starvation risk on low-core devices if more blocking work
  lands on the runtime.
- **Where:** `decoders/mediacodec.rs`, `decoders/mediacodec_audio.rs`,
  runtime setup in `platform/android/src/lib.rs`.
- **Next:** move each decoder onto a dedicated thread (or one
  `spawn_blocking` per decoder lifetime). Not per-call `block_in_place`: that
  was measured worse.

## Direct-mode vsync snapping skips pulldown cadences

- **What:** release stamps snap to the vsync grid only for whole-vsync
  cadences; 24p on a 60 Hz display (3:2 pulldown) stays unsnapped.
- **Impact:** the phase-crawl stutter can still occur when AFR is off or the
  display refuses a 24 Hz mode.
- **Where:** `player/src/present_lead.rs`.
- **Next:** measure with `dumpsys SurfaceFlinger --latency` on a 60 Hz panel
  before deciding whether pulldown needs its own snapping.

## Windows GPU matrix untested

- **What:** decoder/renderer GPU pinning and the larger D3D11VA pool were
  verified on a single-GPU Intel machine only.
- **Impact:** possible `OpenSharedHandle failed` on dual-GPU laptops, or a
  decoder-open failure from memory on integrated GPUs (30 × 4K P010 surfaces
  ≈ 750 MB of shared RAM).
- **Where:** `decoders/ffmpeg_hw.rs` (`EXTRA_HW_FRAMES`,
  `set_render_adapter_luid`).
- **Next:** on an integrated + discrete laptop, check that the
  `[ffmpeg_hw] D3D11VA on adapter` name matches `[renderer] … adapter=`, with
  and without forcing the discrete GPU; run ABR switches with 4K HDR on an
  integrated GPU and on NVIDIA/AMD discrete. If memory fails, scale
  `EXTRA_HW_FRAMES` with resolution.

## Apple: kept VT session not checked visually

- **What:** decoder reuse on Apple (same-size rungs) and the new-session path
  were verified from logs; nobody watched for flicker or a wrong frame right
  after a switch.
- **Where:** `decoders/videotoolbox.rs`.
- **Next:** a visual check on macOS and an iPhone across several switches,
  including SDR ↔ 10-bit PQ.
