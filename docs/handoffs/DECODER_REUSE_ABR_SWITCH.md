# ABR switch keeps the hardware decoder (decoder reuse)

Status 2026-10-02: Android verified on three devices, web verified functionally,
Windows unchanged and checked. **macOS / iOS (VideoToolbox) implemented but neither
compiled nor run** - this handoff is for that.

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

- **Windows warm handoff, rare:** `Static surface pool size exceeded` →
  `send_packet: Not enough space` → pipeline retry. Seen 1 in 3 runs on master
  too, so not caused by this change. The gate holds 2 frames, but the D3D11VA
  pool still runs out sometimes.
- **Web, fixed on the way:** `Instant::now() - Duration` panicked right after a
  page reload ("overflow when subtracting duration from instant"):
  `performance.now()` starts at page load. Now `rt::instant_ago`.
  Measure web switches in a VISIBLE tab and start with a real click (Web Audio
  needs a user gesture). Use `?hud=1` and `window.__player.setVideoTrackSoft(0, i)`.
- **Amlogic:** the codec sometimes dies across Home (`dequeueInputBuffer -10000`
  or `input-buffer stall` after return). The supervisor recovers since 0.1.54;
  root cause not investigated.
