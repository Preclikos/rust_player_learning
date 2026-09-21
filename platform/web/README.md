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
  async resolveKey(kidHex) { return keyHex; },          // ClearKey lookup
  async intercept(url, kind) { return { url, headers: [['Authorization', '…']] }; }, // optional
}, { clearKeys: { kidHex: keyHex }, startPositionMs: 0, autoSelectSubtitle: false });
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

## HDR

The browser converts every `VideoFrame` to RGB itself and tone-maps PQ
content with its own curve before any of our shaders run (measured in
Chrome: `importExternalTexture` and `copyExternalImageToTexture` return the
same compressed values regardless of the canvas `toneMapping` mode; hardware
HEVC frames are opaque, so the planes cannot be read out). The engine's
PQ → SDR mapping (`shader_hdr.wgsl`) is therefore unreachable in the browser.

That mapping was calibrated to land on the SDR ladder's displayed values, so
the default `hdr: "sdr"` — SDR representations only — shows the same picture
as the native players. `hdr: "browser"` allows the HDR rungs with Chrome's
tone-map (different look, higher resolution ceiling on the test fixture).

## Known limits (first cut)

- HEVC only, like the other platforms; codec support is whatever the
  browser's WebCodecs exposes (AC-3 / E-AC-3 audio is Safari-only in practice).
- Frames go GPU→GPU (`copyExternalImageToTexture`), one copy per frame on
  the GPU; `importExternalTexture` (no copy) would need the wgpu webgpu
  backend's external-texture path, which is `unimplemented!` upstream.
- Representations the browser reports as undecodable (WebCodecs
  `isConfigSupported`) are removed from the track tree before selection; the
  probe is the browser's own answer, so a codec it mis-reports stays hidden.
- No DRM beyond ClearKey (EME requires MSE, a different architecture).
