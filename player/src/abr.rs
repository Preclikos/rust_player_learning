//! Adaptive bitrate strategy. See PLAYER_INTEGRATION.md §6.2.
//!
//! Two modes:
//!   - [`AbrStrategy::Manual`] — the player never changes the user's selection.
//!     `set_video_track` / `change_video_track` are sticky.
//!   - [`AbrStrategy::BandwidthEwma`] — a background tick task periodically
//!     reconsiders the chosen representation against `ewma_bps`, picking the
//!     highest representation whose `bitrate * safety_factor` fits the
//!     measured EWMA. Switches fire `TrackChanged` events.
//!
//! ### HDR / bit-depth policy
//!
//! Orthogonal to bandwidth, the auto-pick respects an [`AbrVideoProfile`]
//! that constrains *which* representations are eligible at all. The bitrate
//! selector then picks among the filtered set. The default `Adaptive` is a
//! no-op (every representation eligible); the other variants let the host
//! UI scope ABR to SDR-only, HDR-preferred, or a fixed bit-depth lane.
//! See [`AbrVideoProfile::filter_indices`].
//!
//! ### Manual override
//!
//! When the consumer calls `Player::change_video_track`, the strategy
//! automatically flips back to `Manual`. The intent is: "the user just
//! made an explicit choice, stick with it until they re-enable ABR".
//! Re-arm ABR with another `set_abr_strategy(BandwidthEwma { .. })`.

use crate::tracks::video::VideoRepresenation;

/// Configures how the player picks among the video representations in the
/// currently selected `VideoAdaptation`.
#[derive(Clone, Copy, Debug, Default)]
pub enum AbrStrategy {
    /// Fixed selection — never auto-switch. The player respects whatever
    /// was passed to `set_video_track` / `change_video_track`.
    #[default]
    Manual,
    /// Bandwidth-based ABR. On each tick, pick the highest representation
    /// whose `bitrate_bps * safety_factor <= ewma_bps`. A safety factor of
    /// `1.25` is a sane default: leaves 25% headroom for transient dips.
    BandwidthEwma { safety_factor: f32 },
}

/// Bit-depth / HDR policy applied to the candidate set *before* the bandwidth
/// selector picks. Lets the host UI restrict the auto-switch to a slice of
/// the available representations without rewriting the manifest query.
///
/// All variants are no-ops on adaptations that don't contain the relevant
/// flavour (e.g. `HdrPreferred` is identical to `Adaptive` for an
/// SDR-only stream).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AbrVideoProfile {
    /// Only 8-bit SDR representations are eligible. HDR10 and Dolby Vision
    /// reps are filtered out even when present in the adaptation set.
    /// Use this when the host has decided the user (or hardware) shouldn't
    /// see HDR — e.g. `PlayerCapabilities::hdr10 == false`, or the user
    /// explicitly toggled HDR off in settings.
    SdrOnly,

    /// HDR10 representations are preferred when the adaptation contains
    /// any. The bitrate selector then picks the highest HDR10 rep that
    /// fits the bandwidth budget; SDR fallback only kicks in when there
    /// are no HDR10 reps in the set.
    HdrPreferred,

    /// Lock to a specific Y'CbCr bit depth (typically 8 or 10). The
    /// selector ignores reps that don't match. Useful when ABR mid-stream
    /// decoder reinit is undesirable — pin the lane and let bitrate alone
    /// drive switches.
    LockedDepth(u8),

    /// No restriction — bitrate is the only criterion. Default. Matches
    /// the pre-AbrVideoProfile behaviour.
    #[default]
    Adaptive,
}

impl AbrVideoProfile {
    /// Return the indices (into `reps`) of representations eligible under
    /// this profile. The bitrate selector should run against the
    /// corresponding bandwidth slice and remap the result back through
    /// these indices.
    ///
    /// Returns an empty `Vec` only when the profile filters out every
    /// representation — caller should keep the currently-playing rep in
    /// that case rather than picking something the policy forbids.
    pub fn filter_indices(&self, reps: &[VideoRepresenation]) -> Vec<usize> {
        let any_hdr = reps.iter().any(|r| r.hdr10 || r.dolby_vision);
        reps.iter()
            .enumerate()
            .filter(|(_, r)| self.allows(r, any_hdr))
            .map(|(i, _)| i)
            .collect()
    }

    fn allows(&self, r: &VideoRepresenation, any_hdr_in_set: bool) -> bool {
        // A Dolby Vision rep without a playable base layer (profile 5
        // IPTPQc2) can never render correctly — no profile may select it.
        if r.dolby_vision && !r.dv_base_layer_playable() {
            return false;
        }
        match self {
            AbrVideoProfile::SdrOnly => !r.hdr10 && !r.dolby_vision,
            AbrVideoProfile::HdrPreferred => {
                if any_hdr_in_set {
                    r.hdr10 || r.dolby_vision
                } else {
                    // The adaptation has no HDR reps at all — fall through
                    // to "everything eligible" so we don't strand the user
                    // on no representation.
                    true
                }
            }
            AbrVideoProfile::LockedDepth(8) => !r.is_10bit(),
            AbrVideoProfile::LockedDepth(10) => r.is_10bit(),
            // Unknown bit depth requested — be permissive rather than
            // strand the user.
            AbrVideoProfile::LockedDepth(_) => true,
            AbrVideoProfile::Adaptive => true,
        }
    }
}

/// hls.js `abrBandWidthFactor` (0.95): the rung being played is KEPT while
/// its bitrate still fits this fraction of the estimate. Switching down
/// therefore needs the estimate to fall below the rung itself, while
/// switching up needs `bitrate × safety_factor ≤ estimate` — two different
/// thresholds, which is what stops an estimate hovering at a rung boundary
/// from flapping every switch interval. (Measured on the Streamer: 4K ↔
/// 1440p every ~10 s with the estimate swinging 15.9–19.5 Mbps around
/// 14 Mbps × 1.25; with this rule the same trace stays put.)
pub const ABR_STAY_FACTOR: f64 = 0.95;

/// Given the available representations (in any order), the current EWMA in
/// bits per second and the index of the rung being played (`None` at start),
/// return the index of the representation the ABR engine wants to play, or
/// `None` on an empty list.
///
/// Up: the highest `bandwidth` with `bandwidth * safety_factor <= ewma_bps`
/// (`safety_factor` 1.43 ≈ hls.js `abrBandWidthUpFactor` / ExoPlayer
/// `bandwidthFraction` 0.7). Down: only when the current rung no longer fits
/// `ewma_bps × ABR_STAY_FACTOR`, and then to the highest rung that does fit
/// the up-budget. If nothing fits at all, the lowest-bitrate rung — better
/// to render something than nothing.
pub fn pick_representation(
    bandwidths_bps: &[u64],
    ewma_bps: u64,
    safety_factor: f32,
    current: Option<usize>,
) -> Option<usize> {
    if bandwidths_bps.is_empty() {
        return None;
    }
    let budget = (ewma_bps as f64 / safety_factor.max(0.1) as f64) as u64;
    let mut best: Option<(usize, u64)> = None;
    let mut min: Option<(usize, u64)> = None;
    for (i, &bw) in bandwidths_bps.iter().enumerate() {
        if bw <= budget {
            match best {
                Some((_, cur)) if cur >= bw => {}
                _ => best = Some((i, bw)),
            }
        }
        match min {
            Some((_, cur)) if cur <= bw => {}
            _ => min = Some((i, bw)),
        }
    }
    let pick = best.map(|(i, _)| i).or(min.map(|(i, _)| i))?;
    // Hysteresis: a down-switch is only taken once the current rung has
    // genuinely stopped fitting the estimate.
    if let Some(cur) = current.filter(|&c| c < bandwidths_bps.len()) {
        let cur_bw = bandwidths_bps[cur];
        if bandwidths_bps[pick] < cur_bw && (cur_bw as f64) <= ewma_bps as f64 * ABR_STAY_FACTOR {
            return Some(cur);
        }
    }
    Some(pick)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_highest_within_budget() {
        let bw = [1_000_000, 3_000_000, 5_000_000, 8_000_000];
        // 5 Mbps EWMA, 1.25 safety → budget = 4 Mbps → pick 3 Mbps.
        assert_eq!(pick_representation(&bw, 5_000_000, 1.25, None), Some(1));
    }

    #[test]
    fn falls_back_to_lowest_when_starved() {
        let bw = [3_000_000, 5_000_000];
        // 1 Mbps EWMA → nothing fits → return lowest.
        assert_eq!(pick_representation(&bw, 1_000_000, 1.25, None), Some(0));
        // Even when a higher rung is playing: below the stay threshold too.
        assert_eq!(pick_representation(&bw, 1_000_000, 1.25, Some(1)), Some(0));
    }

    #[test]
    fn handles_empty() {
        assert_eq!(pick_representation(&[], 5_000_000, 1.25, None), None);
    }

    #[test]
    fn down_switches_only_once_the_current_rung_stops_fitting() {
        // The Streamer trace: 8 Mbps and 14 Mbps rungs, the estimate
        // hovering around 14 × 1.25 = 17.5 Mbps. Without hysteresis every
        // 8 s tick flipped between them.
        let bw = [8_000_000, 14_000_000];
        let trace = [19_532_001u64, 15_870_719, 17_603_956, 17_466_348, 19_455_411, 17_045_096, 18_604_157, 16_059_807];
        let mut cur = pick_representation(&bw, trace[0], 1.25, None).unwrap();
        assert_eq!(cur, 1, "19.5 Mbps takes the 14 Mbps rung");
        let mut switches = 0;
        for &ewma in &trace[1..] {
            let next = pick_representation(&bw, ewma, 1.25, Some(cur)).unwrap();
            if next != cur {
                switches += 1;
                cur = next;
            }
        }
        // 14 Mbps fits 0.95 × every estimate in the trace (min 15.9 Mbps),
        // so the rung is kept throughout.
        assert_eq!(switches, 0);
        // Once the estimate really drops below the rung, the down-switch
        // fires (9.8 Mbps × 0.95 < 14 Mbps → 8 Mbps).
        assert_eq!(pick_representation(&bw, 9_827_177, 1.25, Some(1)), Some(0));
        // And the same rule on the way back up: 10.9 Mbps / 1.25 < 14 Mbps
        // → stays on 8 Mbps; a real recovery to 20 Mbps takes 14 Mbps.
        assert_eq!(pick_representation(&bw, 10_894_718, 1.25, Some(0)), Some(0));
        assert_eq!(pick_representation(&bw, 20_000_000, 1.25, Some(0)), Some(1));
    }

    // ---- AbrVideoProfile filter tests ----

    fn make_rep(id: u32, codecs: &str, hdr10: bool, dolby_vision: bool) -> VideoRepresenation {
        use crate::tracks::segment::Segment;
        let empty_seg = Segment::new(&String::new(), &String::new(), 0, 0, None, None, None)
            .expect("test stub segment");
        VideoRepresenation {
            id,
            base_url: String::new(),
            file_url: String::new(),
            segment_init: empty_seg.clone(),
            segment_range: empty_seg,
            segments: Vec::new(),
            bandwidth: 1_000_000,
            codecs: codecs.to_string(),
            mime_type: "video/mp4".to_string(),
            width: 1920,
            height: 1080,
            sar: String::new(),
            hdr10,
            dolby_vision,
        }
    }

    #[test]
    fn adaptive_lets_everything_playable_through() {
        let reps = vec![
            make_rep(1, "hvc1.1.6.L120.90", false, false),
            make_rep(2, "hvc1.2.4.L120.90", true, false),
            make_rep(3, "dvh1.05.06", false, true),
            make_rep(4, "dvh1.08.06", false, true),
        ];
        let idx = AbrVideoProfile::Adaptive.filter_indices(&reps);
        // DV profile 5 (IPTPQc2, no compatible base layer) is never
        // eligible; profile 8 plays via its HDR10-compatible BL.
        assert_eq!(idx, vec![0, 1, 3]);
    }

    #[test]
    fn sdr_only_drops_hdr_and_dv() {
        let reps = vec![
            make_rep(1, "hvc1.1.6.L120.90", false, false),
            make_rep(2, "hvc1.2.4.L120.90", true, false),
            make_rep(3, "dvh1.05.06", false, true),
        ];
        let idx = AbrVideoProfile::SdrOnly.filter_indices(&reps);
        assert_eq!(idx, vec![0]);
    }

    #[test]
    fn hdr_preferred_filters_to_hdr_when_present() {
        let reps = vec![
            make_rep(1, "hvc1.1.6.L120.90", false, false),
            make_rep(2, "hvc1.2.4.L120.90", true, false),
        ];
        let idx = AbrVideoProfile::HdrPreferred.filter_indices(&reps);
        assert_eq!(idx, vec![1]);
    }

    #[test]
    fn hdr_preferred_falls_back_when_no_hdr() {
        let reps = vec![
            make_rep(1, "hvc1.1.6.L120.90", false, false),
            make_rep(2, "avc1.64001f", false, false),
        ];
        let idx = AbrVideoProfile::HdrPreferred.filter_indices(&reps);
        assert_eq!(idx, vec![0, 1]);
    }

    #[test]
    fn locked_depth_8_keeps_sdr_only() {
        let reps = vec![
            make_rep(1, "hvc1.1.6.L120.90", false, false),
            make_rep(2, "hvc1.2.4.L120.90", true, false),
        ];
        let idx = AbrVideoProfile::LockedDepth(8).filter_indices(&reps);
        assert_eq!(idx, vec![0]);
    }

    #[test]
    fn locked_depth_10_keeps_main10_only() {
        let reps = vec![
            make_rep(1, "hvc1.1.6.L120.90", false, false),
            make_rep(2, "hvc1.2.4.L120.90", true, false),
            make_rep(3, "dvh1.08.06", false, true),
        ];
        let idx = AbrVideoProfile::LockedDepth(10).filter_indices(&reps);
        // Both Main10 (id 2) and Dolby Vision profile 8 (id 3) are 10-bit.
        assert_eq!(idx, vec![1, 2]);
    }

    #[test]
    fn dv_profile_5_never_eligible() {
        let reps = vec![
            make_rep(1, "dvh1.05.06", false, true),
            make_rep(2, "dvhe.08.06", false, true),
        ];
        assert_eq!(AbrVideoProfile::Adaptive.filter_indices(&reps), vec![1]);
        assert_eq!(AbrVideoProfile::HdrPreferred.filter_indices(&reps), vec![1]);
        assert_eq!(AbrVideoProfile::LockedDepth(10).filter_indices(&reps), vec![1]);
    }
}
