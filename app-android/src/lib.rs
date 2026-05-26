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

use std::collections::HashMap;
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
use tokio::join;
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
        let mut player = Player::new(window.clone());

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

        // Start playback in a background tokio task — mirror of the desktop
        // app's setup loop. Uses the same encrypted stream + keys to verify
        // the MediaCodec pipeline against known content.
        tokio::spawn(async move {
            if let Err(e) = player
                .open_url("https://preclikos.cz/examples/encrypted/manifest.mpd")
                .await
            {
                log::error!("open_url: {}", e);
                return;
            }

            let mut keys = HashMap::new();
            keys.insert(
                "0fd37dac41c0e987e68d43b801b1210c".to_string(),
                "fd8d9f408c2bd702970afcd3b219e791".to_string(),
            );
            keys.insert(
                "519af81ab2d284f52aa8257d96b5e4bd".to_string(),
                "627ef72b42d98770dec20ecab46cd1f4".to_string(),
            );
            if let Err(e) = player.set_clearkey(keys) {
                log::error!("set_clearkey: {}", e);
                return;
            }

            if let Err(e) = player.prepare().await {
                log::error!("prepare: {}", e);
                return;
            }

            let tracks = match player.get_tracks() {
                Ok(t) => t,
                Err(e) => {
                    log::error!("get_tracks: {}", e);
                    return;
                }
            };

            // Pick 720p HEVC — index 5 matches the desktop default.
            let selected_video = tracks.video.first().unwrap();
            let selected_video_repr = &selected_video.representations[5];
            player.set_video_track(selected_video, selected_video_repr);
            log::info!(
                "android: selected video {}x{} {}",
                selected_video_repr.width,
                selected_video_repr.height,
                selected_video_repr.codecs
            );

            // Pick the first AAC (mp4a) audio representation — skip EC-3/other codecs
            // that have no esds box and aren't supported by MediaCodec audio/mp4a-latm.
            let selected_audio = tracks
                .audio
                .iter()
                .find(|a| a.representations.iter().any(|r| r.codecs.starts_with("mp4a")))
                .unwrap_or_else(|| tracks.audio.first().unwrap());
            let selected_audio_repr = selected_audio
                .representations
                .iter()
                .find(|r| r.codecs.starts_with("mp4a"))
                .unwrap_or_else(|| selected_audio.representations.first().unwrap());
            player.set_audio_track(selected_audio, selected_audio_repr);
            log::info!(
                "android: selected audio {} {}Hz",
                selected_audio_repr.codecs,
                selected_audio_repr.bandwidth,
            );

            loop {
                let handle = match player.play() {
                    Ok(h) => h,
                    Err(e) => {
                        log::error!("play(): {}", e);
                        break;
                    }
                };
                let _ = join!(handle);
            }
        });
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
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
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
