# HDR → SDR Tonemap Tuning

How the player converts HDR10 content into something a regular SDR display
can show, and which knobs the host UI can expose so users can dial in the
look they want.

## What the player does

When the active video representation is HEVC Main 10 (HDR10), the
decoder produces a P010 surface in BT.2020 + PQ colour space. The
player's fragment shader (`player/src/renderers/shader_hdr.wgsl`) then:

1. Decodes the PQ EOTF → linear cd/m² (up to 10 000 nits).
2. Normalises by a **reference white** so the bulk of HDR content
   lands in the ACES tonemap's near-linear range.
3. Runs the ACES filmic tonemap (Narkowicz fit) — soft shoulder for
   highlights, gentle toe for blacks.
4. Applies a **shadow lift gamma** to recover midtone visibility that
   ACES would otherwise crush.
5. Maps BT.2020 → BT.709 primaries.
6. Encodes sRGB OETF for the display.

The host controls steps 2 and 4 at runtime by pushing an
`HdrTonemapParams` value into `Player::set_hdr_tonemap`.

## Where this works

| Platform        | Tunable? | Why                                                |
|-----------------|----------|----------------------------------------------------|
| Windows         | ✅       | Player's own shader runs the tonemap (DX12 P010)   |
| Linux           | ✅       | Same (Vulkan VAAPI P010)                           |
| macOS / iOS     | ❌       | VideoToolbox tonemaps internally before our shader |
| Android         | ❌       | No HDR path wired yet                              |

Check `PlayerCapabilities::hdr_tonemap_tunable` at startup and **hide
the settings UI entirely** when it's `false`. The API still accepts
the call (no-op) so cross-platform host code doesn't need cfg gates,
but the user can't see the effect on platforms where the OS owns the
conversion.

## The two parameters

### `reference_white_nits: f32`

What HDR input nit level the tonemap treats as "SDR diffuse white"
(the curve's input domain `1.0`).

- **Lower** = brighter overall output. Most HDR content sits in the
  tonemap's near-linear range; less compression.
- **Higher** = dimmer overall output. Bigger headroom for highlights,
  but typical content (a face under indoor light, ~50 nits) drops
  into the steeper "shadow" part of the curve and looks dim.

| Value  | Look                                                          |
|--------|---------------------------------------------------------------|
| 40     | Bright living-room: SDR diffuse white maps near peak output.  |
| 60     | Slightly brighter than reference, low contrast.               |
| 100    | Strict BT.2390. Brighter than most real SDR *grades* look.    |
| 200    | "Cinema" look — much darker tonemap, lots of highlight headroom. |
| 400    | **Default** — API ceiling. Matches the (dark) SDR grade of the test content; see *Measured fit* below. |

Range accepted by the API: **[10, 400]** (clamped). Outside that the
output goes degenerate (divide-by-near-zero for very small, near-black
output for very large).

### `shadow_lift_gamma: f32`

Applied as `pow(tonemap_output, gamma)` after ACES. Affects
shadows + midtones most, highlights almost not at all (high
values already saturate near 1.0 so `pow(x, <1)` barely moves them).

- **<1** = lift shadows + midtones. Less contrasty / more
  "TV-like" appearance. Recovers detail crushed by the ACES toe.
- **= 1** = identity. Pure ACES output.
- **>1** = deepen shadows. More "cinematic" / "punchy" look.

| Value  | Look                                                                  |
|--------|-----------------------------------------------------------------------|
| 0.75   | Strong lift — flat, very TV-like. Useful for very dark mastering.     |
| 0.85   | Moderate lift. Recovers midtone detail without flatness.              |
| 0.95   | **Default** — mild lift, essentially neutral (near pure ACES).        |
| 1.00   | Pure ACES (no lift). Cinematic, deeper blacks.                        |
| 1.15   | Inverse lift — deeper shadows, higher contrast.                       |

Range accepted by the API: **[0.5, 1.5]** (clamped).

## Suggested presets

Host UIs that want a simple preset selector instead of two sliders can
use these:

| Preset name    | `reference_white_nits` | `shadow_lift_gamma` | When to pick                     |
|----------------|------------------------|---------------------|-----------------------------------|
| Brighter       | 50                     | 0.80                | Living-room TV, ambient light     |
| Balanced       | 100                    | 0.95                | Brighter mid-ground, less extreme |
| **SDR-match**  | 400                    | 0.95                | Default — tracks the SDR grade    |
| Punchy         | 200                    | 1.10                | Dark with deeper shadows          |
| Reference      | 100                    | 1.00                | Strict BT.2390 mapping            |

The names are suggestions — pick whatever fits the host UI's voice.
What matters is that the player sees the two `f32`s.

## API usage

```rust
use player::{HdrTonemapParams, Player};

let caps = player::capabilities();
if caps.hdr_tonemap_tunable {
    // Show HDR tonemap controls in the settings UI.

    let user_choice = HdrTonemapParams {
        reference_white_nits: 50.0,
        shadow_lift_gamma: 0.80,
    };
    player.set_hdr_tonemap(user_choice);
}
```

Or with `Default`:

```rust
player.set_hdr_tonemap(HdrTonemapParams::default()); // = ::DEFAULT
```

## Persistence

The **player does not persist** the value across runs. It only holds
the current setting in memory for the lifetime of the `Player`
instance. The host must:

1. Load the user's saved preference from its own settings storage at
   app launch.
2. Call `set_hdr_tonemap(loaded_params)` once after `Player` is
   created (even before `open_url`).
3. Save the new value to its own storage when the user changes it.

The clamp range above is safe to round-trip through any host
settings format — `f32` JSON / proto / config-toml are all fine.

## Timing

- `set_hdr_tonemap` is **safe to call from any thread, at any time**.
- The change takes effect on **the next P010 frame rendered** —
  roughly one frame of latency. Fluid for slider UIs.
- The setter is **cheap** (one `Arc` swap, no GPU calls). Spamming
  it at slider drag rates is fine.
- The player does not expose a getter — the host already has the
  value it pushed (and persists it in its own settings storage, see
  Persistence above).

## Defaults

Both `HdrTonemapParams::DEFAULT` and the shader's startup-time
uniform initialiser use:

- `reference_white_nits = 400.0`
- `shadow_lift_gamma   = 0.95`

These are calibrated to approximate the SDR (BT.709) grade of the test
content (see *Measured fit* below). If the host doesn't call
`set_hdr_tonemap` at all, this is what the user sees.

## Measured fit against the SDR grade

The test stream (`preclikos.cz/examples/encrypted`) ships the same
content as both an SDR BT.709 representation and an HDR10 PQ/BT.2020
(Dolby Vision 8.1) representation. That gives matched SDR/HDR frames of
the identical shot, so we can measure — not eyeball — which knob values
make our HDR→SDR tonemap land on the SDR grade.

Method: an offline NumPy replica of this shader (same PQ EOTF → ACES →
gamma → BT.2020→709 → sRGB chain) was run on the *raw PQ* HDR frame and
compared, per pixel (letterbox masked), to the BT.709-decoded SDR frame.
Four shots (landscape + three portraits) were swept over the full clamp
domain and optimised jointly.

| Params          | Joint RMSE vs SDR | Note                                   |
|-----------------|-------------------|----------------------------------------|
| 60 / 0.85 (old) | 0.280             | ~+0.30 per-channel bias — over-exposed |
| 400 / 0.95      | 0.096             | 66 % closer; chosen default            |

Every individual shot's optimum landed at `reference_white_nits = 400`
(the clamp ceiling): the SDR grade is *darker / more contrasty* than the
ACES tonemap produces at any lower reference white, so the true optimum
is pinned by the [10, 400] clamp. `shadow_lift_gamma` is only weakly
constrained (per-shot optima 0.7–1.5); 0.95 is the joint best.

Caveats:
- The two knobs are achromatic (exposure + contrast). They **cannot**
  fix the residual white-balance / saturation gap — the SDR grade is a
  touch warmer + more saturated than two-knob tonemapping can reach.
- The fit is calibrated to *this title's* SDR grade. Content graded
  brighter may look dim at 400; that is what `set_hdr_tonemap` is for.
- Because the optimum is clamped, fully matching darker SDR grades would
  need either a higher `reference_white_nits` ceiling or a different
  tone curve — a larger change than these two runtime knobs allow.
