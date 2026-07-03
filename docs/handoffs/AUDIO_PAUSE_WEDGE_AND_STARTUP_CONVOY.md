# Startup convoy (root-caused: blocking JNI intercepts) + long-pause wedge

**Status:** root-caused with device-verified fixes (this working tree / 0.1.4).
**Date:** 2026-07-03. Device: Google TV Streamer (`kirkwood`, armv7), BlackZone
TV app, movie resume-at-58% via deep link, repeated fresh starts.

## User-reported symptoms

1. **TV:** after a (long, ≥minutes) pause, playback never resumes.
2. **Mobile:** audio out of sync with video.
3. (Found while testing) **Startup randomly freezes at ~1 fps** — reproduced on
   the SHIPPED 0.1.3 in a 5× fresh-start loop: 1× healthy, 1× frozen at 1 fps,
   1× failed to start, 2× degraded (9-12 fps). This pre-existing race turned out
   to underlie most of the flakiness attributed to other causes.

## ROOT CAUSE — the startup convoy

`platform/android/src/lib.rs`, `AndroidHost::intercept` / `resolve_key` ran the
JNI upcall to the Kotlin provider **inline on the tokio runtime worker**. The
host's `onRequest` does a synchronous network round-trip (`runBlocking {
linkRemoteService.getLink(..) }`, plus a possible Keycloak token refresh), and
`resolveKey` a synchronous HTTP POST. At playback start, 4-6 segment requests
intercept **concurrently** (8 s buffer fill, per-playback cold link cache) — so
every runtime worker blocks at once. That silences the whole runtime, *timer
driver included*: the vsync pacing loop, both starvation watchdogs and the
audio feed simply stop being polled. The pipeline then convoys — video at
~1 fps (forced through by occasional drains), audio delivering a single partial
frame (`put_samples` observed stopping at exactly 128 sends = one tokio coop
budget, never re-polled), decoders wedged on backpressure. Whether a given
start freezes depends on how the CDN/API response times overlap → the observed
~40-60% race, on shipped and on every experimental sink alike.

**Fix:** run both upcalls via `tokio::task::spawn_blocking` (the JavaVM is
re-derived from `ndk_context` inside the closure; the provider `GlobalRef` is
cloned in). Additionally the runtime gets a worker floor of 6
(`worker_threads(max(cores, 6))`) as headroom against the remaining in-poll
blocking waits in the MediaCodec dequeue loops (`std::thread::sleep` in
`mediacodec.rs` / `mediacodec_audio.rs` — the proper long-term fix is moving
those onto the blocking pool too).

## Long-pause resume wedge

During a multi-minute pause the ABR tick still fires (EWMA decays → downswitch),
and the rebuild's flush was never serviced because the `audio_track_pcm` writer
thread sat in a **blocking JNI `AudioTrack.write`** on the paused track. After
resume the audio clock stayed frozen and video starved against it
(`A/V drift 9196ms… +4.7 s per 5 s`, 1 fps).

**Fix (audio_track_pcm.rs):** `write_floats` now paces with NON-blocking writes
in a retry loop that aborts on the renderer's flush flag and exits on teardown —
same device-rate back-pressure, no unbounded JNI block. Verified: 5-minute
pause → instant clean resume at 24 fps.

**Host-pause semantics (audio.rs + sink):** `Player::pause/resume` land on the
inherent `AudioRenderer::set_paused` (concrete-type calls shadow the trait);
internal transport parks (av_sync anchor gate, starvation gates, seek re-parks)
arrive via the `AudioSink` trait from generic code. The inherent path now also
(a) pauses/plays the AudioTrack directly (a host pause goes silent immediately;
a host resume wakes even a track parked by an internal gate that nothing else
would unpark), and (b) sets `host_paused`, which stops the writer's consumption
so queued samples survive the pause (cpal's silence-without-drain). Internal
parks deliberately do NOT stop consumption — holding the pre-anchor backlog
back is what previously backed the pipeline up (channel → audio_rx → demux
inter-ES skew) into the convoy even without the JNI blocking.

**Self-heal (sink):** after each accepted write, if the sink isn't supposed to
be parked but `getPlayState != PLAYING`, call `play()` — heals any lost
lazy-play/unpark race within ~85 ms (observed firing exactly in racy starts).

## Also learned (kept for posterity)

- `played_ms` must report `Some(0)` from the start (not `None`-until-flowing):
  av_sync's anchor gate WAITS for the clock to move off 0; skipping the gate
  anchors early and the later clock switch steps backward → vsync stall.
- A `Some(0)` that never advances (track played with a sliver of data, head
  stuck at 0) pins MediaClock to a frozen position — the counter-based clock
  (accepted frames) is immune to the underrun-frozen-head variant.
- `audio_sync_loop` pushes ONE sample per `send().await` (~96 000 awaits/s) —
  worth batching someday, though with the blocking-pool fix it no longer
  matters for liveness.

## Repro / verification loop

```
for run in 1..N:
  adb shell am force-stop site.blackzone.tvplayer.rust; adb logcat -c
  adb shell am start -a android.intent.action.VIEW -d 'blackzone://detail?id=<id>&type=movie'
  <press play>; sleep 30
  adb logcat -d | grep HEALTH | tail -1     # want decoded=24+/s drift≈0
Long pause: play → pause → 300 s → resume → HEALTH/f# must advance at 24 fps.
```
