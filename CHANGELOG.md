# Changelog

One version line for all platforms (see `docs/RELEASING.md`). Desktop hosts
pin `desktop-vX.Y.Z`; Android, iOS and web consume the published packages.

## 0.2.18

### All platforms

- **`open_url` starts a fresh session on a reused `Player`.** The next item
  plays from the start (or from `set_start_position`, set after `open_url`)
  instead of from where the previous one stopped: picking tracks before the
  first frame no longer rebuilds at the previous stream's position,
  `position()` reads 0 right after `open_url`, a resume position parked by a
  failed previous stream is dropped, and track selections from the old
  manifest are cleared. Hosts that build a new `Player` per item (the
  Android, iOS and web shells) were not affected. See
  `PLAYER_INTEGRATION.md` §2.

### Tests and tooling

- Unit tests for a reused `Player` (offline manifest, null video sink).

## 0.2.17

### Desktop (Windows)

- **Vulkan renderer with zero-copy D3D11VA frames.** A desktop host can run
  the player on wgpu's Vulkan backend; decoding stays on D3D11VA. Decoded
  frames are copied into a small pool of shared textures, each imported into
  Vulkan once (`VK_KHR_external_memory_win32`), and the queue waits for the
  copy on the GPU through the D3D11 fence imported as a timeline semaphore
  (`VK_KHR_external_semaphore_win32`); drivers without the semaphore
  extension fall back to a CPU wait. DX12 stays the default and is unchanged.
  Hosts that create the wgpu device themselves (`Player::new_offscreen`)
  request `Features::VULKAN_EXTERNAL_MEMORY_WIN32` on Vulkan — see
  `PLAYER_INTEGRATION.md` §3.1.1.
- The D3D11VA decoder opens on the renderer's GPU on both backends (adapter
  LUID from DX12 or from `VkPhysicalDeviceIDProperties`).
- wgpu fork: `Queue::add_wait_semaphore` (Vulkan), and
  `VK_KHR_external_semaphore_win32` enabled when the driver has it.

### Android

- The 5 s audio-writer heartbeat and the playback-position-jump line log at
  debug level (verbose) instead of info.

### Tests and tooling

- GPU test of the D3D11 → Vulkan import (six NV12 frames through the pool,
  read back plane by plane, byte-exact).
- `examples/desktop-slint`: `--backend vulkan|dx12` (Vulkan by default on
  Windows).

## 0.2.10 – 0.2.16 (summary)

- **Subtitles (desktop, web, Apple):** one cue texture, reused and only grown,
  instead of a new texture for every cue size; GPU regression test.
- **Logs:** the periodic diagnostics (`[vsync] f#N`, `[watchdog gen]`,
  `[sidx]`, `[dec] seg done`) log at debug level, so host log files only
  get events.
- **`examples/desktop-slint`:** the in-app composition desktop hosts use —
  Slint (FemtoVG on wgpu) and the player on one shared device, frames handed
  over as `slint::Image::try_from`; UI-present gauge next to the player's
  stats, JSON summary per run (`--sub`, `--ui-load-ms`, `--no-play`).
- **`examples/desktop`:** `subfile <path> [offset_ms]` console command loads a
  sidecar `.vtt`/`.srt`.
- **CI:** the conformance run also triggers on `Cargo.lock` / `Cargo.toml`,
  and the release gate counts them — a dependency bump needs its own green
  run.
