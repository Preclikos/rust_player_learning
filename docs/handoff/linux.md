# Handoff: Linux fixes

Brief for an agent working on a Linux machine with a GPU. It comes from the
static analysis of 2026-09-28 (report:
<https://claude.ai/artifact/5MBTZbQt3RFm6ASthhpFEc>, shared on request).
Everything below was found by reading the code; none of it has been run on
Linux yet.

## Ground rules

- **The current release is stable; keep it that way.** Every fix is as small
  as possible and changes behaviour only where the bug is.
- **Prove each change with an A/B**: the same scenario on the previous build
  (`git stash` → build → run → `git stash pop`) and on the fix, and report
  both numbers.
- **Do not release.** Commit to `master` in small commits with messages that
  say what was wrong and how it was verified; the maintainer tags releases
  (`scripts/release.sh`, `docs/RELEASING.md`).
- Do not touch Android/Windows/web paths unless a shared fix requires it; if
  it does, `cargo ndk -t arm64-v8a --platform 26 check -p bridge-android`
  must still pass.

## Setup

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
git clone https://github.com/Preclikos/rust_player_learning.git && cd rust_player_learning
cargo test -p player --lib                   # 186 tests must pass
vainfo                                        # must list an HEVC decode profile
```

FFmpeg: build or install it the way the CI job does (step "Vendored FFmpeg"
in `.github/workflows/conformance.yml`).

## Test harness

```bash
gh release download conformance-asset-v1 -D asset
python3 scripts/conformance/rangesrv.py 8123 asset &
cargo run --release --example conformance -p player -- \
  http://127.0.0.1:8123/manifest.mpd \
  --key 00112233445566778899aabbccddeeff:0123456789abcdef0123456789abcdef \
  --secs 60 --switches 3 --seeks 2
```

It prints `PASS`/`FAIL` per check, `CONFORMANCE_JSON` and `PROF_JSON`.

**Known blocker — the Linux CI gate tests nothing today.** The CI runner
(NVIDIA RTX 3050, Vulkan) prints
`CONFORMANCE_SKIP no-hw-decoder: av_hwdevice_ctx_create failed: -542398533`
— there is no VAAPI on that box — and the job is still green. The same log
also shows `ffmpeg_audio: send_packet failed ... Invalid data` and
`resampler init (0Hz 0ch -> 48000Hz 2ch)` before the skip. So:

1. Run everything below on a machine with working VAAPI (Intel or AMD, or
   NVIDIA with `nvidia-vaapi-driver`).
2. Look at why AAC decode fails on the CI box (FFmpeg build flags? extradata
   not passed?) — it may be a real bug on Linux, not just a CI artefact.
3. Worth proposing: make the Linux gate real (VAAPI-capable runner or
   driver), or make `CONFORMANCE_SKIP` fail the job so a skip is visible.

## Tasks, most serious first

### 1. Vulkan memory freed while the GPU may still use it (high)

`player/src/renderers/video/video_frame.rs:223` — `impl Drop for VideoFrame`
calls `free_memory` immediately. The frame drops right after `queue.submit`
in `render` (`player/src/renderers/video.rs`), while the command buffer can
still reference the image; the `vk::Image` itself is destroyed later by wgpu
(drop callback `None` in `video_vulkan.rs`). This violates Vulkan's rules
and can end in device-lost. Nothing in the renderer waits for submitted work
today (`on_submitted_work_done` is not used anywhere).

**Fix:** release the memory (and the keepalive from task 2) from
`queue.on_submitted_work_done(...)`, or give the image a hal drop callback
that frees the memory after wgpu destroys it.

**Verify:** conformance A/B with Vulkan validation layers on
(`VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation`): no validation errors
about freed memory in use, no device-lost.

### 2. VAAPI surface returned to FFmpeg's pool while still being sampled (high)

`video_frame.rs:34` (the `cfg(linux)` `VideoFrame::new`) exports the VAAPI
surface as a DMA-BUF and imports it zero-copy, but the `VideoFrame` keeps no
reference to the `Arc<Video>`. Dropping the frame after submit lets FFmpeg
reuse the surface and decode into it mid-draw → content/pts mismatch
flicker (same class as the Android AImage bug fixed earlier).

**Fix:** store the `Arc<Video>` in the same keepalive as task 1.

**Verify:** a scene-cut-heavy clip; record the output (or dump frames) and
look for a flash of the wrong frame at cuts. A/B.

### 3. VAAPI export is incomplete (medium)

`player/src/renderers/video/video_vaapi.rs:162` (`export_shared_handle`):
- flags `0x1000` is not a libva flag — read-only is
  `VA_EXPORT_SURFACE_READ_ONLY = 0x0001`, and neither
  `VA_EXPORT_SURFACE_SEPARATE_LAYERS` (0x4) nor `COMPOSED_LAYERS` (0x8) is
  set;
- only `objects[0].fd` is imported (`:194`); the other fds leak, and fd 0
  leaks on every error path;
- layer offsets/pitches and `drm_format_modifier` are ignored, OPTIMAL
  tiling is assumed;
- no `vaSyncSurface` before the import;
- `panic!("Cannot create va shared handle")` (`:171`) kills the render task.

**Fix:** `READ_ONLY | COMPOSED_LAYERS`, `vaSyncSurface` first, close every
fd not handed to Vulkan, import with `VK_EXT_image_drm_format_modifier` and
explicit plane layouts from the descriptor, and return an error instead of
panicking.

**Verify:** conformance A/B on Intel and, if available, AMD (different
modifiers); `ls /proc/<pid>/fd | wc -l` must stay flat over a 10-minute run.

### 4. cpal: device loss and sample format (high, shared with macOS/Windows)

`player/src/renderers/audio/audio_cpal.rs:164-176`: `err_fn` only logs, so
when the output device goes away (USB/Bluetooth headset, PipeWire restart)
the stream dies and the audio clock — and with it the picture — freezes; the
stream is built as f32 with `.expect()`, so an i16-only device panics the
audio thread.

**Fix:** signal a rebuild from `err_fn`, take the format from
`default_output_config().sample_format()`, fall back to the null sink
instead of panicking. **Coordinate with the macOS agent** — same file; one of
you implements, both test.

**Verify:** unplug/replug a USB or Bluetooth headset (or
`systemctl --user restart pipewire`) during playback: playback continues.

### 5. FFmpeg HW submit (low-medium, suspected)

`player/src/decoders/ffmpeg_hw.rs:253-278` sends each NALU as its own packet
and turns `EAGAIN` from `send_packet` into a hard error mid-sample.
**Fix:** send the whole Annex-B access unit as one packet; on `EAGAIN`,
drain `receive_frame` and retry. Also applies to Windows — the maintainer
can A/B it there (local Windows conformance works).

## When done

Append to this file: what was done, commit hashes, the A/B numbers, and
anything found but not fixed.
