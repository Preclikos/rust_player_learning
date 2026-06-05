//! Capability surface for hosts (UIs, integration tests, etc.) that need
//! to know what *this build of the player* can play before any decode or
//! render path starts.
//!
//! Two entry points:
//!
//!   - [`capabilities`] — synchronous, compile-time-only. Returns the
//!     intended capability shape for the current target_os. No GPU probe,
//!     no I/O. Use this for instant UI rendering, e.g. settings panels.
//!
//!   - [`probe_capabilities`] — asynchronous. Same shape, but each flag is
//!     additionally gated by what the available GPU adapter actually
//!     exposes. Creates a transient wgpu instance + adapter request, so
//!     expect 50-200ms on first call. Use this once at startup when the
//!     host needs a definitive answer (e.g. before asking the manifest API
//!     for HDR representations on a system whose GPU might not support
//!     them).
//!
//! Today the only capability axes are HDR10 and Dolby Vision — driven by
//! the `?hdr=true&dolbyVision=false` query parameters the host's manifest
//! API takes. The struct is `non_exhaustive` so adding fields later
//! (HDR pass-through output, AV1 decode, …) doesn't break consumers.

/// What this player build claims it can play.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlayerCapabilities {
    /// HEVC Main 10 (BT.2020 + PQ) — decoded as P010, tonemapped to SDR
    /// in the shader, displayed on whatever swapchain the host configured.
    /// `true` means the build has the full 10-bit code path wired through
    /// decoder + wgpu shader chain on the current target.
    pub hdr10: bool,

    /// Dolby Vision (any profile). Currently always `false`: there is no
    /// DV metadata parser and DV-only representations would fail at codec
    /// init.
    pub dolby_vision: bool,

    /// `true` when the build's HDR→SDR conversion runs in the player's
    /// own shader (Windows / Linux) and therefore responds to runtime
    /// tuning via [`Player::set_hdr_tonemap`](crate::Player::set_hdr_tonemap).
    /// `false` on platforms where the OS / decoder framework owns the
    /// conversion (macOS + iOS = VideoToolbox internal tonemap;
    /// Android — no HDR path yet). Settings UIs should hide the HDR
    /// tonemap controls when this is `false`.
    pub hdr_tonemap_tunable: bool,
}

/// Static, compile-time capabilities. Synchronous, no GPU probe.
///
/// For most consumers this is enough — the desktop targets all wire HDR10
/// end-to-end and downgrading at the GPU level happens transparently if
/// the adapter lacks `TEXTURE_FORMAT_P010` (the player just won't request
/// HDR-tier representations once the renderer reports the missing
/// feature). When the host needs a stricter answer up front, call
/// [`probe_capabilities`].
pub const fn capabilities() -> PlayerCapabilities {
    PlayerCapabilities {
        // HDR10 (HEVC Main10) is wired on the desktop targets:
        //   - Windows: D3D11VA hwaccel + DX11→DX12 P010 shared import,
        //              wgpu shader_hdr (Rec.2020 + PQ → SDR tonemap).
        //   - Linux:   VAAPI hwaccel + DMA-fd Vulkan P010 import.
        //   - macOS:   VideoToolbox + Metal CVPixelBuffer P010 plane.
        // Android (MediaCodec / AHardwareBuffer) and iOS (VTDecompression)
        // can decode Main10 but the 10-bit render path isn't wired through
        // them yet — keep them false until the AHB-P010 and Metal-P010
        // paths exist.
        hdr10: cfg!(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "macos",
        )),

        dolby_vision: false,

        // Tunable only where the player's own shader does the HDR→SDR
        // conversion. macOS / iOS hand us pre-tonemapped 8-bit NV12 from
        // VideoToolbox — the shader knobs would do nothing there, so
        // hide the setting in the UI.
        hdr_tonemap_tunable: cfg!(any(
            target_os = "windows",
            target_os = "linux",
        )),
    }
}

/// Runtime-probed capabilities. Asks the wgpu instance which features the
/// active adapter actually exposes and gates each flag on what's truly
/// available.
///
/// Cost: one wgpu instance + one adapter request. The transient instance
/// drops at the end of this function, so it's safe to call from startup
/// before the real player builds its own. Don't poll this — cache the
/// result for the lifetime of the host.
pub async fn probe_capabilities() -> PlayerCapabilities {
    let mut caps = capabilities();
    if !caps.hdr10 {
        return caps;
    }

    // HDR10 needs the wgpu `TEXTURE_FORMAT_P010` feature on the adapter we
    // intend to use. The real renderer picks DX12 on Windows, Vulkan on
    // Linux, Metal on macOS — probe the same backend so the answer
    // matches what playback will actually see.
    #[cfg(target_os = "windows")]
    let backends = wgpu::Backends::DX12;
    #[cfg(target_os = "linux")]
    let backends = wgpu::Backends::VULKAN;
    #[cfg(target_os = "macos")]
    let backends = wgpu::Backends::METAL;
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    let backends = wgpu::Backends::PRIMARY;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    });

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        })
        .await;

    // On macOS the Metal backend always supports the P010 path (we sample
    // it as two separate single-plane MTLTextures, not as a wgpu P010
    // texture), so the adapter probe only needs to confirm an adapter
    // exists at all. On Windows/Linux we additionally require the
    // `TEXTURE_FORMAT_P010` feature for the wgpu plane-view import path.
    let p010_ok = match adapter {
        Ok(adapter) => {
            #[cfg(target_os = "macos")]
            {
                let _ = adapter;
                true
            }
            #[cfg(not(target_os = "macos"))]
            {
                adapter
                    .features()
                    .contains(wgpu::Features::TEXTURE_FORMAT_P010)
            }
        }
        Err(_) => false,
    };

    if !p010_ok {
        caps.hdr10 = false;
    }
    caps
}
