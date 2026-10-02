# ABR switch keeps the hardware decoder (decoder reuse)

Status 2026-10-02: Android verified on three devices, web verified functionally,
Windows unchanged and checked. **macOS / iOS (VideoToolbox): compiled and run on
the Mac Pro and the iPhone SE** — see "Apple outcome" at the end.

## What changed

Where two decoders cannot run side by side, an ABR switch used to tear OLD's
decoder down and build NEW's: a 300-400 ms hole with nothing to show (LATE
frames at every switch).

- **Core** (`player/src/player/decode.rs`, `supervisor.rs`, `pipeline.rs`):
  - `HwVideoDecoder::try_reconfigure(&params) -> bool` (default `false`).
  - At switch start the supervisor asks for the running pipeline's decoder
    (`DecoderHandoff::request`). The decode task parks it instead of dropping it
    (`DecoderGuard`).
  - In step 3, NEW offers its `VideoDecoderParams` to the kept decoder. Only when
    that is refused is the kept decoder dropped and a new one built.
  - OLD stops feeding **at the segment that starts at the switch boundary**
    (`DecoderHandoff::set_cutoff`, checked per segment by its first IDR).
    A per-sample check lost 3 frames: in decode order the last P-frame carries
    a pts past the boundary, and the B-frames before the boundary come after it.
  - `VideoDecoderParams::max_width/max_height` = largest picture of the ladder.
- **Android** (`decoders/mediacodec.rs`, direct mode):
  - When the codec has `adaptive-playback`, it is configured with `max-width` /
    `max-height` set to the ladder max.
  - Reuse needs the same mime and DV profile, the same bit depth / transfer /
    gamut, size ≤ max, and the same video window.
  - The new VPS/SPS/PPS go in-band in front of the next IDR.
- **Web** (WebCodecs): warm handoff enabled. NEW decodes next to OLD, as on
  Windows/Linux. `try_reconfigure` is not used there.
- **Apple** (`decoders/videotoolbox.rs`): the session is kept when
  `VTDecompressionSessionCanAcceptFormatDescription` accepts the new format
  description and the destination format is unchanged (bit depth / transfer /
  gamut / `force_8bit_hdr`). In that case only `format_desc` is swapped; samples
  are wrapped with it in `submit`.
- **Android API**: `RustPlayer.setAbrVideoProfile("sdr"|"8bit"|"10bit"|"hdr"|"adaptive")`.
  Test app: `--es abr_profile 8bit`, `--ei alt_lo 2 --ei alt_hi 4` (scenario
  `abr_soft` alternates between those indices of the video list).

## Verified

Test stream ladder (local copy): 4K + 1440p = Main10 PQ, 1080p/720p/480p =
Main 8-bit SDR. A switch across the two groups always builds a new decoder;
reuse happens within a group.

| Device | Decoder | 4 SDR switches (reuse) | 3 switches across groups + Home | Home during a reuse switch (3×) |
|---|---|---|---|---|
| Google TV Streamer (MTK) | c2.mtk.hevc.decoder, adaptive | first frame 0-19 ms (was 300-430), LATE 0, no missing frames, SurfaceFlinger: no gap > 50 ms | LATE 2 (was 7-8) | LATE 0, resumes |
| Mi TV Stick (Amlogic) | OMX.amlogic.hevc.decoder.awesome2, adaptive | LATE 0, codec holds ≤ 9 buffers, 5/5 clean starts with max 4K | LATE 5 | 2× clean; 1× the known Amlogic post-Home `input-buffer stall`, recovered (pre-existing) |
| Samsung S21 (Exynos) | c2.exynos.hevc.decoder, adaptive | first frame 4-7 ms, LATE 0 | LATE 0 | LATE 0 |
| Windows (D3D11VA, warm handoff) | unchanged path | - | LATE 7 = master | - |
| Chrome (WebCodecs, warm) | visible tab, 5 switches incl. SDR↔HDR | render gap max 57-66 ms, 0-1 late/drop per switch, full frame count (≈175 per 7.5 s) | - | - |
| Chrome, cold path (A/B, warm off) | same | render gap **466-483 ms** per switch, ≈160 frames per 7.5 s (~15-20 frames frozen) | - | - |

How to read a run (Android logcat, test app PID only):
- `[abr] decoder kept: reconfigured in place for repr N (WxH)`: reuse happened.
- `MediaCodecDecoder: new codec for WxH (<reason>)`: reuse refused, and why.
- `[abr] NEW first frame Xms after OLD teardown`: the hole.
- `frames_decoded` per stats second against position: a missing-frames check.

## Open for the macOS / iOS agent

1. **Compile.**
   - `publish-ios.yml` workflow_dispatch builds the xcframework (no publish).
     A dispatch on a branch is refused by the conformance gate; pass
     `skip_conformance=true`, or run it after conformance has passed on the commit.
   - macOS: `cargo build -p example-desktop --release`.
   - New FFI: `VTDecompressionSessionCanAcceptFormatDescription` (VideoToolbox,
     returns `Boolean` = `u8`).
2. **Run on macOS.**
   - `RUST_PLAYER_URL=<stream> RUST_LOG=info ./target/release/example-desktop`.
   - In its console: `l` lists tracks; `s <i>` is a soft (ABR-path) switch.
   - Switch within the SDR group (`s 2`, `s 4`, `s 2`…). Expect
     `VideoToolbox: session kept for WxH` and no hole.
   - Then across groups (`s 0`): expect `new session … output format differs`.
3. **Watch for:**
   - Flicker or corrupt frames right after a kept-session switch. Two VT sessions
     side by side flickered on ios-v0.1.7, which is why Apple is not on warm
     handoff; one kept session should not, but it is unverified.
   - `CanAcceptFormatDescription` returning true while decoding the new
     resolution fails. If so, require equal dimensions or disable reuse
     (return false).
4. **iOS device**: same checks through the iOS test shell (soft switch hook, if
   any) or BlackZone with ABR on.

## Other open points found on the way

- **Windows D3D11VA pool exhaustion: FIXED.**
  - **Cause:** D3D11VA's frame pool is one fixed `Texture2DArray`, auto-sized to
    20 for HEVC. Outside the decoder the pipeline holds the frame channel (8),
    the reorder buffer (4), the renderer (1-2), and during a warm switch the
    gate (2) and the pump (1).
  - **Before:** 3 of 4 switch runs ran the pool dry → `Static surface pool size
    exceeded` → `send_packet ENOMEM` → pipeline retry.
  - **Fix:** `extra_hw_frames = 10` (pool 30): 6/6 runs clean.
  - **Linux/VAAPI is unaffected:** VA-API ≥ 1.0 pools are dynamic and FFmpeg
    ignores `extra_hw_frames` there.

### TODO: Windows GPU matrix (hand to testers with other GPUs)
The log line `[ffmpeg_hw] hw frame pool: N surfaces (WxH)` shows the pool per
decoder (expected 30).
1. **Integrated GPU (Intel UHD/Iris, AMD APU) with 4K HDR**:
   - 30 × 4K P010 surfaces ≈ 750 MB of shared RAM.
   - Check that the decoder opens and that ABR switches stay clean (no
     `surface pool size exceeded`, no decoder-open failure).
   - If memory fails, scale `EXTRA_HW_FRAMES` with resolution instead of a
     flat 10.
2. **NVIDIA and AMD discrete**: the same switch test. The pool must stay ≤ 64
   (D3D11 array limit, FFmpeg clamps). Compare LATE per switch with the Intel
   result.

- **Web, fixed on the way:** `Instant::now() - Duration` panicked right after a
  page reload ("overflow when subtracting duration from instant"):
  `performance.now()` starts at page load. Now `rt::instant_ago`.
  Measure web switches in a VISIBLE tab and start with a real click (Web Audio
  needs a user gesture). Use `?hud=1` and `window.__player.setVideoTrackSoft(0, i)`.
- **Amlogic:** the codec sometimes dies across Home (`dequeueInputBuffer -10000`
  or `input-buffer stall` after return). The supervisor recovers since 0.1.54;
  root cause not investigated.

## Apple outcome (2026-10-02, Intel Mac Pro / RX 570, macOS 14.8; iPhone SE 1st gen, iOS 15.8.8)

macOS: `example-desktop` (release) on the test stream, `abr off`, soft switches
through the console (`s <i>`), log at info. iOS: a throwaway Swift harness on a
`CAMetalLayer` (see `docs/handoff/macos-ios.md`), bridge default ABR armed, log
from the device syslog.

- `VTDecompressionSessionCanAcceptFormatDescription` says **no to every
  resolution change** (720p→1080p, 1080p→480p, 480p→720p, 2160p→1440p — all
  "session does not accept WxH, new session"). It says yes only for a rung of
  the same size: the duplicate 2160p and 1440p rungs of the test ladder switch
  with "session kept". So on Apple the reuse covers same-resolution bitrate
  rungs only; a new session is the normal path.
- That path has no hole: `NEW first frame` 26-85 ms after OLD teardown on
  macOS (SDR 28-40 ms, HDR 10-bit 53-85 ms), 180-280 ms on the iPhone SE
  (the A9 at 2160p/1440p); LATE 0 across 11 switches on macOS except one
  150 ms late first frame on two of the 10-bit switches landing on the 30 s
  boundary (2 drops, then clean). No flicker or wrong-frame report in the
  logs; a visual check was not done.
- Across groups (SDR 8-bit ↔ PQ 10-bit) the output format differs and the
  session is rebuilt as designed (`new session for WxH (output format
  differs)`).
- Automatic ABR on the Mac: 720p → 2160p ~2 s after arming (EWMA ~100 Mb/s),
  no drops, no LATE.
- **iPhone SE: the 2160p climb is now handled** (see the ABR pixel cap in
  `docs/handoff/macos-ios.md`): ABR climbs to 2160p at ~8 s, the device
  presents 13 fps and drops 10-14 frames/s, after 3 s the cap fires and ABR
  settles on 1440p, which then plays with 0 LATE to the end of the stream.
