# `app_shared::bridge` — product-consumer readiness

**Context.** The BlackZone TV app (`BlackZoneAndroidRust/rust-bridge`) wants to drop
its hand-rolled JNI orchestration and consume `app_shared::bridge` directly (a thin
`BlackZoneHost: BridgeHost` + `BridgeHandle`, like `app-android/src/lib.rs`). The
event protocol, provider hooks (`intercept` / `resolve_key`), teardown
(`stop().await` + bounded join), and reaching player knobs via `BridgeHandle::player()`
already fit. Three core gaps were test-shell-shaped and blocked full delegation —
**all three are now addressed** (player/bridge side). The app side migrates next.

Status: **resolved in `app-shared`** (`bridge.rs`, `lib.rs`). The `app-android` /
`app-ios` shells are unchanged in behaviour (they pass `StartConfig::default()`).

---

## Gap 1 — pre-`play()` config seam  ✅ RESOLVED

`bridge::start` now takes a `StartConfig`:

```rust
pub struct StartConfig {
    pub start_position: Option<Duration>, // resume; set_start_position() before play()
    pub audio_passthrough: Option<bool>,  // Some(b) = host's sink-gated decision; None = shell file-flag
    pub auto_select_subtitle: bool,       // false = host applies its own lang/forced policy after play
}
// Default { None, None, auto_select_subtitle: true } == previous shell behaviour.
```

`orchestrate()` applies `start_position` via `player.set_start_position(Some(..))` between
prepare and play, and threads `audio_passthrough` / `auto_select_subtitle` into the shared
`apply_default_tracks(player, tracks, passthrough_override, auto_select_subtitle)`.
`passthrough_override = None` keeps the env / `audio_passthrough.txt` flag; `Some(b)` forces it.

A product host passes `StartConfig { start_position: resume, audio_passthrough: Some(decision),
auto_select_subtitle: false }`. Video still defaults via `video_pref` for now; a host that
needs a specific starting rung can `change_video_track` after start (a richer
`BridgeHost::configure` hook — option (B) in the original analysis — remains the path if hosts
need full index-based pre-play track control).

### Gap 1c — default video pick is a test-fixture index → no playback on product content  ⛳ NEEDS FIX

Device-found migrating BlackZone TV onto the core: `apply_default_tracks` (`lib.rs:242`)
picks video with `video_pref().None => first_adapt.representations.get(5)` — index 5 is the
720p rung of the preclikos.cz test fixture. Product manifests have far fewer reps (e.g. 3:
1080p/720p/480p), so `get(5)` is `None` →
```
E app_shared: no video representation matches preference None (and default index 5 is missing)
E app_shared::bridge: play(): Video Track not set
```
`play()` then fails and **nothing plays**. And there is no host recovery: `play()` errors inside
`orchestrate()` before the host can `change_video_track`, and `video_pref()` only reads
`RUST_PLAYER_VIDEO` / a file under the *test app's* package dir — a product app can't set it
cleanly. (BlackZone has an INTERIM `std::env::set_var("RUST_PLAYER_VIDEO","0")` in its
`nativeStart` to force the first rep so it can test the migration — please make that unnecessary.)

**Ask:** make the `None` branch pick a product-safe default instead of the fixed index 5 —
e.g. the highest rep `≤ 1080p` (then ABR adapts), falling back to the first rep:
```rust
None => all().filter(|(_, r)| r.height <= 1080).max_by_key(|(_, r)| r.height)
              .or_else(|| first_adapt.representations.first().map(|r| (first_adapt, r))),
```
(or expose a `StartConfig.video_pref: Option<...>` so the host states it explicitly). The fixture
default can move to the shells' `StartConfig` so they keep index-5 behaviour.

### Gap 1b — `start_position` is absolute, but product resume is a percent  ✅ RESOLVED

**Landed:** `StartConfig` now also has `start_fraction: Option<f32>`; `orchestrate()` resolves it
against the real duration before play — `start_position.or_else(|| start_fraction.map(|f|
tracks.duration.mul_f32(f.clamp(0.0,1.0))))` — so absolute wins if both are set, else the fraction
is used. `Default` adds `start_fraction: None`, shells unchanged. The BlackZone host passes
`start_fraction: Some(progress/100.0)` for frame-accurate percent resume.

Original analysis (kept for context):

BlackZone (and most catalog apps) store resume as a **percent of duration**, and the host does
NOT know the exact media duration before play — its metadata `runtime` is in whole **minutes**
(`MediaItem.runtime`, rendered `runtime/60`h `runtime%60`m), so `runtime*60000*pct` rounds to
minute granularity → up to ~30 s off at high percents. The exact duration is only known inside
`orchestrate()` after `prepare()`/`get_tracks()` — which is exactly where `start_position` is
applied. So the host cannot fill `start_position: Option<Duration>` precisely.

**Ask:** add a fraction variant resolved against the real duration in `orchestrate()`:
```rust
pub struct StartConfig {
    pub start_position: Option<Duration>,  // absolute (kept)
    pub start_fraction: Option<f32>,       // 0.0..=1.0 of duration; resolved at play, wins if set
    pub audio_passthrough: Option<bool>,
    pub auto_select_subtitle: bool,
}
```
In `orchestrate`, before play: `let pos = config.start_position.or_else(|| config.start_fraction
.map(|f| duration.mul_f32(f.clamp(0.0, 1.0))));  player.set_start_position(pos);`. `Default` adds
`start_fraction: None` → shells unchanged. This mirrors what our old native bridge already did
(`resume_ms = tracks.duration * pct/100` computed inside the orchestration, pre-play) — it just
needs to live in the core now. Then the BlackZone host passes `start_fraction: Some(progress/100)`
and resume stays frame-accurate.

## Gap 2 — `forced` on text tracks  ✅ RESOLVED

`tracks_to_json` text entries now include `"forced": bool` (from
`TextAdaptation::is_forced()`), so language + forced subtitle selection is expressible:

```json
{"adapt":N,"repr":M,"id":..,"lang":"cs","forced":true,"codecs":"wvtt","bandwidth":..,"label":".."}
```

The schema stays **flat** (one entry per representation, `adapt`/`repr` indices into the
`set_*_track(adapt, repr)` calls). That flat shape is the contract — a consumer migrating
from the app's old nested `adaptIndex → representations[]` JSON adapts its parser.

## Gap 3 — dedicated video-size event  ✅ RESOLVED

The event pump synthesizes `{"type":"video_size","width","height"}` the first time (and
whenever) the rendered resolution appears/changes, derived from `Stats.current_resolution`.
Consumers shape their video plane off this instead of parsing the periodic `stats` event.
(`app-android`'s `RustPlayer` listens to `video_size` and `stats` for `onVideoSize`.)

---

## Constraints (held)
- `app-android` / `app-ios` shells compile and behave as before (`StartConfig::default()`).
- `forced` + `video_size` are additive — no break to existing parsers.

## After this lands — app side (separate work in BlackZoneAndroidRust)
- Add an `app-shared` git dep at this pin; ensure a single `player` crate (`cargo tree` shows
  one `player` at the rev — reuse the existing `player` rev pin).
- Rewrite `rust-bridge/src/lib.rs` to `BlackZoneHost: BridgeHost`:
  - `intercept` = bearer headers + `storageId:slug` resolve via the existing Kotlin upcalls
    (`headersFor` / `resolveLink`);
  - `resolve_key` = the ClearKey `/api/licence` POST;
  - `bridge::start(player, url, host, StartConfig { start_position, audio_passthrough:
    Some(sinkDecision), auto_select_subtitle: false })`.
  - Apply HDR / surface / AFR / subtitle-style / safe-inset via the `Player` before start or
    `handle.player()`.
- Switch the Kotlin event protocol from `onEvent(code,a,b,detail)` (EV_* ints) to the unified
  `onEvent(json)`; parse `type` in `PlaybackController.onNativeEventRaw`; drive video size from
  the `video_size` event; keep position polling.
- Subtitle selection: with `auto_select_subtitle: false`, pick by `lang` + `forced` from the
  tracks JSON after the first `playing`, then `set_subtitle_track(adapt, repr)`.
