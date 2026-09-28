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
| 5. cpal device loss / format | not done | Same file as the Linux brief's task 4. Left open so only one agent edits `audio_cpal.rs`; see below. |

`cargo test -p player --lib` on macOS: 182 passed (the brief's 186 counts
tests that do not build on macOS).

### Found, not fixed

- **Memory grows per create/destroy on iOS, in the old build too.** With
  ASan quarantine off, about 0.9 MB per 1 s create/destroy cycle in the
  simulator (78 -> 110 MB over 30 cycles old, 78 -> 101 MB over 20 new);
  on the iPhone SE the footprint also climbs across iterations. Not caused
  by these fixes; not yet traced (candidates: one wgpu instance/device per
  player, Metal pipeline caches, the global tokio runtime's tasks that
  outlive destroy).
- **Destroy does not wait for the orchestrator.** Callbacks are now gated,
  but `player.stop()` still runs after `rustplayer_player_destroy`
  returns. Harmless for the host now; relevant to the leak above.
- **`rustplayer_player_create` blocks the main thread** for the whole wgpu
  setup (several seconds on an iPhone SE). Optional follow-up from task 2.
- **Task 5 (cpal)** is still open. Decide who takes it with the Linux
  agent; on this Mac it can be verified by switching the output device in
  System Settings during the conformance run.
