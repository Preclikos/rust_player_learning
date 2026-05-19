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

use android_activity::AndroidApp;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::android::EventLoopBuilderExtAndroid;
use winit::window::{Window, WindowId};

struct App {
    window: Option<Arc<Window>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes();
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        self.window = Some(window);
        // TODO: construct a Player once the player crate is ported to Android.
        // The decoder backend will be MediaCodec via a `HwVideoDecoder` impl.
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

    let mut app = App { window: None };
    let _ = event_loop.run_app(&mut app);
}
