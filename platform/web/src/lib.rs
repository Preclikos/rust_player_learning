//! Browser bridge — the engine EMBEDDED into a host page's `<canvas>`.
//!
//! This is the web shell of the *unified bridge core* (the `bridge` crate),
//! the wasm-bindgen mirror of the Android JNI and iOS FFI shells. The page
//! owns a `<canvas>`, calls [`RustPlayer::create`] with it, and gets back an
//! object it drives through the same unified control surface the other
//! shells expose.
//!
//! Events flow Rust→host as unified JSON through `host.onEvent(json)`. The
//! provider hooks are JS functions on the same host object, awaited as
//! promises: `host.resolveKey(kidHex) → keyHex` and the optional
//! `host.intercept(url, kind) → {url?, headers?}`. (The demo page completes
//! them synchronously — passthrough + the baked test ClearKeys.)
//!
//!   * Build with `./build.ps1` (wasm-pack → `www/pkg/`), serve `www/`.
//!
//! On non-wasm targets this crate is a no-op so the workspace still builds.

#![cfg(target_arch = "wasm32")]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bridge::{BoxError, BridgeHandle, BridgeHost, PreparedRequest, RequestKind, StartConfig};
use js_sys::{Function, Promise, Reflect};
use player::{AbrVideoProfile, Player};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

// --- module init -------------------------------------------------------------

#[wasm_bindgen(start)]
fn init() {
    console_error_panic_hook::set_once();
    // `RUST_LOG`-less: info by default, the `stats`/`position` firehose is
    // debug-level in the engine already.
    let _ = console_log::init_with_level(log::Level::Trace);
    log::set_max_level(log::LevelFilter::Info);
    log::info!("[web] rustplayer module loaded");
}

// --- JS interop helpers ------------------------------------------------------

fn js_string(v: &JsValue) -> String {
    v.as_string()
        .or_else(|| {
            v.dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
        })
        .unwrap_or_else(|| format!("{:?}", v))
}

fn get(obj: &JsValue, key: &str) -> Option<JsValue> {
    Reflect::get(obj, &JsValue::from_str(key))
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
}

fn get_fn(obj: &JsValue, key: &str) -> Option<Function> {
    get(obj, key).and_then(|v| v.dyn_into::<Function>().ok())
}

/// Await a JS return value that may or may not be a promise.
async fn settle(v: JsValue) -> Result<JsValue, JsValue> {
    match v.dyn_into::<Promise>() {
        Ok(p) => JsFuture::from(p).await,
        Err(v) => Ok(v),
    }
}

/// A `!Send` local future presented as `Send`. The `BridgeHost` trait is
/// `#[async_trait]` (its futures must be `Send` for the native shells); on
/// single-threaded wasm the bound is unobservable — same contract as
/// `player::rt` on this target.
struct SendFut<T>(Pin<Box<dyn Future<Output = T>>>);
unsafe impl<T> Send for SendFut<T> {}
impl<T> Future for SendFut<T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        self.0.as_mut().poll(cx)
    }
}

fn kind_str(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Manifest => "manifest",
        RequestKind::InitSegment => "init",
        RequestKind::Segment => "segment",
        RequestKind::License => "license",
    }
}

fn parse_hex16(s: &str, what: &str) -> Result<[u8; 16], String> {
    let bytes = hex::decode(s.trim()).map_err(|e| format!("{what}: bad hex: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| format!("{what}: expected 16 bytes"))
}

// --- host --------------------------------------------------------------------

/// The page's host object. `JsValue` is `!Send`; one thread — see `SendFut`.
struct WebHost {
    host: JsValue,
    /// ClearKeys passed in `options.clearKeys`, consulted before
    /// `host.resolveKey`.
    keys: HashMap<[u8; 16], [u8; 16]>,
}
unsafe impl Send for WebHost {}
unsafe impl Sync for WebHost {}

impl WebHost {
    fn intercept_js(&self, url: String, kind: RequestKind) -> SendFut<Result<PreparedRequest, String>> {
        let host = self.host.clone();
        SendFut(Box::pin(async move {
            let Some(f) = get_fn(&host, "intercept") else {
                return Ok(PreparedRequest {
                    url,
                    ..Default::default()
                });
            };
            let ret = f
                .call2(&host, &JsValue::from_str(&url), &JsValue::from_str(kind_str(kind)))
                .map_err(|e| js_string(&e))?;
            let v = settle(ret).await.map_err(|e| js_string(&e))?;
            if v.is_undefined() || v.is_null() {
                return Ok(PreparedRequest {
                    url,
                    ..Default::default()
                });
            }
            // A bare string is a rewritten URL; an object may carry
            // `url` and `headers` (array of [name, value] pairs or a
            // plain object).
            if let Some(s) = v.as_string() {
                return Ok(PreparedRequest {
                    url: s,
                    ..Default::default()
                });
            }
            let mut prep = PreparedRequest {
                url: get(&v, "url").and_then(|u| u.as_string()).unwrap_or(url),
                ..Default::default()
            };
            if let Some(h) = get(&v, "headers") {
                if let Some(arr) = h.dyn_ref::<js_sys::Array>() {
                    for pair in arr.iter() {
                        let pair: js_sys::Array = match pair.dyn_into() {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        if let (Some(k), Some(val)) = (pair.get(0).as_string(), pair.get(1).as_string()) {
                            prep.headers.push((k, val));
                        }
                    }
                } else if let Ok(keys) = Reflect::own_keys(&h) {
                    for k in keys.iter() {
                        if let (Some(name), Some(val)) =
                            (k.as_string(), get(&h, &k.as_string().unwrap_or_default()).and_then(|x| x.as_string()))
                        {
                            prep.headers.push((name, val));
                        }
                    }
                }
            }
            Ok(prep)
        }))
    }

    fn resolve_js(&self, kid: [u8; 16]) -> SendFut<Result<[u8; 16], String>> {
        let host = self.host.clone();
        SendFut(Box::pin(async move {
            let Some(f) = get_fn(&host, "resolveKey") else {
                return Err("host has no resolveKey and no clearKeys option covers this KID".into());
            };
            let ret = f
                .call1(&host, &JsValue::from_str(&hex::encode(kid)))
                .map_err(|e| js_string(&e))?;
            let v = settle(ret).await.map_err(|e| js_string(&e))?;
            let s = v.as_string().ok_or("resolveKey must return a hex string")?;
            parse_hex16(&s, "resolveKey")
        }))
    }
}

#[async_trait]
impl BridgeHost for WebHost {
    fn on_event(&self, json: String) {
        if let Some(f) = get_fn(&self.host, "onEvent") {
            if let Err(e) = f.call1(&self.host, &JsValue::from_str(&json)) {
                log::warn!("[web] onEvent threw: {}", js_string(&e));
            }
        }
    }

    async fn intercept(&self, url: String, kind: RequestKind) -> Result<PreparedRequest, BoxError> {
        self.intercept_js(url, kind).await.map_err(|e| e.into())
    }

    async fn resolve_key(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        if let Some(k) = self.keys.get(&kid) {
            return Ok(*k);
        }
        self.resolve_js(kid).await.map_err(|e| e.into())
    }
}

// --- options -----------------------------------------------------------------

/// HDR policy for the browser. The browser converts every `VideoFrame` to
/// RGB itself before any of our shaders run; for PQ content Chrome was
/// measured to apply the BT.2020 → BT.709 matrix to the still PQ-encoded
/// values and sRGB-encode them — no tone-map, a washed-out picture. The
/// renderer verifies that conversion at start-up with a synthetic frame and,
/// when it holds, undoes it in the shader and runs the engine's own PQ →
/// SDR tonemap on the GPU — the same math and numbers as the native players
/// (`player/src/renderers/shader_chrome_inverse.wgsl`).
///
/// - `Auto` (default): HDR representations play through the engine tonemap
///   when the calibration passed; otherwise only SDR representations play
///   (the SDR ladder is the same picture the tonemap was calibrated to).
/// - `SdrOnly`: SDR representations only, whatever the browser does.
/// - `Browser`: HDR representations with the browser's own conversion
///   (comparison / a host that prefers it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebHdrPolicy {
    Auto,
    SdrOnly,
    Browser,
}

fn read_options(options: &JsValue) -> Result<(StartConfig, HashMap<[u8; 16], [u8; 16]>, WebHdrPolicy), String> {
    let mut config = StartConfig::default();
    let mut keys = HashMap::new();
    let mut hdr = WebHdrPolicy::Auto;
    if options.is_undefined() || options.is_null() {
        return Ok((config, keys, hdr));
    }
    match get(options, "hdr").and_then(|v| v.as_string()).as_deref() {
        None | Some("auto") => {}
        Some("sdr") => hdr = WebHdrPolicy::SdrOnly,
        Some("browser") => hdr = WebHdrPolicy::Browser,
        Some(other) => {
            return Err(format!("options.hdr: expected \"auto\", \"sdr\" or \"browser\", got {other:?}"))
        }
    }
    if let Some(ms) = get(options, "startPositionMs").and_then(|v| v.as_f64()) {
        config.start_position = Some(Duration::from_millis(ms.max(0.0) as u64));
    }
    if let Some(f) = get(options, "startFraction").and_then(|v| v.as_f64()) {
        config.start_fraction = Some(f as f32);
    }
    if let Some(b) = get(options, "autoSelectSubtitle").and_then(|v| v.as_bool()) {
        config.auto_select_subtitle = b;
    }
    config.preferred_audio_language = get(options, "preferredAudioLanguage").and_then(|v| v.as_string());
    config.preferred_subtitle_language =
        get(options, "preferredSubtitleLanguage").and_then(|v| v.as_string());
    // wrappedLicence: { url, info? } — keys arrive wrapped and end up as
    // non-extractable WebCrypto keys (docs/CLEARKEY_WRAPPED_LICENCE.md).
    if let Some(wl) = get(options, "wrappedLicence") {
        let url = if let Some(s) = wl.as_string() {
            s
        } else {
            get(&wl, "url")
                .and_then(|v| v.as_string())
                .ok_or("options.wrappedLicence needs a url")?
        };
        config.wrapped_licence_url = Some(url);
        config.wrapped_licence_hkdf_info = get(&wl, "info").and_then(|v| v.as_string());
    }
    if let Some(ck) = get(options, "clearKeys") {
        let names = Reflect::own_keys(&ck).map_err(|e| js_string(&e))?;
        for k in names.iter() {
            let Some(kid_hex) = k.as_string() else { continue };
            let Some(key_hex) = get(&ck, &kid_hex).and_then(|v| v.as_string()) else { continue };
            keys.insert(parse_hex16(&kid_hex, "clearKeys kid")?, parse_hex16(&key_hex, "clearKeys key")?);
        }
    }
    Ok((config, keys, hdr))
}

// --- the exported player -----------------------------------------------------

/// One embedded player on one canvas. Create with [`RustPlayer::create`];
/// call [`RustPlayer::shutdown`] before letting it go so the pipeline stops
/// and the AudioContext closes.
#[wasm_bindgen]
pub struct RustPlayer {
    handle: BridgeHandle,
}

#[wasm_bindgen]
impl RustPlayer {
    /// Build the player on `canvas` and start `manifest_url`.
    ///
    /// `host`: `{ onEvent(json), resolveKey?(kidHex) → keyHex, intercept?(url, kind) → {url?, headers?} }`.
    /// `options`: `{ startPositionMs?, startFraction?, autoSelectSubtitle?,
    /// preferredAudioLanguage?, preferredSubtitleLanguage?, clearKeys?: {kidHex: keyHex},
    /// wrappedLicence?: { url, info? } (keys fetched wrapped, unwrapped into
    /// non-extractable WebCrypto keys — see docs/CLEARKEY_WRAPPED_LICENCE.md;
    /// auth headers via `intercept(url, "license")`),
    /// hdr?: "auto" | "sdr" | "browser" }` — `hdr` defaults to `"auto"`: HDR
    /// representations render through the engine's own PQ → SDR tonemap on
    /// the GPU (same numbers as the native players) when the renderer could
    /// verify the browser's frame conversion, else SDR representations only;
    /// `"sdr"` forces SDR only; `"browser"` shows the HDR rungs as the
    /// browser converts them (see [`WebHdrPolicy`]).
    ///
    /// Call from a user gesture (a click handler): the browser only lets the
    /// `AudioContext` run after one. The canvas's drawing-buffer size
    /// (`canvas.width`/`height`) is the render size; keep it at
    /// `clientSize × devicePixelRatio` and forward changes via `resize`.
    pub async fn create(
        canvas: web_sys::HtmlCanvasElement,
        manifest_url: String,
        host: JsValue,
        options: JsValue,
    ) -> Result<RustPlayer, JsValue> {
        let (config, keys, hdr) = read_options(&options).map_err(|e| JsValue::from_str(&e))?;
        let (w, h) = (canvas.width().max(1), canvas.height().max(1));
        log::info!("[web] creating player on {}x{} canvas for {} (hdr policy {:?})", w, h, manifest_url, hdr);
        let player = Player::new_from_canvas(canvas, w, h).await;
        let engine_tonemap = player.web_hdr_tonemap_available();
        match hdr {
            WebHdrPolicy::Auto if engine_tonemap => {
                log::info!("[web] HDR representations enabled: engine PQ → SDR tonemap on the GPU");
            }
            WebHdrPolicy::Auto => {
                log::info!("[web] browser frame conversion not verified → SDR representations only");
                player.set_abr_video_profile(AbrVideoProfile::SdrOnly);
            }
            WebHdrPolicy::SdrOnly => player.set_abr_video_profile(AbrVideoProfile::SdrOnly),
            WebHdrPolicy::Browser => {
                log::info!("[web] HDR representations with the browser's own conversion (hdr: \"browser\")");
                player.set_web_hdr_passthrough(true);
            }
        }
        let host: Arc<dyn BridgeHost> = Arc::new(WebHost { host, keys });
        let handle = bridge::start(player, manifest_url, host, config);
        Ok(RustPlayer { handle })
    }

    pub fn play(&self) {
        self.handle.play();
    }
    pub fn pause(&self) {
        self.handle.pause();
    }
    #[wasm_bindgen(js_name = isPaused)]
    pub fn is_paused(&self) -> bool {
        self.handle.is_paused()
    }
    #[wasm_bindgen(js_name = seekMs)]
    pub fn seek_ms(&self, position_ms: f64) {
        self.handle.seek_ms(position_ms.max(0.0) as i64);
    }
    /// Absolute volume, 0.0..=1.0.
    #[wasm_bindgen(js_name = setVolume)]
    pub fn set_volume(&self, volume: f32) {
        self.handle.set_volume(volume);
    }
    #[wasm_bindgen(js_name = positionMs)]
    pub fn position_ms(&self) -> f64 {
        self.handle.position_ms() as f64
    }
    #[wasm_bindgen(js_name = durationMs)]
    pub fn duration_ms(&self) -> f64 {
        self.handle.duration_ms() as f64
    }
    /// Unified tracks snapshot JSON (`"{}"` until `tracks_ready`).
    #[wasm_bindgen(js_name = tracksJson)]
    pub fn tracks_json(&self) -> String {
        self.handle.tracks_json()
    }
    /// Manual quality switch: immediate (pipeline restart at the current
    /// position) and locks ABR to manual. Wire the quality menu to this.
    #[wasm_bindgen(js_name = setVideoTrack)]
    pub fn set_video_track(&self, adapt: u32, repr: u32) {
        self.handle.set_video_track(adapt as usize, repr as usize);
    }
    /// Seamless swap via the ABR path (takes effect at the next segment
    /// boundary, ABR stays armed). A test hook for that path, like the
    /// Android/iOS shells expose — not for user-driven switches.
    #[wasm_bindgen(js_name = setVideoTrackSoft)]
    pub fn set_video_track_soft(&self, adapt: u32, repr: u32) {
        self.handle.set_video_track_soft(adapt as usize, repr as usize);
    }
    #[wasm_bindgen(js_name = setVideoAuto)]
    pub fn set_video_auto(&self) {
        self.handle.set_video_auto();
    }
    #[wasm_bindgen(js_name = setAudioTrack)]
    pub fn set_audio_track(&self, adapt: u32, repr: u32) {
        self.handle.set_audio_track(adapt as usize, repr as usize);
    }
    #[wasm_bindgen(js_name = setSubtitleTrack)]
    pub fn set_subtitle_track(&self, adapt: u32, repr: u32) {
        self.handle.set_subtitle_track(adapt as usize, repr as usize);
    }
    #[wasm_bindgen(js_name = clearSubtitles)]
    pub fn clear_subtitles(&self) {
        self.handle.clear_subtitles();
    }
    /// New drawing-buffer size in device pixels (also set `canvas.width/height`).
    pub fn resize(&self, width: u32, height: u32) {
        self.handle.resize(width, height);
    }
    /// Stop playback and tear the pipeline down. Drop the object afterwards.
    pub fn shutdown(&self) {
        self.handle.shutdown();
    }
}

/// Runtime log threshold for the engine's `log::` output on the console:
/// `"error" | "warn" | "info" | "debug" | "trace"`. Default `info`; `debug`
/// adds the per-second stats, the audio callback diagnostics and the
/// segment-preparation timings.
/// Cumulative microseconds per subsystem since the module loaded, as JSON:
/// `{"audio_output":{"ms":..,"calls":..}, ..}`.
///
/// The web build runs everything on one thread, so "which subsystem" and "how
/// busy is the thread" are the same question. Measured against wall-clock
/// playback time, each entry reads directly as a share of that thread.
#[wasm_bindgen(js_name = profJson)]
pub fn prof_json() -> String {
    player::prof::json()
}

#[wasm_bindgen(js_name = setLogLevel)]
pub fn set_log_level(level: &str) {
    let lvl = match level {
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => log::LevelFilter::Info,
    };
    log::set_max_level(lvl);
}

/// The bundled test stream + its ClearKeys (JSON `{kidHex: keyHex}`), for
/// the demo page — the same fixture the Android/iOS smoke-test apps play.
#[wasm_bindgen(js_name = testStream)]
pub fn test_stream() -> JsValue {
    let obj = js_sys::Object::new();
    let _ = Reflect::set(&obj, &"url".into(), &JsValue::from_str(bridge::TEST_MANIFEST_URL));
    let keys = js_sys::Object::new();
    for (kid, key) in bridge::test_clearkeys() {
        let _ = Reflect::set(&keys, &JsValue::from_str(&kid), &JsValue::from_str(&key));
    }
    let _ = Reflect::set(&obj, &"clearKeys".into(), &keys);
    obj.into()
}
