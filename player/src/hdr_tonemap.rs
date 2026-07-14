//! Runtime-tunable parameters for the HDR10 → SDR display tonemap.
//!
//! The player's HDR→SDR conversion is a faithful WGSL port of FFmpeg's
//! `tonemap_opencl` filter (libavfilter/opencl/tonemap.cl), so HDR10
//! representations render with the *same colour profile* as SDR
//! representations that were transcoded offline with that filter. The
//! four knobs below are exactly the filter's options of the same name;
//! the defaults reproduce the reference transcode command used for this
//! project's SDR ladder:
//!
//! ```text
//! tonemap_opencl=tonemap=mobius:param=0.01:desat=0:r=tv:p=bt709:t=bt709:m=bt709
//! ```
//!
//! Like the filter, the shader runs libplacebo-style frame peak/average
//! detection (63-frame rolling window with scene-change reset), so the
//! curve adapts to content brightness the same way the offline transcode
//! did. See `player/HDR_TONEMAP.md` for the full pipeline description.
//!
//! ## When this applies
//!
//! On every platform where the player owns the HDR→SDR conversion:
//!   - Windows (D3D11VA → DX12 P010 import → our shader_hdr.wgsl)
//!   - Linux   (VAAPI    → Vulkan P010 import → our shader_hdr.wgsl)
//!   - macOS / iOS (VideoToolbox → Metal plane import → the same
//!     shader_hdr.wgsl). Preferred surface is 10-bit 'x420'; when VT
//!     refuses it the 8-bit NV12 fallback is still a PQ/BT.2020 signal
//!     (VT converts pixel format only, never colour) and tonemaps
//!     through the same shader at 8-bit precision.
//!
//! Pipeline selection is per frame from the decoder-stamped
//! `VideoColorInfo` (SPS VUI transfer), not from the surface bit depth.
//!
//! Use [`PlayerCapabilities::hdr_tonemap_tunable`](crate::PlayerCapabilities)
//! at startup so the settings UI can hide the controls on platforms
//! where they won't do anything.

/// Knobs the HDR10 → SDR tonemap reads at draw time — a 1:1 mirror of
/// FFmpeg `tonemap_opencl` options. Host UI sends these in through
/// `Player::set_hdr_tonemap`; the player consumes them without
/// persisting (host is responsible for round-tripping the user's
/// choice through its own settings storage).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrTonemapParams {
    /// Mobius knee point `j` (the filter's `param`): input below this
    /// stays linear, above it is compressed toward the peak. Smaller =
    /// softer, more compressive curve; the FFmpeg default is 0.3, the
    /// reference transcode uses 0.01. Clamped to [0.001, 0.999] — the
    /// curve degenerates at 0 (everything compressed) and ≥1 (knee
    /// beyond SDR white).
    pub tone_param: f32,

    /// Desaturation strength (the filter's `desat`): highlights blend
    /// toward luma before tonemapping, scaled by `pow(coeff, 10/desat)`.
    /// 0 disables (the reference transcode's choice; FFmpeg defaults
    /// to 0.5). Clamped to [0, 100].
    pub desat: f32,

    /// Source signal peak override in REFERENCE_WHITE (100 nit) units —
    /// the filter's `peak` option, so 100.0 = 10 000 nits. 0 = auto:
    /// the PQ untagged-source default of 100, matching
    /// `ff_determine_signal_peak` when no mastering metadata is present.
    /// Only meaningful for the first frame(s) of playback — the frame
    /// peak detection takes over immediately after. Clamped to [0, 200].
    pub peak: f32,

    /// Scene-change threshold for the peak/average detection (the
    /// filter's `threshold`): when a frame's average signal differs
    /// from the rolling average by more than this, the detection
    /// window resets. FFmpeg default 0.2; 0 disables scene resets.
    /// Clamped to [0, 10].
    pub scene_threshold: f32,
}

impl HdrTonemapParams {
    /// Defaults reproducing the project's reference SDR transcode:
    /// `tonemap=mobius:param=0.01:desat=0` with the filter's stock
    /// scene threshold (0.2) and auto peak. Used as the fallback when
    /// the host has not pushed a per-user choice via
    /// `Player::set_hdr_tonemap`.
    pub const DEFAULT: Self = Self {
        tone_param: 0.01,
        desat: 0.0,
        peak: 0.0,
        scene_threshold: 0.2,
    };

    /// Clamp into the ranges the shader can render without going
    /// degenerate (divide-by-near-zero, knee outside the curve's
    /// domain, etc.). Called by `Player::set_hdr_tonemap` so a host
    /// that hands us slider values from an untrusted UI doesn't break
    /// rendering.
    pub fn sanitised(self) -> Self {
        Self {
            tone_param: self.tone_param.clamp(0.001, 0.999),
            desat: self.desat.clamp(0.0, 100.0),
            peak: if self.peak == 0.0 {
                0.0
            } else {
                self.peak.clamp(1.0, 200.0)
            },
            scene_threshold: self.scene_threshold.clamp(0.0, 10.0),
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
    fn default_matches_reference_transcode() {
        // The DEFAULT must mirror the reference ffmpeg command
        // (tonemap=mobius:param=0.01:desat=0, stock threshold, auto
        // peak) — that's the whole point of the port. DEFAULT must
        // also survive sanitisation unchanged (be in-range).
        assert_eq!(HdrTonemapParams::DEFAULT.tone_param, 0.01);
        assert_eq!(HdrTonemapParams::DEFAULT.desat, 0.0);
        assert_eq!(HdrTonemapParams::DEFAULT.peak, 0.0);
        assert_eq!(HdrTonemapParams::DEFAULT.scene_threshold, 0.2);
        assert_eq!(HdrTonemapParams::DEFAULT, HdrTonemapParams::DEFAULT.sanitised());
    }

    #[test]
    fn sanitised_clamps_low() {
        let p = HdrTonemapParams {
            tone_param: 0.0,
            desat: -1.0,
            peak: 0.5,
            scene_threshold: -0.1,
        }
        .sanitised();
        assert_eq!(p.tone_param, 0.001);
        assert_eq!(p.desat, 0.0);
        assert_eq!(p.peak, 1.0);
        assert_eq!(p.scene_threshold, 0.0);
    }

    #[test]
    fn sanitised_clamps_high() {
        let p = HdrTonemapParams {
            tone_param: 2.0,
            desat: 1000.0,
            peak: 10000.0,
            scene_threshold: 50.0,
        }
        .sanitised();
        assert_eq!(p.tone_param, 0.999);
        assert_eq!(p.desat, 100.0);
        assert_eq!(p.peak, 200.0);
        assert_eq!(p.scene_threshold, 10.0);
    }

    #[test]
    fn sanitised_keeps_auto_peak() {
        // peak == 0 means "auto" (PQ default 100) and must pass
        // through unclamped — clamping it to 1.0 would silently turn
        // auto into a 100-nit source.
        let p = HdrTonemapParams {
            peak: 0.0,
            ..HdrTonemapParams::DEFAULT
        };
        assert_eq!(p.sanitised().peak, 0.0);
    }

    #[test]
    fn sanitised_passes_sensible_values() {
        let p = HdrTonemapParams {
            tone_param: 0.3,
            desat: 0.5,
            peak: 100.0,
            scene_threshold: 0.2,
        };
        assert_eq!(p, p.sanitised());
    }
}
