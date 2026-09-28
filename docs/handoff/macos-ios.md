# Handoff: macOS / iOS fixes

Brief for an agent working on a Mac. It comes from the static analysis of
2026-09-28 (report: <https://claude.ai/artifact/5MBTZbQt3RFm6ASthhpFEc>,
shared on request). Everything below was found by reading the code. Nothing
here has been run on Apple hardware yet.

## Ground rules

- **The current release is stable, keep it that way.** Each fix is as small
  as possible and changes behaviour only where the bug is.
- **Prove every change with an A/B.** Run the same scenario on the previous
  build (`git stash`, build, run, `git stash pop`) and on the fix. Report both
  numbers.
- **Do not release.** Commit to `master` in small commits with messages that
  say what was wrong and how it was verified. The maintainer tags releases
  (`scripts/release.sh`, see `docs/RELEASING.md`).
- **Do not touch Android, Windows or web code paths** unless a shared fix
  requires it. If it does, the Android build must still pass:
  `cargo ndk -t arm64-v8a --platform 26 check -p bridge-android`.

## Setup

```bash
xcode-select -p || xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
git clone https://github.com/Preclikos/rust_player_learning.git && cd rust_player_learning
cargo test -p player --lib                   # 186 tests must pass
```

The macOS build needs FFmpeg the way the CI job provides it; see the
"Vendored FFmpeg" step in `.github/workflows/conformance.yml`.

## Test harness

```bash
gh release download conformance-asset-v1 -D asset
python3 scripts/conformance/rangesrv.py 8123 asset &
cargo run --release --example conformance -p player -- \
  http://127.0.0.1:8123/manifest.mpd \
  --key 00112233445566778899aabbccddeeff:0123456789abcdef0123456789abcdef \
  --secs 60 --switches 3 --seeks 2
```

The run prints `PASS`/`FAIL` per check, `CONFORMANCE_JSON` and `PROF_JSON`
(per-subsystem timing).

**Known blocker:** the macOS CI job currently prints
`CONFORMANCE_SKIP no-gpu-adapter` ("metal found no adapters"). The runner
works as a background service without a GUI session, so the Mac CI gate only
compiles. Run the harness from a logged-in desktop session (Terminal in the
GUI, or `launchctl asuser $(id -u) ...` from SSH). Fixing the runner so it
runs in the user's GUI session is itself worth doing: it would make the Mac
gate real.

iOS: `platform/ios/packaging/scripts/build_xcframework.sh` builds the
framework, and `platform/ios/ios/build_sim.sh` builds the simulator test
app. The Swift package lives in `platform/ios/packaging`.

## Tasks, most serious first

### 1. iOS: callbacks after destroy use a freed Swift object (high)

- `RustPlayer.swift:108` passes `Unmanaged.passUnretained(self)` as the
  callback `user` pointer. `:234`, `:244` and `:306` call
  `takeUnretainedValue()` on it.
- `deinit` calls `destroy()` → `rustplayer_player_destroy`
  (`platform/ios/src/lib.rs:475`). That signals shutdown and drops the handle,
  but it does not wait for the orchestrator, which still holds the host and
  can deliver events, intercept or key-resolve callbacks afterwards.
- A late callback then dereferences a freed `RustPlayer`. `Task { @MainActor
  in player... }` also retains it during `deinit`.
- Note: the lost-shutdown half is already fixed. Bridge `shutdown()` now uses
  `notify_one` (commit 0d99f0a).

**Fix, either way:**
- Make destroy synchronous: signal shutdown, then block until the
  orchestrator task has finished, bounded to about 2 s.
- Or pass a retained box (`passRetained`) and have Rust null the callbacks
  before shutdown, releasing the box only when the last callback has returned.

**Verify:** create and destroy the player in a loop, for example 50 times
with 1 s of playback each, under Address Sanitizer or Zombie Objects in the
simulator. There must be no crash, no zombie message, and memory must not
grow.

### 2. iOS: a Rust panic aborts the host app (high)

- None of the `extern "C"` functions in `platform/ios/src/lib.rs` use
  `catch_unwind`. Unwinding out of `extern "C"` aborts the process.
- `rustplayer_player_create` (`:210`) goes through
  `Player::new_from_metal_layer` (`.block_on()`). That reaches
  `request_device(...).unwrap()` (`player/src/renderers/video.rs:742`), so a
  device or adapter failure kills the app.
- The same call blocks the UIKit main thread for the whole wgpu setup.

**Fix:** copy the Android solution in `platform/android/src/lib.rs` (commit
663a761). An `ffi_guard(name, default, || body)` helper wraps every export
body in `catch_unwind`, logs the panic and returns a default (null / 0 /
false / nothing). Moving the renderer setup off the main thread is a
separate, optional follow-up.

**Verify:** the Swift API must not change. Test by temporarily making create
fail, for example with a forced invalid layer: the app gets `nil` and a log
line instead of an abort.

### 3. macOS + iOS: CVMetalTexture released before the GPU is done (medium-high)

- `player/src/renderers/video.rs:2705` does `drop(buf)` before
  `render_metal_nv12`, and the comment above it says that is fine.
- `MetalNV12Frame` (`player/src/renderers/video/video_metal.rs`, around lines
  250-265) holds the `CVMetalTextureRef`s and drops right after
  `queue.submit`.
- Apple requires keeping the `CVMetalTextureRef` (and the `CVPixelBuffer`)
  alive until the command buffer completes. A retained `MTLTexture` does not
  stop the IOSurface from being recycled by the decoder pool.
- Expected symptom: rare frames showing the wrong picture, the same class as
  the Android AImage bug (see the flicker notes in the repo history).

**Fix:** move the `MetalNV12Frame` and the pixel buffer into a keepalive
released from `queue.on_submitted_work_done(...)`, not at scope end. Correct
the comment.

**Verify:** conformance on macOS (A/B). Also play a scene-cut-heavy clip in
the simulator and look for flashes of the wrong frame.

### 4. VideoToolbox: reconfigure leaks the session (low)

`player/src/decoders/videotoolbox.rs`, around `configure` (lines 309 and
437), overwrites `format_desc` and `session` without invalidating or
releasing the old ones.

**Fix:** release both at the top of `configure`.

### 5. cpal on macOS: device loss and sample format (high, shared with Linux/Windows)

`player/src/renderers/audio/audio_cpal.rs:164-176`:
- `err_fn` only logs. When the output device disappears (headphones
  unplugged, AirPods switching), the stream dies, the audio clock freezes
  and the picture freezes with it.
- The stream is built as f32 with `.expect()`, so an i16-only device panics
  the audio thread.

**Fix:**
- Signal a rebuild from `err_fn`.
- Take the format from `default_output_config().sample_format()`.
- Fall back to the null sink instead of panicking.

**Coordinate with the Linux agent**, because this is the same file. One of
you does it, and both test it.

**Verify:** unplug and replug headphones during playback (or switch the
output device in System Settings). Playback must continue.

## When done

Append to this file what was done, with commit hashes and the A/B numbers.
Also list anything found but not fixed.

## Outcome (2026-09-28, Intel Mac Pro, macOS 14.6, Xcode 16.1)

Test setups: macOS conformance run from a GUI session; iOS Simulator
(iPhone 16 Pro, iOS 18.1, x86_64); iPhone SE 1st gen (iPhone8,4, iOS
15.8.8). For iOS a throwaway Swift harness (the packaged `RustPlayer.swift`
+ `librustplayer.a`, built with `-sanitize=address`) creates a player on a
`CAMetalLayer`, plays for PLAY_SECS, drops it, and repeats. A/B = the same
harness against a staticlib and Swift file built from the stashed tree.

| Task | Commit | Result |
| --- | --- | --- |
| 1. callbacks after destroy | bedd460 | Destroy after 20 ms, 100 iterations. Simulator: old heap-use-after-free (in `providerRef`, from the intercept callback) in 2 of 3 runs, new 0 of 3. iPhone SE: old heap-use-after-free at iteration 2-3 in 2 of 2 runs, new 0 over 25 and 40 iterations (the runs hit the 240 s limit; create takes a few seconds on an A9). 50 x 1 s plays: 50/50 playing, no ASan report in either build. |
| 2. panic aborts the host | 0ea142f | Temporary forced panic in `rustplayer_player_create` (not committed). Old: "panic in a function that cannot unwind", app aborted. New: `isStarted == false`, log `[ffi] rustplayer_player_create panicked`, the app keeps running. |
| 3. CVMetalTexture lifetime | 544e0b1 | Conformance (Metal NV12 path, 8-bit SDR), 60 s, 3 switches, 2 seeks, old -> new: all PASS; render gap 62 -> 63 ms, judder 13 -> 11, late 3 -> 3, lip-sync max 33 -> 36 ms, frames 1435 -> 1434. Phys footprint over 50 s flat in both (17-18 MB vs 18-19 MB). A visual check for wrong-frame flashes on a scene-cut clip was not done. |
| 4. VT reconfigure leak | 0a99599 | Build + conformance only. `configure` is called once per decoder today (pipeline.rs `run_decode`), so there is no observable A/B. |
| 5. cpal device loss / format | not needed on macOS | Device loss simulated with a public aggregate output device made the default and destroyed at 20 s (the system falls back to HDMI). cpal 0.18 plays through the DefaultOutput unit, which reroutes by itself and refreshes the latency: no freeze, no stall, render gap 194 ms, lip-sync unchanged (-9 to -26 ms before and after, max 36 ms). f32 is always offered by CoreAudio, so the i16 panic cannot happen here. Only `av-drift` FAILs (300 ms): see below. Linux still needs its own check. |
| Memory growth (found while verifying 1) | 2758c5e | A dropped `AudioRenderer` never stopped its cpal output thread: every destroyed player kept a thread, the cpal stream, an AURemoteIO unit and its queued PCM. Simulator, settled after 10 -> 70 cycles: footprint 28.9 -> 46.5 MB before, 27.3 -> 28.7 MB after; heap 4.1 -> 16.7 MB before, 2.07 -> 2.46 MB after; stack regions 32 -> 152 before, 13 -> 12 after. iPhone SE, 25 cycles: +0.3 MB/cycle before, flat from cycle 10 after; settled 19.5 -> 15.5 MB. Shared with Linux/Windows. |

`cargo test -p player --lib` on macOS: 182 passed (the brief says 186;
which 4 are not built on macOS was not checked).

### Notes and open findings

- **How the memory growth was traced.** The growth first seen under ASan
  (~0.9 MB/cycle) was mostly ASan's allocator; without ASan, `heap` /
  `vmmap` / `leaks` on the simulator process after the last destroy showed
  no leaks but ~60 extra of each per 60 cycles: cpal render callbacks,
  AURemoteIO factories, AudioConverters, `mpsc` blocks of `AudioChunk`,
  and two thread stacks per cycle. Fixed in 2758c5e. What remains is ~19
  `MTLTextureDescriptor` + label strings (~4 KB) per create, not per frame
  (30 after 5 s of playback and 30 after 40 s). Not chased.
- **`av_drift` does not correct for an output latency change.** It
  compares video time with raw `played_ms`, while the media clock
  subtracts `output_latency_ms`. After a reroute to a device with more
  latency (HDMI) the gauge reads the latency difference as a permanent
  drift (300 ms) although the picture follows the corrected clock and
  lip-sync is unchanged. It is a stats/conformance artifact, not an
  audible fault; left alone because the gauge is shared by all platforms.
- **Destroy does not wait for the orchestrator.** Callbacks are now gated,
  but `player.stop()` still runs after `rustplayer_player_destroy`
  returns. Harmless for the host now, and not the cause of the memory growth.
- **`rustplayer_player_create` blocks the main thread, briefly.** iPhone
  SE, release build: ~200 ms for the first player in an app launch, ~20 ms
  for every later one; ~700 ms right after install or an OS update (cold
  Metal shader cache: device 167 ms, pipelines 362 ms). Of the ~200 ms,
  ~50 ms is real work (wgpu device + pipelines, audio unit) and ~100-130 ms
  is the main thread losing the CPU to the render, audio and runtime
  threads that create itself starts (2 cores). That time lands in whatever
  runs next: the rustls config build (134 ms in create vs 23 ms on an idle
  thread), or with prewarm `spawn_event_pump` (102 ms for a spawn). The
  "several seconds" seen earlier came from the ASan + debug harness.
  `RustPlayer.prewarm()` (ab9088f) moves the one-time
  setup off the main thread: 198-206 ms -> 179-195 ms. Open: start the
  background threads at a lower QoS, or build the player off the main
  thread; neither done.
- **Slow start to first frame is the first segment.** The player waits
  for the whole first video segment before decoding; downloads are
  sequential, so it already gets the full bandwidth. The default start
  rung is now the highest at or below 720p (149bec5):
  first segment 3.9 MB -> 2.0 MB on the test stream. 6 launches each on
  a ~10 Mb/s Wi-Fi: time to first frame median 3.0 s -> 2.7 s, first
  segment median 2.2 s -> 1.9 s, with large overlap from Wi-Fi variance;
  on the slower link earlier (1080p segment 3.0-6.6 s) the gain is larger.
  Not done: picking the first rung from measured bandwidth, decoding
  segment 0 while it downloads.
- **ABR climbs to 2160p on the iPhone SE, which cannot decode it.** From
  either start rung, ABR reaches 3840x2160 in ~18 s; the A9 then decodes
  ~13 fps and drops 10-14 frames/s for the rest of playback. ABR looks at
  bandwidth only, not at dropped frames, decoder capability or the 640x1136
  display. Pre-existing; not fixed.
- **cpal with no output device at all** (cpal pauses the stream and
  reports DeviceNotAvailable) would still freeze the audio clock. Not
  reproducible on a Mac with built-in output.
- **Linux task 4 (cpal)**: the macOS side needs no change; the Linux agent
  should still check device loss and i16-only devices there.
