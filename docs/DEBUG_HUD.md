# Debug HUD

The engine builds the HUD itself, so every platform shows the same
information: a structured snapshot (JSON) and the same content as ready-made
text lines. A host only has to draw the text, or pick fields out of the JSON.

## API

| Platform | Snapshot JSON | HUD text (`events` = event-log lines at the end) |
| --- | --- | --- |
| Rust (`player`) | `player.debug_snapshot().to_json()` | `player.debug_snapshot().lines(events)` |
| Bridge | `BridgeHandle::debug_json()` | `BridgeHandle::debug_text(events)` |
| Android (Kotlin) | `RustPlayer.debugJson()` | `RustPlayer.debugText(events = 8)` |
| iOS (Swift) | `RustPlayer.debugJSON()` | `RustPlayer.debugText(events: 8)` |
| iOS (C) | `rustplayer_player_debug_json(h)` | `rustplayer_player_debug_text(h, events)` (free both with `rustplayer_string_free`) |
| Web | `player.debugJson()` | `player.debugText(events)` |

Test shells: Android test app `HUD` button or `--ez hud true`; web demo `HUD`
checkbox or `?hud=1`; desktop example key `H` (dumps to the terminal once a
second); conformance harness `--hud` (every 5 s, with the build time).

## Cost

Nothing runs for the HUD while no one asks. The snapshot reads counters the
player keeps anyway plus a few written once per segment (download, queue,
prepare) and the event log, which is appended only on rare events (state
changes, switches, failures), never per frame. Building a snapshot takes
~20-70 µs on a desktop; poll it at 1-2 Hz and only while the HUD is visible.
The Android sinks answer their line with one memoised clock read each.

## What it shows

```
playing  0:49 / 1:00  up 29s
V 3840x2160 hvc1.2.4.L150.90 14.0 Mb/s  repr 0  MediaCodec
V out MediaCodec direct -> video plane; overlay (subtitles): wgpu Gl  surface 1920x984 Rgba8Unorm AutoVsync
V buf 11.2s  seg #9 (14 done)  queue 2/2  last 12080 KiB in 179 ms / 6006 ms media
V frames 642  drop 1  late 3  prepare 51 ms  boundary stalls 1 (last 316 ms)
V render gap max 240 ms  judder 213  int <25 42  25-41 272  42-58 311  >58 16
V present lead 78 ms (display 78 ms)  deadline 34.0 ms  vsync 41.7 ms snapped  starved 0  shown/released 24/24 per s
A ec-3 6ch 48000 Hz  PASSTHROUGH  repr 7  underruns 0
A out passthrough E-AC-3 48000 Hz 6 ch (enc 6)  written 902 AUs = 28.9s (2.6 MiB)  consumed 26.7s  played 26.7s  in device 2159 ms
A buf 11.2s  seg #9 (7 done)  queue 2/2  last 566 KiB in 12 ms / 5990 ms media
sync drift -10 ms (max 10 ms)  clock audio (fallbacks 0)  out latency 0 ms
net 407.9 Mb/s  total 113.9 MiB  stall 0 ms   abr BandwidthEwma { safety_factor: 1.43 } / Adaptive  target 8s  last switch 16s ago
session stalls 0 (0 ms)  pipeline retries 0  audio output rebuilds 0
  12.2s  0:32 abr      switch repr 0 (2160p) -> 6 (480p) from segment 6, measured 354.3 Mb/s
  16.1s  0:36 abr      new rung's first frame 309 ms after the old one ended
```

- **state**: `playing`, `paused`, `buffering`, `held (surface gone)`.
- **V / A pipeline** (`video.pipeline`, `audio.pipeline`): media buffered ahead
  of the picture (downloaded counts, as in `Stats`), the last downloaded
  segment index and count, the download → decoder queue (`queued/capacity`),
  the last segment's size, download time and media duration (download faster
  than media = keeping up), download retries.
- **V**: representation, codec, size, bitrate, decoder, how frames reach the
  screen (Android: direct MediaCodec to the video plane, or ImageReader + GLES;
  desktop / web: wgpu backend and surface), frames decoded / dropped / late,
  the last segment's join + decrypt + parse time, segment-boundary stalls,
  render gap, judder and the interval histogram.
- **V present** (Android direct mode only): how early frames are released
  (the display's deadline + one vsync, a cap after decoder starvation),
  the display's deadline and vsync, `snapped` while release stamps sit on
  the vsync grid, decoder-starvation events, and frames the codec reported
  shown vs released in the last second (`-` below API 33). Fewer shown
  than released = the display dropped frames.
- **A**: representation, codec, channels, rate, passthrough, and the output's
  own line: Android PCM AudioTrack (written / presented / in track / dropped),
  passthrough AudioTrack (codec, AUs and bytes written, consumed, played, held
  in the device), cpal (rate, channels, played, device latency) or Web Audio
  (running or suspended).
- **sync**: A/V drift (last and max), which clock drives the picture (`audio`
  or `wall`), wall-clock fallbacks, output latency.
- **net / abr / session**: bandwidth EWMA, bytes, network stall; ABR strategy,
  HDR profile, buffer target, time since the last switch; stalls, pipeline
  retries, audio output rebuilds.
- **events** (up to the last 30 in the JSON, 80 kept): every player event
  except the periodic `Position` / `Stats`, plus `abr` (switch decisions with
  the measured bandwidth, the new rung's first-frame latency), `pipeline`
  (failures, retries), `decode` (boundary stalls > 50 ms), `net` (segment
  retries), `surface` (a plane gone / back), `audio` (passthrough engaged,
  output rebuilt), `seek`. Each entry has seconds since the player was
  created and the playback position.

The JSON field names follow `player::DebugSnapshot` (`state`, `position_ms`,
`duration_ms`, `uptime_s`, `video{…, pipeline{…}}`, `audio{…, pipeline{…}}`,
`sync`, `network`, `abr`, `session`, `events[{t_s, position_ms, kind, text}]`).
Add fields freely; do not rename them (hosts may read them).
