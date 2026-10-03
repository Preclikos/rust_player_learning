//! Direct-mode release lead (Android): how long before its display time a
//! decoded frame is released to the video Surface.
//!
//! Two forces pull on it:
//!
//!   * **The display** needs the buffer queued `presentationDeadline` before
//!     the vsync it should appear at. Released later, SurfaceFlinger shows it
//!     a vsync late or drops it — the micro-stutter the Google TV Streamer
//!     showed at a fixed 50 ms (34 ms deadline + a 41.7 ms vsync at 24 Hz).
//!     The lower bound is therefore that deadline, plus a margin for timer
//!     wake-up jitter, plus one vsync (a release stamp is not vsync-aligned),
//!     and never less than ExoPlayer's 50 ms. The host reports the display's
//!     numbers ([`crate::Player::set_display_timing`]); unknown = 50 ms.
//!   * **The decoder** loses one output buffer for every frame waiting in
//!     SurfaceFlinger. Amlogic (13 buffers, 8 needed for decoding) starved at
//!     100 ms. When the sync loop sees the codec refusing input while no
//!     decoded frame is waiting, the lead is capped one step lower; the cap
//!     relaxes again after a quiet period.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// ExoPlayer's release window, and the lead when the display is unknown.
pub const MIN_LEAD_MS: u64 = 50;
/// Timer wake-up jitter on top of the display deadline.
const WAKE_JITTER_MS: u64 = 2;
/// The starvation cap never goes below the display deadline plus jitter
/// (frames would be shown late for sure), nor below this.
const CAP_FLOOR_MIN_MS: u64 = 30;
/// One starvation cap step, down on starvation, up on recovery.
const CAP_STEP_MS: u64 = 10;
/// At most one cap step down per this interval: one starvation episode is
/// seen by many consecutive frames.
const STARVE_REARM: Duration = Duration::from_secs(1);
/// The cap relaxes one step after this long without starvation.
const CAP_RECOVER_AFTER: Duration = Duration::from_secs(30);

fn ns_to_ms_ceil(ns: i64) -> u64 {
    (ns.max(0) as u64).div_ceil(1_000_000)
}

/// Lower bound from the display: deadline + jitter + one vsync, ≥ 50 ms.
pub fn display_floor_ms(vsync_ns: i64, deadline_ns: i64) -> u64 {
    if vsync_ns <= 0 {
        return MIN_LEAD_MS;
    }
    let deadline_ms = ns_to_ms_ceil(deadline_ns);
    (deadline_ms + WAKE_JITTER_MS + ns_to_ms_ceil(vsync_ns)).max(MIN_LEAD_MS)
}

/// The lowest a starvation cap may push the lead.
pub fn cap_floor_ms(deadline_ns: i64) -> u64 {
    (ns_to_ms_ceil(deadline_ns) + WAKE_JITTER_MS).max(CAP_FLOOR_MIN_MS)
}

/// The lead in force: the display floor, lowered by a starvation cap
/// (0 = none) but never below the cap floor.
pub fn effective_lead_ms(display_floor: u64, cap: u64, cap_floor: u64) -> u64 {
    if cap == 0 {
        display_floor
    } else {
        display_floor.min(cap.max(cap_floor))
    }
}

/// Where between two hardware vsyncs a frame's release stamp goes: this
/// fraction of a period BEFORE the vsync it should be shown at. SurfaceFlinger
/// decides "this vsync or the next" at a stamp right on the hardware vsync
/// (measured: Google TV Streamer, Mi TV Stick), so the middle of the gap is
/// the stamp a few ms of jitter cannot flip. ExoPlayer's 80 % of the
/// Choreographer grid landed ON that boundary on the Streamer (Choreographer
/// times there are 30 ms = the app vsync offset after the hardware vsync).
#[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
const VSYNC_RELEASE_OFFSET_PERCENT: i64 = 50;

/// Snap a frame's ideal display time `t` to the vsync grid (`period`,
/// `anchor` = any hardware vsync timestamp, CLOCK_MONOTONIC ns): the closest vsync,
/// but never the vsync of the previous frame (`last_vsync`) or one before it
/// — two frames on one vsync make SurfaceFlinger drop the older one. Returns
/// (vsync, release stamp).
///
/// Unsnapped stamps drift against the display (the media clock is the audio
/// device's crystal, ~10 ppm off the display's): sitting on a vsync boundary
/// for minutes, with the display's own few-ms present jitter, SurfaceFlinger
/// alternately dropped and repeated frames (Google TV Streamer, ~65 per
/// minute for ~8 minutes every hour). Snapped, the drift shows as a single
/// repeated frame when the closest vsync moves on.
#[cfg_attr(not(test), allow(dead_code))]
pub fn snap_to_vsync(t: i64, period: i64, anchor: i64, last_vsync: Option<i64>) -> (i64, i64) {
    snap_to_vsync_at(t, period, anchor, last_vsync, VSYNC_RELEASE_OFFSET_PERCENT)
}

/// [`snap_to_vsync`] with the stamp `offset_percent` of a period before the vsync.
#[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
pub fn snap_to_vsync_at(t: i64, period: i64, anchor: i64, last_vsync: Option<i64>, offset_percent: i64) -> (i64, i64) {
    let k = (t - anchor + period / 2).div_euclid(period);
    let mut vsync = anchor + k * period;
    if let Some(last) = last_vsync {
        // Only a collision with the previous frame moves the vsync; a seek or
        // a backward re-base (raw time far behind) snaps freely.
        if vsync <= last && last - vsync < 4 * period {
            vsync = last + period;
        }
    }
    (vsync, vsync - period * offset_percent / 100)
}

/// Snap only when a frame lasts a whole number of vsyncs (within 2 %): 24p on
/// a 24 Hz display, 25p on 50 Hz. With 3:2 pulldown (24p on 60 Hz, 2.5 vsyncs
/// per frame) every other frame's ideal time sits halfway between two vsyncs,
/// where "closest vsync" itself flips on any jitter — snapping made the Mi TV
/// Stick's cadence worse, so those stay unsnapped.
#[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
pub fn whole_vsync_cadence(frame_ns: i64, period_ns: i64) -> bool {
    if frame_ns <= 0 || period_ns <= 0 {
        return false;
    }
    let ratio = frame_ns as f64 / period_ns as f64;
    ratio >= 0.98 && (ratio - ratio.round()).abs() <= 0.02 * ratio.round()
}

#[derive(Default)]
struct StarveTimes {
    /// Last starvation that stepped the cap down.
    last_starved: Option<Instant>,
    /// Last change of the cap (down or up).
    last_change: Option<Instant>,
}

/// Shared lead state: written by the host (display timing) and the video sync
/// loop (starvation, per-second counters), read by the debug HUD.
#[derive(Default)]
pub struct PresentLead {
    vsync_ns: AtomicI64,
    deadline_ns: AtomicI64,
    app_vsync_offset_ns: AtomicI64,
    /// Starvation cap, ms; 0 = no cap.
    cap_ms: AtomicU64,
    times: Mutex<StarveTimes>,
    starved_events: AtomicU64,
    /// Lead applied to the last released frame, ms (0 = none yet).
    applied_ms: AtomicU64,
    /// Frames released to the Surface / reported rendered by the codec in
    /// the last full second. `rendered` is `u64::MAX` when the platform has
    /// no rendered callback (API < 33).
    released_per_s: AtomicU64,
    rendered_per_s: AtomicU64,
    /// A recent vsync timestamp (CLOCK_MONOTONIC ns) from the host's
    /// Choreographer; 0 = none, release stamps are then not snapped.
    vsync_anchor_ns: AtomicI64,
    /// Test only: run release stamps this many ppm fast against the media
    /// clock, so clock-vs-display drift crosses a vsync in seconds.
    test_skew_ppm: AtomicI64,
    /// Test only: stamp offset before the vsync, percent; 0 = the default.
    test_offset_percent: AtomicI64,
    /// Whether the last frame's stamp was snapped to the vsync grid.
    snapping: std::sync::atomic::AtomicBool,
}

/// HUD view of [`PresentLead`].
#[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PresentLeadSnapshot {
    pub lead_ms: u64,
    pub display_floor_ms: u64,
    /// 0 = no starvation cap.
    pub cap_ms: u64,
    pub vsync_ns: i64,
    pub deadline_ns: i64,
    pub app_vsync_offset_ns: i64,
    pub starved_events: u64,
    pub released_per_s: u64,
    /// `None` without a rendered callback.
    pub rendered_per_s: Option<u64>,
    /// Release stamps snapped to the vsync grid (last frame).
    pub vsync_snap: bool,
}

impl PresentLead {
    /// Display timing from the host (ns; 0 = unknown). Applies from the next
    /// frame; a refresh-rate switch (adaptive frame rate) reports again.
    pub fn set_display_timing(&self, vsync_ns: i64, deadline_ns: i64, app_vsync_offset_ns: i64) {
        self.vsync_ns.store(vsync_ns.max(0), Ordering::Relaxed);
        self.deadline_ns.store(deadline_ns.max(0), Ordering::Relaxed);
        self.app_vsync_offset_ns.store(app_vsync_offset_ns, Ordering::Relaxed);
        log::info!(
            "[lead] display timing: vsync {:.2} ms, presentation deadline {:.2} ms, app vsync offset {:.2} ms -> floor {} ms",
            vsync_ns as f64 / 1e6,
            deadline_ns as f64 / 1e6,
            app_vsync_offset_ns as f64 / 1e6,
            display_floor_ms(vsync_ns, deadline_ns)
        );
    }

    /// A vsync timestamp from the host (Choreographer frame time).
    pub fn on_vsync(&self, frame_time_ns: i64) {
        if self.vsync_anchor_ns.swap(frame_time_ns, Ordering::Relaxed) == 0 && frame_time_ns > 0 {
            log::info!("[lead] vsync phase reported: release stamps snap to the vsync grid");
        }
    }

    /// The hardware vsync grid to snap to: (period, anchor), when both are
    /// known. Choreographer frame times run the app vsync offset behind the
    /// hardware vsync.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn vsync_grid(&self) -> Option<(i64, i64)> {
        let period = self.vsync_ns.load(Ordering::Relaxed);
        let anchor = self.vsync_anchor_ns.load(Ordering::Relaxed);
        let app_offset = self.app_vsync_offset_ns.load(Ordering::Relaxed);
        (period > 0 && anchor > 0).then_some((period, anchor - app_offset))
    }

    #[doc(hidden)]
    pub fn set_test_skew_ppm(&self, ppm: i64) {
        self.test_skew_ppm.store(ppm, Ordering::Relaxed);
        if ppm != 0 {
            log::warn!("[lead] TEST: release stamps skewed {ppm} ppm against the media clock");
        }
    }

    /// Record whether the frame just stamped was snapped (HUD).
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn set_snapping(&self, on: bool) {
        self.snapping.store(on, Ordering::Relaxed);
    }

    #[doc(hidden)]
    pub fn set_test_offset_percent(&self, percent: i64) {
        self.test_offset_percent.store(percent, Ordering::Relaxed);
        log::warn!("[lead] TEST: release stamps {percent} % of a vsync before it");
    }

    /// Stamp offset before the vsync, percent of a period.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn offset_percent(&self) -> i64 {
        match self.test_offset_percent.load(Ordering::Relaxed) {
            0 => VSYNC_RELEASE_OFFSET_PERCENT,
            p => p,
        }
    }

    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn test_skew_ppm(&self) -> i64 {
        self.test_skew_ppm.load(Ordering::Relaxed)
    }

    fn floors(&self) -> (u64, u64) {
        let vsync = self.vsync_ns.load(Ordering::Relaxed);
        let deadline = self.deadline_ns.load(Ordering::Relaxed);
        (display_floor_ms(vsync, deadline), cap_floor_ms(deadline))
    }

    /// The lead for the frame being released now. Relaxes the starvation cap
    /// one step once it has been quiet for [`CAP_RECOVER_AFTER`].
    #[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
    pub fn lead_ms(&self, now: Instant) -> u64 {
        let (floor, cap_floor) = self.floors();
        let mut cap = self.cap_ms.load(Ordering::Relaxed);
        if cap != 0 {
            let mut t = self.times.lock().unwrap_or_else(|e| e.into_inner());
            let quiet = |at: Option<Instant>| at.is_none_or(|at| now.duration_since(at) >= CAP_RECOVER_AFTER);
            if quiet(t.last_starved) && quiet(t.last_change) {
                cap += CAP_STEP_MS;
                if cap >= floor {
                    cap = 0;
                }
                self.cap_ms.store(cap, Ordering::Relaxed);
                t.last_change = Some(now);
                log::info!(
                    "[lead] no decoder starvation for {}s -> cap {}",
                    CAP_RECOVER_AFTER.as_secs(),
                    if cap == 0 { "off".to_string() } else { format!("{cap} ms") }
                );
            }
        }
        let lead = effective_lead_ms(floor, cap, cap_floor);
        self.applied_ms.store(lead, Ordering::Relaxed);
        lead
    }

    /// The decoder ran out of output buffers (input refused, nothing decoded
    /// waiting). Steps the cap one below the current lead; returns the new
    /// lead when it changed.
    #[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
    pub fn on_decoder_starved(&self, now: Instant) -> Option<u64> {
        let (floor, cap_floor) = self.floors();
        let mut t = self.times.lock().unwrap_or_else(|e| e.into_inner());
        if t.last_starved.is_some_and(|at| now.duration_since(at) < STARVE_REARM) {
            return None;
        }
        t.last_starved = Some(now);
        self.starved_events.fetch_add(1, Ordering::Relaxed);
        let current = effective_lead_ms(floor, self.cap_ms.load(Ordering::Relaxed), cap_floor);
        let next = current.saturating_sub(CAP_STEP_MS).max(cap_floor);
        if next >= current {
            return None;
        }
        self.cap_ms.store(next, Ordering::Relaxed);
        t.last_change = Some(now);
        Some(next)
    }

    /// Per-second release / rendered counts from the sync loop's stats tick.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn set_rates(&self, released_per_s: u64, rendered_per_s: Option<u64>) {
        self.released_per_s.store(released_per_s, Ordering::Relaxed);
        self.rendered_per_s.store(rendered_per_s.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    #[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
    pub fn snapshot(&self) -> PresentLeadSnapshot {
        let (floor, _) = self.floors();
        let rendered = self.rendered_per_s.load(Ordering::Relaxed);
        PresentLeadSnapshot {
            lead_ms: self.applied_ms.load(Ordering::Relaxed),
            display_floor_ms: floor,
            cap_ms: self.cap_ms.load(Ordering::Relaxed),
            vsync_ns: self.vsync_ns.load(Ordering::Relaxed),
            deadline_ns: self.deadline_ns.load(Ordering::Relaxed),
            app_vsync_offset_ns: self.app_vsync_offset_ns.load(Ordering::Relaxed),
            starved_events: self.starved_events.load(Ordering::Relaxed),
            released_per_s: self.released_per_s.load(Ordering::Relaxed),
            rendered_per_s: (rendered != u64::MAX).then_some(rendered),
            vsync_snap: self.snapping.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VSYNC_24: i64 = 41_708_333;
    const VSYNC_60: i64 = 16_666_667;

    #[test]
    fn display_floor_follows_the_display() {
        // Google TV Streamer at 23.976 Hz: 34 ms deadline.
        assert_eq!(display_floor_ms(VSYNC_24, 34_000_000), 34 + 2 + 42);
        // A 60 Hz display with a short deadline stays at ExoPlayer's 50 ms.
        assert_eq!(display_floor_ms(VSYNC_60, 12_000_000), MIN_LEAD_MS);
        // Unknown display.
        assert_eq!(display_floor_ms(0, 0), MIN_LEAD_MS);
    }

    #[test]
    fn starvation_caps_the_lead_and_recovers() {
        let lead = PresentLead::default();
        lead.set_display_timing(VSYNC_24, 34_000_000, 0);
        let t0 = Instant::now();
        assert_eq!(lead.lead_ms(t0), 78);
        assert_eq!(lead.on_decoder_starved(t0), Some(68));
        // The same episode seen again within a second: no second step.
        assert_eq!(lead.on_decoder_starved(t0 + Duration::from_millis(500)), None);
        assert_eq!(lead.on_decoder_starved(t0 + Duration::from_secs(1)), Some(58));
        assert_eq!(lead.lead_ms(t0 + Duration::from_secs(2)), 58);
        // Quiet for 30 s: one step back up, then another 30 s later.
        assert_eq!(lead.lead_ms(t0 + Duration::from_secs(31)), 68);
        assert_eq!(lead.lead_ms(t0 + Duration::from_secs(40)), 68);
        assert_eq!(lead.lead_ms(t0 + Duration::from_secs(61)), 78);
        assert_eq!(lead.snapshot().cap_ms, 0, "back at the floor = cap off");
        assert_eq!(lead.snapshot().starved_events, 2);
    }

    #[test]
    fn stamps_snap_to_the_closest_vsync_mid_gap() {
        let p = VSYNC_24;
        let anchor = 1_000_000_000;
        // 3 ms after vsync 10 -> vsync 10, stamped half a period before it.
        let (v, stamp) = snap_to_vsync(anchor + 10 * p + 3_000_000, p, anchor, None);
        assert_eq!(v, anchor + 10 * p);
        assert_eq!(stamp, v - p / 2);
        // Just short of the midpoint before vsync 11 -> still 10; past it -> 11.
        assert_eq!(snap_to_vsync(anchor + 10 * p + p / 2 - 1, p, anchor, None).0, anchor + 10 * p);
        assert_eq!(snap_to_vsync(anchor + 10 * p + p / 2 + 1, p, anchor, None).0, anchor + 11 * p);
        // An anchor in the future works the same.
        assert_eq!(snap_to_vsync(anchor - 3 * p + 1_000, p, anchor, None).0, anchor - 3 * p);
    }

    #[test]
    fn every_frame_gets_its_own_vsync() {
        let p = VSYNC_24;
        let anchor = 0;
        let mut last = None;
        let mut shown = Vec::new();
        // 24p ideal times drifting +2 ms per frame against a 24p display:
        // they cross a vsync boundary; no two frames may share a vsync.
        for i in 0..40i64 {
            let t = i * p + i * 2_000_000;
            let (v, _) = snap_to_vsync(t, p, anchor, last);
            if let Some(l) = last {
                assert!(v > l, "frame {i} landed on vsync {v} <= {l}");
            }
            shown.push(v);
            last = Some(v);
        }
        let holds = shown.windows(2).filter(|w| w[1] - w[0] > p).count();
        assert!(holds >= 1, "the drift must surface as held vsyncs, not drops");
        // A seek back far behind the last vsync snaps freely.
        assert_eq!(snap_to_vsync(5 * p, p, anchor, Some(400 * p)).0, 5 * p);
    }

    #[test]
    fn the_grid_is_the_hardware_vsync() {
        let lead = PresentLead::default();
        assert_eq!(lead.vsync_grid(), None);
        lead.set_display_timing(VSYNC_24, 34_000_000, 30_416_752);
        lead.on_vsync(1_000_030_416_752);
        assert_eq!(lead.vsync_grid(), Some((VSYNC_24, 1_000_000_000_000)));
    }

    #[test]
    fn only_whole_vsync_cadences_snap() {
        let frame_24 = 41_708_333;
        assert!(whole_vsync_cadence(frame_24, VSYNC_24));
        assert!(whole_vsync_cadence(41_666_667, VSYNC_24), "24.000 fps content on 23.976 Hz");
        assert!(whole_vsync_cadence(40_000_000, 20_000_000), "25p on 50 Hz");
        assert!(!whole_vsync_cadence(frame_24, VSYNC_60), "3:2 pulldown");
        assert!(!whole_vsync_cadence(33_366_667, VSYNC_24), "30p on 24 Hz");
        assert!(!whole_vsync_cadence(0, VSYNC_24));
    }

    #[test]
    fn a_60_hz_grid_keeps_3_2_pulldown() {
        let p = VSYNC_60;
        let frame = 41_708_333i64;
        let mut last = None;
        let mut gaps = Vec::new();
        for i in 0..48i64 {
            let (v, _) = snap_to_vsync(i * frame, p, 0, last);
            if let Some(l) = last {
                gaps.push(((v - l) as f64 / p as f64).round() as i64);
            }
            last = Some(v);
        }
        assert!(gaps.iter().all(|g| *g == 2 || *g == 3), "{gaps:?}");
    }

    #[test]
    fn the_cap_never_undercuts_the_deadline() {
        let lead = PresentLead::default();
        lead.set_display_timing(VSYNC_24, 34_000_000, 0);
        let mut t = Instant::now();
        for _ in 0..10 {
            lead.on_decoder_starved(t);
            t += Duration::from_secs(1);
        }
        assert_eq!(lead.lead_ms(t), 36);
        // An unknown display caps down to the absolute floor.
        let unknown = PresentLead::default();
        let mut t = Instant::now();
        for _ in 0..10 {
            unknown.on_decoder_starved(t);
            t += Duration::from_secs(1);
        }
        assert_eq!(unknown.lead_ms(t), CAP_FLOOR_MIN_MS);
    }
}
