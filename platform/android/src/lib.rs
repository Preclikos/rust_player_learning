// Android bridge — EMBEDDED into a host's Surfaces (no winit).
//
// This is the Android shell of the unified bridge core (the `bridge` crate),
// exposing a GENERIC, ExoPlayer/Shaka-style player over JNI: the host provides
// a manifest URL + a request/key provider, and the library plays it. NO
// app-specific concepts (auth, CDN, DRM endpoints) live here — those go in the
// host's provider hooks (`onRequest` / `resolveKey`), invisible to the player.
//
// JNI symbols: Java_cz_preclikos_rustplayer_NativeBridge_*. The idiomatic API
// is the Kotlin `RustPlayer` wrapper; the `:app` smoke test is just one consumer.

#![cfg(target_os = "android")]

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, OnceLock};

use bridge::{
    self, BoxError, BridgeHandle, BridgeHost, PreparedRequest, RequestKind, StartConfig,
};
use async_trait::async_trait;
use jni::objects::{JByteArray, JClass, JObject, JObjectArray, JString, JValue};
use jni::refs::Global;
use jni::sys::{jboolean, jfloat, jint, jlong, jstring};
use jni::{jni_sig, jni_str, Env, EnvUnowned, JavaVM, Outcome};
use player::{Player, SubtitleStyle};

/// Player bridge + the `ANativeWindow` refs it renders into.
struct Handle {
    bridge: BridgeHandle,
    /// Keeps the host callback object + JavaVM alive for the player's lifetime.
    _host: Arc<AndroidHost>,
    /// Overlay (wgpu/GLES) window — UI/subtitles, or video in non-direct mode.
    /// Swappable at runtime (`setOverlaySurface`) like the video window.
    native_window: AtomicPtr<ndk_sys::ANativeWindow>,
    /// Video plane window — MediaCodec renders into it in direct mode. Swappable
    /// at runtime (`setVideoSurface`), so behind an atomic with old-ref release.
    video_window: AtomicPtr<ndk_sys::ANativeWindow>,
}

/// Bridges the platform-agnostic [`BridgeHost`] to a Kotlin provider object:
/// `onEvent(String)` (events), `onRequest(String,int)->String[]` (URL rewrite +
/// headers), `resolveKey([B)->[B` (DRM key). All generic — no app knowledge.
struct AndroidHost {
    vm: JavaVM,
    /// Global ref to the Kotlin provider bridge passed to `nativeStart`. Shared
    /// with the blocking-pool upcalls, which need an owned `'static` handle.
    cb: Arc<Global<JObject<'static>>>,
}

fn request_kind_int(kind: RequestKind) -> i32 {
    match kind {
        RequestKind::Manifest => 0,
        RequestKind::InitSegment => 1,
        RequestKind::Segment => 2,
        RequestKind::License => 3,
    }
}

#[async_trait]
impl BridgeHost for AndroidHost {
    fn on_event(&self, json: String) {
        // The callback runs in its own local frame, so the event string is
        // freed per event. Before, on a runtime worker that stays attached,
        // every event's string stayed referenced until the thread ended.
        let _ = self.vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let jstr = env.new_string(&json)?;
            env.call_method(
                self.cb.as_obj(),
                jni_str!("onEvent"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(&jstr)],
            )?;
            Ok(())
        });
    }

    async fn intercept(
        &self,
        url: String,
        kind: RequestKind,
    ) -> Result<PreparedRequest, BoxError> {
        // The JNI upcall runs on the BLOCKING pool, not a runtime worker: the
        // host's onRequest typically performs a synchronous network round-trip
        // (link resolution, token refresh). At playback start 4-6 segment
        // requests intercept CONCURRENTLY (buffer fill, cold caches) — run
        // inline they block every runtime worker at once, the timer driver
        // included, and the whole pipeline (vsync pacing, audio feed, all
        // watchdogs) freezes into the ~1 fps startup convoy documented in
        // docs/handoffs/AUDIO_PAUSE_WEDGE_AND_STARTUP_CONVOY.md.
        let cb = self.cb.clone();
        tokio::task::spawn_blocking(move || -> Result<PreparedRequest, BoxError> {
            vm_from_ndk_context().attach_current_thread(|env| -> Result<PreparedRequest, BoxError> {
                let jurl = env.new_string(&url)?;
                let obj = env
                    .call_method(
                        cb.as_obj(),
                        jni_str!("onRequest"),
                        jni_sig!("(Ljava/lang/String;I)[Ljava/lang/String;"),
                        &[JValue::Object(&jurl), JValue::Int(request_kind_int(kind))],
                    )?
                    .l()?;
                if obj.is_null() {
                    return Ok(PreparedRequest { url, ..Default::default() });
                }
                let arr = env.cast_local::<JObjectArray<JString>>(obj)?;
                let len = arr.len(env)?;
                if len < 1 {
                    return Ok(PreparedRequest { url, ..Default::default() });
                }
                let elem = |env: &mut Env, i: usize| -> jni::errors::Result<String> {
                    arr.get_element(env, i)?.try_to_string(env)
                };
                let new_url = elem(env, 0)?;
                let mut headers = Vec::new();
                let mut i = 1;
                while i + 1 < len {
                    headers.push((elem(env, i)?, elem(env, i + 1)?));
                    i += 2;
                }
                Ok(PreparedRequest {
                    url: new_url,
                    headers,
                    ..Default::default()
                })
            })
        })
        .await
        .map_err(|e| -> BoxError { format!("intercept join: {e}").into() })?
    }

    async fn resolve_key(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        // Blocking pool for the same reason as `intercept`: the licence upcall
        // does a synchronous HTTP POST in the host.
        let cb = self.cb.clone();
        tokio::task::spawn_blocking(move || -> Result<[u8; 16], BoxError> {
            vm_from_ndk_context().attach_current_thread(|env| -> Result<[u8; 16], BoxError> {
                let jkid = env.byte_array_from_slice(&kid)?;
                let obj = env
                    .call_method(
                        cb.as_obj(),
                        jni_str!("resolveKey"),
                        jni_sig!("([B)[B"),
                        &[JValue::Object(&jkid)],
                    )?
                    .l()?;
                if obj.is_null() {
                    return Err("provider.resolveKey returned null (no key)".into());
                }
                let arr = env.cast_local::<JByteArray>(obj)?;
                let bytes = env.convert_byte_array(&arr)?;
                <[u8; 16]>::try_from(bytes.as_slice()).map_err(|_| {
                    format!("resolveKey returned {} bytes, expected 16", bytes.len()).into()
                })
            })
        })
        .await
        .map_err(|e| -> BoxError { format!("resolve_key join: {e}").into() })?
    }
}

/// The process-wide `JavaVM` from `ndk_context` (seeded in `init_ndk_context`).
/// Used by blocking-pool upcalls, which can't borrow `&self` across the
/// `spawn_blocking` 'static boundary.
fn vm_from_ndk_context() -> JavaVM {
    let ctx = ndk_context::android_context();
    unsafe { JavaVM::from_raw(ctx.vm().cast()) }
}

/// Dedicated multi-thread Tokio runtime (the host owns the UI looper).
///
/// Worker floor of 6 (not the core-count default): the MediaCodec decode paths
/// sit in blocking dequeue/retry loops INSIDE async task polls (mediacodec.rs /
/// mediacodec_audio.rs use `std::thread::sleep` + blocking NDK dequeues), so a
/// worker is held for the whole wait. On a low-core TV SoC the default pool is
/// 2-4 workers — when the video and audio decoders both stall (video waits for
/// the sync loop to release codec buffers; audio waits on a full channel), ALL
/// workers are held, the reactive tasks (vsync pacing, audio_sync, av_sync,
/// every timer) stop being polled entirely and playback freezes at ~1 fps with
/// the process idle. Observed as a ~40% startup race on the Google TV Streamer
/// (see docs/handoffs/AUDIO_PAUSE_WEDGE_AND_STARTUP_CONVOY.md). The floor keeps
/// headroom so the chronically-blocking polls can never exhaust the pool; the
/// long-term fix is moving those dequeue loops onto `spawn_blocking`.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .max(6);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("rustplayer-rt")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Seed `ndk_context` with (JavaVM, Context) so cpal et al. resolve the runtime.
fn init_ndk_context(env: &mut Env, context: &JObject) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let vm = env.get_java_vm().expect("get_java_vm");
        // Leaked on purpose: ndk_context holds the Context for the process.
        let ctx_raw = env
            .new_global_ref(context)
            .expect("new_global_ref(context)")
            .into_raw() as *mut c_void;
        unsafe {
            ndk_context::initialize_android_context(vm.get_raw() as *mut c_void, ctx_raw);
        }
    });
}

/// `nativeSetVerboseLogging` state, kept so a call made before the first
/// `nativeStart` (the natural place for it) survives `init_logging`.
static VERBOSE_LOGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn init_logging() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // The logger itself passes everything; the effective level is the
        // `log` crate's global max, Info by default and Debug under
        // nativeSetVerboseLogging — configuring the logger at Info here
        // would silently defeat that switch.
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Trace),
        );
        log::set_max_level(if VERBOSE_LOGGING.load(std::sync::atomic::Ordering::Relaxed) {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        });
        std::panic::set_hook(Box::new(|info| {
            log::error!("rust panic: {}", info);
        }));
        log::info!("rustplayer: android bridge shell loaded");
    });
}

/// Run a JNI export body, turning a Rust panic into `default`.
///
/// Unwinding out of an `extern "system"` function aborts the whole process, so
/// before this guard any panic (a failed GPU adapter in nativeStart, an
/// unexpected unwrap deeper down) killed the host app with no Java exception.
/// The panic hook set in JNI_OnLoad still logs the message and location.
fn ffi_guard<R>(name: &str, default: R, body: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(r) => r,
        Err(_) => {
            log::error!("[jni] {} panicked; returning a default instead of aborting", name);
            default
        }
    }
}

/// [`ffi_guard`] for an export that needs the `Env`: the body gets it and
/// handles its own JNI errors; a panic returns `default`.
fn with_env_or<'local, R>(
    name: &str,
    env: &mut EnvUnowned<'local>,
    default: R,
    body: impl FnOnce(&mut Env<'local>) -> R,
) -> R {
    match env.with_env(|env| -> jni::errors::Result<R> { Ok(body(env)) }).into_outcome() {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => {
            log::error!("[jni] {}: {}", name, e);
            default
        }
        Outcome::Panic(_) => {
            log::error!("[jni] {} panicked; returning a default instead of aborting", name);
            default
        }
    }
}

unsafe fn handle_ref<'a>(handle: jlong) -> Option<&'a Handle> {
    if handle == 0 {
        None
    } else {
        Some(&*(handle as *const Handle))
    }
}

/// `nativeStart(Context, provider, overlaySurface, videoSurface, w, h, hdrTypes,
/// manifestUrl, startFraction, audioPassthrough, autoSelectSubtitle) -> long`.
///
/// Builds a player rendering into the surfaces, wires the generic provider, and
/// starts `manifestUrl`. `startFraction` < 0 = no resume; `audioPassthrough`
/// -1 = library default, 0/1 = off/on. Returns an opaque handle or 0.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeStart<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    context: JObject<'local>,
    bridge_cb: JObject<'local>,
    surface: JObject<'local>,
    video_surface: JObject<'local>,
    width: jint,
    height: jint,
    display_hdr_types: jint,
    manifest_url: JString<'local>,
    start_fraction: jfloat,
    audio_passthrough: jint,
    auto_select_subtitle: jboolean,
    preferred_audio_lang: JString<'local>,
    preferred_subtitle_lang: JString<'local>,
) -> jlong {
    with_env_or("nativeStart", &mut env, 0, move |env| {
        init_logging();
        init_ndk_context(env, &context);

        let manifest: String = match manifest_url.try_to_string(env) {
            Ok(s) => s,
            Err(_) => {
                log::error!("nativeStart: manifestUrl missing");
                return 0;
            }
        };

        // Optional BCP-47 language prefs (null / "" → None). Applied during default
        // selection so no post-start selectAudio/selectSubtitle rebuild is needed.
        let opt_lang = |env: &Env, s: &JString| -> Option<String> {
            s.try_to_string(env).ok().filter(|s| !s.is_empty())
        };
        let preferred_audio_language = opt_lang(env, &preferred_audio_lang);
        let preferred_subtitle_language = opt_lang(env, &preferred_subtitle_lang);

        let native_window = unsafe {
            ndk_sys::ANativeWindow_fromSurface(env.get_raw() as *mut _, surface.as_raw() as *mut _)
        };
        if native_window.is_null() {
            log::error!("nativeStart: ANativeWindow_fromSurface returned null");
            return 0;
        }
        // A null video Surface = no video plane: frames go through ImageReader
        // and are drawn with GLES into the overlay (the path direct mode
        // replaced; kept reachable for diagnostics and single-surface hosts).
        let video_window = if video_surface.is_null() {
            std::ptr::null_mut()
        } else {
            let w = unsafe {
                ndk_sys::ANativeWindow_fromSurface(env.get_raw() as *mut _, video_surface.as_raw() as *mut _)
            };
            if w.is_null() {
                log::error!("nativeStart: video ANativeWindow_fromSurface returned null");
                unsafe { ndk_sys::ANativeWindow_release(native_window) };
                return 0;
            }
            w
        };

        let w = width.max(1) as u32;
        let h = height.max(1) as u32;
        log::info!("nativeStart: {}x{} hdr={:#06b} url={}", w, h, display_hdr_types, manifest);

        let vm = match env.get_java_vm() {
            Ok(vm) => vm,
            Err(e) => {
                log::error!("nativeStart: get_java_vm: {}", e);
                unsafe {
                    ndk_sys::ANativeWindow_release(native_window);
                    if !video_window.is_null() {
                        ndk_sys::ANativeWindow_release(video_window);
                    }
                }
                return 0;
            }
        };
        let cb = match env.new_global_ref(&bridge_cb) {
            Ok(g) => g,
            Err(e) => {
                log::error!("nativeStart: new_global_ref(provider): {}", e);
                unsafe {
                    ndk_sys::ANativeWindow_release(native_window);
                    if !video_window.is_null() {
                        ndk_sys::ANativeWindow_release(video_window);
                    }
                }
                return 0;
            }
        };
        let host = Arc::new(AndroidHost { vm, cb: Arc::new(cb) });

        let _guard = runtime().enter();
        let player = Player::new_from_android_surface(native_window as *mut c_void, w, h);

        if display_hdr_types != 0 {
            player.set_display_hdr_types(display_hdr_types as u32);
        }
        // Direct MediaCodec→Surface mode is the production path (HW video plane →
        // native HDR/DV); the host detaches via setVideoSurface(null). Without a
        // video Surface at start the player renders through the overlay.
        if video_window.is_null() {
            log::info!("nativeStart: no video Surface, frames are drawn into the overlay (GLES)");
        } else {
            player.set_video_output_window(video_window as *mut c_void);
        }

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
            preferred_audio_language,
            preferred_subtitle_language,
            // Set after create via nativeSetWrappedLicence (fixed create signature).
            wrapped_licence_url: None,
            wrapped_licence_hkdf_info: None,
        };

        let bridge = bridge::start(player, manifest, host.clone(), config);

        let handle = Box::new(Handle {
            bridge,
            _host: host,
            native_window: AtomicPtr::new(native_window),
            video_window: AtomicPtr::new(video_window),
        });
        Box::into_raw(handle) as jlong
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetSize(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    width: jint,
    height: jint,
) {
    ffi_guard("nativeSetSize", (), move || {
        let Some(h) = (unsafe { handle_ref(handle) }) else {
            return;
        };
        let _guard = runtime().enter();
        h.bridge.resize(width.max(1) as u32, height.max(1) as u32);
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativePlay(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
) {
    ffi_guard("nativePlay", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let _guard = runtime().enter();
            h.bridge.play();
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativePause(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
) {
    ffi_guard("nativePause", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let _guard = runtime().enter();
            h.bridge.pause();
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeIsPaused(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
) -> jboolean {
    ffi_guard("nativeIsPaused", false, move || {
        unsafe { handle_ref(handle) }.is_some_and(|h| h.bridge.is_paused())
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSeekMs(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    position_ms: jlong,
) {
    ffi_guard("nativeSeekMs", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let _guard = runtime().enter();
            h.bridge.seek_ms(position_ms);
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativePositionMs(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
) -> jlong {
    ffi_guard("nativePositionMs", 0, move || {
        unsafe { handle_ref(handle) }
            .map(|h| h.bridge.position_ms())
            .unwrap_or(0)
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeDurationMs(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
) -> jlong {
    ffi_guard("nativeDurationMs", 0, move || {
        unsafe { handle_ref(handle) }
            .map(|h| h.bridge.duration_ms())
            .unwrap_or(0)
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVolume(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    volume: jfloat,
) {
    ffi_guard("nativeSetVolume", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            let _guard = runtime().enter();
            h.bridge.set_volume(volume);
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeGetTracksJson<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    with_env_or("nativeGetTracksJson", &mut env, std::ptr::null_mut(), move |env| {
        let json = unsafe { handle_ref(handle) }
            .map(|h| h.bridge.tracks_json())
            .unwrap_or_else(|| "{}".to_string());
        match env.new_string(json) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVideoTrack(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    adapt: jint,
    repr: jint,
) {
    ffi_guard("nativeSetVideoTrack", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.set_video_track(adapt.max(0) as usize, repr.max(0) as usize);
        }
    })
}

/// Soft (ABR-style) video switch: the running supervisor swaps the
/// representation make-before-break, audio and the A/V clock stay up, and no
/// pipeline is rebuilt. Exposed so the switch-quality harness can trigger the
/// seamless path deterministically instead of having to provoke the bandwidth
/// estimator into doing it.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVideoTrackSoft(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    adapt: jint,
    repr: jint,
) {
    ffi_guard("nativeSetVideoTrackSoft", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge
                .set_video_track_soft(adapt.max(0) as usize, repr.max(0) as usize);
        }
    })
}

/// Wrapped ClearKey licence endpoint (docs/CLEARKEY_WRAPPED_LICENCE.md).
/// `info` may be null for the default HKDF info. Call right after nativeStart.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetWrappedLicence<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    url: JString<'local>,
    info: JString<'local>,
) {
    with_env_or("nativeSetWrappedLicence", &mut env, (), move |env| {
        let Some(h) = (unsafe { handle_ref(handle) }) else { return };
        let url: String = match url.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                log::error!("nativeSetWrappedLicence: url: {}", e);
                return;
            }
        };
        let info: Option<String> = if info.is_null() {
            None
        } else {
            info.try_to_string(env).ok().filter(|s| !s.is_empty())
        };
        let _guard = runtime().enter();
        h.bridge.set_wrapped_licence(url, info);
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVideoAuto(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
) {
    ffi_guard("nativeSetVideoAuto", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.set_video_auto();
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetAudioTrack(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    adapt: jint,
    repr: jint,
) {
    ffi_guard("nativeSetAudioTrack", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.set_audio_track(adapt.max(0) as usize, repr.max(0) as usize);
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetSubtitleTrack(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    adapt: jint,
    repr: jint,
) {
    ffi_guard("nativeSetSubtitleTrack", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.set_subtitle_track(adapt.max(0) as usize, repr.max(0) as usize);
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeClearSubtitles(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
) {
    ffi_guard("nativeClearSubtitles", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.clear_subtitles();
        }
    })
}

// --- generic player knobs (parity with ExoPlayer surface/track/format API) ---

/// Re-point (or detach with a null surface) the MediaCodec video plane. Use on
/// a surface swap / background→foreground; pass null to stop rendering to an
/// abandoned window.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVideoOutputWindow(
    env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    surface: JObject,
) {
    ffi_guard("nativeSetVideoOutputWindow", (), move || {
        let Some(h) = (unsafe { handle_ref(handle) }) else {
            return;
        };
        let new_window = if surface.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe {
                ndk_sys::ANativeWindow_fromSurface(env.as_raw() as *mut _, surface.as_raw() as *mut _)
            }
        };
        let _guard = runtime().enter();
        h.bridge
            .player()
            .set_video_output_window(new_window as *mut c_void);
        let old = h.video_window.swap(new_window, Ordering::AcqRel);
        if !old.is_null() {
            unsafe { ndk_sys::ANativeWindow_release(old) };
        }
    })
}

/// Re-point (or detach with a null surface) the overlay / presentation window
/// the renderer draws into — the picture itself on the GLES path, subtitles in
/// direct mode. Call with null from `surfaceDestroyed` (returns once the
/// renderer no longer touches the old window) and with the new Surface from
/// `surfaceCreated`, e.g. after Home → back with the player kept alive.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetOverlayWindow(
    env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    surface: JObject,
) {
    ffi_guard("nativeSetOverlayWindow", (), move || {
        let Some(h) = (unsafe { handle_ref(handle) }) else {
            return;
        };
        let new_window = if surface.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe {
                ndk_sys::ANativeWindow_fromSurface(env.as_raw() as *mut _, surface.as_raw() as *mut _)
            }
        };
        let _guard = runtime().enter();
        // Blocks until the renderer has left the previous window, so it can be
        // released right after.
        h.bridge
            .player()
            .set_android_overlay_window(new_window as *mut c_void);
        let old = h.native_window.swap(new_window, Ordering::AcqRel);
        if !old.is_null() {
            unsafe { ndk_sys::ANativeWindow_release(old) };
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetSubtitleSafeInsetBottom(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    bottom_px: jint,
) {
    ffi_guard("nativeSetSubtitleSafeInsetBottom", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.player().set_subtitle_safe_insets(bottom_px.max(0) as u32);
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetAdaptiveFrameRate(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    enabled: jboolean,
) {
    ffi_guard("nativeSetAdaptiveFrameRate", (), move || {
        if let Some(h) = unsafe { handle_ref(handle) } {
            h.bridge.player().set_adaptive_frame_rate(enabled);
        }
    })
}

/// ARGB ints (Android `Color`), like ExoPlayer `CaptionStyleCompat`.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetSubtitleStyle(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
    text_argb: jint,
    outline_argb: jint,
    size_scale: jfloat,
) {
    ffi_guard("nativeSetSubtitleStyle", (), move || {
        let Some(h) = (unsafe { handle_ref(handle) }) else {
            return;
        };
        fn argb_to_rgba(c: jint) -> [u8; 4] {
            let c = c as u32;
            [
                ((c >> 16) & 0xff) as u8, // R
                ((c >> 8) & 0xff) as u8,  // G
                (c & 0xff) as u8,         // B
                ((c >> 24) & 0xff) as u8, // A
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

/// Verbose logging toggle (default off → per-frame vsync/HEALTH spam gated).
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVerboseLogging(
    _env: EnvUnowned,
    _class: JClass,
    enabled: jboolean,
) {
    ffi_guard("nativeSetVerboseLogging", (), move || {
        VERBOSE_LOGGING.store(enabled, std::sync::atomic::Ordering::Relaxed);
        log::set_max_level(if enabled {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        });
    })
}

/// `nativeDestroy(long)` — tear down and release the window refs.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeDestroy(
    _env: EnvUnowned,
    _class: JClass,
    handle: jlong,
) {
    ffi_guard("nativeDestroy", (), move || {
        if handle == 0 {
            return;
        }
        let _guard = runtime().enter();
        let h = unsafe { Box::from_raw(handle as *mut Handle) };
        let Handle {
            bridge,
            _host,
            native_window,
            video_window,
        } = *h;
        bridge.shutdown();
        drop(bridge);
        drop(_host);
        let vwin = video_window.load(Ordering::Acquire);
        let native_window = native_window.load(Ordering::Acquire);
        unsafe {
            if !native_window.is_null() {
                ndk_sys::ANativeWindow_release(native_window);
            }
            if !vwin.is_null() {
                ndk_sys::ANativeWindow_release(vwin);
            }
        }
    })
}
