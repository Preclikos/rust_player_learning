# Non-passthrough PCM audio "stolen" on Google TV Streamer → video slideshow

**Status:** root-caused, NOT yet fixed. Needs a real fix (see Recommendation).
**Date:** 2026-06-29. Investigated against rustplayer 0.1.1 (and 0.1.0).

## Symptom

On the **Google TV Streamer** (`kirkwood`, MediaTek, armeabi-v7a), BlackZone
playback in **non-passthrough** mode (PCM/AAC, `passthrough=false`) plays video as
a **slideshow**: `[vsync] HEALTH decoded=1-3/s drift=…ms` climbing unbounded, no
audio. With **passthrough** (E-AC3 via AudioTrack) engaged, playback is fine.

## Root cause

The cpal (AAudio) PCM output stream is **stolen at start** by the device's audio
HAL. Sequence (logcat):

```
AAudioStreamBuilder_openStream() returns AAUDIO_OK            # open succeeds (stereo F32 48 kHz)
AAudioStream_requestStart(s#N) called
AAudioService: startClient(), invalid streamHandle = 0x1
AudioStreamInternal_Client: requestStart_l() error = -892, stream was probably stolen
AAudioStream_requestStart(s#N) returned -899                 # AAUDIO_ERROR_DISCONNECTED
```

`player/src/renderers/audio.rs` then panics on `stream.play().expect(...)` (~L317),
killing the audio output thread. Because the cpal sink's `played_ms()` keeps
returning `Some(0)` (it derives from `samples_consumed`, which never advances,
while `sample_rate != 0`), `MediaClock` never falls back to the wall clock — it
stays pinned at 0, so the video sync loop paces every frame against a frozen
clock and decode collapses to ~1 fps with unbounded drift.

`audio_flinger` shows the primary output is a **fast-track / MMAP** path
(`AudioOut_D`, `AUDIO_OUTPUT_FLAG_PRIMARY`, fast tracks). The MediaTek HAL appears
to preempt the app's shared AAudio output stream.

## Device matrix (same APK, same armeabi-v7a .so)

| Device | Result |
|---|---|
| Google TV Streamer (`kirkwood`, MediaTek) | **FAILS** — every AAudio output stream stolen at `requestStart` |
| Chromecast w/ Google TV (`sabrina`, Android 14) | **WORKS** — `requestStart` returns 0 (AAUDIO_OK) |

So it is **device/HAL-specific**, not a config or code regression.

## Ruled out

- **Not a 0.1.1 regression** — 0.1.0 fails identically.
- **Not config/format** — `open()` succeeds with stereo F32 48 kHz; hard-coding
  that config (skipping `default_output_config()`/`supported_output_configs()`
  probe streams) did **not** help.
- **Not transient** — a reopen-on-disconnect retry (6× rebuild+`play()` over
  ~1.5 s) had **every** attempt stolen instantly (`s#1..s#6` all -899).

## Recommendation (the real fix)

Passthrough works on this HAL because it outputs via **AudioTrack**, not AAudio.
So: on Android, route **non-passthrough PCM** through an **AudioTrack-based sink**
(mirror the existing `AudioPassthrough` AudioTrack path, but for PCM frames)
instead of cpal/AAudio. AudioTrack is not stolen on this HAL.

Alternatives:
- Force cpal/AAudio onto a **non-fast / deep-buffer** (or legacy OpenSL ES) path —
  but cpal 0.18 doesn't expose AAudio performance/sharing mode, so this needs a
  cpal fork/patch.

## Defensive (NOT a fix — product owner wants audio, not silent video)

Independently worth doing: when the output stream never starts, make
`played_ms()` return `None` (track a `stream_started` flag) so `MediaClock` uses
the wall clock and **video at least plays** instead of starving. And replace the
`stream.play().expect(...)` panic with graceful handling. This keeps a missing /
stolen audio device from collapsing video to a slideshow — but on the Streamer it
would play silently, which is not acceptable as the end state.

## Repro

`adb` deep link into playback (non-passthrough): `am start -a android.intent.action.VIEW
-d 'blackzone://detail?id=<movieId>&type=movie'`, then press play; watch
`logcat | grep -E "requestStart|stolen|HEALTH|audio.rs"`.
