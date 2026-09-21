# platform/web — the browser shell

The same engine (`player`) and bridge core (`bridge`) as Android and iOS,
compiled to `wasm32-unknown-unknown` and exposed to a page through
wasm-bindgen as the `RustPlayer` class. What the platform provides:

| engine seam        | browser backend                                                |
|--------------------|----------------------------------------------------------------|
| video decode       | WebCodecs `VideoDecoder` (`player/src/decoders/webcodecs.rs`)   |
| audio decode       | WebCodecs `AudioDecoder` (same file)                            |
| video render       | wgpu on WebGPU (WebGL2 fallback, SDR only) into a `<canvas>`   |
| audio output       | Web Audio `ScriptProcessorNode` (`renderers/audio/audio_web.rs`)|
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
JSON.parse(player.tracksJson()); player.setVideoTrackSoft(adapt, repr); player.setVideoAuto();
player.setAudioTrack(adapt, repr); player.setSubtitleTrack(adapt, repr); player.clearSubtitles();
player.resize(w, h);   // drawing-buffer size in device pixels
player.shutdown(); player.free();
</script>
```

`create` must run inside a user gesture (a click handler), otherwise the
browser keeps the `AudioContext` suspended.

## Known limits (first cut)

- HEVC only, like the other platforms; codec support is whatever the
  browser's WebCodecs exposes (AC-3 / E-AC-3 audio is Safari-only in practice).
- Frames take one CPU copy (`VideoFrame.copyTo`) per frame; 4K is heavy on
  the main thread. Zero-copy `VideoFrame → GPUExternalTexture` needs a wgpu
  addition.
- `ScriptProcessorNode` output; an `AudioWorklet` would lower latency and
  survive a busy main thread better.
- No DRM beyond ClearKey (EME requires MSE, a different architecture).
