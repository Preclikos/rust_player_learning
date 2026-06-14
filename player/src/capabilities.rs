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
        // HDR10 (HEVC Main10) is wired on:
        //   - Windows: D3D11VA hwaccel + DX11→DX12 P010 shared import,
        //              wgpu shader_hdr (Rec.2020 + PQ → SDR tonemap).
        //   - Linux:   VAAPI hwaccel + DMA-fd Vulkan P010 import.
        //   - macOS / iOS: VTDecompressionSession with a 10-bit ('x420')
        //              destination, imported as R16/RG16 plane textures
        //              through CVMetalTextureCache into the same
        //              shader_hdr pipeline + detection passes as desktop.
        //              Falls back to VT-internal 8-bit conversion when the
        //              10-bit destination is refused.
        //   - Android: MediaCodec 10-bit AHardwareBuffer surface + the
        //              GLES OES PQ→SDR mobius tonemap program
        //              (video_gles_egl.rs). Colorimetry comes from the SPS
        //              VUI, so this works even when the MPD mis-signals.
        hdr10: cfg!(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "android",
        )),

        dolby_vision: false,

        // Tunable everywhere the player's own shader does the HDR→SDR
        // conversion: the wgpu shader_hdr path (desktop + Apple 10-bit
        // destination) and the Android GLES tonemap program all read
        // tone_param/desat/peak from set_hdr_tonemap per frame. Only the
        // Apple 8-bit fallback (VT-internal conversion) ignores them.
        hdr_tonemap_tunable: cfg!(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "android",
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
    let caps = capabilities();
    if !caps.hdr10 {
        return caps;
    }

    // Android renders HDR through the GLES OES external-texture program —
    // the wgpu TEXTURE_FORMAT_P010 feature plays no part there, so the
    // desktop-oriented adapter probe below would only produce a false
    // negative. The OES path degrades gracefully at runtime anyway
    // (SDR passthrough + warning if the HDR program fails to compile).
    #[cfg(target_os = "android")]
    return caps;

    #[cfg(not(target_os = "android"))]
    {
    let mut caps = caps;

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

    // On Apple Metal (macOS + iOS) the P010 path samples two separate
    // single-plane MTLTextures, not a wgpu P010 texture, so the adapter
    // probe only needs to confirm an adapter exists at all. On
    // Windows/Linux we additionally require the `TEXTURE_FORMAT_P010`
    // feature for the wgpu plane-view import path.
    let p010_ok = match adapter {
        Ok(adapter) => {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            {
                let _ = adapter;
                true
            }
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
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
}
