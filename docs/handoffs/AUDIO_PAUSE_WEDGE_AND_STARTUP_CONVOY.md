# The 1 fps "convoy" (root causes: AudioFlinger underrun-drop + present-stamp wedge) + long-pause wedge

**Status:** root-caused and FIXED, device-verified (Google TV Streamer,
`kirkwood`, armv7). Fixed across 0.1.4 (pause wedge) and 0.1.5 (convoy).
**Date:** 2026-07-03 … 2026-07-06.

## Symptoms

1. Playback randomly starts (or degrades to) ~1 fps "slideshow", audio silent
   or missing — on SHIPPED 0.1.3 a cold fresh-start loop measured ~half the
   starts degraded/frozen. Reported by users as "it never runs realtime".
2. Pause ≥ minutes → resume never plays (TV).

## Root cause 1 — the startup convoy (FIXED in 0.1.5)

Mechanism, confirmed by the `CCodecBufferChannel: no present fence for frame N`
+ `BufferPoolAccessor2.0: 8 total buffers - 8 used` smoking gun in an
unfiltered logcat of a wedged run:

1. `av_sync_handler` anchors the pipeline clock after `video_ready`, waiting at
   most **500 ms** for the audio clock (`played_ms`) to start ticking. On a
   slow-startup TV SoC the first audio reaches the device 1-2 s later (decode +
   first sink writes happen only after the sync loops spawn) — so the gate
   times out and the anchor lands on the WALL clock.
2. When audio then starts, `MediaClock` switches wall → audio and **steps
   BACKWARD** by the audio-startup gap.
3. The vsync pacing gate was single-shot: it had already slept "until due" on
   the pre-jump clock, fell through, and released frames stamped
   `now + pts_to_go` where `pts_to_go` was recomputed on the post-jump clock —
   i.e. **seconds in the future**.
4. `eglPresentationTimeANDROID`/direct-release timestamps that far ahead make
   SurfaceFlinger HOLD the buffers; the direct MediaCodec output pool is only
   8 buffers, so the codec wedges (`dequeue_input stall`, `produced=N` — the
   #23 family), the demux/decode chain backs up, audio starves, the clock
   crawls, and every subsequent frame recomputes even further in the future —
   a self-sustaining ~1 fps convoy. Whether a given start trips it depends on
   how far decode raced ahead during the wall-clock window: the observed
   randomness.

**Fix (player.rs): `MAX_PRESENT_LEAD_NS` (250 ms) present-stamp cap.** The
frame's present stamp never exceeds `now + 250 ms` (≈6 frames — inside the
8-buffer direct pool), so SurfaceFlinger can never be handed enough far-future
frames to drain the codec pool, no matter what the clock does. Pacing stays
the shipped single-shot sleep. Measured: most starts clean at a continuous
24 fps, and seek-forward / pause / HOME-return / BACK-replay / 5-min-pause
scenarios pass; a residual ~1-in-5 start still comes up degraded (recoverable
by HOME+return, which restarts the pipeline) — see OPEN below.

Dead ends, so nobody retries them (each measured on-device):
- UNbounded re-evaluating pacing gate (hold the frame until the re-read clock
  says due): 10/10 clean starts but DEADLOCKS the pipeline after a seek.
- BOUNDED (3 s) re-evaluating gate + cap: the hold itself starves audio
  through the demux interlock (holding video frames keeps codec buffers
  captive → video decode input stalls → demux stalls → audio starves) — 3/8
  starts.
- Extending the av_sync audio-start anchor gate (500 ms → 3 s / wait-for-
  movement) is inert for PCM: the audio_sync loop that would move the counter
  is spawned only AFTER the anchor, so the gate can only ever time out — it
  just adds startup latency. Left at the shipped 500 ms.
- Coarse per-sample `block_in_place` around MediaCodec calls: measurably WORSE
  (0/10 healthy starts; constant worker-migration churn).

Verified with the stamp cap: repeated cold fresh-starts clean (continuous
24 fps, zero HEALTH anomaly logs; wedged runs show `decoded=1/s` + runaway
drift), seek FORWARD, short pause/resume, HOME+relaunch, BACK+replay, 5-min
pause (ABR fires mid-pause) → resume. Transient `no present fence` warnings
for the first few frames remain (SF settling) — those fences DO fire; in the
wedged state they never did.

## Root cause 1b — the DEEPER trigger: AudioFlinger drops the underrunning track (FIXED in 0.1.5)

The residual degraded starts (and the seek-backward wedge) were pinned with
permanent per-stage counters (`StatsState.diag_*`, logged by the `bz-watchdog`
thread every 3 s). A frozen run showed: audio DECODE healthy (350 frames), but
`put_samples` #94 parked forever — the sink channel full, the writer spinning
on `written=0` with the device buffer full, and the playback HEAD static. The
smoking gun in `dumpsys media.audio_flinger`:

```
00:19:49.440 AT::add    (track)  … new     ← our play()
00:19:49.941 AT::remove (track)  …         ← dropped from the mix 500 ms later
```

Mechanism: the lazy `play()` fired at the FIRST accepted write — with 122
floats ≈ 2.5 ms of audio in the track. The track underruns instantly; after
~0.5 s of underruns (`kMaxTrackRetries`) AudioFlinger REMOVES the track from
the active mix and never re-adds it on its own. Crucially the CLIENT-side
`getPlayState()` still says PLAYING — so a playstate-based self-heal can't see
it. The head freezes, writes only trickle, the audio clock freezes, video
paces against the frozen clock, back-pressure stalls demux/decode: the convoy.
Whether a given start trips it depends on how much PCM the decoder queued
before the first write — the observed randomness. The same mechanism explains
the seek-backward wedge: after a flush the track plays out its device buffer
and underruns while gen-2 re-fetches segments → AF drops it → the gen-2 clock
is dead and both decoders trickle.

**Device fact that frames everything: the Google TV Streamer runs a 32-bit
Android 14 build** (`ro.product.cpu.abilist = armeabi-v7a,armeabi`) — there is
no arm64 on this box; armv7 is the production configuration.

**The wedge has two faces**, both = AudioFlinger effectively stops serving the
track while the CLIENT play state stays PLAYING (getPlayState is useless):

- *Crawl*: the server consumes at ~2-4 % of realtime with a decaying rate;
  tiny writes keep being accepted, so any consecutive-zero-writes detector
  never fires. Head advances a trickle, clock crawls, video paces against it —
  the ~1 fps convoy.
- *Hard*: the track accepts almost nothing from creation (`write` returns 0
  with an empty 24624-frame buffer, head pinned at 0, `getUnderrunCount()` 0).
  Comes in box-wide EPISODES (whole hours), during which every new track —
  same process or fresh, before or after a reboot — is dead on arrival.
  **Root cause of the episodes: the TV is OFF.** With no active HDMI sink the
  Streamer's MediaTek/Dolby-MS12 output pipeline (`ms12_input_deep_buffer` in
  the AF mix graph) stops consuming entirely instead of playing into the void
  like most devices. Confirmed: episodes correlate exactly with the TV's
  power state (CEC `mDeviceInfos` empty + One Touch Play timeout during an
  episode); routing stays correct (HDMI_Sink port) and other apps' tracks
  starve identically. Unfixable app-side — and it only ever affects playback
  nobody is watching. The surrender layer keeps the player consistent and
  restores audio within ~30 s of the TV coming back.

**Fix (audio_track_pcm.rs): layered self-defense, all driven from the writer**
1. *Prime before play*: `play()` only once ≥ ~100 ms is written, the device
   buffer reports full, or 500 ms passed since the first accepted write —
   starting with a near-empty buffer is what triggers the AF drop (`AT::add` →
   `AT::remove` ~0.5 s later, `kMaxTrackRetries` underruns). All other play
   paths (`set_paused` unpark) are gated on primed.
2. *Rate watchdog* (`check_stall`, every writer iteration, judged per ~1 s):
   primed & unpaused ⇒ the head must advance ≥10 % of realtime. Catches both
   faces. Windows >3 s are re-baselined, not judged (writer idle ≠ stall).
3. *Heal ladder*: strikes 1-2 → pause()/play() cycle (the PAUSED→PLAYING
   transition re-adds the track to the mix — a bare `play()` on a PLAYING
   client is a server-side no-op, confirmed via the `AT::` event log); strike
   3 → release the track, wait 3.5 s (the output thread's standby delay —
   `outStream->standby()` is the only client-reachable HAL reset), build a
   fresh track, re-prime. Clock continuity: the old track's final head rolls
   into `head_base`, discarded PCM into `dropped_frames`;
   `played_ms = head_base + head + dropped`.
4. *Surrender*: if even the fresh track stalls (hard episode), go silent but
   keep playing — `played_ms` turns `None`, `MediaClock` falls back to the
   wall clock, video runs realtime (instead of the slideshow), and a fresh
   track is retried every 30 s so audio comes back by itself when the box
   recovers. Discarding is PACED to realtime (`discard_paced`) and engages
   mid-batch: an instant discard lets decode race ahead of the wall clock,
   inflating `dropped_frames`, and the revival point then leaps forward with
   video sprinting to catch up. Verified live during a hard episode: silent
   realtime 24 fps video, periodic revival attempts, clean audio return once
   a revived track accepted writes.

Measured dead ends (do not retry):
- Bare `play()` re-issue on stall: server no-op, 59 attempts changed nothing.
- `flush()` + play on the wedged track: 56 attempts, still dead.
- IMMEDIATE track recreate (no standby wait): 10-15 fresh tracks in a row all
  dead on arrival — the new track lands on the same wedged HAL stream.
- getPlayState-based self-heal: client state lies (always PLAYING).
- Consecutive-zero-writes stall detection: defeated by the crawl face.

The `MAX_PRESENT_LEAD_NS` stamp cap (1a) stays: it makes video robust against
ANY clock discontinuity independent of the audio path.

## Root cause 2 — long-pause resume wedge (FIXED in 0.1.4)

During a multi-minute pause the ABR tick still fires (EWMA decays →
downswitch) and the rebuild's flush was never serviced: the `audio_track_pcm`
writer sat in a **blocking JNI `AudioTrack.write`** on the paused track.
After resume the audio clock stayed frozen and video starved against it.

**Fix (audio_track_pcm.rs):** `write_floats` paces with NON-blocking writes in
a retry loop that aborts on the renderer's flush flag and exits on teardown —
same device-rate back-pressure, no unbounded JNI block. Verified: 5-minute
pause → instant clean 24 fps resume.

**Host-pause semantics (audio.rs + sink, 0.1.4):** `Player::pause/resume` land
on the inherent `AudioRenderer::set_paused` (concrete-type calls shadow the
trait; internal transport parks arrive via the `AudioSink` trait from generic
code). The inherent path pauses/plays the AudioTrack directly (instant
silence; resume wakes even a track parked by an internal gate) and sets
`host_paused`, which stops the writer's consumption so queued samples survive
the pause. Internal parks deliberately keep consumption running. A self-heal
(getPlayState per accepted batch) recovers any lost lazy-play/unpark race.

## Hardening that shipped along the way (kept; not the root causes)

- `AndroidHost::intercept`/`resolve_key` run on the blocking pool — the host's
  provider does synchronous network round-trips in them; running those on
  runtime workers is wrong regardless (0.1.4).
- Runtime worker floor 6 + named threads (`rustplayer-rt`): the MediaCodec
  dequeue loops still do blocking waits inline in async tasks
  (mediacodec.rs / mediacodec_audio.rs); the floor keeps headroom. The clean
  long-term shape is dedicated decoder threads / spawn_blocking — but coarse
  per-sample `block_in_place` was MEASURABLY WORSE (0/10 healthy starts due to
  constant worker migration churn); don't go that way.
- The audio decoder's per-segment CENC decrypt + mp4 parse runs in
  `block_in_place` (coarse, once per ~6 s segment — the same offload the video
  path does via `spawn_blocking`).

## Verification loop

```
for run in 1..10:
  adb shell am force-stop <pkg>; adb logcat -c
  adb shell am start -a android.intent.action.VIEW -d 'blackzone://detail?id=<id>&type=movie'
  <press play>; sleep 30
  # HEALTHY = vsync f# advancing ~24/s and NO "HEALTH" anomaly lines.
  adb logcat -d | grep -aoE '\[vsync\] f#[0-9]+' | tail -1
Scenario matrix: seek fwd/back, short pause/resume, HOME+relaunch,
BACK+replay, 5-min pause (ABR fires) → resume — frames must advance ≥~20 fps
after each transition.
```
