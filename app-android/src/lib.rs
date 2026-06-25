// Android bridge — EMBEDDED into a host Activity's Surfaces (no winit).
//
// This is the Android shell of the *unified bridge core* (`app_shared::bridge`).
// The shell stays thin: it converts the host `Surface`s into `ANativeWindow`s,
// implements `BridgeHost` (forwarding player events to a Kotlin `PlayerBridge`
// object as JSON, and the provider hooks back into Kotlin), then hands a
// `Player` to `bridge::start` and exposes the unified control surface over JNI.
//
// All the open_url → prepare → tracks → play() orchestration, the event JSON
// schema, and the track-switch command channel live in `app_shared::bridge` —
// shared verbatim with the iOS shell. The same shape is what a future generated
// Kotlin binding would wrap.
//
// It is still self-contained: it plays the bundled encrypted test stream
// (`app_shared::TEST_MANIFEST_URL`). The provider hooks are TEST policy — the
// Kotlin `PlayerBridge` returns passthrough headers and the baked ClearKeys.
//
// Build with cargo-ndk (Gradle's buildRust task does this automatically):
//   cargo ndk -t arm64-v8a build -p app-android

#![cfg(target_os = "android")]

use std::ffi::c_void;
use std::sync::{Arc, OnceLock};

use app_shared::bridge::{self, BoxError, BridgeHandle, BridgeHost, StartConfig};
use async_trait::async_trait;
use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JValue};
use jni::sys::{jboolean, jfloat, jint, jlong, jstring};
use jni::{JNIEnv, JavaVM};
use player::Player;

/// Player bridge + the `ANativeWindow` refs it renders into. The window refs
/// are acquired by `ANativeWindow_fromSurface` and must outlive the player's
/// wgpu surface / MediaCodec output, so we keep them here and release them in
/// `nativeDestroy` *after* the bridge (and player) is dropped.
struct Handle {
    bridge: BridgeHandle,
    /// Keeps the host callback object + JavaVM alive for the player's lifetime.
    _host: Arc<AndroidHost>,
    /// Overlay (wgpu/GLES) window — UI/subtitles, or video in non-direct mode.
    native_window: *mut ndk_sys::ANativeWindow,
    /// Video plane window — MediaCodec renders into it in direct mode.
    video_window: *mut ndk_sys::ANativeWindow,
}

/// Implements the platform-agnostic [`BridgeHost`]: pushes player events to the
/// Kotlin `PlayerBridge.onEvent(String)` and delegates the provider hooks to
/// `PlayerBridge.resolveKey([B)[B`. Auth header injection is left as the
/// default passthrough (the test stream needs none).
struct AndroidHost {
    vm: JavaVM,
    /// Global ref to the Kotlin `PlayerBridge` object passed to `nativeStart`.
    cb: GlobalRef,
}

#[async_trait]
impl BridgeHost for AndroidHost {
    fn on_event(&self, json: String) {
        // Runs on a Tokio worker — attach to the JVM for the upcall. Kotlin
        // hops to the UI thread itself (RustPlayer dispatches on the main looper).
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

    async fn resolve_key(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        // Synchronous JNI upcall (no await inside, so the future stays Send).
        // The test PlayerBridge does a constant-time map lookup, so blocking
        // the worker briefly is fine.
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
            return Err("PlayerBridge.resolveKey returned null".into());
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

/// Dedicated multi-thread Tokio runtime. The host owns the UI thread / looper,
/// so the player needs its own runtime for decode/render/download tasks.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Seed `ndk_context` with (JavaVM, Activity) so cpal et al. can resolve the
/// Android runtime. Idempotent; the global ref is intentionally leaked because
/// the process keeps the Activity around for its whole lifetime.
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

/// `NativeBridge.nativeStart(Context, PlayerBridge, Surface, Surface, int, int, int) -> long`.
///
/// Builds a player rendering into the overlay `Surface` (and, in direct mode,
/// decoding into the video `Surface`), wires the `PlayerBridge` callback object,
/// and starts the bundled encrypted test stream via `app_shared::bridge`.
/// Returns an opaque handle (`jlong`) or 0 on failure.
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
) -> jlong {
    init_logging();
    init_ndk_context(&mut env, &context);

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
    log::info!(
        "nativeStart: {}x{} display_hdr_types={:#06b}",
        w, h, display_hdr_types
    );

    // Build the host callback bridge from the JavaVM + a global ref to the
    // Kotlin PlayerBridge.
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
            log::error!("nativeStart: new_global_ref(bridge): {}", e);
            unsafe {
                ndk_sys::ANativeWindow_release(native_window);
                ndk_sys::ANativeWindow_release(video_window);
            }
            return 0;
        }
    };
    let host = Arc::new(AndroidHost { vm, cb });

    // Building the renderer block_on's and spawns Tokio tasks internally.
    let _guard = runtime().enter();
    let player = Player::new_from_android_surface(native_window as *mut c_void, w, h);

    // HDR passthrough opt-in (same file-flag mechanism as before).
    let passthrough_optin = std::fs::read_to_string(
        "/storage/emulated/0/Android/data/cz.preclikos.rust_player/files/hdr_passthrough.txt",
    )
    .map(|s| s.trim() == "1")
    .unwrap_or(false);
    if passthrough_optin {
        player.set_display_hdr_types(display_hdr_types as u32);
    } else {
        log::info!("nativeStart: HDR passthrough opt-in absent — shader tonemap path");
    }

    // Direct MediaCodec→Surface mode (opt-out via direct.txt == "0").
    let direct = std::fs::read_to_string(
        "/storage/emulated/0/Android/data/cz.preclikos.rust_player/files/direct.txt",
    )
    .map(|s| s.trim() != "0")
    .unwrap_or(true);
    if direct {
        player.set_video_output_window(video_window as *mut c_void);
        log::info!("nativeStart: direct MediaCodec→Surface mode enabled");
    } else {
        log::info!("nativeStart: direct mode disabled by direct.txt — GL path");
    }

    // Hand off to the shared bridge core: wires interceptor/resolver, spawns
    // the event pump + orchestrator (open_url → prepare → tracks → play()).
    let bridge = bridge::start(
        player,
        app_shared::TEST_MANIFEST_URL.to_string(),
        host.clone(),
        StartConfig::default(),
    );

    let handle = Box::new(Handle {
        bridge,
        _host: host,
        native_window,
        video_window,
    });
    Box::into_raw(handle) as jlong
}

/// `nativeSetSize(long, int, int)` — reconfigure on layout change.
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
    // `resize` spawns a renderer-resize task; needs the runtime context.
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
    // Stop the orchestrator + drop the player (its wgpu surface AND the
    // MediaCodec attached to the video window) before releasing the windows.
    bridge.shutdown();
    drop(bridge);
    drop(_host);
    unsafe {
        ndk_sys::ANativeWindow_release(native_window);
        ndk_sys::ANativeWindow_release(video_window);
    }
}
