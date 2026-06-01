// iOS smoke test — EMBEDDED into a host-owned UIView (no winit).
//
// This crate is the iOS counterpart of the desktop/Android smoke shells, but
// it follows the *embed* model that real apps use: the Objective-C host
// (`ios/RustPlayer/main.m`) owns `UIApplicationMain` and a `CAMetalLayer`-backed
// view, then hands that layer to `rust_player_start`. The player renders
// straight into it — no winit, no `UIApplicationDelegate` takeover.
//
// It is intentionally self-contained: it plays the bundled encrypted test
// stream from `app_shared` (clearkeys baked in). Provider auth / licence
// callbacks are NOT wired here — that belongs to the product bridge that
// consumes the same `Player::new_from_metal_layer` API from outside this repo.
//
//   * Build with `cargo build -p app-ios --target aarch64-apple-ios-sim`
//     (see `ios/build_sim.sh`); the resulting `libapp_ios.a` links into the
//     Obj-C app bundle.
//
// On non-iOS targets this crate is a no-op so the workspace still builds
// without a cross-compile toolchain.

#![cfg(target_os = "ios")]

use std::ffi::c_void;
use std::sync::OnceLock;

use player::{PhysicalSize, Player};

/// Dedicated multi-thread Tokio runtime that drives the player. The host owns
/// the CFRunLoop (via `UIApplicationMain`), so the player needs its own runtime
/// for the decode/render/download tasks — there is no winit event loop here.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// One-time logging + panic routing into oslog. Without the panic hook a
/// `panic_cannot_unwind` aborts the process while the message is still on
/// stderr — which is `/dev/null` inside the simulator app sandbox.
fn init_once() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = oslog::OsLogger::new("com.rust.player")
            .level_filter(log::LevelFilter::Info)
            .init();

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
    });
}

/// Create a player rendering into `metal_layer` (a `CAMetalLayer*`) and start
/// the bundled encrypted test stream. Returns an opaque handle the host keeps
/// for `rust_player_set_size` / `rust_player_destroy`. The host guarantees the
/// layer outlives the player.
///
/// Declare in the Obj-C host as:
///   extern void *rust_player_start(void *metal_layer, uint32_t w, uint32_t h);
#[no_mangle]
pub extern "C" fn rust_player_start(
    metal_layer: *mut c_void,
    width: u32,
    height: u32,
) -> *mut c_void {
    init_once();

    if metal_layer.is_null() {
        log::error!("rust_player_start: null metal_layer");
        return std::ptr::null_mut();
    }

    log::info!("rust_player_start: {}x{}", width, height);

    // `Player::new_from_metal_layer` blocks on building the renderer, which
    // spawns Tokio tasks internally — so it must run inside the runtime.
    let _guard = runtime().enter();
    let player = Player::new_from_metal_layer(metal_layer, width.max(1), height.max(1));

    // Drive the shared smoke-test fixture: open_url → clearkey → prepare →
    // pick tracks → play(). Identical stream/keys as the desktop + Android shells.
    runtime().spawn(app_shared::run_test_playback(player.clone()));

    Box::into_raw(Box::new(player)) as *mut c_void
}

/// Reconfigure the surface after a layout/orientation change. `width`/`height`
/// are in physical pixels (the host multiplies points by the layer's
/// `contentsScale`).
#[no_mangle]
pub extern "C" fn rust_player_set_size(handle: *mut c_void, width: u32, height: u32) {
    if handle.is_null() {
        return;
    }
    let player = unsafe { &*(handle as *mut Player) };
    player.resize(PhysicalSize::new(width.max(1), height.max(1)));
}

/// Tear down the player and free the handle. Safe to call with NULL.
#[no_mangle]
pub extern "C" fn rust_player_destroy(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    let _ = unsafe { Box::from_raw(handle as *mut Player) };
}
