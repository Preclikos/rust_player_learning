// iOS bridge — EMBEDDED into a host-owned UIView's CAMetalLayer (no winit).
//
// This is the iOS shell of the *unified bridge core* (`app_shared::bridge`),
// the FFI mirror of the Android shell. The Objective-C host (`ios/RustPlayer/
// main.m`) owns the app lifecycle and a `CAMetalLayer`, hands it to
// `bz_player_create`, and gets back an opaque handle it drives through the same
// unified control surface the Android JNI exposes.
//
// Events flow Rust→host as unified JSON via a C `event_cb`. The provider hooks
// (`intercept` / `resolve_key`) use an **async token bridge**: Rust fires the
// host callback with a token and awaits a oneshot; the host calls back
// `bz_intercept_complete(token, …)` / `bz_resolve_key_complete(token, …)`.
// (The test host completes them synchronously — passthrough + baked ClearKeys.)
//
//   * Build with `./ios/build_sim.sh` (links libapp_ios.a into the Obj-C app).
//
// On non-iOS targets this crate is a no-op so the workspace still builds.

#![cfg(target_os = "ios")]

use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use app_shared::bridge::{
    self, BoxError, BridgeHandle, BridgeHost, PreparedRequest, RequestKind, StartConfig,
};
use async_trait::async_trait;
use player::Player;
use tokio::sync::oneshot;

// --- C ABI callback types ----------------------------------------------------

/// `void (*)(void *user, const char *url, int kind, uint64_t token)`.
type InterceptCb = extern "C" fn(*mut c_void, *const c_char, c_int, u64);
/// `void (*)(void *user, const uint8_t kid[16], uint64_t token)`.
type ResolveKeyCb = extern "C" fn(*mut c_void, *const u8, u64);
/// `void (*)(void *user, const char *json_event)`.
type EventCb = extern "C" fn(*mut c_void, *const c_char);

/// Opaque host pointer (e.g. the Swift/ObjC controller). Raw pointers aren't
/// `Send`/`Sync`; the host guarantees it outlives the player, so we assert it.
struct UserPtr(*mut c_void);
unsafe impl Send for UserPtr {}
unsafe impl Sync for UserPtr {}

struct IosHost {
    intercept_cb: InterceptCb,
    resolve_key_cb: ResolveKeyCb,
    event_cb: EventCb,
    user: UserPtr,
}

#[async_trait]
impl BridgeHost for IosHost {
    fn on_event(&self, json: String) {
        if let Ok(c) = CString::new(json) {
            (self.event_cb)(self.user.0, c.as_ptr());
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
        let c_url = CString::new(url).map_err(|e| Box::new(e) as BoxError)?;
        (self.intercept_cb)(self.user.0, c_url.as_ptr(), kind_to_int(kind), token);
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
        (self.resolve_key_cb)(self.user.0, kid.as_ptr(), token);
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

// --- lifecycle ---------------------------------------------------------------

/// Create a player rendering into `metal_layer` (a `CAMetalLayer*`), wire the
/// host callbacks, and start the bundled encrypted test stream. Returns an
/// opaque handle (or NULL on failure). The host guarantees the layer + `user`
/// outlive the player.
///
/// Declare in the Obj-C host (see `blackzone_player.h`-style decls in main.m).
#[no_mangle]
pub extern "C" fn bz_player_create(
    metal_layer: *mut c_void,
    width: u32,
    height: u32,
    intercept_cb: InterceptCb,
    resolve_key_cb: ResolveKeyCb,
    event_cb: EventCb,
    user: *mut c_void,
) -> *mut c_void {
    init_once();
    if metal_layer.is_null() {
        log::error!("bz_player_create: null metal_layer");
        return std::ptr::null_mut();
    }
    log::info!("bz_player_create: {}x{}", width, height);

    let host = Arc::new(IosHost {
        intercept_cb,
        resolve_key_cb,
        event_cb,
        user: UserPtr(user),
    });

    let _guard = runtime().enter();
    let player = Player::new_from_metal_layer(metal_layer, width.max(1), height.max(1));
    let bridge = bridge::start(
        player,
        app_shared::TEST_MANIFEST_URL.to_string(),
        host.clone(),
        StartConfig::default(),
    );

    Box::into_raw(Box::new(Handle {
        bridge,
        _host: host,
    })) as *mut c_void
}

#[no_mangle]
pub extern "C" fn bz_player_set_size(handle: *mut c_void, width: u32, height: u32, _scale: f32) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        let _guard = runtime().enter();
        h.bridge.resize(width.max(1), height.max(1));
    }
}

#[no_mangle]
pub extern "C" fn bz_player_play(handle: *mut c_void) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        let _guard = runtime().enter();
        h.bridge.play();
    }
}

#[no_mangle]
pub extern "C" fn bz_player_pause(handle: *mut c_void) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        let _guard = runtime().enter();
        h.bridge.pause();
    }
}

#[no_mangle]
pub extern "C" fn bz_player_is_paused(handle: *mut c_void) -> bool {
    unsafe { handle_ref(handle) }
        .map(|h| h.bridge.is_paused())
        .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn bz_player_seek_ms(handle: *mut c_void, position_ms: i64) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        let _guard = runtime().enter();
        h.bridge.seek_ms(position_ms);
    }
}

#[no_mangle]
pub extern "C" fn bz_player_position_ms(handle: *mut c_void) -> i64 {
    unsafe { handle_ref(handle) }
        .map(|h| h.bridge.position_ms())
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn bz_player_duration_ms(handle: *mut c_void) -> i64 {
    unsafe { handle_ref(handle) }
        .map(|h| h.bridge.duration_ms())
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn bz_player_set_volume(handle: *mut c_void, volume: f32) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        let _guard = runtime().enter();
        h.bridge.set_volume(volume);
    }
}

/// Returns a heap C string the caller MUST free with [`bz_string_free`].
#[no_mangle]
pub extern "C" fn bz_player_tracks_json(handle: *mut c_void) -> *mut c_char {
    let json = unsafe { handle_ref(handle) }
        .map(|h| h.bridge.tracks_json())
        .unwrap_or_else(|| "{}".to_string());
    match CString::new(json) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn bz_string_free(s: *mut c_char) {
    if !s.is_null() {
        unsafe {
            drop(CString::from_raw(s));
        }
    }
}

#[no_mangle]
pub extern "C" fn bz_player_select_video(handle: *mut c_void, adapt: u32, repr: u32, soft: bool) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        if soft {
            h.bridge.set_video_track_soft(adapt as usize, repr as usize);
        } else {
            h.bridge.set_video_track(adapt as usize, repr as usize);
        }
    }
}

#[no_mangle]
pub extern "C" fn bz_player_select_video_auto(handle: *mut c_void) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.set_video_auto();
    }
}

#[no_mangle]
pub extern "C" fn bz_player_select_audio(handle: *mut c_void, adapt: u32, repr: u32) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.set_audio_track(adapt as usize, repr as usize);
    }
}

#[no_mangle]
pub extern "C" fn bz_player_select_subtitle(handle: *mut c_void, adapt: u32, repr: u32) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.set_subtitle_track(adapt as usize, repr as usize);
    }
}

#[no_mangle]
pub extern "C" fn bz_player_clear_subtitles(handle: *mut c_void) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.clear_subtitles();
    }
}

#[no_mangle]
pub extern "C" fn bz_player_destroy(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    let _guard = runtime().enter();
    let h = unsafe { Box::from_raw(handle as *mut Handle) };
    h.bridge.shutdown();
    drop(h);
}

// --- host → Rust completion callbacks (async token bridge) -------------------

#[no_mangle]
pub extern "C" fn bz_intercept_complete(token: u64, url: *const c_char) {
    let url = unsafe { cstr(url) };
    if let Some(tx) = intercept_registry().lock().unwrap().remove(&token) {
        let _ = tx.send(Ok(PreparedRequest {
            url,
            ..Default::default()
        }));
    }
}

#[no_mangle]
pub extern "C" fn bz_intercept_fail(token: u64, message: *const c_char) {
    let m = unsafe { cstr(message) };
    if let Some(tx) = intercept_registry().lock().unwrap().remove(&token) {
        let _ = tx.send(Err(m));
    }
}

#[no_mangle]
pub extern "C" fn bz_resolve_key_complete(token: u64, key16: *const u8) {
    if key16.is_null() {
        bz_resolve_key_fail(token, std::ptr::null());
        return;
    }
    let mut key = [0u8; 16];
    unsafe { std::ptr::copy_nonoverlapping(key16, key.as_mut_ptr(), 16) };
    if let Some(tx) = resolve_registry().lock().unwrap().remove(&token) {
        let _ = tx.send(Ok(key));
    }
}

#[no_mangle]
pub extern "C" fn bz_resolve_key_fail(token: u64, message: *const c_char) {
    let m = unsafe { cstr(message) };
    if let Some(tx) = resolve_registry().lock().unwrap().remove(&token) {
        let _ = tx.send(Err(if m.is_empty() {
            "resolve_key failed".to_string()
        } else {
            m
        }));
    }
}
