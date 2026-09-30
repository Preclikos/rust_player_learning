# Buffering and network outages

How far ahead the player downloads, how it refills, and how long it rides out
a lost network. One setting, `BufferConfig`, the same on every platform.

## What each field does

| Field | Default | Meaning |
| --- | --- | --- |
| `max_secs` | 30 | Media downloaded ahead of the picture, filled up to this. |
| `min_secs` | = `max_secs` | Once full, downloading resumes when the buffer falls below this. Equal to `max_secs` = fill continuously (ExoPlayer's default for video). Lower = download in bursts, which lets a mobile radio sleep in between. |
| `max_bytes` | 96 MiB | Cap on downloaded, not yet played media per pipeline (video and audio each), estimated from the representation bitrate. The buffer is the smaller of `max_secs` and this. |
| `network_outage_secs` | 180 | How long one segment keeps being retried through a lost network before playback fails with an error. |

During an outage the player plays what it has, then shows `Buffering`
(`reason: Stall`) and keeps retrying (every 0.5, 1, 2, 4, then 5 s). When the
network is back it continues where it stopped: nothing is skipped, and ABR
measures the link again on the next segments. Only after
`network_outage_secs` of one segment failing does it report an error.

Memory: one segment holds about `bitrate × segment length / 8`. 30 s of 4K at
14 Mb/s is ~52 MB; at 40 Mb/s it is ~150 MB, so the 96 MiB cap then limits
the buffer to ~19 s. Audio adds little (E-AC-3 640 kb/s: ~2.4 MB per 30 s).

The buffer the player picked is in the log (`play(): buffer 30s (refill
below 30s, cap 96 MiB) -> 5 segments in flight …`) and in the debug HUD
(`queue n/capacity`, `buf`, `retries`, the `net` event-log lines).

## Recommended settings

| Device | `max_secs` | `min_secs` | `max_mb` | `outage_secs` | Why |
| --- | --- | --- | --- | --- | --- |
| **TV / streaming stick** (Android TV, Google TV, Fire TV) | 30 | 30 (continuous) | 64 on 1-1.5 GB RAM, 96 on 2 GB+ | 120 | Wired or home Wi-Fi: steady link, outages are router restarts. RAM is the limit (4K + the app + the system on 1.5-2 GB). Continuous filling keeps ABR measurements fresh. |
| **Phone / tablet** | 60 | 20 | 128 | 300 | Cellular: tunnels, lifts, cell handovers. A deep buffer rides a 1-2 minute tunnel out without a spinner; burst refill (download to 60 s, pause until 20 s) lets the radio idle and saves battery. Phones have RAM to spare at 1080p (6 Mb/s × 60 s ≈ 45 MB). |
| **PC / Mac** (desktop app) | 60 | 60 (continuous) | 256 | 120 | Plenty of RAM, usually a steady link; a deep buffer covers Wi-Fi hiccups and laptop sleep/wake. |
| **Web** (browser, any device) | 30 | 30 (continuous) | 128 | 120 | A tab shares memory with the rest of the browser; the browser may also throttle a hidden tab. |

Streams above ~20 Mb/s (4K high bitrate): keep `max_mb` as above and let the
cap do its job rather than raising it; the player shows the effective buffer
in the HUD.

## API

Call it right after starting playback: the first pipeline starts only once the
manifest and init segments are in, so the setting is in place for it. Later
calls apply from the next seek or track change. `0` (or a missing field)
keeps that field's default; `min_secs` 0 = fill continuously.

| Platform | Call |
| --- | --- |
| Rust | `player.set_buffer_config(BufferConfig { max_secs, min_secs, max_bytes, network_outage_secs })` |
| Android (Kotlin) | `rustPlayer.setBufferConfig(maxSecs = 60, minSecs = 20, maxMb = 128, outageSecs = 300)` after `start(...)` |
| iOS (Swift) | `player.setBufferConfig(maxSecs: 60, minSecs: 20, maxMb: 128, outageSecs: 300)` after create |
| iOS (C) | `rustplayer_player_set_buffer_config(h, 60, 20, 128, 300)` |
| Web | `RustPlayer.create(canvas, url, host, { buffer: { maxSecs: 30, maxMb: 128, outageSecs: 120 } })` |

`Player::set_buffer_target_secs(n)` still works and sets `max_secs`.

## Compared with other players

| | Buffer | Refill | Retries |
| --- | --- | --- | --- |
| This player (default) | 30 s, 96 MiB cap | continuous | up to 180 s per segment, 0.5 → 5 s apart |
| ExoPlayer (video default) | 50 s (min = max) | continuous | backoff up to 5 s |
| Shaka Player | 10 s goal, 30 s kept behind | continuous | 2 attempts, 1 s ×2 backoff |
