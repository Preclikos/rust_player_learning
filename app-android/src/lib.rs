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

            // Pick 720p HEVC for first cut — same as desktop's default.
            let selected_video = tracks.video.first().unwrap();
            let selected_video_repr = &selected_video.representations[5];
            player.set_video_track(selected_video, selected_video_repr);
            log::info!(
                "android: selected video {}x{} {}",
                selected_video_repr.width,
                selected_video_repr.height,
                selected_video_repr.codecs
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
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    log::info!("android_main: starting");

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
