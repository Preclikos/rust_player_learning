//! Runtime-tunable parameters for the HDR10 → SDR display tonemap.
//!
//! Two knobs control how HDR10 (BT.2020 + PQ) content is mapped onto an
//! SDR display by the player's own fragment shader. Defaults are picked
//! to produce a "reasonable, slightly brighter than reference" image
//! that matches what most non-HDR displays in living rooms want. The
//! host UI surfaces these as user-facing settings (preset buttons or
//! slider) and pushes the resulting numbers in through
//! [`Player::set_hdr_tonemap`](crate::Player::set_hdr_tonemap).
//!
//! ## When this applies
//!
//! Only on platforms where the player owns the HDR→SDR conversion:
//!   - Windows (D3D11VA → DX12 P010 import → our shader_hdr.wgsl)
//!   - Linux   (VAAPI    → Vulkan P010 import → our shader_hdr.wgsl)
//!
//! On Apple platforms (macOS, iOS) VideoToolbox tonemaps internally and
//! hands the renderer pre-converted 8-bit BT.709 NV12 — our HDR shader
//! is never used. Calls to `set_hdr_tonemap` from those platforms are
//! silently no-op (the API exists for cross-platform host code).
//!
//! Use [`PlayerCapabilities::hdr_tonemap_tunable`](crate::PlayerCapabilities)
//! at startup so the settings UI can hide the slider on platforms
//! where it won't do anything.
//!
//! See `player/HDR_TONEMAP.md` for what each parameter does perceptually
//! and what preset values commonly produce.

/// Knobs the HDR10 → SDR tonemap reads at draw time. Host UI sends
/// these in through `Player::set_hdr_tonemap`; the player consumes them
/// without persisting (host is responsible for round-tripping the
/// user's choice through its own settings storage).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrTonemapParams {
    /// What input nit level the ACES tonemap treats as "SDR diffuse
    /// white" (i.e. its input domain's 1.0 point). BT.2390 specifies
    /// 100; HDR content graded for 200-nit+ peak displays consistently
    /// looks under-exposed on SDR at that reference, so we default
    /// lower. Smaller value = brighter overall output. Sensible range
    /// roughly 30-150; clamped to [10, 400] in the setter so a typo'd
    /// extreme can't divide-by-near-zero in the shader.
    pub reference_white_nits: f32,

    /// Post-tonemap perceptual gamma applied as `pow(tonemap_out, gamma)`.
    /// Values <1 lift shadows + midtones (less contrasty look),
    /// 1.0 = identity, >1 deepens shadows (more cinematic).
    /// Sensible range roughly 0.7-1.1; clamped to [0.5, 1.5] in the
    /// setter.
    pub shadow_lift_gamma: f32,
}

impl HdrTonemapParams {
    /// Values matching the shader's compile-time defaults before this
    /// API existed. Suitable as a fallback when the host has no
    /// per-user persisted choice yet.
    pub const DEFAULT: Self = Self {
        reference_white_nits: 60.0,
        shadow_lift_gamma: 0.85,
    };

    /// Clamp into the ranges the shader can render without going
    /// degenerate (divide-by-near-zero, full-black, etc.). Called by
    /// `Player::set_hdr_tonemap` so a host that hands us slider
    /// values from an untrusted UI doesn't break rendering.
    pub fn sanitised(self) -> Self {
        Self {
            reference_white_nits: self.reference_white_nits.clamp(10.0, 400.0),
            shadow_lift_gamma: self.shadow_lift_gamma.clamp(0.5, 1.5),
        }
    }
}

impl Default for HdrTonemapParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_shader_constants() {
        // If this test ever fails, double-check that the WGSL
        // shader_hdr.wgsl defaults and this struct's DEFAULT stay in
        // sync — they're meant to render identically before any host
        // override is pushed.
        assert_eq!(HdrTonemapParams::DEFAULT.reference_white_nits, 60.0);
        assert_eq!(HdrTonemapParams::DEFAULT.shadow_lift_gamma, 0.85);
    }

    #[test]
    fn sanitised_clamps_low() {
        let p = HdrTonemapParams {
            reference_white_nits: 0.5,
            shadow_lift_gamma: 0.1,
        }
        .sanitised();
        assert_eq!(p.reference_white_nits, 10.0);
        assert_eq!(p.shadow_lift_gamma, 0.5);
    }

    #[test]
    fn sanitised_clamps_high() {
        let p = HdrTonemapParams {
            reference_white_nits: 10000.0,
            shadow_lift_gamma: 50.0,
        }
        .sanitised();
        assert_eq!(p.reference_white_nits, 400.0);
        assert_eq!(p.shadow_lift_gamma, 1.5);
    }

    #[test]
    fn sanitised_passes_sensible_values() {
        let p = HdrTonemapParams {
            reference_white_nits: 100.0,
            shadow_lift_gamma: 0.95,
        };
        assert_eq!(p, p.sanitised());
    }
}
