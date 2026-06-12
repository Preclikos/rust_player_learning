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

/// Player + the `ANativeWindow` refs it renders into. The window refs are
/// acquired by `ANativeWindow_fromSurface` and must outlive the player's
/// wgpu surface / MediaCodec output, so we keep them here and release them
/// in `nativeDestroy` *after* the player is dropped.
struct Handle {
    player: Player,
    /// Overlay (wgpu/GLES) window — UI/subtitles, or video in non-direct mode.
    native_window: *mut ndk_sys::ANativeWindow,
    /// Video plane window — MediaCodec renders into it in direct mode.
    video_window: *mut ndk_sys::ANativeWindow,
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
/// Android runtime without us having to thread a `NativeActivity` through.
/// Idempotent — the global state is set on the first call and left alone
/// thereafter; the global ref is intentionally leaked because the process
/// keeps the Activity around for its whole lifetime.
fn init_ndk_context(env: &mut JNIEnv, context: &JObject) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let vm = env.get_java_vm().expect("get_java_vm");
        let ctx_global = env
            .new_global_ref(context)
            .expect("new_global_ref(context)");
        // ndk_context just stores the raw pointers; both must outlive the
        // process. Leak the GlobalRef so the JVM doesn't reclaim it.
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
        // Info by default so per-frame vsync/mediacodec traces stay out of
        // logcat. Bump to Debug/Trace when diagnosing playback.
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Info),
        );
        // Route Rust panics through android_logger so they land in logcat
        // under the `app_android` tag. Without this hook a panic in a
        // background tokio task (or in the JNI thread itself) just delivers
        // SIGABRT with no message, leaving only an unsymbolicated tombstone.
        std::panic::set_hook(Box::new(|info| {
            log::error!("rust panic: {}", info);
        }));
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
    mut env: JNIEnv,
    _class: JClass,
    context: JObject,
    activity: JObject,
    surface: JObject,
    video_surface: JObject,
    width: jint,
    height: jint,
    display_hdr_types: jint,
) -> jlong {
    init_logging();
    init_ndk_context(&mut env, &context);

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
    let video_window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(
            env.get_raw() as *mut _,
            video_surface.as_raw() as *mut _,
        )
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

    // Building the renderer block_on's and spawns Tokio tasks internally, so it
    // must run inside the runtime.
    let _guard = runtime().enter();
    let player = Player::new_from_android_surface(native_window as *mut c_void, w, h);
    // HDR passthrough is opt-in for the test shell: on HDMI boxes the HWC
    // typically refuses to switch the output to HDR for a GPU-composited
    // layer (verified on the Google TV Streamer: layer carries BT2020_PQ +
    // SMPTE2086/CTA861.3 metadata, output stays BT709 — SurfaceFlinger's
    // own PQ→SDR mapping then looks worse than the player's mobius
    // tonemap). Phone panels may behave better — flip the file to test:
    //   echo 1 > /sdcard/Android/data/cz.preclikos.rust_player/files/hdr_passthrough.txt
    // Real passthrough for TV boxes lands with the direct
    // MediaCodec→Surface mode.
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

    // Direct playback mode: MediaCodec renders straight into the dedicated
    // video Surface (HW video plane → HDR/HDR10+/DV reach the display).
    // Opt-out for A/B testing:
    //   echo 0 > /sdcard/Android/data/cz.preclikos.rust_player/files/direct.txt
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

    // Feed content-size changes back to the Activity so it can shape the
    // video SurfaceView to the content aspect (MediaCodec stretches to
    // fill the surface). Uses its own JNI attachment — the events task
    // runs on a tokio worker.
    {
        let vm = env.get_java_vm().expect("get_java_vm");
        let activity_ref = env.new_global_ref(&activity).expect("global ref activity");
        let mut events = player.events();
        runtime().spawn(async move {
            let mut last = (0u32, 0u32);
            loop {
                match events.recv().await {
                    Ok(player::PlayerEvent::Stats {
                        current_resolution: Some((w, h)),
                        ..
                    }) if (w, h) != last && w > 0 && h > 0 => {
                        last = (w, h);
                        if let Ok(mut env) = vm.attach_current_thread() {
                            let _ = env.call_method(
                                activity_ref.as_obj(),
                                "onVideoSize",
                                "(II)V",
                                &[(w as jint).into(), (h as jint).into()],
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
    }

    // Drive the shared smoke-test fixture: open_url → clearkey → prepare →
    // pick tracks → play(). Same stream/keys as the desktop + iOS shells.
    runtime().spawn(app_shared::run_test_playback(player.clone()));

    let handle = Box::new(Handle {
        player,
        native_window,
        video_window,
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
    // `Player::resize` calls `tokio::spawn` internally to drive the renderer
    // resize on a worker; without a runtime context the spawn aborts the
    // process. nativeSetSize fires from the JNI/UI thread, which has no
    // implicit runtime — enter ours explicitly, same pattern as nativeStart.
    let _guard = runtime().enter();
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
        video_window,
    } = *h;
    // Drop the player (its wgpu surface AND the MediaCodec attached to the
    // video window) first, then release the window refs from nativeStart.
    drop(player);
    unsafe {
        ndk_sys::ANativeWindow_release(native_window);
        ndk_sys::ANativeWindow_release(video_window);
    }
}
