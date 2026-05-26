// Android entry point.
//
// Build with cargo-ndk, e.g.:
//   cargo ndk -t arm64-v8a build -p app-android
// Then bundle the resulting libapp_android.so into an Android Studio project
// that loads it via System.loadLibrary("app_android"). The NativeActivity
// dispatches to `android_main` below.
//
// This crate is intentionally a no-op when not targeting Android, so the
// workspace can still `cargo build` on desktop without an NDK toolchain.

#![cfg(target_os = "android")]

use std::sync::Arc;

/// Sets FLAG_KEEP_SCREEN_ON on the NativeActivity window so the screen stays
/// on during video playback without needing a WakeLock.
fn keep_screen_on(app: &android_activity::AndroidApp) {
    use android_activity::WindowManagerFlags;
    app.set_window_flags(WindowManagerFlags::KEEP_SCREEN_ON, WindowManagerFlags::empty());
    log::info!("keep-screen-on: FLAG_KEEP_SCREEN_ON set");
}

/// Hints to SurfaceFlinger that this window produces 24fps content.
/// SurfaceFlinger then selects a display refresh rate that is an integer
/// multiple of 24Hz (e.g. 48Hz, 120Hz) for perfect cadence.
///
/// On modern Android the linker uses per-library namespaces, so
/// dlsym(RTLD_DEFAULT) doesn't find symbols in libandroid.so even on
/// API 34. We must dlopen the library explicitly.
fn set_frame_rate_24fps(window: &winit::window::Window) {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else { return };
    let native_ptr = match handle.as_raw() {
        RawWindowHandle::AndroidNdk(h) => h.a_native_window.as_ptr(),
        _ => return,
    };

    // ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_FIXED_SOURCE = 1
    // ANATIVEWINDOW_CHANGE_FRAME_RATE_ALWAYS = 0
    const FIXED_SOURCE: i8 = 1;
    const ALWAYS: i8 = 0;

    unsafe {
        // libandroid.so is loaded by the system but in a private namespace;
        // RTLD_DEFAULT can't see it. dlopen with RTLD_NOLOAD grabs the already-
        // loaded handle without a second load; fall back to a fresh dlopen.
        let lib = {
            let name = b"libandroid.so\0".as_ptr() as *const libc::c_char;
            let h = libc::dlopen(name, libc::RTLD_NOW | libc::RTLD_NOLOAD);
            if h.is_null() { libc::dlopen(name, libc::RTLD_NOW) } else { h }
        };
        if lib.is_null() {
            log::warn!("set_frame_rate_24fps: dlopen(libandroid.so) failed");
            return;
        }
        let sym = libc::dlsym(lib, b"ANativeWindow_setFrameRate\0".as_ptr() as *const libc::c_char);
        if sym.is_null() {
            log::info!("ANativeWindow_setFrameRate unavailable (API < 30)");
            libc::dlclose(lib);
            return;
        }
        type Fn = unsafe extern "C" fn(*mut libc::c_void, f32, i8, i8) -> libc::c_int;
        let f: Fn = std::mem::transmute(sym);
        let ret = f(native_ptr.cast(), 24.0, FIXED_SOURCE, ALWAYS);
        log::info!("ANativeWindow_setFrameRate(24.0, FIXED_SOURCE, ALWAYS) = {}", ret);
        libc::dlclose(lib);
    }
}

use android_activity::AndroidApp;
use player::Player;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::android::EventLoopBuilderExtAndroid;
use winit::window::{Window, WindowId};

struct App {
    window: Option<Arc<Window>>,
    player: Option<Player>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes();
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let player = Player::new(window.clone());

        // NOTE: ANativeWindow_setFrameRate(24, FIXED_SOURCE) DOES NOT help here.
        // On Samsung devices (tested: Galaxy S21 120Hz panel, Samsung TV via
        // Google TV Streamer) the compositor reacts to the 24fps hint by
        // downgrading the display from 120Hz (a perfect 5x multiple of 24)
        // to 60Hz — exactly the rate that produces 3:2 pulldown judder.
        // Without the hint the panel stays at its native rate (120Hz on phones)
        // and 24fps content plays with a clean 5-VSyncs-per-frame cadence.
        // (TVs forced to 60Hz by their own logic still pulldown, but the hint
        // doesn't help there either.)
        // set_frame_rate_24fps(&window);

        let _ = set_frame_rate_24fps; // suppress unused warning when not called

        self.window = Some(window);
        self.player = Some(player.clone());
        log::info!("window + player created");

        // Same hardcoded encrypted stream + keys as the desktop shell —
        // logic lives in app-shared so both shells stay in sync.
        tokio::spawn(app_shared::run_test_playback(player));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. } => {
                use winit::keyboard::{Key, NamedKey};
                if let Key::Named(NamedKey::GoBack) = &event.logical_key {
                    if event.state == winit::event::ElementState::Released {
                        log::info!("back button: exiting");
                        event_loop.exit();
                    }
                }
            }
            WindowEvent::Resized(new_size) => {
                log::info!("window resized: {}x{}", new_size.width, new_size.height);
                if let Some(player) = &self.player {
                    player.resize(new_size);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}

#[no_mangle]
fn android_main(app: AndroidApp) {
    // Default to Info so per-frame [vsync]/[mc] traces (24 lines/sec at
    // 24fps) stay out of logcat. Bump to Debug / Trace when diagnosing
    // playback issues — every player log site now uses the standard
    // log:: levels (no rogue println!s).
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    log::info!("android_main: starting");

    keep_screen_on(&app);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _guard = rt.enter();

    let event_loop = EventLoop::builder()
        .with_android_app(app)
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App {
        window: None,
        player: None,
    };
    let _ = event_loop.run_app(&mut app);
}
