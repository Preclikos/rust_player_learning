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

## Outcome (2026-09-28, Intel UHD 630, Manjaro, kernel 7.1, iHD 26.2.4, Mesa ANV)

Test setup: vendored FFmpeg from `player/scripts/build-ffmpeg.sh linux`,
the conformance harness above, `VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation`
with `VK_LAYER_ENABLE_MESSAGE_LIMIT=false` (the default limit caps each
message at 20, which hides the real counts). A/B = the previous commit's
binary against the fix, same scenario (60 s, 3 switches, 2 seeks).
`cargo test -p player --lib`: 186 passed.

| Task | Commit | Result |
| --- | --- | --- |
| 1. Vulkan memory freed in use | f93c862 | `VUID-vkFreeMemory-memory-00677` ("can't be called on VkDeviceMemory ... in use by VkCommandBuffer") 2900 -> 0. Conformance 17/17 PASS both; judder 14 -> 16, late 7 -> 7, lip-sync max 37 -> 39 ms, render gap max 107 -> 53 ms. RSS 210 / 209 MB and 42 / 42 fds at 60 s: the deferred release does not accumulate. No device-lost in either build. |
| 2. VAAPI surface back to the pool | f93c862 | Same fix: `render` drops `(VideoFrame, Arc<Video>)` from `queue.on_submitted_work_done`, the contract the Apple path uses. Not verified visually (no scene-cut recording); covered only by the reasoning and the task 1 numbers. |
| 3. VAAPI export / DMA-BUF import | 6fb5ac4 + wgpu fork e4729c2 | `VUID-VkImageCreateInfo-pNext-00990` 2902 -> 0 and `VUID-VkImportMemoryWin32HandleInfoKHR-handleType-09861` (misnamed by the layer; it is the DMA-BUF import) 2902 -> 0. Conformance 17/17 PASS; 28 flash/beep pairs before and after, so the picture content is right; lip-sync max 39 -> 38 ms, judder 16 -> 15. fds stay at 42 (VA exports one object per surface here). |
| 4. cpal device loss / format | not done | See open findings. |
| 5. FFmpeg HW submit | not done | |
| (hashes) | | The 6fb5ac4 message calls its baseline "41711aa": that is f93c862 before a rebase onto 8234e84. |

Notes on task 3:
- The wgpu fork did not enable `VK_EXT_image_drm_format_modifier`;
  e4729c2 on `Preclikos/wgpu` `trunk` enables it (optional, needs Vulkan 1.1
  and `VK_KHR_image_format_list` / 1.2). The player's `Cargo.lock` now points
  at it, so every platform picks up the extra optional extension.
  `cargo ndk ... check -p bridge-android` was NOT run: no Android NDK or
  cargo-ndk on this machine. The change has no `cfg` and builds on Linux.
- ANV exposes NV12 with modifiers 0x0 (linear), X- and Y-tiled; with Y-tiled
  (what iHD exports, 0x0100000000000002) `ALIAS` makes the combination
  unsupported, so the modifier image is created without it.
- ANV exposes no modifiers for P010 at all. The import asks
  `vkGetPhysicalDeviceImageFormatProperties2` first and falls back to the
  previous OPTIMAL import when the modifier is not importable, so P010 keeps
  working exactly as before. The conformance asset is 8-bit, so that path
  was not exercised here.
- Not tried on AMD.

### Open findings

- **Build on a machine with a system FFmpeg.** `alsa-sys` emits
  `-L /usr/lib` ahead of the vendored FFmpeg, so a box with FFmpeg 9 installed
  links against it and fails (`undefined symbol: avcodec_close`). Workaround:
  `RUSTFLAGS="-L native=$PWD/player/vendor/linux-x64/lib"` (what the macOS
  CI job already does). CI has no system FFmpeg, so it does not see this.
- **AAC on the CI box.** With the vendored FFmpeg here AAC decodes cleanly
  (`resampler 48000Hz 2ch -> 48000Hz 2ch`); the CI's `send_packet ... Invalid
  data` / `0Hz 0ch` did not reproduce. Worth checking whether the CI job
  really uses the vendored build (the `|| true` after `build-ffmpeg.sh`
  hides a failed build; the script also exits 1 here after a successful
  install).
- **Task 4.** The platform-wide `audio_output_watchdog` rebuilds the pipeline
  (and with it the cpal stream) when the consumed position stands for
  1.5 s, so a dead stream may already recover on Linux; this was not tested
  yet (`systemctl --user restart pipewire` during playback). The macOS agent
  found no change needed there. The i16 `.expect()` is real but only hits a
  default device without float support (bare ALSA `hw:`); PipeWire/Pulse
  take f32.
- **`asset/`** (the conformance download) is not in `.gitignore`.
- **Conformance on a short asset.** The asset is ~60 s; `--secs` beyond that
  ends in EndOfStream unless seeks keep landing before the end (the harness
  seeks back to 0 when the remaining scenario exceeds the asset). Actions
  are not interleaved: all switches come first, then all seeks, so a long
  run needs `--switches 0` (or few) for the first seek to land before 60 s.
- **A seek after EndOfStream does not resume playback** (suspected bug, both
  builds alike). 500 s, 8 switches then 32 seeks: EndOfStream at 60 s, then
  every `seek(0)` rebuilt the pipeline but the audio position never moved
  again ("32 of 34 rebuilds never advanced the audio position"), 1450
  frames decoded in 500 s. Not chased; could be the harness's expectation
  rather than the player's.
- Unrelated validation noise left alone: 4x `VUID-StandaloneSpirv-None-10684`
  (a shader variable without an explicit layout decoration).
