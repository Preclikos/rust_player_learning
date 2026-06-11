# HDR → SDR Tonemap

How the player converts HDR10 content into something a regular SDR display
can show, what the host UI can tune, and **what changed for UI integrations**
in the tonemap_opencl rework.

## ⚠ Breaking change for host UIs (tonemap_opencl rework)

The tonemap was replaced wholesale. The old ACES pipeline and its two knobs
(`reference_white_nits`, `shadow_lift_gamma`) **no longer exist** — the
fields are gone from `HdrTonemapParams`, so host code that constructed it
field-by-field will not compile until updated.

| | Old | New |
|---|---|---|
| Algorithm | ACES filmic (Narkowicz), static | FFmpeg `tonemap_opencl` port: Möbius + frame peak/average detection |
| Output transfer | sRGB OETF | BT.1886-inverse (pure 1/2.4 power), like the filter's `t=bt709` |
| Fields | `reference_white_nits`, `shadow_lift_gamma` | `tone_param`, `desat`, `peak`, `scene_threshold` |
| Default look | hand-calibrated approximation of the SDR grade | **identical colour profile to the SDR ladder's reference transcode** |
| Brightness adaptation | none | per-scene, 63-frame rolling window (like the offline transcode) |

**What a UI app must do:**

1. **Delete any persisted `reference_white_nits` / `shadow_lift_gamma`
   values and their sliders/presets.** There is no mapping from old to new
   values — the algorithms are unrelated. Don't try to migrate numbers.
2. **The recommended setting is: don't call `set_hdr_tonemap` at all.**
   `HdrTonemapParams::DEFAULT` reproduces the exact command the SDR ladder
   was transcoded with, which is the whole point — an ABR switch between an
   HDR and an SDR representation no longer changes the picture's colour
   grade. Only expose the new knobs if you genuinely want a "custom HDR
   look" feature.
3. `PlayerCapabilities::hdr_tonemap_tunable` works as before — keep hiding
   any tonemap UI when it's `false`.
4. `set_hdr_tonemap` call semantics are unchanged: any thread, any time,
   takes effect next frame, player does not persist.

## What the player does

When the active video representation is HEVC Main 10 (HDR10), the decoder
produces a P010 surface in BT.2020 + PQ. The player then runs a faithful
WGSL port of FFmpeg's `tonemap_opencl` filter, configured like the
reference transcode of this project's SDR ladder:

```
tonemap_opencl=tonemap=mobius:param=0.01:desat=0:r=tv:p=bt709:t=bt709:m=bt709
```

Per frame, entirely on the GPU (zero-copy — the imported decoder texture is
read in place, statistics live in a 536-byte GPU buffer, nothing is ever
read back to the CPU):

1. Three small compute passes (`shader_hdr_detect.wgsl`) update the
   frame peak/average statistics — the filter's libplacebo-derived
   `detect_peak_avg`: per-workgroup average signal, 63-frame rolling
   window, scene-change reset.
2. The fragment shader (`shader_hdr.wgsl`) decodes limited-range
   BT.2020-NCL Y'CbCr, linearises with the ST 2084 (PQ) EOTF, converts
   primaries BT.2020 → BT.709, tonemaps the max RGB component with the
   Möbius curve scaled by the detected peak/average, and encodes with the
   filter's bt709 "delinearize" (pure 1/2.4 power).

The offline transcode packs that result into BT.709 TV-range NV12; the
player's SDR shader decodes such files straight back to the same R'G'B'.
Net effect: **playing the HDR representation and playing the
ffmpeg-transcoded SDR representation of the same content display the same
image** (up to the transcode's 4:2:0 chroma subsampling and encoder
quantisation).

The SDR NV12 shader was fixed as part of this work — it previously
under-scaled luma (white at ~86 %), skipped the chroma range expansion and
used BT.601-ish coefficients, so SDR representations rendered darker and
duller than the correctly-decoded grade on Windows/Linux/Android-Vulkan
paths. It now does an exact limited-range BT.709 decode.

## Where this works

| Platform        | Tunable? | Why                                                |
|-----------------|----------|----------------------------------------------------|
| Windows         | ✅       | Player's own shader runs the tonemap (DX12 P010)   |
| Linux           | ✅       | Same (Vulkan VAAPI P010)                           |
| macOS / iOS     | ❌       | VideoToolbox tonemaps internally before our shader |
| Android         | ❌       | No HDR path wired yet                              |

Check `PlayerCapabilities::hdr_tonemap_tunable` at startup and **hide the
settings UI entirely** when it's `false`. The API still accepts the call
(no-op) so cross-platform host code doesn't need cfg gates.

## The parameters

All four are 1:1 mirrors of the `tonemap_opencl` options of the same name.
Defaults (= `HdrTonemapParams::DEFAULT`) reproduce the reference transcode.

### `tone_param: f32` — default `0.01`

The Möbius knee point `j` (the filter's `param`). Signal below `j` passes
through linearly; above it is compressed so the source peak lands at 1.0.
Smaller = softer/flatter highlights, larger = more linear with a harder
shoulder. FFmpeg's own default is 0.3; the reference transcode uses 0.01.
Clamped to **[0.001, 0.999]**.

### `desat: f32` — default `0.0` (off)

Highlight desaturation strength (the filter's `desat`). 0 disables — the
reference transcode's choice. FFmpeg's own default is 0.5; values around
there bleach very bright highlights toward white instead of letting them
clip saturated. Clamped to **[0, 100]**.

### `peak: f32` — default `0.0` (auto)

Source signal peak override in 100-nit units (the filter's `peak`: 100.0 =
10 000 nits). 0 = auto, i.e. the PQ untagged-source fallback of 100. Only
seeds the very first frame of a scene — the frame detection takes over
immediately — so leave it at 0 unless you're debugging. Clamped to
**[1, 200]** when non-zero.

### `scene_threshold: f32` — default `0.2`

Scene-change threshold of the brightness detection (the filter's
`threshold`). When a frame's average signal differs from the rolling
average by more than this, the 63-frame window resets so the curve adapts
instantly instead of fading over a second. 0 disables scene resets.
Clamped to **[0, 10]**.

## API usage

```rust
use player::{HdrTonemapParams, Player};

// Recommended: do nothing. DEFAULT == the reference transcode profile.

// Or explicitly (equivalent to the default):
player.set_hdr_tonemap(HdrTonemapParams::DEFAULT);

// Custom look, e.g. FFmpeg's stock mobius with highlight desaturation:
let caps = player::capabilities();
if caps.hdr_tonemap_tunable {
    player.set_hdr_tonemap(HdrTonemapParams {
        tone_param: 0.3,
        desat: 0.5,
        ..HdrTonemapParams::DEFAULT
    });
}
```

## Persistence

Unchanged: the **player does not persist** the value across runs. A host
that exposes custom values must round-trip them through its own settings
storage and call `set_hdr_tonemap` once per `Player` init. Hosts that stick
with the default need no persistence at all.

## Timing

- `set_hdr_tonemap` is **safe to call from any thread, at any time**.
- Takes effect on **the next P010 frame rendered** (~one frame of latency).
- The setter is **cheap** (one `Arc` swap, no GPU calls) — slider-drag
  rates are fine.
- No getter — the host owns the value it pushed.

## Fidelity notes (vs. the ffmpeg filter)

Intentional, visually irrelevant differences from `tonemap_opencl`:

- Chroma is sampled bilinearly; the filter uses nearest within its 2×2
  work-item quad. Spatial difference only — tone and colour are identical.
- The filter bakes its colour matrices into the kernel at 4 decimal places
  (`%.4f`); the shaders use the same matrices at full precision (≤ 5×10⁻⁵
  per coefficient apart).
- Mastering-display/MaxCLL metadata is not parsed, so the first frame
  seeds with the untagged-PQ peak (100) even when the stream carries
  metadata — the filter would seed with the tagged value. The detection
  replaces the seed from frame 2 onward either way.
- Detection statistics roll one frame later than the filter's (which
  folds detection into the tonemap kernel itself); the published values
  cover "previous frames only" in both cases.
