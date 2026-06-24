# Handoff — ABR rebuild orphans the old downloader → "downstream receiver dropped" wedge

> **Update `99854b4`:** ✅ primary fix landed. `download_task` now checks
> `segment_sender.is_closed()` at the top of the segment loop, the top of the
> retry loop, and on a send error — a dropped receiver terminates the task
> immediately instead of retrying the SendError for 30 s. Kills the orphan and
> the retry-loop noise. **Still to confirm on device:** that the NEW pipeline
> (segment 10 in the trace) no longer starves once the orphan exits promptly —
> if it still does, the rebuild channel wiring needs a look (handoff fix #3).
>
> **Device retest on `99854b4` (kirkwood, 4K/ec-3):** ✅ the `downstream receiver
> dropped` wedge is GONE — no SendError loops, no "gave up", no pipeline collapse,
> no crash; the healthy pipeline runs `decoded=24-27/s drift~1.8s res=3840x2160`.
> ❌ but a **duplicate video pipeline still spawns on the ABR swap** (fix #3): the
> `[vsync] HEALTH` line alternates between two concurrent pipelines on different
> threads —
> ```
> decoded=25/s  drift=1818ms    res=3840x2160   ← healthy (drives display)
> decoded=1/s   drift=177142ms  res=4096x2176   ← stale duplicate, frozen ~1fps
> decoded=1/s   drift=183477ms  res=4096x2176     drift grows at real time
> decoded=1/s   drift=189847ms  res=4096x2176     (177k→183k→189k→196k over ~20s)
> ```
> So the orphaned *downloader* is fixed, but the orphaned *pipeline/decoder+vsync*
> on the OLD representation is not torn down on the swap — it keeps a HW decoder
> slot and a vsync loop alive at 1 fps with unbounded drift. Root: the rebuild
> spawns the NEW pipeline without joining/stopping the OLD one's decode+present
> tasks (only the download side now exits). Next: cancel the old pipeline's
> decoder/vsync on swap (single source of truth for "active generation"), or gate
> vsync/decode on a generation token so a superseded pipeline self-terminates.

**Status:** OPEN. Found integration-testing `25ff833` in the BlackZone TV app on
the Google TV Streamer (kirkwood, armeabi-v7a, 4K HDR, E-AC-3 passthrough). The
app appears to "crash" but **does not** — the crash buffer (`logcat -b crash`) is
empty, no tombstone. Playback **wedges** after an ABR switch and buffers forever;
from the couch that reads as a crash.

This is the "duplicate-task SPAWN on rebuild / downloader outlives its receiver"
orchestration issue called out as still-open in the b030db8 crash-fix and
32a01b7 notes. `25ff833`'s net read-timeout + head watchdog addressed the
*stalled-network* and *wedged-head* freezes, but not this *rebuild-teardown* one.

## Evidence (device, pid 2891, build 25ff833)

```
[mc-direct] output format: 3840x2160 (content)
[abr] NEW first frame 591ms after OLD teardown (pts=6083ms)   ← ABR swap completes, new pipeline has a frame
[vsync] starvation recovered after 142ms
[vsync] LATE #144..148  pts=6083..6500ms                      ← post-swap frames land late
[vsync] A/V drift 2094ms
[dl] segment 8 failed, retrying in 8s: downstream receiver dropped: SendError { .. }
[dec] seg done: pts 6083..11958ms
[dl] segment 8 failed, retrying in 8s: downstream receiver dropped: SendError { .. }
[dl] segment 8 gave up after 30s of retries: downstream receiver dropped: SendError { .. }
video decoder_task: dequeueOutputBuffer(direct): -10000
[video] supervisor: pipeline exited naturally; closing
[dl] segment 10 failed, retrying in 500ms: downstream receiver dropped: SendError { .. }
[vsync] no frame for 300ms — entering buffering
[dl] segment 10 failed … (8s backoff, repeating indefinitely)
```

## Diagnosis

1. An ABR switch tears down the OLD pipeline (drops the segment channel's
   receiver) but the OLD `download_task` keeps running and `send()`s into the
   dropped channel → `SendError` ("downstream receiver dropped").
2. The download **retry policy treats `SendError` as a retryable error** and
   retries with backoff for 30 s before giving up. It is NOT a network failure —
   the consumer is simply gone — so it should terminate the task immediately, not
   retry. The retries are pure noise and tie the task up.
3. The churn cascades: the decoder hits `dequeueOutputBuffer(direct): -10000`,
   the video supervisor exits ("pipeline exited naturally; closing"), playback
   drops to buffering, and the NEXT segment (10) repeats the same `SendError`
   loop — wedged indefinitely.

## Suggested fixes (native)

- On a pipeline rebuild / ABR swap, **cancel the previous `download_task`**
  (set its stop_flag / drop its handle) before/with dropping the receiver, so no
  orphaned downloader survives.
- In the download loop, **distinguish a dropped-receiver `SendError` from a
  network error**: a closed/dropped channel means "consumer gone → stop now",
  never retry. Only genuine transport errors should hit the RetryPolicy.
- Confirm the NEW pipeline isn't also starving for an unrelated reason (segment
  10 failing right after the OLD task's give-up suggests the swap left the new
  downloader without a live receiver too — worth checking the channel wiring on
  the rebuild path).

## Notes

- No host involvement: `downstream receiver dropped` is entirely internal to the
  player's download→decode channel. Bridge/host wiring (surface, interceptor) is
  unchanged and healthy.
- Repro: 4K/ec-3 title on kirkwood; let it run until ABR switches representation
  (≈ first few seconds here, "NEW first frame after OLD teardown"). Pin in the
  app: `25ff833` (rust-bridge/Cargo.toml).
