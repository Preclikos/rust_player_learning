// Android bridge — EMBEDDED into a host's Surfaces (no winit).
//
// This is the Android shell of the unified bridge core (`app_shared::bridge`),
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

use app_shared::bridge::{
    self, BoxError, BridgeHandle, BridgeHost, PreparedRequest, RequestKind, StartConfig,
};
use async_trait::async_trait;
use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JObjectArray, JString, JValue};
use jni::sys::{jboolean, jfloat, jint, jlong, jstring};
use jni::{JNIEnv, JavaVM};
use player::{Player, SubtitleStyle};

/// Player bridge + the `ANativeWindow` refs it renders into.
struct Handle {
    bridge: BridgeHandle,
    /// Keeps the host callback object + JavaVM alive for the player's lifetime.
    _host: Arc<AndroidHost>,
    /// Overlay (wgpu/GLES) window — UI/subtitles, or video in non-direct mode.
    native_window: *mut ndk_sys::ANativeWindow,
    /// Video plane window — MediaCodec renders into it in direct mode. Swappable
    /// at runtime (`setVideoSurface`), so behind an atomic with old-ref release.
    video_window: AtomicPtr<ndk_sys::ANativeWindow>,
}

/// Bridges the platform-agnostic [`BridgeHost`] to a Kotlin provider object:
/// `onEvent(String)` (events), `onRequest(String,int)->String[]` (URL rewrite +
/// headers), `resolveKey([B)->[B` (DRM key). All generic — no app knowledge.
struct AndroidHost {
    vm: JavaVM,
    /// Global ref to the Kotlin provider bridge passed to `nativeStart`.
    cb: GlobalRef,
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
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        if let Ok(jstr) = env.new_string(&json) {
            let _ = env.call_method(
                self.cb.as_obj(),
                "onEvent",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&jstr)],
            );
        }
    }

    async fn intercept(
        &self,
        url: String,
        kind: RequestKind,
    ) -> Result<PreparedRequest, BoxError> {
        // Synchronous JNI upcall (no await → future stays Send). Kotlin returns
        // a flat String[]: [finalUrl, k1, v1, k2, v2, …]. null/empty = passthrough.
        let mut env = self.vm.attach_current_thread()?;
        let jurl = env.new_string(&url)?;
        let res = env.call_method(
            self.cb.as_obj(),
            "onRequest",
            "(Ljava/lang/String;I)[Ljava/lang/String;",
            &[JValue::Object(&jurl), JValue::Int(request_kind_int(kind))],
        )?;
        let obj = res.l()?;
        if obj.is_null() {
            return Ok(PreparedRequest { url, ..Default::default() });
        }
        let arr = JObjectArray::from(obj);
        let len = env.get_array_length(&arr)?;
        if len < 1 {
            return Ok(PreparedRequest { url, ..Default::default() });
        }
        let elem = |env: &mut JNIEnv, i: i32| -> Result<String, BoxError> {
            let o = env.get_object_array_element(&arr, i)?;
            Ok(env.get_string(&JString::from(o))?.into())
        };
        let new_url = elem(&mut env, 0)?;
        let mut headers = Vec::new();
        let mut i = 1;
        while i + 1 < len {
            let k = elem(&mut env, i)?;
            let v = elem(&mut env, i + 1)?;
            headers.push((k, v));
            i += 2;
        }
        Ok(PreparedRequest {
            url: new_url,
            headers,
            ..Default::default()
        })
    }

    async fn resolve_key(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        let mut env = self.vm.attach_current_thread()?;
        let jkid = env.byte_array_from_slice(&kid)?;
        let res = env.call_method(
            self.cb.as_obj(),
            "resolveKey",
            "([B)[B",
            &[JValue::Object(&jkid)],
        )?;
        let obj = res.l()?;
        if obj.is_null() {
            return Err("provider.resolveKey returned null (no key)".into());
        }
        let arr = JByteArray::from(obj);
        let bytes = env.convert_byte_array(&arr)?;
        if bytes.len() != 16 {
            return Err(format!("resolveKey returned {} bytes, expected 16", bytes.len()).into());
        }
        let mut key = [0u8; 16];
        key.copy_from_slice(&bytes);
        Ok(key)
    }
}

/// Dedicated multi-thread Tokio runtime (the host owns the UI looper).
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Seed `ndk_context` with (JavaVM, Context) so cpal et al. resolve the runtime.
fn init_ndk_context(env: &mut JNIEnv, context: &JObject) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let vm = env.get_java_vm().expect("get_java_vm");
        let ctx_global = env
            .new_global_ref(context)
            .expect("new_global_ref(context)");
        let ctx_raw = ctx_global.as_raw() as *mut c_void;
        std::mem::forget(ctx_global);
        unsafe {
            ndk_context::initialize_android_context(vm.get_java_vm_pointer() as *mut c_void, ctx_raw);
        }
    });
}

fn init_logging() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Info),
        );
        std::panic::set_hook(Box::new(|info| {
            log::error!("rust panic: {}", info);
        }));
        log::info!("app-android: bridge shell loaded");
    });
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
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    context: JObject,
    bridge_cb: JObject,
    surface: JObject,
    video_surface: JObject,
    width: jint,
    height: jint,
    display_hdr_types: jint,
    manifest_url: JString,
    start_fraction: jfloat,
    audio_passthrough: jint,
    auto_select_subtitle: jboolean,
) -> jlong {
    init_logging();
    init_ndk_context(&mut env, &context);

    let manifest: String = match env.get_string(&manifest_url) {
        Ok(s) => s.into(),
        Err(_) => {
            log::error!("nativeStart: manifestUrl missing");
            return 0;
        }
    };

    let native_window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(env.get_raw() as *mut _, surface.as_raw() as *mut _)
    };
    if native_window.is_null() {
        log::error!("nativeStart: ANativeWindow_fromSurface returned null");
        return 0;
    }
    let video_window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(env.get_raw() as *mut _, video_surface.as_raw() as *mut _)
    };
    if video_window.is_null() {
        log::error!("nativeStart: video ANativeWindow_fromSurface returned null");
        unsafe { ndk_sys::ANativeWindow_release(native_window) };
        return 0;
    }

    let w = width.max(1) as u32;
    let h = height.max(1) as u32;
    log::info!("nativeStart: {}x{} hdr={:#06b} url={}", w, h, display_hdr_types, manifest);

    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            log::error!("nativeStart: get_java_vm: {}", e);
            unsafe {
                ndk_sys::ANativeWindow_release(native_window);
                ndk_sys::ANativeWindow_release(video_window);
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
                ndk_sys::ANativeWindow_release(video_window);
            }
            return 0;
        }
    };
    let host = Arc::new(AndroidHost { vm, cb });

    let _guard = runtime().enter();
    let player = Player::new_from_android_surface(native_window as *mut c_void, w, h);

    if display_hdr_types != 0 {
        player.set_display_hdr_types(display_hdr_types as u32);
    }
    // Direct MediaCodec→Surface mode is the production path (HW video plane →
    // native HDR/DV). Always on; the host detaches via setVideoSurface(null).
    player.set_video_output_window(video_window as *mut c_void);

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
        auto_select_subtitle: auto_select_subtitle != 0,
    };

    let bridge = bridge::start(player, manifest, host.clone(), config);

    let handle = Box::new(Handle {
        bridge,
        _host: host,
        native_window,
        video_window: AtomicPtr::new(video_window),
    });
    Box::into_raw(handle) as jlong
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetSize(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    width: jint,
    height: jint,
) {
    let Some(h) = (unsafe { handle_ref(handle) }) else {
        return;
    };
    let _guard = runtime().enter();
    h.bridge.resize(width.max(1) as u32, height.max(1) as u32);
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativePlay(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        let _guard = runtime().enter();
        h.bridge.play();
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativePause(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        let _guard = runtime().enter();
        h.bridge.pause();
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeIsPaused(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jboolean {
    match unsafe { handle_ref(handle) } {
        Some(h) if h.bridge.is_paused() => 1,
        _ => 0,
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSeekMs(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    position_ms: jlong,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        let _guard = runtime().enter();
        h.bridge.seek_ms(position_ms);
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativePositionMs(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jlong {
    unsafe { handle_ref(handle) }
        .map(|h| h.bridge.position_ms())
        .unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeDurationMs(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jlong {
    unsafe { handle_ref(handle) }
        .map(|h| h.bridge.duration_ms())
        .unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVolume(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    volume: jfloat,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        let _guard = runtime().enter();
        h.bridge.set_volume(volume);
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeGetTracksJson<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    let json = unsafe { handle_ref(handle) }
        .map(|h| h.bridge.tracks_json())
        .unwrap_or_else(|| "{}".to_string());
    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => JObject::null().into_raw() as jstring,
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVideoTrack(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    adapt: jint,
    repr: jint,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.set_video_track(adapt.max(0) as usize, repr.max(0) as usize);
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVideoAuto(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.set_video_auto();
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetAudioTrack(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    adapt: jint,
    repr: jint,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.set_audio_track(adapt.max(0) as usize, repr.max(0) as usize);
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetSubtitleTrack(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    adapt: jint,
    repr: jint,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.set_subtitle_track(adapt.max(0) as usize, repr.max(0) as usize);
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeClearSubtitles(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.clear_subtitles();
    }
}

// --- generic player knobs (parity with ExoPlayer surface/track/format API) ---

/// Re-point (or detach with a null surface) the MediaCodec video plane. Use on
/// a surface swap / background→foreground; pass null to stop rendering to an
/// abandoned window.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVideoOutputWindow(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    surface: JObject,
) {
    let Some(h) = (unsafe { handle_ref(handle) }) else {
        return;
    };
    let new_window = if surface.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe {
            ndk_sys::ANativeWindow_fromSurface(env.get_raw() as *mut _, surface.as_raw() as *mut _)
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
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetSubtitleSafeInsetBottom(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    bottom_px: jint,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.player().set_subtitle_safe_insets(bottom_px.max(0) as u32);
    }
}

#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetAdaptiveFrameRate(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    enabled: jboolean,
) {
    if let Some(h) = unsafe { handle_ref(handle) } {
        h.bridge.player().set_adaptive_frame_rate(enabled != 0);
    }
}

/// ARGB ints (Android `Color`), like ExoPlayer `CaptionStyleCompat`.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetSubtitleStyle(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    text_argb: jint,
    outline_argb: jint,
    size_scale: jfloat,
) {
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
    }
    .sanitised();
    h.bridge.player().set_subtitle_style(style);
}

/// Verbose logging toggle (default off → per-frame vsync/HEALTH spam gated).
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeSetVerboseLogging(
    _env: JNIEnv,
    _class: JClass,
    enabled: jboolean,
) {
    log::set_max_level(if enabled != 0 {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    });
}

/// `nativeDestroy(long)` — tear down and release the window refs.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_NativeBridge_nativeDestroy(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
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
    unsafe {
        ndk_sys::ANativeWindow_release(native_window);
        if !vwin.is_null() {
            ndk_sys::ANativeWindow_release(vwin);
        }
    }
}
