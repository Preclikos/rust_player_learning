// iOS bridge — EMBEDDED into a host-owned UIView's CAMetalLayer (no winit).
//
// This is the iOS shell of the *unified bridge core* (the `bridge` crate),
// the FFI mirror of the Android shell. The Objective-C host (`ios/RustPlayer/
// main.m`) owns the app lifecycle and a `CAMetalLayer`, hands it to
// `rustplayer_player_create`, and gets back an opaque handle it drives through the same
// unified control surface the Android JNI exposes.
//
// Events flow Rust→host as unified JSON via a C `event_cb`. The provider hooks
// (`intercept` / `resolve_key`) use an **async token bridge**: Rust fires the
// host callback with a token and awaits a oneshot; the host calls back
// `rustplayer_intercept_complete(token, …)` / `rustplayer_resolve_key_complete(token, …)`.
// (The test host completes them synchronously — passthrough + baked ClearKeys.)
//
//   * Build with `./ios/build_sim.sh` (links librustplayer.a into the Obj-C app).
//
// On non-iOS targets this crate is a no-op so the workspace still builds.

#![cfg(target_os = "ios")]

use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use bridge::{
    self, BoxError, BridgeHandle, BridgeHost, PreparedRequest, RequestKind, StartConfig,
};
use async_trait::async_trait;
use bytes::Bytes;
use player::{Player, SubtitleStyle};
use reqwest::Method;
use tokio::sync::oneshot;

// --- C ABI callback types ----------------------------------------------------

/// `void (*)(void *user, const char *url, int kind, uint64_t token)`.
type InterceptCb = extern "C" fn(*mut c_void, *const c_char, c_int, u64);
/// `void (*)(void *user, const uint8_t kid[16], uint64_t token)`.
type ResolveKeyCb = extern "C" fn(*mut c_void, *const u8, u64);
/// `void (*)(void *user, const char *json_event)`.
type EventCb = extern "C" fn(*mut c_void, *const c_char);

/// Opaque host pointer (e.g. the Swift/ObjC controller). Raw pointers aren't
/// `Send`/`Sync`; the host keeps it valid until `rustplayer_player_destroy`
/// returns, and [`IosHost`] never touches it after that, so we assert it.
struct UserPtr(*mut c_void);
unsafe impl Send for UserPtr {}
unsafe impl Sync for UserPtr {}

struct IosHost {
    intercept_cb: InterceptCb,
    resolve_key_cb: ResolveKeyCb,
    event_cb: EventCb,
    /// `None` once the host has destroyed the player. The orchestrator, the
    /// event pump and in-flight fetches outlive `rustplayer_player_destroy`
    /// and keep calling in; every call into the host holds the read lock,
    /// so [`IosHost::close`] (write lock) returns only after the last
    /// callback has left the host, and no later one reaches `user`.
    user: RwLock<Option<UserPtr>>,
}

impl IosHost {
    /// Call `f` with the host `user` pointer unless the host is gone.
    /// Returns `false` when the call was skipped.
    fn with_user(&self, f: impl FnOnce(*mut c_void)) -> bool {
        let guard = self.user.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(u) => {
                f(u.0);
                true
            }
            None => false,
        }
    }

    /// Stop all further callbacks into the host, waiting for running ones.
    fn close(&self) {
        *self.user.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[async_trait]
impl BridgeHost for IosHost {
    fn on_event(&self, json: String) {
        if let Ok(c) = CString::new(json) {
            self.with_user(|user| (self.event_cb)(user, c.as_ptr()));
        }
    }

    async fn intercept(
        &self,
        url: String,
        kind: RequestKind,
    ) -> Result<PreparedRequest, BoxError> {
        let (tx, rx) = oneshot::channel();
        let token = next_token();
        intercept_registry().lock().unwrap().insert(token, tx);
        let c_url = match CString::new(url) {
            Ok(c) => c,
            Err(e) => {
                intercept_registry().lock().unwrap().remove(&token);
                return Err(Box::new(e));
            }
        };
        if !self.with_user(|user| (self.intercept_cb)(user, c_url.as_ptr(), kind_to_int(kind), token)) {
            intercept_registry().lock().unwrap().remove(&token);
            return Err("player destroyed".into());
        }
        match rx.await {
            Ok(Ok(p)) => Ok(p),
            Ok(Err(m)) => Err(m.into()),
            Err(_) => Err("swift interceptor cancelled".into()),
        }
    }

    async fn resolve_key(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        let (tx, rx) = oneshot::channel();
        let token = next_token();
        resolve_registry().lock().unwrap().insert(token, tx);
        if !self.with_user(|user| (self.resolve_key_cb)(user, kid.as_ptr(), token)) {
            resolve_registry().lock().unwrap().remove(&token);
            return Err("player destroyed".into());
        }
        match rx.await {
            Ok(Ok(k)) => Ok(k),
            Ok(Err(m)) => Err(m.into()),
            Err(_) => Err("swift licence resolver cancelled".into()),
        }
    }
}

// --- token registries (host completes an in-flight callback by token) --------

fn next_token() -> u64 {
    static TOKENS: AtomicU64 = AtomicU64::new(1);
    TOKENS.fetch_add(1, Ordering::Relaxed)
}

type InterceptTx = oneshot::Sender<Result<PreparedRequest, String>>;
type ResolveTx = oneshot::Sender<Result<[u8; 16], String>>;

fn intercept_registry() -> &'static Mutex<HashMap<u64, InterceptTx>> {
    static R: OnceLock<Mutex<HashMap<u64, InterceptTx>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn resolve_registry() -> &'static Mutex<HashMap<u64, ResolveTx>> {
    static R: OnceLock<Mutex<HashMap<u64, ResolveTx>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn kind_to_int(kind: RequestKind) -> c_int {
    match kind {
        RequestKind::Manifest => 0,
        RequestKind::InitSegment => 1,
        RequestKind::Segment => 2,
        RequestKind::License => 3,
    }
}

// --- runtime / handle --------------------------------------------------------

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

fn init_once() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = oslog::OsLogger::new("com.rust.player")
            .level_filter(log::LevelFilter::Info)
            .init();
        std::panic::set_hook(Box::new(|info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown>".to_string());
            log::error!("RUST PANIC at {}: {}", location, info);
        }));
    });
}

struct Handle {
    bridge: BridgeHandle,
    _host: Arc<IosHost>,
}

/// Run an FFI export body, turning a Rust panic into `default`.
///
/// Unwinding out of an `extern "C"` function aborts the process, so any
/// panic (a failed GPU adapter in `rustplayer_player_create`, an unexpected
/// unwrap deeper down) killed the host app. The panic hook set in
/// `init_once` still logs the message and location. Mirrors the Android
/// shell's guard.
fn ffi_guard<R>(name: &str, default: R, body: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(r) => r,
        Err(_) => {
            log::error!("[ffi] {} panicked; returning a default instead of aborting", name);
            default
        }
    }
}

unsafe fn handle_ref<'a>(handle: *mut c_void) -> Option<&'a Handle> {
    if handle.is_null() {
        None
    } else {
        Some(&*(handle as *const Handle))
    }
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Read a NUL-terminated flat `[k0,v0,k1,v1,...,NULL]` C array into pairs.
/// A trailing key with no value (odd count before the NULL) is dropped.
///
/// SAFETY: `headers` is either NULL or points to such an array, valid for the
/// duration of this call.
unsafe fn read_flat_headers(headers: *const *const c_char) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if headers.is_null() {
        return out;
    }
    let mut i = 0isize;
    loop {
        let kp = *headers.offset(i);
        if kp.is_null() {
            break;
        }
        let vp = *headers.offset(i + 1);
        if vp.is_null() {
            break; // dangling key without a value
        }
        out.push((cstr(kp), cstr(vp)));
        i += 2;
    }
    out
}

// --- lifecycle ---------------------------------------------------------------

/// Create a player rendering into `metal_layer` (a `CAMetalLayer*`), wire the
/// host callbacks, and start the bundled encrypted test stream. Returns an
/// opaque handle (or NULL on failure). The host guarantees the layer + `user`
/// outlive the player.
///
/// Declare in the Obj-C host (see the `extern` decls in main.m, or include
/// `rustplayer_ffi.h`).
#[no_mangle]
pub extern "C" fn rustplayer_player_create(
    metal_layer: *mut c_void,
    width: u32,
    height: u32,
    manifest_url: *const c_char,
    start_fraction: f32,        // < 0 = no resume
    audio_passthrough: i32,     // -1 = default, 0 = off, 1 = on
    auto_select_subtitle: bool,
    intercept_cb: InterceptCb,
    resolve_key_cb: ResolveKeyCb,
    event_cb: EventCb,
    user: *mut c_void,
) -> *mut c_void {
    ffi_guard("rustplayer_player_create", std::ptr::null_mut(), move || {
        init_once();
        if metal_layer.is_null() {
            log::error!("rustplayer_player_create: null metal_layer");
            return std::ptr::null_mut();
        }
        let manifest = unsafe { cstr(manifest_url) };
        if manifest.is_empty() {
            log::error!("rustplayer_player_create: empty manifest_url");
            return std::ptr::null_mut();
        }
        log::info!("rustplayer_player_create: {}x{} url={}", width, height, manifest);

        let host = Arc::new(IosHost {
            intercept_cb,
            resolve_key_cb,
            event_cb,
            user: RwLock::new(Some(UserPtr(user))),
        });

        let _guard = runtime().enter();
        let player = Player::new_from_metal_layer(metal_layer, width.max(1), height.max(1));
        let config = StartConfig {
            start_position: None,
            start_fraction: if start_fraction >= 0.0 {
                Some(start_fraction)
            } else {
                None
            },
            audio_passthrough: match audio_passthrough {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            },
            auto_select_subtitle,
            // iOS bridge does not yet expose language prefs; wire when needed.
            ..Default::default()
        };
        let bridge = bridge::start(player, manifest, host.clone(), config);

        Box::into_raw(Box::new(Handle {
            bridge,
            _host: host,
        })) as *mut c_void
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_set_size(handle: *mut c_void, width: u32, height: u32, _scale: f32) {
    ffi_guard("rustplayer_player_set_size", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let _guard = runtime().enter();
            h.bridge.resize(width.max(1), height.max(1));
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_play(handle: *mut c_void) {
    ffi_guard("rustplayer_player_play", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let _guard = runtime().enter();
            h.bridge.play();
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_pause(handle: *mut c_void) {
    ffi_guard("rustplayer_player_pause", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let _guard = runtime().enter();
            h.bridge.pause();
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_is_paused(handle: *mut c_void) -> bool {
    ffi_guard("rustplayer_player_is_paused", false, move || {
        unsafe { handle_ref(handle) }
            .map(|h| h.bridge.is_paused())
            .unwrap_or(false)
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_seek_ms(handle: *mut c_void, position_ms: i64) {
    ffi_guard("rustplayer_player_seek_ms", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let _guard = runtime().enter();
            h.bridge.seek_ms(position_ms);
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_position_ms(handle: *mut c_void) -> i64 {
    ffi_guard("rustplayer_player_position_ms", 0, move || {
        unsafe { handle_ref(handle) }
            .map(|h| h.bridge.position_ms())
            .unwrap_or(0)
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_duration_ms(handle: *mut c_void) -> i64 {
    ffi_guard("rustplayer_player_duration_ms", 0, move || {
        unsafe { handle_ref(handle) }
            .map(|h| h.bridge.duration_ms())
            .unwrap_or(0)
    })
}

/// Wrapped ClearKey licence endpoint (docs/CLEARKEY_WRAPPED_LICENCE.md);
/// `hkdf_info` may be NULL for the default. Call right after create.
#[no_mangle]
pub extern "C" fn rustplayer_player_set_wrapped_licence(
    handle: *mut c_void,
    url: *const c_char,
    hkdf_info: *const c_char,
) {
    ffi_guard("rustplayer_player_set_wrapped_licence", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let url = unsafe { cstr(url) };
            if url.is_empty() {
                log::error!("rustplayer_player_set_wrapped_licence: empty url");
                return;
            }
            let info = if hkdf_info.is_null() {
                None
            } else {
                Some(unsafe { cstr(hkdf_info) }).filter(|s| !s.is_empty())
            };
            let _guard = runtime().enter();
            h.bridge.set_wrapped_licence(url, info);
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_set_volume(handle: *mut c_void, volume: f32) {
    ffi_guard("rustplayer_player_set_volume", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let _guard = runtime().enter();
            h.bridge.set_volume(volume);
        }
    })
}

/// Returns a heap C string the caller MUST free with [`rustplayer_string_free`].
#[no_mangle]
pub extern "C" fn rustplayer_player_tracks_json(handle: *mut c_void) -> *mut c_char {
    ffi_guard("rustplayer_player_tracks_json", std::ptr::null_mut(), move || {
        let json = unsafe { handle_ref(handle) }
            .map(|h| h.bridge.tracks_json())
            .unwrap_or_else(|| "{}".to_string());
        match CString::new(json) {
            Ok(c) => c.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// Buffer size, refill policy and network-outage tolerance
/// (`player::BufferConfig`); 0 keeps a field's default, `min_secs` 0 = fill
/// continuously. Call right after create (the first pipeline starts once the
/// manifest is in); later calls apply from the next seek or track change.
#[no_mangle]
pub extern "C" fn rustplayer_player_set_buffer_config(
    handle: *mut c_void,
    max_secs: u32,
    min_secs: u32,
    max_mb: u32,
    outage_secs: u32,
) {
    ffi_guard("rustplayer_player_set_buffer_config", (), move || {
        let Some(h) = (unsafe { handle_ref(handle) }) else { return };
        let d = player::BufferConfig::default();
        let max_secs = if max_secs > 0 { max_secs } else { d.max_secs };
        h.bridge.player().set_buffer_config(player::BufferConfig {
            max_secs,
            min_secs: if min_secs > 0 { min_secs } else { max_secs },
            max_bytes: if max_mb > 0 { max_mb as u64 * 1_048_576 } else { d.max_bytes },
            network_outage_secs: if outage_secs > 0 { outage_secs } else { d.network_outage_secs },
        });
    })
}

/// Debug HUD snapshot JSON (see `BridgeHandle::debug_json`). Returns a heap
/// C string the caller MUST free with [`rustplayer_string_free`].
#[no_mangle]
pub extern "C" fn rustplayer_player_debug_json(handle: *mut c_void) -> *mut c_char {
    ffi_guard("rustplayer_player_debug_json", std::ptr::null_mut(), move || {
        let json = unsafe { handle_ref(handle) }
            .map(|h| h.bridge.debug_json())
            .unwrap_or_else(|| "{}".to_string());
        CString::new(json).map(CString::into_raw).unwrap_or(std::ptr::null_mut())
    })
}

/// Debug HUD text with the `events` newest event-log lines. Free with
/// [`rustplayer_string_free`].
#[no_mangle]
pub extern "C" fn rustplayer_player_debug_text(handle: *mut c_void, events: u32) -> *mut c_char {
    ffi_guard("rustplayer_player_debug_text", std::ptr::null_mut(), move || {
        let text = unsafe { handle_ref(handle) }
            .map(|h| h.bridge.debug_text(events as usize))
            .unwrap_or_default();
        CString::new(text).map(CString::into_raw).unwrap_or(std::ptr::null_mut())
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_string_free(s: *mut c_char) {
    ffi_guard("rustplayer_string_free", (), move || {
        if !s.is_null() {
            unsafe {
                drop(CString::from_raw(s));
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_select_video(handle: *mut c_void, adapt: u32, repr: u32, soft: bool) {
    ffi_guard("rustplayer_player_select_video", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            if soft {
                h.bridge.set_video_track_soft(adapt as usize, repr as usize);
            } else {
                h.bridge.set_video_track(adapt as usize, repr as usize);
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_select_video_auto(handle: *mut c_void) {
    ffi_guard("rustplayer_player_select_video_auto", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.set_video_auto();
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_select_audio(handle: *mut c_void, adapt: u32, repr: u32) {
    ffi_guard("rustplayer_player_select_audio", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.set_audio_track(adapt as usize, repr as usize);
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_select_subtitle(handle: *mut c_void, adapt: u32, repr: u32) {
    ffi_guard("rustplayer_player_select_subtitle", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.set_subtitle_track(adapt as usize, repr as usize);
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_clear_subtitles(handle: *mut c_void) {
    ffi_guard("rustplayer_player_clear_subtitles", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.clear_subtitles();
        }
    })
}

// --- generic player knobs ---

/// ARGB ints (like Android `Color` / ExoPlayer `CaptionStyleCompat`).
#[no_mangle]
pub extern "C" fn rustplayer_player_set_subtitle_style(
    handle: *mut c_void,
    text_argb: i32,
    outline_argb: i32,
    size_scale: f32,
) {
    ffi_guard("rustplayer_player_set_subtitle_style", (), move || {
        let Some(h) = (unsafe { handle_ref(handle) }) else {
            return;
        };
        fn argb_to_rgba(c: i32) -> [u8; 4] {
            let c = c as u32;
            [
                ((c >> 16) & 0xff) as u8,
                ((c >> 8) & 0xff) as u8,
                (c & 0xff) as u8,
                ((c >> 24) & 0xff) as u8,
            ]
        }
        let style = SubtitleStyle {
            text_color: argb_to_rgba(text_argb),
            outline_color: argb_to_rgba(outline_argb),
            size_scale,
            ..SubtitleStyle::DEFAULT
        }
        .sanitised();
        h.bridge.player().set_subtitle_style(style);
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_set_subtitle_safe_inset_bottom(handle: *mut c_void, bottom_px: u32) {
    ffi_guard("rustplayer_player_set_subtitle_safe_inset_bottom", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.player().set_subtitle_safe_insets(bottom_px);
        }
    })
}

/// Debug/compat: force HDR (PQ/HLG) video to decode to an 8-bit
/// destination. The in-player HDR→SDR tonemap still runs — colours stay
/// correct, at 8-bit quantization of the PQ signal. Sampled at decoder
/// configure time, so it applies from the next pipeline (re)build
/// (play / retry / ABR swap). See player/HDR_TONEMAP.md.
#[no_mangle]
pub extern "C" fn rustplayer_player_set_hdr_decode_8bit(handle: *mut c_void, enabled: bool) {
    ffi_guard("rustplayer_player_set_hdr_decode_8bit", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.player().set_hdr_decode_8bit(enabled);
        }
    })
}

/// Verbose logging toggle (default off → per-frame spam gated).
#[no_mangle]
pub extern "C" fn rustplayer_player_set_verbose_logging(enabled: bool) {
    ffi_guard("rustplayer_player_set_verbose_logging", (), move || {
        log::set_max_level(if enabled {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        });
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_player_destroy(handle: *mut c_void) {
    ffi_guard("rustplayer_player_destroy", (), move || {
        if handle.is_null() {
            return;
        }
        let _guard = runtime().enter();
        let h = unsafe { Box::from_raw(handle as *mut Handle) };
        h.bridge.shutdown();
        // The Swift side frees `user` right after this returns, while the
        // orchestrator is still stopping. Close the host first.
        h._host.close();
        drop(h);
    })
}

// --- host → Rust completion callbacks (async token bridge) -------------------

/// C mirror of [`PreparedRequest`] (see `RustPlayerPreparedRequest` in
/// rustplayer_ffi.h). All fields but `url` are optional; the host fills it in
/// its request filter and hands a pointer to [`rustplayer_intercept_complete`].
#[repr(C)]
pub struct RustPlayerPreparedRequest {
    url: *const c_char,
    headers: *const *const c_char, // flat [k0,v0,...,NULL] or NULL
    method: *const c_char,         // "GET"/"POST"/... or NULL
    body: *const u8,               // optional; NULL = none
    body_len: usize,
}

#[no_mangle]
pub extern "C" fn rustplayer_intercept_complete(token: u64, prepared: *const RustPlayerPreparedRequest) {
    ffi_guard("rustplayer_intercept_complete", (), move || {
        if prepared.is_null() {
            rustplayer_intercept_fail(token, std::ptr::null());
            return;
        }
        // SAFETY: the host keeps `prepared` and everything it points at alive for
        // the duration of this call (see the header contract).
        let p = unsafe { &*prepared };
        let url = unsafe { cstr(p.url) };
        let headers = unsafe { read_flat_headers(p.headers) };
        let method = if p.method.is_null() {
            None
        } else {
            // Unparseable method strings fall back to the kind default rather than
            // failing the request.
            Method::from_bytes(unsafe { cstr(p.method) }.as_bytes()).ok()
        };
        let body = if p.body.is_null() || p.body_len == 0 {
            None
        } else {
            let slice = unsafe { std::slice::from_raw_parts(p.body, p.body_len) };
            Some(Bytes::copy_from_slice(slice))
        };
        if let Some(tx) = intercept_registry().lock().unwrap().remove(&token) {
            let _ = tx.send(Ok(PreparedRequest {
                url,
                headers,
                method,
                body,
            }));
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_intercept_fail(token: u64, message: *const c_char) {
    ffi_guard("rustplayer_intercept_fail", (), move || {
        let m = unsafe { cstr(message) };
        if let Some(tx) = intercept_registry().lock().unwrap().remove(&token) {
            let _ = tx.send(Err(m));
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_resolve_key_complete(token: u64, key16: *const u8) {
    ffi_guard("rustplayer_resolve_key_complete", (), move || {
        if key16.is_null() {
            rustplayer_resolve_key_fail(token, std::ptr::null());
            return;
        }
        let mut key = [0u8; 16];
        unsafe { std::ptr::copy_nonoverlapping(key16, key.as_mut_ptr(), 16) };
        if let Some(tx) = resolve_registry().lock().unwrap().remove(&token) {
            let _ = tx.send(Ok(key));
        }
    })
}

#[no_mangle]
pub extern "C" fn rustplayer_resolve_key_fail(token: u64, message: *const c_char) {
    ffi_guard("rustplayer_resolve_key_fail", (), move || {
        let m = unsafe { cstr(message) };
        if let Some(tx) = resolve_registry().lock().unwrap().remove(&token) {
            let _ = tx.send(Err(if m.is_empty() {
                "resolve_key failed".to_string()
            } else {
                m
            }));
        }
    })
}
