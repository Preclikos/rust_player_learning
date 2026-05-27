// iOS host bootstrap.
//
// The Rust staticlib (libapp_ios.a) exports `rust_main()` which spins up
// the winit event loop and a Tokio runtime. The Objective-C side is the
// thinnest possible UIApplication shell — UIApplicationMain() is what
// initialises CoreAnimation / CALayer / UIScene so that winit can attach
// its CAMetalLayer-backed UIView when `create_window` runs.
//
// We don't ship our own AppDelegate / scene-delegate classes: winit
// installs its own UIApplicationDelegate from inside `rust_main`. Passing
// nil here just lets UIApplicationMain create the default empty
// UIApplication; winit takes over before any UI is needed.

#import <UIKit/UIKit.h>

extern void rust_main(void);

int main(int argc, char *argv[]) {
    @autoreleasepool {
        // Hand off to Rust. UIApplicationMain blocks until the app exits,
        // which on iOS effectively means "forever" — so do this after we've
        // told Rust to start running. We invoke `rust_main` first so the
        // EventLoop is constructed before UIApplicationMain runs the main
        // CFRunLoop; winit detects this and integrates with the running
        // run loop rather than starting its own.
        rust_main();
        // rust_main is expected to call UIApplicationMain itself via winit's
        // iOS backend. If it ever returns we fall through here, which
        // matches the documented winit-on-iOS behaviour (never returns).
        return 0;
    }
}
