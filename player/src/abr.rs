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

/// Given the available representations (sorted highest→lowest or in any
/// order) and the current EWMA in bits per second, return the index of the
/// representation the ABR engine wants to play, or `None` if no
/// representation fits the budget (caller keeps the current one).
///
/// Picks the highest `bandwidth` that satisfies
/// `bandwidth * safety_factor <= ewma_bps`. If every representation
/// exceeds the budget, returns the lowest-bitrate one — better to render
/// something than nothing.
pub fn pick_representation(
    bandwidths_bps: &[u64],
    ewma_bps: u64,
    safety_factor: f32,
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
    best.map(|(i, _)| i).or(min.map(|(i, _)| i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_highest_within_budget() {
        let bw = [1_000_000, 3_000_000, 5_000_000, 8_000_000];
        // 5 Mbps EWMA, 1.25 safety → budget = 4 Mbps → pick 3 Mbps.
        assert_eq!(pick_representation(&bw, 5_000_000, 1.25), Some(1));
    }

    #[test]
    fn falls_back_to_lowest_when_starved() {
        let bw = [3_000_000, 5_000_000];
        // 1 Mbps EWMA → nothing fits → return lowest.
        assert_eq!(pick_representation(&bw, 1_000_000, 1.25), Some(0));
    }

    #[test]
    fn handles_empty() {
        assert_eq!(pick_representation(&[], 5_000_000, 1.25), None);
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
    fn adaptive_lets_everything_through() {
        let reps = vec![
            make_rep(1, "hvc1.1.6.L120.90", false, false),
            make_rep(2, "hvc1.2.4.L120.90", true, false),
            make_rep(3, "dvh1.05.06", false, true),
        ];
        let idx = AbrVideoProfile::Adaptive.filter_indices(&reps);
        assert_eq!(idx, vec![0, 1, 2]);
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
            make_rep(3, "dvh1.05.06", false, true),
        ];
        let idx = AbrVideoProfile::LockedDepth(10).filter_indices(&reps);
        // Both Main10 (id 2) and Dolby Vision (id 3) are 10-bit.
        assert_eq!(idx, vec![1, 2]);
    }
}
