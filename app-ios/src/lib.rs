// iOS entry point.
//
// Build with cargo-lipo or cargo-xcode, e.g.:
//   cargo build --target aarch64-apple-ios --release -p app-ios
// Then link the resulting libapp_ios.a into an Xcode project. The app's
// Objective-C/Swift `main()` calls `rust_main()` after creating the
// UIApplication; winit takes over the UIScene/run loop from there.
//
// This crate is intentionally a no-op when not targeting iOS, so the
// workspace can `cargo build` on Windows/Linux without an iOS toolchain.

#![cfg(target_os = "ios")]

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

struct App {
    window: Option<Arc<Window>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes();
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        self.window = Some(window);
        // TODO: construct a Player once the player crate is ported to iOS.
        // The decoder backend will be VideoToolbox via a `HwVideoDecoder` impl.
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
pub extern "C" fn rust_main() {
    let _ = oslog::OsLogger::new("com.rust.player")
        .level_filter(log::LevelFilter::Info)
        .init();
    log::info!("rust_main: starting");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _guard = rt.enter();

    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App { window: None };
    let _ = event_loop.run_app(&mut app);
}
