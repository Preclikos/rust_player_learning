//! macOS host glue for AppKit-based embedders (winit, `examples/desktop`):
//! what the screen can show, and the direct-mode video layer under the
//! render layer. The iOS equivalents live in the Swift wrapper
//! (`platform/ios/packaging/Sources/RustPlayer/RustPlayer.swift`).
//!
//! Everything here is `unsafe` Objective-C messaging on host-owned AppKit
//! objects, so it must run on the main thread.

#![cfg(target_os = "macos")]

use std::ffi::c_void;

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::NSRect;

/// `Display.HdrCapabilities`-order bits shared with Android and the Swift
/// wrapper (see `Player::set_display_hdr_types`).
const DISPLAY_DOLBY_VISION: u32 = 1 << 0;
const DISPLAY_HDR10: u32 = 1 << 1;
const DISPLAY_HLG: u32 = 1 << 2;

/// `AVPlayerHDRMode` option set bit for Dolby Vision.
const AV_PLAYER_HDR_MODE_DOLBY_VISION: isize = 4;

// Referencing the framework keeps it linked, so `AVPlayer` /
// `AVSampleBufferDisplayLayer` resolve at runtime.
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

/// The display mask for a screen with the given EDR headroom and
/// `AVPlayer.availableHDRModes`: HDR10 + HLG once the panel has headroom,
/// plus Dolby Vision when AVFoundation lists it. Pure; see the tests.
fn display_hdr_mask(potential_edr_headroom: f64, available_hdr_modes: isize) -> u32 {
    if potential_edr_headroom <= 1.0 {
        return 0;
    }
    let dv = if available_hdr_modes & AV_PLAYER_HDR_MODE_DOLBY_VISION != 0 {
        DISPLAY_DOLBY_VISION
    } else {
        0
    };
    DISPLAY_HDR10 | DISPLAY_HLG | dv
}

/// The `NSScreen` showing `ns_view`, or the main screen when the view is
/// null or not in a window yet.
unsafe fn screen_of_view(ns_view: *mut c_void) -> *mut AnyObject {
    let mut screen: *mut AnyObject = std::ptr::null_mut();
    if let Some(view) = (ns_view as *mut AnyObject).as_ref() {
        let window: *mut AnyObject = msg_send![view, window];
        if let Some(window) = window.as_ref() {
            screen = msg_send![window, screen];
        }
    }
    if screen.is_null() {
        if let Some(cls) = AnyClass::get(c"NSScreen") {
            screen = msg_send![cls, mainScreen];
        }
    }
    screen
}

/// The [`Player::set_display_hdr_types`](crate::Player::set_display_hdr_types)
/// mask for the screen showing `ns_view` (an `NSView*`, e.g. winit's
/// `AppKitWindowHandle::ns_view`), or for the main screen when it is null
/// or not in a window yet. A screen with EDR headroom
/// (`maximumPotentialExtendedDynamicRangeColorComponentValue > 1`: XDR /
/// HDR panels, HDR external monitors in HDR mode) reports HDR10 | HLG, plus
/// Dolby Vision when `AVPlayer.availableHDRModes` lists it; anything else
/// reports 0 and the player tonemaps.
///
/// Main thread only. Re-check when the window moves to another screen.
///
/// # Safety
/// `ns_view` must be null or a valid `NSView*`.
pub unsafe fn macos_display_hdr_types(ns_view: *mut c_void) -> u32 {
    let Some(screen) = (unsafe { screen_of_view(ns_view).as_ref() }) else { return 0 };
    let headroom: f64 =
        unsafe { msg_send![screen, maximumPotentialExtendedDynamicRangeColorComponentValue] };
    let modes: isize = match AnyClass::get(c"AVPlayer") {
        Some(cls) => unsafe { msg_send![cls, availableHDRModes] },
        None => 0,
    };
    display_hdr_mask(headroom, modes)
}

/// Direct-mode glue: put an `AVSampleBufferDisplayLayer` under the wgpu
/// `CAMetalLayer` in `ns_view`'s layer (index 0, aspect-fit, resizing with
/// the view) and return it retained (+1) for
/// [`Player::set_video_output_layer`](crate::Player::set_video_output_layer).
/// Release it with `CFRelease` after the player is gone, or let it live with
/// the window. Null when the view isn't layer-backed yet. Main thread only.
///
/// # Safety
/// `ns_view` must be a valid `NSView*`.
pub unsafe fn macos_install_direct_video_layer(ns_view: *mut c_void) -> *mut c_void {
    // kCALayerWidthSizable | kCALayerHeightSizable
    const AUTORESIZE_WIDTH_HEIGHT: u32 = 2 | 16;

    unsafe {
        let Some(view) = (ns_view as *mut AnyObject).as_ref() else { return std::ptr::null_mut() };
        let host: *mut AnyObject = msg_send![view, layer];
        let Some(host) = host.as_ref() else { return std::ptr::null_mut() };
        let (Some(layer_cls), Some(string_cls)) =
            (AnyClass::get(c"AVSampleBufferDisplayLayer"), AnyClass::get(c"NSString"))
        else {
            return std::ptr::null_mut();
        };
        let layer: *mut AnyObject = msg_send![layer_cls, new];
        let Some(l) = layer.as_ref() else { return std::ptr::null_mut() };
        let bounds: NSRect = msg_send![host, bounds];
        let _: () = msg_send![l, setFrame: bounds];
        let _: () = msg_send![l, setAutoresizingMask: AUTORESIZE_WIDTH_HEIGHT];
        let gravity: *mut AnyObject =
            msg_send![string_cls, stringWithUTF8String: c"AVLayerVideoGravityResizeAspect".as_ptr()];
        let _: () = msg_send![l, setVideoGravity: gravity];
        let _: () = msg_send![host, insertSublayer: l, atIndex: 0u32];
        log::info!("[direct] AVSampleBufferDisplayLayer inserted under the render layer");
        layer as *mut c_void
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_screen_reports_nothing() {
        assert_eq!(display_hdr_mask(1.0, AV_PLAYER_HDR_MODE_DOLBY_VISION), 0);
        assert_eq!(display_hdr_mask(0.0, 0), 0);
    }

    #[test]
    fn hdr_screen_reports_hdr10_hlg_and_dv_when_listed() {
        assert_eq!(display_hdr_mask(2.0, 0), DISPLAY_HDR10 | DISPLAY_HLG);
        assert_eq!(
            display_hdr_mask(16.0, AV_PLAYER_HDR_MODE_DOLBY_VISION | 2),
            DISPLAY_HDR10 | DISPLAY_HLG | DISPLAY_DOLBY_VISION
        );
    }
}
