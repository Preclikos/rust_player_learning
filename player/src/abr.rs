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
//! ### Manual override
//!
//! When the consumer calls `Player::change_video_track`, the strategy
//! automatically flips back to `Manual`. The intent is: "the user just
//! made an explicit choice, stick with it until they re-enable ABR".
//! Re-arm ABR with another `set_abr_strategy(BandwidthEwma { .. })`.

/// Configures how the player picks among the video representations in the
/// currently selected `VideoAdaptation`.
#[derive(Clone, Copy, Debug)]
pub enum AbrStrategy {
    /// Fixed selection — never auto-switch. The player respects whatever
    /// was passed to `set_video_track` / `change_video_track`.
    Manual,
    /// Bandwidth-based ABR. On each tick, pick the highest representation
    /// whose `bitrate_bps * safety_factor <= ewma_bps`. A safety factor of
    /// `1.25` is a sane default: leaves 25% headroom for transient dips.
    BandwidthEwma { safety_factor: f32 },
}

impl Default for AbrStrategy {
    fn default() -> Self {
        AbrStrategy::Manual
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
}
