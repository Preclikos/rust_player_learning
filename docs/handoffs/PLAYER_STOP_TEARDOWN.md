# Handoff — no public stop(): playback survives teardown (audio plays in background)

**Status:** OPEN (host has a stopgap). Found in the BlackZone TV app: pressing
Back (or backgrounding via Home) saved progress and tore down the host UI, but
**audio kept playing in the background** and pipeline tasks lingered.

## Root cause

The bridge's `nativeDestroy` does `shared.shutdown.notify_waiters()` then drops
the `Handle`. `shutdown` only breaks the bridge's track-switch command loop in
`run_playback`, which then calls `play_task.abort()`. But:

- `play_task` is a wrapper that does `let h = player.play()?; h.await;`.
  `player.play()` returns a `JoinHandle<()>` (player.rs:3960). Aborting the
  wrapper drops that inner `JoinHandle` — and **dropping a tokio `JoinHandle`
  detaches the task, it does not abort it**. So the play loop keeps running.
- The play loop's children (audio_passthrough_task, decode, download) are spawned
  inside `play()` and aren't owned by the wrapper either, so nothing cancels them.
- There is **no public `stop()`** on `Player` — only `pause()` (3432). The
  internal `stop`/`stop_flag` (Arc<Notify> / AtomicBool, checked all over the
  pipeline) are never set true from outside (only an internal error path at
  ~2833 sets it; 4139 resets it on rebuild). So a consumer literally cannot stop
  the pipeline.

## Host stopgap (shipped, rust-bridge)

`run_playback` now calls `player.pause()` before `play_task.abort()` on shutdown,
so the **audible** bitstream output stops immediately on Back/background. The
pipeline tasks still linger (paused) — a leak, and a fresh `nativeStart` then
runs a second player alongside the old paused one.

## Suggested fix (player)

Add `pub fn stop(&self)` that:
- `self.stop_flag.store(true, Relaxed)` and `self.stop.notify_waiters()` (whatever
  the pipeline tasks already wait on), so video_supervisor / vsync / decode /
  download / audio_passthrough all observe stop and return;
- pauses/stops the audio sink and releases the AudioTrack;
- ideally is idempotent and awaitable (or the play `JoinHandle` it returns
  completes once children are joined) so the bridge can `nativeDestroy` knowing
  the pipeline is actually gone.

Then the bridge calls `player.stop()` in `run_playback` on shutdown (replacing the
`pause()` stopgap) and/or in `nativeDestroy`. This also removes the lingering-task
leak that compounds the ABR duplicate-pipeline issue
(ABR_REBUILD_ORPHANED_DOWNLOADER).

## Repro

Play anything on the TV app, press Back (or Home) → audio continues from the
device until the stopgap; with the stopgap audio stops but a second pipeline
spawns on the next play.
