# platform/web — the browser shell

The same engine (`player`) and bridge core (`bridge`) as Android and iOS,
compiled to `wasm32-unknown-unknown` and exposed to a page through
wasm-bindgen as the `RustPlayer` class. What the platform provides:

| engine seam        | browser backend                                                |
|--------------------|----------------------------------------------------------------|
| video decode       | WebCodecs `VideoDecoder` (`player/src/decoders/webcodecs.rs`)   |
| audio decode       | WebCodecs `AudioDecoder` (same file)                            |
| video render       | wgpu on WebGPU (WebGL2 fallback, SDR only) into a `<canvas>`   |
| audio output       | Web Audio `AudioWorkletNode` (`renderers/audio/audio_web.rs`; processor embedded, Blob URL) |
| network            | reqwest over `fetch` (CORS applies — the CDN must allow the page origin and the `Range` header) |
| async runtime      | `player::rt` over wasm-bindgen-futures + `setTimeout` (`player/src/rt/web.rs`) |

Decoded frames are copied out of the `VideoFrame` into CPU planes and
uploaded as two textures, so the NV12 / P010 shaders, the HDR tonemap and
the subtitle overlay are the shared ones. Everything runs on the page's
main thread (no `SharedArrayBuffer` / atomics needed).

## Build & run

```powershell
./build.ps1                 # dev build → www/pkg/
./build.ps1 -Serve          # …and serve www/ on http://localhost:8080/
./build.ps1 -Profile release
```

Open the page in a browser with WebCodecs HEVC support (Chrome / Edge,
Safari 16.4+; Firefox has no HEVC in WebCodecs) and press **Start**. The URL
box is pre-filled with the bundled test stream and its ClearKeys.
`?licence=<url>` makes the demo fetch the keys **wrapped** from that endpoint
instead (`docs/CLEARKEY_WRAPPED_LICENCE.md`); `scripts/wrapped_licence_mock.py`
serves the test keys that way on `http://localhost:8090/licence/wrapped`.

## Consuming the published package

Published like the Android AAR and the iOS XCFramework: a tag `web-vX.Y.Z`
runs `.github/workflows/publish-web.yml`, which publishes
`@preclikos/rustplayer@X.Y.Z` to GitHub Packages (npm) and attaches the same
tarball to the release. Consumers compile no Rust:

```
# .npmrc — GitHub Packages needs a token with read:packages
@preclikos:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_TOKEN}
```
```
npm install @preclikos/rustplayer
```
```js
import init, { RustPlayer } from '@preclikos/rustplayer';
await init();                 // fetches rustplayer_bg.wasm next to the module
```

The package is the wasm-pack `--target web` output (`rustplayer.js` +
`rustplayer_bg.wasm` + `.d.ts`), so it works from a plain `<script
type="module">` and from bundlers that resolve `new URL(..., import.meta.url)`
assets (Vite, webpack 5). The page must be served with the wasm as
`application/wasm` and, for WebCodecs/WebGPU, over HTTPS or localhost.

## Embedding

```html
<script type="module">
import init, { RustPlayer } from './pkg/rustplayer.js';
await init();
const player = await RustPlayer.create(canvas, manifestUrl, {
  onEvent(json) { /* unified event JSON — see bridge::event_to_json */ },
  async resolveKey(kidHex) { return keyHex; },          // ClearKey lookup (raw keys)
  async intercept(url, kind) { return { url, headers: [['Authorization', '…']] }; }, // optional
}, { clearKeys: { kidHex: keyHex }, startPositionMs: 0, autoSelectSubtitle: false });
// Wrapped keys instead of raw ones (docs/CLEARKEY_WRAPPED_LICENCE.md): the player
// POSTs to the endpoint itself (headers via intercept(url, "license")) and unwraps
// each key into a non-extractable WebCrypto key — no key bytes ever reach JS/wasm.
//   { wrappedLicence: 'https://api.example/licence/wrapped' }        // or { url, info }
player.play(); player.pause(); player.seekMs(ms); player.setVolume(0.5);
JSON.parse(player.tracksJson()); player.setVideoTrack(adapt, repr); player.setVideoAuto();
// setVideoTrack is the manual switch: immediate, locks ABR to manual. setVideoTrackSoft
// is the ABR-style seamless swap (next segment boundary) — a test hook, not a UI control.
player.setAudioTrack(adapt, repr); player.setSubtitleTrack(adapt, repr); player.clearSubtitles();
player.resize(w, h);   // drawing-buffer size in device pixels
player.shutdown(); player.free();
</script>
```

`create` must run inside a user gesture (a click handler), otherwise the
browser keeps the `AudioContext` suspended.

**End of stream.** The engine emits `{"type":"end_of_stream"}` and stops; it
never auto-advances. The host decides: for a "next episode" call `shutdown()`
+ `free()` and `create` the next manifest (the demo page does this for a
whitespace/comma-separated URL list and closes the player after the last
one); for a replay control call `play()` (restarts from the beginning) or
`seekMs(ms)` (restarts at that position) — before this, both were ignored
after the end.

## HDR

The browser converts every `VideoFrame` to RGB itself before any of our
shaders run, and hardware HEVC frames are opaque (no plane readout). For PQ
content Chrome was measured (synthetic 10-bit ramp + colour patches, then
pixel-for-pixel on real HDR10 content) to apply the BT.2020 → BT.709 matrix to
the still PQ-encoded values and sRGB-encode them — no tone-map at all, which
is why HDR looked washed out and lifted.

Both steps are invertible, so the renderer keeps the frame on the GPU
(`copyExternalImageToTexture` into rgba16float), undoes the conversion in the
shader (`shader_chrome_inverse.wgsl`) and runs the engine's own PQ → SDR
tonemap and frame peak/average detection — the same math and numbers as the
native players. At start-up the renderer verifies the browser's conversion on
a synthetic frame (one 32 KiB readback, not video); if a browser behaves
differently the engine tonemap is disabled and `hdr: "auto"` falls back to SDR
representations only.

- `hdr: "auto"` (default): HDR representations through the engine tonemap when
  verified, else SDR representations only.
- `hdr: "sdr"`: SDR representations only.
- `hdr: "browser"`: HDR representations as the browser converts them
  (comparison; the washed-out look).

## Known limits (first cut)

- HEVC only, like the other platforms; codec support is whatever the
  browser's WebCodecs exposes (AC-3 / E-AC-3 audio is Safari-only in practice).
- Frames are presented on the display's vsync: the sync loop waits for
  `requestAnimationFrame` ticks and draws each frame in the tick whose
  upcoming vsync is nearest its clock time (what the `<video>` element and
  the W3C WebCodecs sample player do), one frame per tick. On a 60 Hz display
  24 fps content shows the regular 3:2 cadence; on a display or browser
  limited to 30 fps (Chrome's energy saver, a 30 Hz panel) it is 1:2 — check
  that before reading judder from the stats. A hidden tab gets no ticks, so
  video advances on a 250 ms fallback (audio keeps playing) until it is
  visible again.
- Audio output follows the system output: when the browser's destination
  offers six or more channels (OS output configured as 5.1/7.1) the engine
  opens a 6-channel discrete output and 5.1 tracks play as 5.1 PCM; on a
  stereo output they fold down (ITU-R BS.775). No bitstream passthrough in a
  browser — there is no API for it.
- Frames go GPU→GPU (`copyExternalImageToTexture`), one copy per frame on
  the GPU; `importExternalTexture` (no copy) would need the wgpu webgpu
  backend's external-texture path, which is `unimplemented!` upstream.
- Representations the browser reports as undecodable (WebCodecs
  `isConfigSupported`) are removed from the track tree before selection; the
  probe is the browser's own answer, so a codec it mis-reports stays hidden.
- No DRM beyond ClearKey (EME requires MSE, a different architecture).
