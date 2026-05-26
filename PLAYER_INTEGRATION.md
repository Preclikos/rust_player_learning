# Player Integration Spec — request for `rust_player_learning`

> **Audience:** AI / engineer working on `rust_player_learning`.
> **Author:** BlackZone Console client team.
> **Status:** request — please review, comment, and implement.

This document describes what the `player` crate needs to expose so that a
real‑world streaming client (BlackZone Console — a Rust TUI) can drive it.
Everything below is additive: existing tests and the `app/` desktop binary
must keep working unchanged.

> ### ⚠ Important — provider‑agnostic only
>
> **Nothing in this document asks you to add provider‑specific code to
> the `player` crate.** No BlackZone strings, hostnames, URL patterns,
> license body formats, header names, regex, or business logic should
> appear in the player. The player should only expose generic
> primitives (interceptor trait, license resolver trait, event stream,
> IPC binary). All BlackZone‑specific behaviour (URL rewrites, bearer
> tokens, license POST body shape, etc.) is implemented in a **separate
> downstream crate** that consumes the player. References to BlackZone
> below appear only to explain why these generic primitives are shaped
> the way they are — they are **consumer context, not implementation
> tasks**.

---

## 1. Context

**Consumer:** `BlackZoneConsole` — cross‑platform Rust TUI (Windows / Linux /
macOS) using `ratatui`. It already authenticates via Keycloak, browses the
catalogue, and obtains a DASH manifest URL from
`GET https://api.blackzone.site/api/manifest/{type}/{id}`. Playback is
currently stubbed.

**What's missing in `player` for us to wire it up:**

1. We can't add `Authorization: Bearer <token>` to outgoing requests.
2. DASH segment URLs in the manifest use a pseudo‑URI form
   `<storageId>:<slug>` that must be resolved at request time via
   `GET /api/Link/{storageId}/{slug}` → `{validUntil, link}`, with the
   resolved link cached until `validUntil`. (Android does this via
   ExoPlayer's `ResolvingDataSource`.)
3. The stream is ClearKey‑encrypted with keys served from
   `POST /api/licence`. `set_clearkey(HashMap)` requires keys upfront —
   we need a callback fired when a new KID is encountered.
4. The TUI needs an event stream (state, position, buffer, bitrate, frame
   stats). Today only `position()` polling is exposed.

**Architecturally we'd like player to run in a separate process** (winit
window) talking to the TUI over stdio, so neither side can crash the
other. See §6.

---

## 2. Goals (priority order)

| Pri | Item | Section |
|---|---|---|
| **P0** | Centralised `HttpClient` injected everywhere | §3.1 |
| **P0** | `RequestInterceptor` trait + setter on `Player` | §3.2 |
| **P0** | `LicenseResolver` trait + setter on `Player` | §3.3 |
| **P1** | `PlayerEvent` enum + `Player::events()` watch channel | §4 |
| **P1** | `pause()` / `resume()` / `is_paused()` | §4.3 |
| **P1** | Track‑metadata accessors (codec, channels, label, HDR/DV flags) | §5 |
| **P2** | Frame/underrun stats, ABR strategy, subtitle cues | §6 |

P0+P1 is the whole ask. P2 is polish, can wait.

> ### Window / process model = consumer responsibility
>
> `Player::new(window: Arc<winit::window::Window>)` already takes the
> window from the caller — that's the right shape and we don't want it
> changed. Each consumer creates its own surface and pushes it into the
> player: Android does it via the NativeActivity, iOS via UIView, our
> downstream crate does it by spawning a sibling process that opens a
> standalone winit window and hosts the `Player`. The player crate
> stays out of the windowing / process / IPC business entirely.

---

## 3. P0 — Network injection

### 3.1 Centralise HTTP

Today three places construct their own `reqwest::Client`:

- `manifest.rs:25` — `Client::new()` in `Manifest::download`
- `tracks/segment.rs:75` — `Client::new()` in `Segment::download`
- `networking.rs::HttpClient::new` — exists but unused

**Change:**

- Promote `networking::HttpClient` to the canonical entry point. Strip
  the debug `println!`s.
- Internally owns a single `Arc<reqwest::Client>` (reqwest's Client is
  already an Arc; cloning is cheap and shares the connection pool).
- Methods:
  ```rust
  impl HttpClient {
      pub fn new() -> Self;
      pub async fn get(&self, url: String, kind: RequestKind) -> Result<bytes::Bytes, BoxError>;
      pub async fn get_range(&self, url: String, kind: RequestKind, start: u64, end: u64)
          -> Result<bytes::Bytes, BoxError>;
      pub async fn get_text(&self, url: String, kind: RequestKind) -> Result<String, BoxError>;
      pub async fn post(&self, url: String, kind: RequestKind, body: bytes::Bytes, content_type: &str)
          -> Result<bytes::Bytes, BoxError>;
      pub fn set_interceptor(&self, interceptor: Arc<dyn RequestInterceptor>);
  }
  ```
- `Player` holds `Arc<HttpClient>`. `Manifest::new`, `Tracks::new`,
  `Segment::download` take `&HttpClient` (or `Arc<HttpClient>`) instead
  of creating their own.

### 3.2 `RequestInterceptor` trait

New file `player/src/net.rs` (or add to `networking.rs`):

```rust
use std::error::Error;
pub type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// DASH MPD document.
    Manifest,
    /// MP4 init segment (moov + sidx, no media data).
    InitSegment,
    /// MP4 media segment (a few seconds of A/V).
    Segment,
    /// ClearKey / Widevine license POST.
    License,
}

#[derive(Default, Debug)]
pub struct PreparedRequest {
    /// Final URL to fetch. Interceptor may rewrite this completely.
    pub url: String,
    /// Headers to ADD (existing client defaults are not removed).
    pub headers: Vec<(String, String)>,
    /// Optional method override (defaults: GET for everything except License = POST).
    pub method: Option<reqwest::Method>,
    /// Optional body substitution (for License only).
    pub body: Option<bytes::Bytes>,
}

#[async_trait::async_trait]
pub trait RequestInterceptor: Send + Sync + 'static {
    /// Called once per outgoing request, BEFORE it is sent. The
    /// returned `PreparedRequest` is what `HttpClient` actually
    /// dispatches. Returning `Err` aborts the request — the original
    /// caller sees a network error.
    async fn intercept(
        &self,
        url: String,
        kind: RequestKind,
    ) -> Result<PreparedRequest, BoxError>;
}

/// Default — pass URL through, no headers added. Used by the existing
/// `app/` binary so behaviour for current tests is unchanged.
pub struct NoopInterceptor;

#[async_trait::async_trait]
impl RequestInterceptor for NoopInterceptor {
    async fn intercept(&self, url: String, _kind: RequestKind)
        -> Result<PreparedRequest, BoxError>
    {
        Ok(PreparedRequest { url, ..Default::default() })
    }
}
```

Setter on Player:

```rust
impl<V, A> Player<V, A> {
    pub fn set_request_interceptor(&self, interceptor: Arc<dyn RequestInterceptor>);
}
```

`HttpClient` dispatch shape:

```rust
async fn get(&self, url: String, kind: RequestKind) -> Result<Bytes, BoxError> {
    let prep = self.interceptor.load().intercept(url, kind).await?;
    let mut req = self.client.request(
        prep.method.unwrap_or(reqwest::Method::GET),
        &prep.url,
    );
    for (k, v) in prep.headers { req = req.header(k, v); }
    if let Some(b) = prep.body { req = req.body(b); }
    let resp = req.send().await?.error_for_status()?;
    Ok(resp.bytes().await?)
}
```

> **Caveat:** `intercept()` is async and can fail (e.g. token refresh
> hit the network and lost). The player must propagate that error
> cleanly — no `unwrap`.

### 3.3 `LicenseResolver` trait

```rust
#[async_trait::async_trait]
pub trait LicenseResolver: Send + Sync + 'static {
    /// Given a 16‑byte key ID found in a `tenc` box, return the AES‑128
    /// key. The player caches `(kid → key)` for the rest of the
    /// session. If the key is permanently unavailable, return Err —
    /// the player treats this as a fatal stream error.
    async fn resolve(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError>;
}

impl<V, A> Player<V, A> {
    pub fn set_license_resolver(&self, resolver: Arc<dyn LicenseResolver>);
}
```

**Where it plugs in:** `crypto.rs` currently constructs
`ClearKeyDecryptor` from a static `HashMap<String, String>` via
`set_clearkey`. Wrap that map in a cache and add a fallback:

```rust
async fn key_for(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
    if let Some(k) = self.cache.lock().unwrap().get(&kid).copied() {
        return Ok(k);
    }
    let resolver = self.resolver.as_ref()
        .ok_or("no license resolver set and key not in cache")?;
    let k = resolver.resolve(kid).await?;
    self.cache.lock().unwrap().insert(kid, k);
    Ok(k)
}
```

`set_clearkey(HashMap)` becomes sugar that pre‑populates the cache —
existing tests with hardcoded `preclikos.cz` keys keep working without
ever calling the resolver.

### 3.4 Consumer context — do **NOT** put any of this in the player crate

This subsection exists only so the player team understands the shape of
real consumer workloads when designing the trait. **None of the
behaviour below is implemented in `player`** — it lives in the
downstream BlackZone Console crate, which provides its own
`impl RequestInterceptor`.

- Some segment URLs use a `<storageId>:<slug>` pseudo‑URI form that
  the consumer's interceptor rewrites into a real CDN URL (with
  expiration caching).
- Some requests need a bearer token + custom headers when targeting a
  specific host; others (CDN) don't.
- The license endpoint expects a specific JSON body shape and returns
  a specific JSON shape — the consumer's interceptor handles that
  transformation via `PreparedRequest.body`.

All host names, header names, URL patterns, and body formats live in
the consumer. The player just executes whatever `PreparedRequest` it's
handed.

---

## 4. P1 — Player events

### 4.1 `PlayerEvent` enum

```rust
use std::time::Duration;

#[derive(Clone, Debug)]
pub enum PlayerEvent {
    /// Initial — before `open_url`.
    Idle,
    /// `open_url` succeeded.
    ManifestLoaded {
        duration: Duration,
        video_tracks: usize,
        audio_tracks: usize,
        subtitle_tracks: usize,
    },
    /// `prepare()` done — init segments fetched, decoders ready.
    Prepared,
    /// Waiting for media data.
    Buffering { reason: BufferingReason },
    /// Playback active.
    Playing,
    /// Paused by user (see §4.3).
    Paused,
    /// Periodic — emitted at ≤ 4 Hz during playback.
    Position {
        position: Duration,
        duration: Duration,
        /// Seconds of decoded video ahead of `position`.
        buffered_ahead_secs: f32,
        /// EWMA bytes/s over last N segment downloads.
        bandwidth_bps: u64,
    },
    /// Track selection changed (initial or user switch or ABR).
    TrackChanged { kind: TrackKind, info: TrackInfo },
    /// Decoder hiccup that the player recovered from. UI hint, not fatal.
    GlitchRecovered { detail: String },
    /// Cumulative stats — emitted at ≤ 1 Hz.
    Stats {
        video_frames_decoded: u64,
        video_frames_dropped: u64,
        audio_underruns: u64,
        /// Wall‑clock ms the decoder was blocked waiting on network in
        /// the last second (0 = healthy).
        net_stall_ms: u64,
    },
    /// End of media reached.
    EndOfStream,
    /// Fatal — playback cannot continue. `kind` lets the consumer
    /// branch (retry, re‑auth, abandon) without parsing `detail`.
    Error { kind: PlayerErrorKind, detail: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerErrorKind {
    /// TCP/TLS/DNS — transient connectivity problem.
    Network,
    /// HTTP non‑2xx that wasn't recovered by retry.
    Http { status: u16 },
    /// `RequestInterceptor::intercept` returned Err.
    Interceptor,
    /// `LicenseResolver::resolve` returned Err.
    LicenseResolver,
    /// MPD parse failed or has unsupported structure (e.g. multi‑period).
    ManifestParse,
    /// Decoder pipeline failed unrecoverably.
    Decoder,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub enum TrackKind { Video, Audio, Subtitle }

#[derive(Clone, Copy, Debug)]
pub enum BufferingReason {
    Initial,
    Stall,
    Seek,
    TrackSwitch,
}

/// Exact frame rate. DASH carries fractional rates like NTSC drop‑frame
/// (`30000/1001` → 29.97) and cinema NTSC (`24000/1001` → 23.976).
/// Storing num/den preserves precision; f32 doesn't.
#[derive(Clone, Copy, Debug)]
pub struct Fps {
    pub num: u32,
    pub den: u32, // typically 1, 1000, or 1001
}

impl Fps {
    pub fn as_f32(&self) -> f32 { self.num as f32 / self.den as f32 }
}

impl std::fmt::Display for Fps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 24000/1001 → "23.976", 30/1 → "30"
        let v = self.as_f32();
        if self.den == 1 { write!(f, "{}", self.num) }
        else { write!(f, "{:.3}", v) }
    }
}

#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub representation_id: u32,
    pub codec: String,         // simplified: "HEVC", "H.264", "AAC", "DDP"
    pub bitrate_bps: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<Fps>,
    pub channels: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub language: Option<String>,
    pub label: String,         // pre‑formatted, e.g. "1080p HEVC · 8.5 Mbps"
    pub hdr10: bool,
    pub dolby_vision: bool,
}
```

`buffered_ahead_secs` in `Position` is defined precisely as:
**(end PTS of the latest segment whose download AND decode both
completed) minus (current playback PTS)**. I.e. the amount of media
that's safe to play through if the network drops *right now*. It does
**not** include segments that are merely downloaded but not yet
decoded, nor ones queued for download.

### 4.2 Exposure

```rust
impl<V, A> Player<V, A> {
    /// Subscribe to the event stream. Each subscriber gets every event
    /// from the moment of subscription forward (broadcast semantics).
    /// The channel buffer holds 64 events; if a subscriber falls
    /// behind by more, it receives `RecvError::Lagged(n)` and continues
    /// from the newest event — this is acceptable for UI subscribers
    /// (they just re-render with the latest state).
    pub fn events(&self) -> tokio::sync::broadcast::Receiver<PlayerEvent>;

    /// Convenience: synchronous read of the latest `Position` for code
    /// that doesn't want to subscribe (e.g. one-shot polling).
    pub fn position(&self) -> std::time::Duration;
}
```

**Why `broadcast` instead of `watch`:** lifecycle events
(`ManifestLoaded`, `Prepared`, `EndOfStream`, `Error`) MUST NOT be
lost. `watch` only keeps the latest value, so a fast sequence
`Buffering → Playing → Error` would lose the middle two if the
subscriber reads slowly. `broadcast(64)` keeps a 64-event ring buffer
— since `Position` is rate-limited to 4 Hz that's 16 seconds of slack,
plenty for any subscriber.

**Implementation hint:** `Arc<broadcast::Sender<PlayerEvent>>`
initialized in `Player::new()`. Every transition does
`let _ = sender.send(ev);` — `send` only errors when there are no
subscribers, which is fine to ignore.

**Position emission cadence:** in `video_sync_loop` after each rendered
frame, rate‑limited to every 250 ms.

**Bandwidth:** EWMA over the last ~8 segment downloads.
`Segment::download` already knows byte count and elapsed time — surface
that.

**`buffered_ahead_secs`:** end PTS of the last queued segment minus
current playback PTS.

### 4.3 Pause support

Player has no pause today. Add:

```rust
impl<V, A> Player<V, A> {
    pub fn pause(&self);
    pub fn resume(&self);
    pub fn is_paused(&self) -> bool;
}
```

Internally: `Arc<AtomicBool>` checked in `video_sync_loop` and the
audio output. While paused both loops park on a `Notify`; PTS does not
advance; cpal stream is paused (`stream.pause()`).

---

## 5. P1 — Track metadata accessors

`Tracks` already exposes `AudioAdaptation` / `VideoAdaptation` with raw
MPD fields. The TUI needs human‑friendly labels. Add (no new state, just
derivations):

```rust
impl VideoRepresenation {
    pub fn label(&self) -> String;        // "1080p HEVC 10‑bit · 8.5 Mbps"
    pub fn codec_short(&self) -> &str;    // "HEVC", "H.264", "AV1"
    pub fn is_hdr10(&self) -> bool;
    pub fn is_dolby_vision(&self) -> bool;
}

impl AudioRepresentation {
    pub fn label(&self) -> String;        // "EN · 5.1 · DDP · 384 kbps"
    pub fn channels(&self) -> Option<u32>;
    pub fn codec_short(&self) -> &str;    // "AAC", "DDP", "DD"
}

impl AudioAdaptation {
    pub fn language(&self) -> Option<&str>;
    pub fn role(&self) -> Option<&str>;   // "main" / "dub" / "commentary"
}
```

Codec mapping for `codec_short` (from MPD `@codecs` strings):
- `hev1.*` / `hvc1.*` → `HEVC`
- `avc1.*` / `avc3.*` → `H.264`
- `av01.*` → `AV1`
- `mp4a.40.2` → `AAC`
- `mp4a.40.5` → `AAC‑HE`
- `ec-3` → `DDP`
- `ac-3` → `DD`

HDR/DV detection — check `Representation` siblings/parents:
- HDR10: `<SupplementalProperty schemeIdUri=".../colour_primaries" value="9"/>`
  or `transfer_characteristics` value 16/18
- Dolby Vision: `<EssentialProperty schemeIdUri="…dolby_vision_profile"/>`

---

## 6. P2 — Nice‑to‑haves

### 6.1 Extra stats fields (cheap to add)

Extend `PlayerEvent::Stats` and the matching event JSON:

- `decoder_name: String` — e.g. `"D3D11VA HEVC"`, `"MediaCodec H.264"`,
  `"FFmpeg software AV1"`. Known to the player internally.
- `current_resolution: Option<(u32, u32)>` — what's actually rendered
  post‑ABR, not the chosen representation.
- `audio_peak_db: Option<[f32; 2]>` — last‑frame L/R peak in dB
  (range typically −60..=0). Cheap to compute in the audio renderer.
  Lets the TUI draw a tiny VU meter next to the position bar.

### 6.2 ABR strategy

```rust
pub enum AbrStrategy {
    Manual,                          // current behaviour
    BandwidthEwma { safety_factor: f32 },  // default 1.25
}

impl<V, A> Player<V, A> {
    pub fn set_abr_strategy(&self, strategy: AbrStrategy);
}
```

`BandwidthEwma`: pick the highest representation whose
`bitrate_bps * safety_factor ≤ ewma_bps`. On switch, emit
`TrackChanged` so UI updates.

### 6.3 Subtitles

DASH typically carries subs as a separate `AdaptationSet`
(`contentType="text"`, mimeType `application/mp4` with WebVTT in
fragments, or `application/ttml+xml`).

- Phase 1: just enumerate them in `Tracks` so the consumer can list
  them.
- Phase 2: expose decoded cues via a callback/channel:
  ```rust
  pub fn subtitle_cues(&self) -> tokio::sync::broadcast::Receiver<SubtitleCue>;

  pub struct SubtitleCue {
      pub start: Duration,
      pub end: Duration,
      pub text: String,
  }
  ```
  Rendering them on screen is the consumer's choice.

---

## 7. Error semantics

| Situation | Player behaviour | Surfaced as |
|---|---|---|
| Manifest fetch returns non‑2xx | After retries: `Error` | `Http { status }` |
| Segment 404 / 5xx — transient | Retry per `RetryPolicy`. If success → `GlitchRecovered`. If exhausted → `Error` | `Http { status }` |
| Segment 401 / 403 | **Not retried** — surfaced immediately | `Http { status }` |
| TCP/DNS/TLS error | Retry per `RetryPolicy` | `Network` |
| `RequestInterceptor::intercept` returns `Err` | Not retried — surfaced immediately | `Interceptor` |
| `LicenseResolver::resolve` returns `Err` | Not retried — surfaced immediately | `LicenseResolver` |
| MPD parse failed / multi-period MPD | `Error` from `open_url` | `ManifestParse` |
| Decoder fault — recoverable single GOP | `GlitchRecovered`, continue | — |
| Decoder fault — unrecoverable | `Error` | `Decoder` |
| EOS | `EndOfStream` | — |

### 7.1 RetryPolicy

```rust
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,        // including the first try
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub multiplier: f32,           // back-off multiplier
    pub jitter: f32,               // 0.0..=1.0 — relative jitter band
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(4),
            multiplier: 2.0,
            jitter: 0.2,           // ±20%
        }
    }
}

impl<V, A> Player<V, A> {
    pub fn set_retry_policy(&self, policy: RetryPolicy);
}
```

Retry-able status codes: **408, 425, 429, 500, 502, 503, 504**. All
others (incl. 401, 403, 404) are surfaced after the first failure.
TCP/TLS/DNS errors are always retry-able.

### 7.2 Interceptor / resolver timeouts

The player never blocks indefinitely on consumer callbacks. After
~10 s with no response, the in‑flight `intercept` / `resolve` future
is dropped and surfaced as `Error{kind: Interceptor, ...}` or
`{kind: LicenseResolver, ...}`. Timeout is configurable via
`Player::set_callback_timeout(Duration)` if 10 s is wrong.

---

## 8. Backward compatibility

Existing test path (`https://preclikos.cz/examples/encrypted/manifest.mpd`
+ hardcoded keys) MUST keep working without changes to `app/`. Concretely:

- Default `RequestInterceptor` = `NoopInterceptor` (no rewrite, no headers).
- Default `LicenseResolver` = `None`; `set_clearkey(...)` pre‑populates
  the key cache so the resolver is never asked.
- `Player::events()` always returns a valid receiver (sender created
  in `Player::new()`).
- `app/` does not need a single line changed.

---

## 9. Acceptance criteria

The work is done when:

1. `cargo test -p player` still passes (no regressions).
2. `cargo run -p app` still plays the existing test stream
   end‑to‑end unchanged.
3. A new integration test in `player/tests/` exercises the injection
   flow with a fake interceptor:
   ```rust
   #[tokio::test]
   async fn interceptor_can_rewrite_url_and_add_header() {
       struct FakeInterceptor;
       #[async_trait::async_trait]
       impl RequestInterceptor for FakeInterceptor {
           async fn intercept(&self, url: String, _kind: RequestKind)
               -> Result<PreparedRequest, BoxError>
           {
               Ok(PreparedRequest {
                   url: rewrite_to_local_fixture(&url),
                   headers: vec![("X-Test".into(), "yes".into())],
                   ..Default::default()
               })
           }
       }
       // ... assert ManifestLoaded → Prepared within 2 s
   }
   ```
4. All new traits / structs / event variants have `///` rustdoc.

That's it — no binary, no IPC, no protocol to ship. The consumer
handles its own window and process model.

---

## 10. Downstream context — what the consumer side looks like

**Not to be implemented in the player crate.** Included so the player
team can see how the proposed primitives are consumed end‑to‑end and
sanity‑check whether the event stream and traits carry enough
information.

The consumer (BlackZone Console) is a `ratatui` raw‑mode TUI. To play
video it spawns a **separate sibling process** it owns (a small Rust
binary in its own repo) which:

- creates a `winit::Window`
- constructs `Player::new(window)`
- installs a `RequestInterceptor` that talks to the parent TUI over
  stdin/stdout for URL rewrites + auth headers
- installs a `LicenseResolver` that does the same for ClearKey lookups
- forwards `Player::events()` items as JSON lines on stdout

That subprocess + IPC protocol live in the consumer's repo, not in
`rust_player_learning`. The player crate only sees its own traits
being called.

The TUI itself shows:

```
┌─ Now Playing ─────────────────────────────────────────────────┐
│ ┌─poster─┐  Inception                                         │
│ │ ASCII  │  2010 · 148 min · EN · HDR · DV                    │
│ │ poster │                                                    │
│ │        │  ━━━━━━━━━━━━━●─────────────  01:23:45 / 02:28:01  │
│ │        │  buffer ▓▓▓▓▓▓░░░░  5.3s ahead                     │
│ └────────┘  ▶ Playing · 8.5 Mbps · D3D11VA · drops 0          │
├────────────────┬────────────────┬─────────────────────────────┤
│  Video         │  Audio         │  Subtitles                  │
│ ▶ 1080p HEVC10 │ ▶ CZ · 5.1 DDP │ ▶ Off                       │
│   720p HEVC    │   EN · 5.1 DDP │   Czech                     │
│   480p H.264   │   EN · 2.0 AAC │   English                   │
│   Auto (ABR)   │                │                             │
├────────────────┴────────────────┴─────────────────────────────┤
│ Space ⏯  ←/→ ±10s  Shift+←/→ ±60s  +/− vol  M mute  V/A/S focus│
└───────────────────────────────────────────────────────────────┘
```

The position bar reads from `PlayerEvent::Position { position,
duration }`. The buffer bar reads from `buffered_ahead_secs`. The
status line reads from the current state (`Buffering` / `Playing` /
`Paused` / `Error`). Track lists come from `Tracks` returned by
`Player::get_tracks()`. Track selection calls
`Player::change_video_track()` / `change_audio_track()`.

The video itself appears in the winit window owned by the child
process — **not** in the terminal. The TUI is purely a remote
control.

---

## 11. Out of scope (explicit)

- **DRM other than ClearKey** — only ClearKey is needed.
- **License renewal** — ClearKey keys don't expire mid‑stream; a
  single `resolve` per KID is enough. If renewal is anticipated,
  please flag and we'll add a `revoke(kid)` to the resolver API.
- **Multi‑period MPDs** — if `mpd.periods.len() > 1` during
  `open_url`, return
  `Err(PlayerErrorKind::ManifestParse, "multi-period MPD not
  supported (got N periods)")` immediately. Don't silently play
  period 0 — that masks bugs.

Thanks! Ping us once any of P0/P1 lands and we'll wire up our
downstream subprocess + interceptor against the new traits.
