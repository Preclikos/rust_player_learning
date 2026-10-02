//! Apple HDR display output: hand the PQ signal to an EDR-capable display
//! instead of tonemapping it to SDR in the shader.
//!
//! An HDR output *session* puts the host's `CAMetalLayer` into
//!   - `rgba16float` (wgpu-hal flips `wantsExtendedDynamicRangeContent` on
//!     for exactly this format when the surface is configured),
//!   - `colorspace = kCGColorSpaceITUR_2100_PQ`,
//!   - `EDRMetadata = HDR10(...)`, so the OS tone-maps the signal onto the
//!     display's current headroom (and onto plain SDR when it has none).
//!
//! The video then draws through `shader_hdr_output.wgsl` — PQ handed
//! through, HLG and SDR converted to PQ — and subtitles through their PQ
//! pipeline. Leaving the session restores the layer's original colorspace
//! and the renderer's normal surface format; the in-shader tonemap takes
//! over again.
//!
//! Whether to enter a session is decided by the renderer from the host's
//! display-capability mask (`Player::set_display_hdr_types`, bit 1 =
//! HDR10). The `RUST_PLAYER_HDR_OUTPUT` env var overrides it for testing:
//! `1`/`force` enters a session even on an SDR display (the OS then maps
//! PQ to SDR itself, so the path can be exercised and eyeballed on any
//! Mac), `0`/`off` never enters one.

#![cfg(any(target_os = "macos", target_os = "ios"))]

use std::ffi::{c_char, c_void};

use objc2::encode::{Encoding, RefEncode};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{msg_send, sel};

/// Surface format of an HDR output session.
pub(super) const HDR_OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Mastering peak handed to the OS tone mapper when the stream's own
/// metadata isn't known (the common HDR10 mastering peak).
pub(super) const DEFAULT_MASTERING_PEAK_NITS: f32 = 1000.0;

/// `RUST_PLAYER_HDR_OUTPUT` override, sampled once per renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HdrOutputOverride {
    /// Follow the host's display-capability mask.
    Auto,
    Force,
    Off,
}

impl HdrOutputOverride {
    pub(super) fn from_env() -> Self {
        Self::parse(std::env::var("RUST_PLAYER_HDR_OUTPUT").ok().as_deref())
    }

    fn parse(v: Option<&str>) -> Self {
        match v.map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if matches!(s.as_str(), "1" | "on" | "force" | "true") => Self::Force,
            Some(s) if matches!(s.as_str(), "0" | "off" | "false") => Self::Off,
            _ => Self::Auto,
        }
    }

    /// Should the renderer run an HDR output session, given the host's
    /// display mask (Display.HdrCapabilities order, bit 1 = HDR10)?
    pub(super) fn wants_session(self, display_hdr_types: u32) -> bool {
        const DISPLAY_HDR10: u32 = 1 << 1;
        match self {
            Self::Force => true,
            Self::Off => false,
            Self::Auto => display_hdr_types & DISPLAY_HDR10 != 0,
        }
    }
}

/// The three output pipelines (shared group-0 plane layout, target
/// [`HDR_OUTPUT_FORMAT`]). Built on the first session entry.
pub(super) struct HdrOutputPipelines {
    pub pq: wgpu::RenderPipeline,
    pub hlg: wgpu::RenderPipeline,
    pub sdr: wgpu::RenderPipeline,
}

impl HdrOutputPipelines {
    pub(super) fn new(
        device: &wgpu::Device,
        plane_layout: &wgpu::BindGroupLayout,
        vertex_layout: wgpu::VertexBufferLayout<'static>,
    ) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shader_hdr_output"),
            source: wgpu::ShaderSource::Wgsl(crate::shader_src::hdr_output().into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout (HDR output)"),
            bind_group_layouts: &[Some(plane_layout)],
            immediate_size: 0,
        });
        let make = |label: &'static str, entry: &'static str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: std::slice::from_ref(&vertex_layout),
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: HDR_OUTPUT_FORMAT,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        Self {
            pq: make("Render Pipeline (HDR output, PQ)", "fs_pq"),
            hlg: make("Render Pipeline (HDR output, HLG)", "fs_hlg"),
            sdr: make("Render Pipeline (HDR output, SDR→PQ)", "fs_sdr"),
        }
    }
}

/// What the renderer must do about the EDR session for this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionStep {
    /// Nothing changes; the session is (still) `active`.
    Keep { active: bool },
    Enter,
    Leave,
}

/// Enter on an HDR frame while the display allows a session, leave as soon
/// as it doesn't. An SDR frame never starts one (SDR content alone must
/// not switch the display into HDR), but keeps a running one.
pub(super) fn session_step(allowed: bool, active: bool, frame_is_hdr: bool) -> SessionStep {
    match (allowed, active, frame_is_hdr) {
        (true, false, true) => SessionStep::Enter,
        (false, true, _) => SessionStep::Leave,
        _ => SessionStep::Keep { active },
    }
}

/// EDR session state of one renderer. Pipelines are built on the first
/// entry; `saved` holds the layer state to restore on exit.
pub(super) struct EdrSession {
    override_: HdrOutputOverride,
    pub(super) pipelines: std::sync::OnceLock<HdrOutputPipelines>,
    pub(super) saved: std::sync::Mutex<Option<SavedLayerState>>,
    active: std::sync::atomic::AtomicBool,
    /// Entering failed (no EDR on the layer, no PQ colour space, offscreen
    /// target): stay on the tonemap for this renderer's lifetime instead
    /// of retrying every frame.
    failed: std::sync::atomic::AtomicBool,
}

impl EdrSession {
    pub(super) fn new() -> Self {
        let override_ = HdrOutputOverride::from_env();
        if override_ != HdrOutputOverride::Auto {
            log::info!("[hdr-output] RUST_PLAYER_HDR_OUTPUT override: {:?}", override_);
        }
        Self {
            override_,
            pipelines: std::sync::OnceLock::new(),
            saved: std::sync::Mutex::new(None),
            active: std::sync::atomic::AtomicBool::new(false),
            failed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(super) fn set_active(&self, active: bool) {
        self.active.store(active, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn fail(&self) {
        self.failed.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The step for a frame, given the host's display mask and whether
    /// direct mode owns the picture (then the renderer never runs one).
    pub(super) fn step(&self, display_hdr_types: u32, frame_is_hdr: bool, direct_mode: bool) -> SessionStep {
        let allowed = !direct_mode
            && !self.failed.load(std::sync::atomic::Ordering::Relaxed)
            && self.override_.wants_session(display_hdr_types);
        session_step(allowed, self.is_active(), frame_is_hdr)
    }
}

// -------------------------------------------------------------------------
// CAMetalLayer colorspace / EDR metadata
// -------------------------------------------------------------------------

/// Opaque `CGColorSpace`, encoded so objc2's debug-mode signature check
/// accepts it as the `CGColorSpaceRef` argument of `-setColorspace:`.
#[repr(C)]
struct CGColorSpace {
    _private: [u8; 0],
}

unsafe impl RefEncode for CGColorSpace {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColorSpace", &[]));
}

type CFStringRef = *const c_void;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFRelease(cf: *const c_void);
    fn CFRetain(cf: *const c_void) -> *const c_void;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGColorSpaceCreateWithName(name: CFStringRef) -> *mut CGColorSpace;
}

/// `CGColorSpaceCreateWithName` by the constant's string value — looking
/// the name up at runtime instead of linking the `kCGColorSpace…` symbol
/// keeps the binary loadable on OS versions that predate it.
fn color_space_named(name: &std::ffi::CStr) -> *mut CGColorSpace {
    unsafe {
        let cf = CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), K_CF_STRING_ENCODING_UTF8);
        if cf.is_null() {
            return std::ptr::null_mut();
        }
        let cs = CGColorSpaceCreateWithName(cf);
        CFRelease(cf);
        cs
    }
}

/// BT.2100 PQ colour space: the macOS 11 / iOS 14 name, then the older
/// macOS 10.15.4 / iOS 13.4 one.
fn pq_color_space() -> *mut CGColorSpace {
    let cs = color_space_named(c"kCGColorSpaceITUR_2100_PQ");
    if !cs.is_null() {
        return cs;
    }
    color_space_named(c"kCGColorSpaceITUR_2020_PQ")
}

fn responds_to(obj: &AnyObject, sel: objc2::runtime::Sel) -> bool {
    let r: Bool = unsafe { msg_send![obj, respondsToSelector: sel] };
    r.as_bool()
}

/// Layer state captured on session entry, restored on exit.
pub(super) struct SavedLayerState {
    /// Retained `CGColorSpaceRef` (may be null = the layer had none).
    colorspace: *mut CGColorSpace,
}

// The pointer is a retained, immutable CoreFoundation object.
unsafe impl Send for SavedLayerState {}
unsafe impl Sync for SavedLayerState {}

impl Drop for SavedLayerState {
    fn drop(&mut self) {
        if !self.colorspace.is_null() {
            unsafe { CFRelease(self.colorspace as *const c_void) };
        }
    }
}

/// The `CAMetalLayer` behind a wgpu surface.
fn with_layer<R>(surface: &wgpu::Surface<'_>, f: impl FnOnce(&AnyObject) -> R) -> Option<R> {
    let hal = unsafe { surface.as_hal::<wgpu::hal::api::Metal>() }?;
    let layer = hal.render_layer().lock();
    let obj: &AnyObject = unsafe { &*(Retained::as_ptr(&layer) as *const AnyObject) };
    Some(f(obj))
}

/// Can this layer run an EDR session at all? `wantsExtendedDynamicRangeContent`
/// is macOS 10.11 / iOS 16 — wgpu-hal calls it unconditionally for an
/// rgba16float surface, so this must hold before the surface is reconfigured.
pub(super) fn layer_supports_edr(surface: &wgpu::Surface<'_>) -> bool {
    with_layer(surface, |layer| {
        responds_to(layer, sel!(setWantsExtendedDynamicRangeContent:))
            && responds_to(layer, sel!(setColorspace:))
    })
    .unwrap_or(false)
}

/// Switch the layer to PQ + HDR10 EDR metadata. Call right AFTER the
/// surface was reconfigured to [`HDR_OUTPUT_FORMAT`]. Returns the state to
/// restore on exit, or `None` when the PQ colour space is unavailable (the
/// caller must then revert the surface format).
pub(super) fn enter_layer_pq(surface: &wgpu::Surface<'_>, mastering_peak_nits: f32) -> Option<SavedLayerState> {
    let pq = pq_color_space();
    if pq.is_null() {
        log::warn!("[hdr-output] no BT.2100 PQ colour space on this OS");
        return None;
    }
    let saved = with_layer(surface, |layer| unsafe {
        let old: *mut CGColorSpace = msg_send![layer, colorspace];
        let old = if old.is_null() { old } else { CFRetain(old as *const c_void) as *mut CGColorSpace };
        let _: () = msg_send![layer, setColorspace: pq];
        // wgpu-hal already set this for rgba16float; re-assert in case a
        // host touched the layer between configure and here.
        let _: () = msg_send![layer, setWantsExtendedDynamicRangeContent: Bool::YES];
        set_edr_metadata(layer, mastering_peak_nits);
        SavedLayerState { colorspace: old }
    });
    unsafe { CFRelease(pq as *const c_void) };
    saved
}

/// `CAEDRMetadata.HDR10(minLuminance:maxLuminance:opticalOutputScale:)` —
/// without it the OS clips PQ above the display's headroom instead of
/// tone-mapping. macOS 10.15 / iOS 16; skipped where absent.
unsafe fn set_edr_metadata(layer: &AnyObject, mastering_peak_nits: f32) {
    let Some(cls) = AnyClass::get(c"CAEDRMetadata") else { return };
    if !responds_to(layer, sel!(setEDRMetadata:)) {
        return;
    }
    // opticalOutputScale 100: a PQ code of 100 nits maps to EDR 1.0 (SDR
    // white), Apple's recommended value for HDR10 video.
    let meta: Option<Retained<AnyObject>> = msg_send![
        cls,
        HDR10MetadataWithMinLuminance: 0.005f32,
        maxLuminance: mastering_peak_nits.max(100.0),
        opticalOutputScale: 100.0f32
    ];
    if let Some(meta) = meta {
        let _: () = msg_send![layer, setEDRMetadata: &*meta];
    }
}

/// Undo [`enter_layer_pq`]. Call right BEFORE the surface is reconfigured
/// back to its SDR format (which also turns EDR off in wgpu-hal).
pub(super) fn exit_layer_pq(surface: &wgpu::Surface<'_>, saved: SavedLayerState) {
    with_layer(surface, |layer| unsafe {
        let _: () = msg_send![layer, setColorspace: saved.colorspace];
        if responds_to(layer, sel!(setEDRMetadata:)) {
            let none: *const AnyObject = std::ptr::null();
            let _: () = msg_send![layer, setEDRMetadata: none];
        }
    });
    drop(saved);
}

#[cfg(test)]
mod tests {
    use super::HdrOutputOverride as O;

    #[test]
    fn env_override_parses() {
        assert_eq!(O::parse(None), O::Auto);
        assert_eq!(O::parse(Some("")), O::Auto);
        assert_eq!(O::parse(Some("1")), O::Force);
        assert_eq!(O::parse(Some(" Force ")), O::Force);
        assert_eq!(O::parse(Some("off")), O::Off);
        assert_eq!(O::parse(Some("0")), O::Off);
    }

    #[test]
    fn session_follows_display_mask_unless_overridden() {
        const HDR10: u32 = 1 << 1;
        const DV: u32 = 1 << 0;
        assert!(O::Auto.wants_session(HDR10));
        assert!(O::Auto.wants_session(HDR10 | DV));
        assert!(!O::Auto.wants_session(DV));
        assert!(!O::Auto.wants_session(0));
        assert!(O::Force.wants_session(0));
        assert!(!O::Off.wants_session(HDR10));
    }

    #[test]
    fn session_enters_on_hdr_frames_only_and_leaves_when_disallowed() {
        use super::SessionStep::*;
        assert_eq!(super::session_step(true, false, true), Enter);
        assert_eq!(super::session_step(true, false, false), Keep { active: false }, "SDR frame");
        assert_eq!(super::session_step(true, true, false), Keep { active: true }, "SDR mid-session");
        assert_eq!(super::session_step(false, true, true), Leave);
        assert_eq!(super::session_step(false, false, true), Keep { active: false });
    }

    #[test]
    fn pq_colour_space_resolves() {
        let cs = super::pq_color_space();
        assert!(!cs.is_null());
        unsafe { super::CFRelease(cs as *const std::ffi::c_void) };
    }
}
