# Crash handoff — b030db8: AFR setFrameRate UAF + passthrough-task leak

**Status:** ✅ FIXED in `5afa722` (both native bugs). Found while integration-
testing `b030db8` in the BlackZone TV app (`BlackZoneAndroidRust`) on device
**Google TV Streamer (kirkwood, armeabi-v7a, Android 14)**, IP 192.168.1.16,
E-AC-3 HDMI passthrough to an AVR. Symptom: *"player po chvilce spadne"* —
playback died after ~15 min.

Two distinct native bugs, both surfacing together. The passthrough one (#2) was
a **regression in `b030db8`**; the AFR one (#1) is latent but newly triggered by
the teardown that #2 (or any pipeline exit) causes.

> **Fix summary (`5afa722`, `player/src/player.rs`):**
> - **#2** — bounded the PRIME phase: a feed whose AudioTrack head never starts
>   now abandons itself instead of priming forever. Stops the runaway/leak.
>   (Refined in `ef8e42e`: bound raised 10 s → **30 s** so a slow-waking HDMI AVR
>   isn't falsely abandoned, and PRIME is now **paused-aware** — a paused track
>   legitimately reports head 0, so it buffers ~3 s then idles instead of being
>   abandoned. See the seek+pause fix.) Healthy startup unaffected (head ~2.3 s).
> - **#1** — the player now holds its own `ANativeWindow_acquire` ref on the
>   direct-mode window via a `DirectWindow` wrapper (released on replace / last
>   `Player` drop), so the host can't free the window's mutex under a `setFrameRate`
>   call. `set_video_output_window(null)` releases the ref — host calls it on
>   `surfaceDestroyed`.
> - **Still open (orchestration):** WHY a second `audio_passthrough_task` is
>   spawned on rebuild — see §2; the PRIME bound only contains its impact. Plus
>   the host↔native surface-lifecycle wiring (call `set_video_output_window(0)`
>   before releasing the Surface) — §1 / integrator notes.
>
> Verified on device: healthy passthrough intact (decoded 24-25 fps, drift
> ~100 ms, write_ahead ~740 ms, no false PRIME abort, AFR switches to 23.976 fps,
> no crash). The duplicate-task + surface-teardown paths need the BlackZone app
> to exercise (the test shell loops one track and holds its Surface).

---

## 1) Hard crash — SIGABRT: `setFrameRate` on a destroyed Surface (UAF)

```
F libc    : FORTIFY: pthread_mutex_lock called on a destroyed mutex (0xeb8df5c8)
F libc    : Fatal signal 6 (SIGABRT) in tid 18188 (Thread-5), pid 18162 (cz.preclikos.rust_player)
F DEBUG   : Abort message: 'FORTIFY: pthread_mutex_lock called on a destroyed mutex'
F DEBUG   :   #04 libc++.so   std::__1::mutex::lock()
F DEBUG   :   #05 libc++.so   std::__1::__shared_mutex_base::lock_shared()
F DEBUG   :   #06 libgui.so   android::Surface::hook_query(ANativeWindow const*, int, int*)
F DEBUG   :   #07 libnativewindow.so  ANativeWindow_setFrameRateWithChangeStrategy+36
F DEBUG   :   #08 base.apk    <our libblackzone_rust_player.so> offset 0xc0000
```

`Surface::hook_query` locks the Surface's internal `shared_mutex` — which has
already been **destroyed**, i.e. the `ANativeWindow` was released by the host
while native code still held its raw pointer.

**Root cause — `player/src/player.rs`:**
- `set_window_frame_rate(window: usize, fps)` (`player.rs:1883`) calls
  `ANativeWindow_setFrameRateWithChangeStrategy(win, …)` on a raw pointer with
  only a `window == 0` guard (`:1885`) — no liveness check.
- Called from the pipeline (re)build path at **`player.rs:4134-4144`**:
  `let direct_window = video_output_window.load(Relaxed);` then
  `set_window_frame_rate(direct_window, fps)`. `video_output_window`
  (`AtomicUsize`, `:265`) holds the ANativeWindow pointer captured at surface
  init and is **never invalidated** when the host destroys/replaces the Surface.

When the host's `SurfaceView` is torn down (activity backgrounding, surface
recreate, or our own playback teardown) the Kotlin side releases the Surface,
but `video_output_window` still points at the freed `ANativeWindow`. The next
AFR re-assert (fires on every pipeline rebuild: seek / ABR swap / **audio-track
switch**, per the `:4137` comment) dereferences it → UAF → SIGABRT.

**Suggested fix (native):** make the window lifetime explicit.
- Host signals surface-gone; clear `video_output_window` to 0 before the
  Surface is released, and stop the AFR/present path from running after that.
- Or have native take an `ANativeWindow_acquire()` ref for the lifetime it uses
  the pointer and `_release()` on teardown, so the Surface can't be destroyed
  under it (note: acquiring does not by itself make `setFrameRate` safe if the
  *host* SurfaceView is gone — coordinate the stop).
- Minimum: gate `set_window_frame_rate` behind a "surface live" flag the host
  controls, and ensure the decode/present loop is joined before the host frees
  the Surface.

---

## 2) Passthrough-task leak / runaway PRIME (regression in b030db8)

After an audio-track switch / pipeline rebuild, **two** `audio_passthrough_task`
instances run concurrently — one healthy, one runaway:

```
# runaway (head FROZEN, write_ahead grows unbounded — ~5 minutes ahead):
I [audio-pt] au=4266954ms head=3975146ms write_ahead=291808ms
I [audio-pt] au=4271562ms head=3975146ms write_ahead=296416ms
I [audio-pt] au=4277706ms head=3975146ms write_ahead=302560ms
# healthy (head tracks, ~750ms ahead):
I [audio-pt] au=4003850ms head=4003107ms write_ahead=743ms
```

Then the rebuild's torn-down channel kills the video pipeline:

```
W [dl] segment 672 failed, retrying …: downstream receiver dropped: SendError
E [dl] segment 672 gave up after 30s of retries: downstream receiver dropped
E video decoder_task: dequeueOutputBuffer(direct): -10000
I [video] supervisor: pipeline exited naturally; closing
```

**Root cause — `player/src/player.rs`, `audio_passthrough_task` (`:2229`):**
- The STEADY pacing gate has a PRIME short-circuit: `if played == 0 { break }`
  (`:2316`), i.e. *while the AudioTrack head has not started, write every AU
  ungated*. `head = base_pts_ms + sink.played_ms()` (`:2337`).
- For the **stale/duplicate** pipeline's sink, `played_ms()` stays **0 forever**
  (that AudioTrack never becomes the active output), so the task is stuck in
  PRIME → writes unbounded → `head` frozen at `base_pts_ms`, `write_ahead`
  climbs without limit. This is the *same* deadlock b030db8 fixed, re-expressed
  as a runaway because PRIME has no upper bound when the head never moves.
- The duplicate exists because a pipeline rebuild (OLD vs NEW, see the
  `prime_target == usize::MAX` comments at `:1817-1821` / `:2026`) spawns a new
  `audio_passthrough_task` (`:2212`) but the OLD task's `stop_flag` is
  apparently not set, so it keeps draining its channel and priming.

**Suggested fix (native):**
- On pipeline rebuild, ensure the previous passthrough task's `stop_flag` is set
  (and its `download_rx` dropped) so it returns before the new one starts.
- Bound PRIME: cap it by wall-time and/or a max `write_ahead` (e.g. abort/yield
  if `played_ms()` is still 0 after N seconds or write_ahead exceeds a ceiling) —
  a sink whose head never starts must not be able to spin forever.

---

## Repro

1. Start playback of an E-AC-3 (passthrough) title on the Google TV Streamer.
2. Let it run, and/or switch the audio track (the BlackZone "original" toggle →
   `change_audio_track`) — this rebuilds the pipeline.
3. Within ~minutes: a runaway `[audio-pt]` (write_ahead → 100s of seconds), the
   video supervisor exits, and a teardown fires `setFrameRate` on the freed
   Surface → SIGABRT.

## Notes for the integrator side (BlackZoneAndroidRust)

- Host wiring is unchanged from `587f0825`; the runaway is purely in the
  passthrough feed. The AFR UAF needs a host↔native surface-lifecycle contract
  (clear `video_output_window` / stop present before releasing the Surface) —
  happy to add the Kotlin half (`PlaybackScreen` surface callbacks +
  `RustPlayerAdapter`) once the native side exposes a "surface released" entry
  point or a liveness flag.
- Player pin currently at `b030db8` in `rust-bridge/Cargo.toml`. Reverting to
  `587f0825` drops bug #2 (the passthrough feed) but loses the E-AC-3
  start-deadlock fix; bug #1 may still be reachable on backgrounding.
