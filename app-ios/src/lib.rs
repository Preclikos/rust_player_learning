// iOS entry point — mirrors the Android shell:
//   * Build with cargo-lipo / cargo-xcode / `cargo build --target …-ios-sim`.
//   * The resulting libapp_ios.a (.staticlib) is linked into an Xcode app
//     bundle that provides UIApplicationMain + the storyboard.
//   * That bundle's AppDelegate calls `rust_main()` after UIApplication
//     boots — winit takes the UIScene / UIWindow lifecycle from there.
//
// On non-iOS targets this crate is a no-op so `cargo check` across the
// workspace stays green without a cross-compile toolchain.

#![cfg(target_os = "ios")]

use std::sync::Arc;

use player::Player;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

struct App {
    window: Option<Arc<Window>>,
    player: Option<Player>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        log::info!("resumed: creating window");
        let attrs = Window::default_attributes();
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("create_window failed: {}", e);
                return;
            }
        };
        log::info!("resumed: window created, constructing Player");

        let player = Player::new(window.clone());
        log::info!("resumed: Player::new returned");

        self.window = Some(window);
        self.player = Some(player.clone());

        log::info!("resumed: spawning run_test_playback");
        tokio::spawn(app_shared::run_test_playback(player));
        log::info!("resumed: done");
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
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

/// Entry point invoked from the Objective-C / Swift app shell after
/// UIApplicationMain finishes booting the runtime. Symbol is `_rust_main`
/// in the .a — declare in the host code as:
///
///   extern void rust_main(void);
#[no_mangle]
pub extern "C" fn rust_main() {
    let _ = oslog::OsLogger::new("com.rust.player")
        .level_filter(log::LevelFilter::Info)
        .init();

    // Route any panics into oslog so they show up in Console.app /
    // `simctl spawn log show`. Without this hook `panic_cannot_unwind`
    // aborts the process while the panic message is still on stderr —
    // which is /dev/null inside the simulator app sandbox.
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        log::error!("RUST PANIC at {}: {}", location, payload);
    }));

    log::info!("rust_main: starting iOS playback shell");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _guard = rt.enter();

    log::info!("rust_main: building EventLoop");
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    log::info!("rust_main: EventLoop built, entering run_app");

    let mut app = App {
        window: None,
        player: None,
    };
    let result = event_loop.run_app(&mut app);
    log::info!("rust_main: run_app returned: {:?}", result.is_ok());
}
