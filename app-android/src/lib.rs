// Android smoke test — EMBEDDED into a host Activity's Surface (no winit).
//
// This is the Android counterpart of the desktop/iOS smoke shells, following
// the *embed* model real apps use: a normal `Activity` owns a `SurfaceView`
// (`android/.../MainActivity.java`) and hands its `Surface` to `nativeStart`
// over JNI. We turn it into an `ANativeWindow` and render straight into it —
// no winit `NativeActivity`, no `android_main`.
//
// It is intentionally self-contained: it plays the bundled encrypted test
// stream from `app_shared` (clearkeys baked in). Provider auth / licence
// callbacks are NOT wired here — that belongs to the product bridge that
// consumes the same `Player::new_from_android_surface` API from outside this
// repo.
//
// Build with cargo-ndk (Gradle's buildRust task does this automatically):
//   cargo ndk -t arm64-v8a build -p app-android
//
// This crate is a no-op when not targeting Android, so the workspace still
// builds on desktop without an NDK toolchain.

#![cfg(target_os = "android")]

use std::ffi::c_void;
use std::sync::OnceLock;

use jni::objects::{JClass, JObject};
use jni::sys::{jint, jlong};
use jni::JNIEnv;
use player::{PhysicalSize, Player};

/// Player + the `ANativeWindow` ref it renders into. The window ref is acquired
/// by `ANativeWindow_fromSurface` and must outlive the player's wgpu surface,
/// so we keep it here and release it in `nativeDestroy` *after* the player is
/// dropped.
struct Handle {
    player: Player,
    native_window: *mut ndk_sys::ANativeWindow,
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

fn init_logging() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // Info by default so per-frame vsync/mediacodec traces stay out of
        // logcat. Bump to Debug/Trace when diagnosing playback.
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Info),
        );
        log::info!("app-android: embedded player loaded");
    });
}

/// `MainActivity.nativeStart(Surface, int, int) -> long`.
///
/// Builds a player rendering into the given `Surface` and starts the bundled
/// encrypted test stream. Returns an opaque handle (as a `jlong`) the host
/// keeps for `nativeSetSize` / `nativeDestroy`. Returns 0 on failure.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_MainActivity_nativeStart(
    env: JNIEnv,
    _class: JClass,
    surface: JObject,
    width: jint,
    height: jint,
) -> jlong {
    init_logging();

    // ANativeWindow_fromSurface returns an *acquired* reference (release with
    // ANativeWindow_release). Raw-pointer `as` casts bridge the jni vs ndk-sys
    // JNIEnv/jobject type aliases without depending on them matching exactly.
    let native_window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(env.get_raw() as *mut _, surface.as_raw() as *mut _)
    };
    if native_window.is_null() {
        log::error!("nativeStart: ANativeWindow_fromSurface returned null");
        return 0;
    }

    let w = width.max(1) as u32;
    let h = height.max(1) as u32;
    log::info!("nativeStart: {}x{}", w, h);

    // Building the renderer block_on's and spawns Tokio tasks internally, so it
    // must run inside the runtime.
    let _guard = runtime().enter();
    let player = Player::new_from_android_surface(native_window as *mut c_void, w, h);

    // Drive the shared smoke-test fixture: open_url → clearkey → prepare →
    // pick tracks → play(). Same stream/keys as the desktop + iOS shells.
    runtime().spawn(app_shared::run_test_playback(player.clone()));

    let handle = Box::new(Handle {
        player,
        native_window,
    });
    Box::into_raw(handle) as jlong
}

/// `MainActivity.nativeSetSize(long, int, int)` — reconfigure on layout change.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_MainActivity_nativeSetSize(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    width: jint,
    height: jint,
) {
    if handle == 0 {
        return;
    }
    let h = unsafe { &*(handle as *mut Handle) };
    h.player
        .resize(PhysicalSize::new(width.max(1) as u32, height.max(1) as u32));
}

/// `MainActivity.nativeDestroy(long)` — tear down and release the window ref.
#[no_mangle]
pub extern "system" fn Java_cz_preclikos_rustplayer_MainActivity_nativeDestroy(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    let h = unsafe { Box::from_raw(handle as *mut Handle) };
    let Handle {
        player,
        native_window,
    } = *h;
    // Drop the player (and its wgpu surface) first, then release the window ref
    // we acquired in nativeStart.
    drop(player);
    unsafe { ndk_sys::ANativeWindow_release(native_window) };
}
