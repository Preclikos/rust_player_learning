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

/// Decoder-overload detector behind the ABR pixel cap.
///
/// A bandwidth-only ABR climbs to whatever the link carries. On a device
/// whose decoder cannot keep up with that rung (an iPhone SE at 2160p
/// decodes ~13 fps and drops 10+ frames a second for the rest of playback)
/// nothing ever brought it back down: the bandwidth estimate says the rung
/// fits. hls.js answers this with `capLevelOnFPSDrop`; this is the same idea
/// on the per-second counters the sync loop already keeps: for `STRIKES`
/// consecutive ticks the frames dropped as late must be at least
/// `DROP_RATIO` of the frames presented, while at least `MIN_DECODED`
/// frames a second still get presented. Whether the decoder or the render
/// path is what cannot keep up does not matter: the rung is too heavy for
/// this device, and the bandwidth estimate will never say so. A tick with
/// almost nothing presented (a hidden browser tab on its 250 ms fallback, a
/// stall) is not a measurement and never counts as a strike.
#[derive(Debug, Default)]
pub(crate) struct DecodeOverloadDetector {
    rung: Option<u32>,
    decoded: u64,
    dropped: u64,
    strikes: u8,
}

impl DecodeOverloadDetector {
    pub(crate) const STRIKES: u8 = 3;
    /// Dropped ≥ this share of the decoded frames in a tick.
    pub(crate) const DROP_RATIO: f64 = 0.25;
    /// Fewer presented frames than this in a tick is a stall or a hidden
    /// tab, not a measurement.
    const MIN_DECODED: u64 = 8;

    /// One ABR tick (~1 s): the pipeline's cumulative frame counters and the
    /// representation they belong to. `true` once the device has been shown
    /// unable to keep up with `rung`.
    pub(crate) fn observe(&mut self, rung: u32, decoded_total: u64, dropped_total: u64) -> bool {
        if self.rung != Some(rung) || decoded_total < self.decoded {
            // New rung, or a rebuilt pipeline with fresh counters: take the
            // baseline. The tick that contains the switch itself drops the
            // frames OLD left behind and must not count against NEW.
            self.rung = Some(rung);
            self.decoded = decoded_total;
            self.dropped = dropped_total;
            self.strikes = 0;
            return false;
        }
        let decoded = decoded_total - self.decoded;
        let dropped = dropped_total.saturating_sub(self.dropped);
        self.decoded = decoded_total;
        self.dropped = dropped_total;
        let overloaded =
            decoded >= Self::MIN_DECODED && dropped as f64 >= Self::DROP_RATIO * decoded as f64;
        self.strikes = if overloaded { self.strikes + 1 } else { 0 };
        if self.strikes >= Self::STRIKES {
            self.strikes = 0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// iPhone SE at 2160p: ~13 presented, ~11 dropped per second.
    #[test]
    fn overload_trips_after_three_bad_ticks() {
        let mut d = DecodeOverloadDetector::default();
        assert!(!d.observe(0, 100, 0)); // baseline on the new rung
        assert!(!d.observe(0, 113, 11));
        assert!(!d.observe(0, 127, 22));
        assert!(d.observe(0, 140, 35));
        // Re-armed afterwards: no immediate second verdict.
        assert!(!d.observe(0, 153, 46));
    }

    #[test]
    fn a_hidden_tab_or_a_stall_is_not_a_measurement() {
        // Hidden browser tab: ~4 frames a second reach the screen on the
        // 250 ms fallback, everything else is dropped as late.
        let mut d = DecodeOverloadDetector::default();
        d.observe(0, 0, 0);
        for i in 1..=6 {
            assert!(!d.observe(0, 4 * i, 20 * i));
        }
        // A stall: next to nothing presented, next to nothing dropped.
        for i in 1..=5 {
            assert!(!d.observe(0, 24 + 2 * i, 120 + 2 * i));
        }
    }

    #[test]
    fn a_clean_tick_resets_the_strikes_and_a_switch_takes_a_baseline() {
        let mut d = DecodeOverloadDetector::default();
        d.observe(0, 0, 0);
        assert!(!d.observe(0, 13, 11));
        assert!(!d.observe(0, 26, 22));
        assert!(!d.observe(0, 50, 22)); // clean second: strikes back to 0
        assert!(!d.observe(0, 63, 33));
        assert!(!d.observe(0, 76, 44));
        // The switch to rung 1 (same counters, OLD's leftovers dropped) is a
        // baseline, never a strike.
        assert!(!d.observe(1, 80, 60));
        assert!(!d.observe(1, 93, 71));
        assert!(!d.observe(1, 106, 82));
        assert!(d.observe(1, 119, 93));
    }

    #[test]
    fn a_rebuilt_pipeline_with_fresh_counters_takes_a_baseline() {
        let mut d = DecodeOverloadDetector::default();
        d.observe(0, 500, 10);
        assert!(!d.observe(0, 513, 21));
        assert!(!d.observe(0, 3, 2)); // counters restarted: baseline, no strike
        assert!(!d.observe(0, 16, 13));
        assert!(!d.observe(0, 29, 24));
        assert!(d.observe(0, 42, 35));
    }

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
