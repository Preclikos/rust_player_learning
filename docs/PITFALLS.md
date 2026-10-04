# Pitfalls

Problems that cost days to find, what caused them, and the rule that keeps
them fixed. Read the section for the area you are about to change. Source of
truth is the code; the paths below point at where each rule lives.

Device names matter here: most of these only showed up on one SoC. The Google
TV Streamer (MediaTek, Dolby MS12 audio) runs a **32-bit-only** Android 14
build (`armeabi-v7a`), so armv7 is a production configuration, not a legacy
one.

## A/V sync and the clock

**One clock, one timeline.** Video is paced by `MediaClock`
(`player/src/player/clock.rs`), which reads the audio sink:
`seek_offset + played_since_flush − output_latency`. Video frames are placed on
the absolute media timeline (`pts − origin`), not anchored to whichever frame
arrived first.

- **Constant A/V offset after every seek or track switch (worst on Android).**
  The clock counted the old audio still sitting in the AudioTrack buffer
  (100–300 ms) as new audio; nothing calls `AudioTrack.flush()` on a rebuild.
  Rule: the sink marks a **flush boundary** at the first chunk of a new
  generation, and `AudioSink::played_since_flush_ms()` measures from there. PCM
  goes to the sink as `AudioChunk { gen }`; consumers drop older generations
  (`FlushState`, `player/src/av_sync.rs`). Do not reintroduce a "snapshot of
  `played_ms` at spawn" base.
- **Audio permanently late by the content's origin.** The PCM trim compared
  absolute composition PTS with a 0-based seek target, so a stream whose first
  segment starts at e.g. 83 ms got 83 ms of padding on every start. Rule: trim
  and pad on the absolute axis (`AudioAligner` in `av_sync.rs`); gaps are filled
  with silence, overlaps cut, so a dropped corrupt AU no longer shifts all later
  audio by 32 ms.
- **The drift gauge does not see offsets.** `av_drift_ms` measures rate
  divergence between two clocks, not a constant offset. Lip-sync is checked
  independently of the engine's clock by `player/examples/conformance.rs`
  (beep/flash detection, `lipsync`, `lipsync-median`, `late-frames`). Any change
  to the clock path must pass it.
- **Position after a seek read as cumulative audio time.** Reported position
  must be on the media timeline; a seekbar driven by raw `played_ms` breaks
  relative seeks and ABR restart-segment choice.
- **The ~1 fps "convoy".** A backward clock step (wall-clock anchor replaced by
  the audio clock once audio starts late) made the vsync gate stamp frames
  seconds into the future. SurfaceFlinger holds far-future buffers, the direct
  MediaCodec output pool (8 buffers) drains, and decode, demux and audio all
  back up. Rule: a present stamp never exceeds `now + 250 ms`
  (`MAX_PRESENT_LEAD_NS`, `player/src/player/sync.rs`). It makes video robust
  against any clock discontinuity; keep it.
  Measured dead ends: an unbounded re-evaluating pacing gate deadlocks after a
  seek; a bounded one starves audio through the demux interlock; a longer
  audio-start gate in `av_sync_handler` is inert for PCM (the loop that moves
  the counter is spawned after the anchor). The start gate now applies to
  passthrough only.
- **Micro-stutter with perfect release stamps.** The audio clock drifts ~12 ppm
  against the display, so release phase crawls across the vsync and sits on
  SurfaceFlinger's decision boundary for minutes at a time (dropped plus
  repeated frames the player cannot see). Rule: in direct mode the release
  stamp snaps to the hardware vsync grid, half a period before the target vsync,
  and the release lead comes from the display (`presentationDeadline` + 2 ms +
  one vsync, at least 50 ms). See `player/src/present_lead.rs`; the host reports
  display timing (`RustPlayer.setDisplayTiming`, sampled by `RustPlayer.kt`).
  Release lead is also capped down when the codec starves: every frame waiting
  in SurfaceFlinger costs a decoder output buffer (Amlogic starved at 100 ms).
- **`video_ready` must mean "first frame at or after the target".** Frames from
  the keyframe up to a mid-segment seek target are discarded before
  `video_ready` fires (`player/src/player/decode.rs`); otherwise audio starts
  before the first visible frame on slow 4K decoders.

## Decoder reuse and ABR switches

- **A switch that tears down the decoder leaves a 300–400 ms hole.** Where two
  hardware decoders cannot run side by side, the supervisor parks OLD's decoder
  (`DecoderHandoff`, `DecoderGuard` in `player/src/player/decode.rs`) and offers
  NEW's `VideoDecoderParams` to it via `HwVideoDecoder::try_reconfigure`. Only a
  refusal builds a new decoder.
- **Cut OLD per segment, not per sample.** In decode order the last P-frame
  carries a PTS past the boundary and the B-frames before the boundary come
  after it; a per-sample cutoff lost 3 frames. OLD stops at the segment that
  starts at the boundary (`DecoderHandoff::set_cutoff`, judged by the segment's
  first IDR).
- **Android reuse needs the codec configured for the ladder.** With
  `adaptive-playback`, configure `max-width`/`max-height` to the largest rung
  (`VideoDecoderParams::max_width/max_height`). Reuse also requires the same
  mime, DV profile, bit depth, transfer, gamut and video window; new
  VPS/SPS/PPS go in-band before the next IDR (`decoders/mediacodec.rs`).
- **Apple: `VTDecompressionSessionCanAcceptFormatDescription` refuses every
  resolution change.** Reuse covers same-size bitrate rungs only; the normal
  path is a new session, which is fast (26–85 ms on a Mac, 180–280 ms on an
  iPhone SE at 2160p). Two VT sessions side by side flickered, which is why
  Apple is not on warm handoff (`decoders/videotoolbox.rs`).
- **Web keeps warm handoff.** NEW decodes next to OLD; the cold path left a
  470 ms render gap per switch.
- **Only one direct codec may own the Surface.** A codec configured while the
  previous one has not finished `AMediaCodec_delete` emits no output
  (`produced=0`) and stalls forever. `DIRECT_WINDOW_BUSY` in
  `decoders/mediacodec.rs` serialises them; the `produced=0` watchdog (~2 s) and
  the backpressure catch-all (~3 s) bail out so the supervisor rebuilds instead
  of spinning. `[mc-direct] dequeue_input stall … produced=N` in the log tells
  the two cases apart.
- **D3D11VA's frame pool is a fixed texture array** (20 surfaces for HEVC). The
  pipeline holds the frame channel, reorder buffer, renderer and, during a warm
  switch, gate and pump frames: the pool ran dry (`Static surface pool size
  exceeded` → `ENOMEM`). Rule: `EXTRA_HW_FRAMES` (`decoders/ffmpeg_hw.rs`); the
  array limit is 64. VA-API pools are dynamic and ignore it.
- **Dual-GPU Windows laptops.** The decoder must open D3D11VA on the adapter
  the renderer uses (LUID via `set_render_adapter_luid`); `OpenSharedHandle`
  cannot cross GPUs. Both the windowed and the offscreen renderer must record
  the LUID.
- **A device that cannot decode the top rung.** ABR climbed to 2160p on an
  iPhone SE that presents 13 fps there. `DecodeOverloadDetector`
  (`player/src/abr.rs`) caps the ladder after sustained drops.

## Audio: Android PCM output (AudioTrack)

- **AAudio streams are stolen on some TV HALs.** On the Google TV Streamer every
  cpal/AAudio output stream fails `requestStart` (`-899`, "stream was probably
  stolen"); a Chromecast with Google TV is fine. Retrying and fixing the format
  do not help. Rule: Android PCM goes through an AudioTrack sink
  (`renderers/audio/audio_track_pcm.rs`), never cpal. A sink that cannot start
  must report `played_ms() == None` so the clock falls back to the wall clock;
  a frozen `Some(0)` turns video into a slideshow.
- **Never hand `AudioTrack.write` a half frame.** It accepts whole frames only;
  a trailing odd sample returns 0 forever and the writer spins on it. That one
  bug produced the "1 fps, audio never starts" family: the track started with
  1–3 frames, AudioFlinger removed it from the mix, the head froze, video paced
  against the frozen clock, and the full channel stalled decode. Rule: audio
  goes to the sink as whole decoded frames, and both the writer and
  `write_floats` truncate any odd batch defensively. Dropping one sample instead
  of carrying it would swap L/R for the rest of the stream.
  Theories tested and ruled out (do not re-walk): TV power/HDMI sink down,
  PCM_FLOAT vs 16-bit, FAST/low-latency path, track born too soon after the
  previous one, post-reboot settling. The decisive control was another player
  playing fine on the same box at the same moment.
- **The client play state lies.** After AudioFlinger drops a starved track
  (`BUFFER TIMEOUT`, ~0.5 s of underruns) `getPlayState()` still says PLAYING,
  and a bare `play()` on a PLAYING client is a server-side no-op. Hardening that
  stays, all in `audio_track_pcm.rs`:
  - *Prime before play*: `play()` only after ~340 ms is written (the deep-buffer
    mixer pulls ~256 ms chunks), the buffer stays full, or 1.5 s passed.
  - *Rate watchdog* (`check_stall`): primed and unpaused, the head must advance
    at ≥10 % of realtime. Zero-write counters miss the "crawl" face.
  - *Heal ladder*: pause()/play() (PAUSED→PLAYING re-adds the track), then
    release, wait ~3.5 s for output standby, recreate. Immediate recreation
    lands on the same wedged HAL stream.
  - *Surrender*: if a fresh track also stalls, go silent with video at realtime
    on the wall clock, discard PCM paced to realtime (`discard_paced`), retry
    every 30 s.
- **Never block in JNI on a paused track.** A blocking `AudioTrack.write` on a
  paused track could not service the flush of an ABR rebuild that fired during a
  long pause, and playback never resumed. `write_floats` uses non-blocking
  writes in a loop that aborts on flush and exits on teardown.
- **Host pause vs internal park.** `Player::pause/resume` pause the AudioTrack
  directly and stop consumption so queued samples survive; internal transport
  parks keep consumption running (`host_paused` in `renderers/audio.rs`).

## Audio passthrough (E-AC-3 / AC-3 over HDMI)

- **A direct compressed AudioTrack does not start until its start threshold is
  buffered** (~1–2.5 s of media on the Streamer + AVR). Until then
  `getTimestamp` fails and `played_ms` is 0. Feeding only 200 ms ahead of a
  head that never moves deadlocks: no audio, video clock frozen, decode at
  1–2 fps. Rule: PRIME writes ungated until the head moves; the track buffer is
  256 KiB (~2.7 s). See `audio_passthrough_task` in
  `player/src/player/pipeline.rs`.
- **Do not pace a direct track's feed to its playback head.** A direct track
  paused for more than a few seconds does not resume until its buffer is back
  above the start threshold; a head-paced feed then writes nothing and output
  stays dead. Keep the track buffer full and let the blocking `write`
  back-pressure, as ExoPlayer's `DefaultAudioSink` does. The clock reads the
  presented position, so the depth costs nothing for lip-sync.
- **PRIME must be bounded, and pause-aware.** A feed whose head never starts
  (a sink that never becomes the active output) primed forever and ran
  minutes ahead. Rule: abandon after 30 s ahead (`PRIME_MAX_AHEAD_MS`; 10 s
  falsely abandoned slow-waking AVRs), but while paused buffer ~3 s and wait
  (`PRIME_PAUSED_TARGET_MS`): a paused track legitimately reports head 0.
- **Lazy `play()`.** Call `play()` on the first written AU, not in the
  constructor; poking a direct track that has not started recycles it ("dead
  IAudioTrack"). `played_ms` returns `None` until started.
- **Gate the first AU on `pipeline_live`.** Otherwise audio runs 1–2 s ahead of
  a video pipeline that is still starting and never catches up.
- **Do not trust a stale timestamp.** An `AudioTimestamp` stops moving while
  paused or while an AVR relocks; extrapolating from it ran the clock ahead and
  poisoned the learned latency. Interpolate only from a fresh timestamp
  (`AudioTrackSink::played_ms`, `renderers/audio/audio_passthrough.rs`).
- **Only one compressed track may own the HDMI output.** The play loop calls
  `set_passthrough(None)` after the previous generation joined and before it
  builds a new sink; a live PCM stream next to the bitstream also broke it
  (`EPIPE`), so cpal is paused while passthrough is engaged. A `stopped` guard
  makes every sink call a no-op after release.

## Teardown and lifecycle

- **Dropping a tokio `JoinHandle` detaches the task; it does not cancel it.**
  Aborting a wrapper that awaited `player.play()` left the play loop, its
  downloads and the audio running after the host had left the screen (audio
  playing in the background). Rule: call `Player::stop().await`, then await the
  handle `play()` returned (`player/src/player/mod.rs`; the bridge does this in
  `orchestrate`, `bridge/src/bridge.rs`).
- **`stop()` must clear the seek target first.** The play loop rebuilds when a
  seek target is set at its tail; `stop()` clears it under the same lock seeks
  write under and bumps `stop_epoch`, so a seek issued just before a stop gives
  up instead of resurrecting the pipeline.
- **A rebuild must join the previous generation before spawning the next.** The
  orphaned-downloader wedge: OLD's `download_task` kept sending into a dropped
  channel and treated the `SendError` as a network error, retrying for 30 s
  while NEW starved. Rules: `download_task` exits as soon as
  `segment_sender.is_closed()` (`player/src/player/net_io.rs`); the play loop
  `join!`s video and audio before restarting; an ABR soft swap signals OLD's
  stop and awaits its decode task before NEW starts
  (`player/src/player/supervisor.rs`). A closed channel means "consumer gone",
  never "retry".
- **Native code must own a reference to the window it uses.**
  `ANativeWindow_setFrameRateWithChangeStrategy` (AFR re-asserted on every
  rebuild) ran on a window the host had already released: SIGABRT on a
  destroyed mutex. Rule: `DirectWindow` (`player/src/player/pipeline.rs`) holds
  an `ANativeWindow_acquire` ref until it is replaced or the last `Player` is
  dropped; `set_video_output_window(null)` releases it. The host must still
  detach before the Surface goes (see `KNOWN_ISSUES.md`).
- **Host network hooks block.** The Android provider's `onRequest`/`resolveKey`
  do synchronous network round-trips; they run on the blocking pool
  (`spawn_blocking` in `platform/android/src/lib.rs`), never on runtime workers.
- **MediaCodec dequeue loops still block inline in async tasks.** The Android
  runtime keeps a worker floor of 6 (`rustplayer-rt` threads) for headroom.
  Coarse per-sample `block_in_place` around codec calls was measurably worse
  (0/10 healthy starts from worker-migration churn); don't go that way.

## Seek, resume and start position

- **`seek()` is fire-and-forget.** It spawns the write of `seek_target`, so a
  `seek()` right before `play()` loses the race and playback starts at 0. Use
  `Player::set_start_position` (synchronous, one-shot, consumed by `play()`);
  the bridge exposes it as `StartConfig.start_position` / `start_fraction`.
- **A track switch before the first frame killed resume.** Hosts apply a saved
  language right after `prepare()`; `change_audio_track` then did
  `seek(position())` with `position() == 0`, and that `Some(0)` shadowed the
  parked resume. Rule: track switches only seek once `pipeline_live` is set;
  before that the play loop reads the new representation at start. Better
  still, pass the language at start (`preferredAudioLang` /
  `preferred_audio_language`) so no rebuild happens at all.
- **The first ABR switch is held until the first frame**, so it cannot collide
  with a resume start (it used to stall direct-mode MediaCodec).
- **Resume as a fraction is resolved inside the bridge.** Hosts often store
  progress as a percentage and only know the runtime in whole minutes; the exact
  duration is known only after `prepare()`. `StartConfig.start_fraction` is
  resolved against the real duration in `orchestrate`; an absolute position wins
  if both are set.
- **Default video pick must work on any manifest.** A fixed representation
  index from a test fixture meant "no video track" on a 3-rung product ladder
  and nothing played. The default is the highest rung at or below 720p
  (`apply_default_tracks`, `bridge/src/lib.rs`); ABR is armed by the bridge
  and climbs from there.

## Subtitles

- **TV overscan crops the bottom ~5–8 %.** A cue anchored 7 % above the bottom
  disappeared on a 2.39:1 film; 11 % ran into the picture. The default bottom
  padding is now 8 %, and the host passes the real bottom inset
  (`Player::set_subtitle_safe_insets` / `RustPlayer.setSubtitleSafeInsetBottom`),
  computed from `WindowInsets` (system bars, display cutout) or a title-safe
  percentage.
- **The player draws on the whole overlay surface and does not know where the
  host put the video plane.** `SubtitleAnchor::Screen` (default) places cues
  relative to the full surface; `SubtitleAnchor::Picture` assumes a centred
  aspect-fit picture. Either way the host must centre the video plane.
- **The overlay z-order is host layout.** With `setZOrderMediaOverlay(true)` an
  opaque host window background can cover the letterbox bar, so cues placed
  there vanish; one integration needed `setZOrderOnTop(true)`.
- **Style crosses the boundary as ARGB.** `setSubtitleStyle(textArgb,
  outlineArgb, sizeScale)` on Android; `SubtitleStyle` is re-exported from the
  crate root and `SubtitleStyle::parse_color` takes names or hex.

## Request filter and the platform boundaries

- **Every field of `player::net::PreparedRequest` must cross every FFI
  boundary.** The iOS completion once took a bare URL and defaulted headers,
  method and body away, so any consumer authenticating with a header could not
  use the package at all. iOS now passes a `RustPlayerPreparedRequest` (URL,
  flat `[k0, v0, …, NULL]` headers, optional method and body) to
  `rustplayer_intercept_complete` (`platform/ios/src/lib.rs`,
  `platform/ios/packaging/include/rustplayer_ffi.h`).
- **No app concepts in the library.** The provider API is the ExoPlayer/Shaka
  shape: `onRequest(url, type) -> PreparedRequest` (rewrite URL, add headers)
  and `resolveKey(kid)`. Auth schemes, token names, licence endpoints and URL
  conventions live in the host's provider. The iOS C ABI uses only the generic
  `rustplayer_` / `RustPlayer` / `RUSTPLAYER_` prefixes; renaming it was a
  breaking ABI change, so treat the prefix as frozen.
- **Track JSON is flat.** `tracks_to_json` emits one entry per representation
  with `adapt`/`repr` indices for the `select*` calls; text entries carry
  `forced`. Video size comes from the dedicated `video_size` event, not from
  parsing `stats`.

## Distribution and packaging

- **Consumers never compile Rust.** Android ships as an AAR
  (`io.github.preclikos:rustplayer`, Kotlin package
  `io.github.preclikos.rustplayer`) with the `.so` per ABI and
  `libc++_shared` inside; iOS ships as `RustPlayerFFI.xcframework` behind the
  SwiftPM package in `platform/ios/packaging`. An xcframework slice holds one
  library, so `build_xcframework.sh` merges the bridge and FFmpeg static libs
  per slice (`libtool -static`) and lipos the simulator arches.
- **`Package.swift` pins `url:` and `checksum:` together.** Both must change on
  every iOS release; a stale checksum breaks every SwiftPM consumer. The pin
  was forgotten for several releases until `scripts/release.sh
  --wait-and-bump-ios` started waiting for the xcframework and committing the
  pin itself. Never edit one without the other.
- **Release versions come from the remote tags, never from memory.** Two
  parallel sessions once tagged consecutive versions on different commits. One
  version line covers Android, iOS and web, and every publish workflow is gated
  on a green conformance run (`docs/RELEASING.md`).
- **The Android release `.so` is published stripped**; unstripped copies with
  the same build-id go only to Crashlytics. This repository and its packages
  are public.

## Web

- **`Instant::now() - Duration` panics right after a page load**:
  `performance.now()` starts at zero. Use `rt::instant_ago`
  (`player/src/rt.rs`).
- Measure web switches in a visible tab, started from a real click (Web Audio
  needs a user gesture); hidden tabs are throttled.

## Debugging habits that paid off

- An Android "crash" is often a wedge: check `logcat -b crash` and tombstones
  before assuming a native crash.
- Filter logcat by PID, not by tag; the tag differs between the test app
  (`io.github.preclikos.rustplayer.demo`) and host apps.
- `[vsync] HEALTH decoded=N/s drift=…` alternating between two value sets means
  two pipelines are alive.
- Verify an audio fix with repeated cold starts (10 in a row) and the scenario
  matrix: seek forward/back, short pause, Home and return, Back and replay,
  5-minute pause with an ABR switch during it. Single runs lie: on the
  half-frame bug a given build was consistently healthy or consistently broken,
  and every rebuild reshuffled it.
