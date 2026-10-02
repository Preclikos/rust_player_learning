//! The host's `AVSampleBufferDisplayLayer` and its timebase, shared by
//! every decoder one `Player` builds (ABR swaps, seeks, retries).
//!
//! Decision logic that doesn't need the layer — eligibility, stream
//! continuity, when to re-anchor the timebase, when the stall guard stops
//! it — is in plain functions at the top so it is unit-tested on its own.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;

use super::ffi::{self, LayerStatus, SampleBuffer, Timebase};
use crate::decoders::DecoderError;
use crate::player::clock_monotonic_ns;

/// `Display.HdrCapabilities`-order bits of the host's display mask.
const DISPLAY_DOLBY_VISION: u32 = 1 << 0;
const DISPLAY_HDR10: u32 = 1 << 1;

/// Frame interval assumed before two stamps have arrived (24 fps).
const DEFAULT_FRAME_INTERVAL_NS: i64 = 41_666_667;
/// Intervals above this are a gap (seek, stall), not a cadence.
const MAX_CADENCE_INTERVAL_US: i64 = 200_000;
/// Timebase drift tolerated before re-anchoring (half a 120 Hz frame).
const REANCHOR_TOLERANCE_NS: i64 = 4_000_000;
/// The stall guard waits at least this long after the last stamp.
const MIN_STALL_SLACK_NS: i64 = 50_000_000;
/// A decoder whose first PTS is further than this from the last enqueued
/// one did not continue the stream (replay, discontinuity) — flush. Seeks
/// announce themselves (`request_flush`), so the backward window only has
/// to tell an ABR splice — the new representation decodes from its segment
/// start, up to one segment behind what is already enqueued — from a
/// restart; DASH segments stay well under this.
const CONTINUITY_BACK_US: i64 = 12_000_000;
const CONTINUITY_FORWARD_US: i64 = 1_500_000;

// ---------------------------------------------------------------------
// Pure decision logic
// ---------------------------------------------------------------------

/// `RUST_PLAYER_DIRECT` override: `1` uses direct mode whenever a layer is
/// installed, even on an SDR display (the OS then tonemaps HDR / DV itself —
/// the way to exercise this path without HDR hardware); `0` never uses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DirectOverride {
    Auto,
    Force,
    Off,
}

impl DirectOverride {
    fn from_env() -> Self {
        Self::parse(std::env::var("RUST_PLAYER_DIRECT").ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Self {
        match value.map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if matches!(s.as_str(), "1" | "on" | "force" | "true") => Self::Force,
            Some(s) if matches!(s.as_str(), "0" | "off" | "false") => Self::Off,
            _ => Self::Auto,
        }
    }

    /// Should the next decoder use direct mode?
    fn wants_direct(self, display_hdr_types: u32, has_layer: bool, failed: bool) -> bool {
        if failed || !has_layer {
            return false;
        }
        match self {
            Self::Force => true,
            Self::Off => false,
            Self::Auto => display_hdr_types & (DISPLAY_DOLBY_VISION | DISPLAY_HDR10) != 0,
        }
    }

    /// Hand a DV stream to the OS as Dolby Vision (`dvh1`) rather than as
    /// its HEVC base layer: when the display takes DV, when there is no
    /// base layer (profile 5), or when forced for testing.
    fn wants_dolby_vision(self, display_hdr_types: u32, dovi_profile: u8) -> bool {
        dovi_profile == 5 || self == Self::Force || display_hdr_types & DISPLAY_DOLBY_VISION != 0
    }
}

/// A new decoder's first sample continues what the layer already holds
/// (ABR swap, retry) unless its PTS jumps — then the layer must be flushed.
fn is_discontinuous(last_enqueued_pts_us: Option<i64>, first_pts_us: i64) -> bool {
    match last_enqueued_pts_us {
        None => false,
        Some(last) => first_pts_us < last - CONTINUITY_BACK_US || first_pts_us > last + CONTINUITY_FORWARD_US,
    }
}

/// Re-anchor only when the timebase is stopped or has drifted from where
/// the sync loop says media time is now; steady playback runs on the host
/// clock without per-frame nudges.
fn needs_reanchor(rate: f64, timebase_now_ns: i64, expected_now_ns: i64) -> bool {
    rate == 0.0 || (timebase_now_ns - expected_now_ns).abs() > REANCHOR_TOLERANCE_NS
}

/// Frame interval to assume after a stamp at `pts_us`, given the previous
/// stamp's PTS and interval: the media delta when it looks like a cadence,
/// otherwise the previous interval (a seek gap must not become the cadence).
fn frame_interval_ns(previous: Option<(i64, i64)>, pts_us: i64) -> i64 {
    match previous {
        Some((prev_pts, _)) if pts_us > prev_pts && pts_us - prev_pts < MAX_CADENCE_INTERVAL_US => {
            (pts_us - prev_pts) * 1000
        }
        Some((_, interval)) => interval,
        None => DEFAULT_FRAME_INTERVAL_NS,
    }
}

/// When the stall guard stops the timebase if no further stamp arrives:
/// two frame intervals after the last frame's present time, at least
/// `MIN_STALL_SLACK_NS` so pacing jitter never trips it.
fn stall_deadline_ns(present_ns: i64, interval_ns: i64) -> i64 {
    present_ns + (2 * interval_ns).max(MIN_STALL_SLACK_NS)
}

// ---------------------------------------------------------------------
// Shared output
// ---------------------------------------------------------------------

/// The last frame the sync loop stamped.
#[derive(Clone, Copy)]
struct Anchor {
    pts_us: i64,
    /// CLOCK_MONOTONIC ns the frame is shown at.
    present_ns: i64,
    /// Frame interval in effect, for the stall deadline.
    interval_ns: i64,
}

/// Retained `AVSampleBufferDisplayLayer`. Fed from the decode thread and
/// stamped from the sync loop; the members touched (enqueue, flush,
/// status, controlTimebase) are documented thread-safe.
struct Layer(Retained<AnyObject>);
unsafe impl Send for Layer {}

struct Inner {
    layer: Option<Layer>,
    timebase: Option<Timebase>,
    /// Largest PTS enqueued since the last flush (stream continuity check).
    last_enqueued_pts_us: Option<i64>,
    /// A seek happened: the next decoder's first sample flushes the layer.
    flush_pending: bool,
    anchor: Option<Anchor>,
    /// Bumped per stamp so the stall guard can tell a stale deadline.
    anchor_generation: u64,
    shutdown: bool,
}

impl Inner {
    fn flush(&mut self) {
        if let Some(layer) = &self.layer {
            ffi::layer_flush(&layer.0);
        }
        if let Some(tb) = &self.timebase {
            tb.stop();
        }
        self.last_enqueued_pts_us = None;
        self.anchor = None;
    }
}

/// One per `Player`: the host's video layer and its timebase.
pub struct AppleDirectOutput {
    inner: Mutex<Inner>,
    /// Wakes the stall guard on every stamp.
    stamped: Condvar,
    display_hdr_types: AtomicU32,
    /// The layer reported `failed`: direct mode is retired for this player.
    failed: AtomicBool,
    /// Decoders currently in direct mode (debug HUD).
    direct_decoders: AtomicUsize,
    override_: DirectOverride,
    /// `RUST_PLAYER_DIRECT_FAIL_AFTER=N` (testing): pretend the layer
    /// failed after N enqueued samples, to exercise the failover.
    fail_after: Option<u64>,
    enqueued: AtomicU64,
}

impl AppleDirectOutput {
    pub fn new() -> Arc<Self> {
        let override_ = DirectOverride::from_env();
        if override_ != DirectOverride::Auto {
            log::info!("[direct] RUST_PLAYER_DIRECT override: {:?}", override_);
        }
        let timebase = Timebase::new();
        let out = Arc::new(Self {
            failed: AtomicBool::new(timebase.is_none()),
            inner: Mutex::new(Inner {
                layer: None,
                timebase,
                last_enqueued_pts_us: None,
                flush_pending: false,
                anchor: None,
                anchor_generation: 0,
                shutdown: false,
            }),
            stamped: Condvar::new(),
            display_hdr_types: AtomicU32::new(0),
            direct_decoders: AtomicUsize::new(0),
            override_,
            fail_after: std::env::var("RUST_PLAYER_DIRECT_FAIL_AFTER").ok().and_then(|v| v.trim().parse().ok()),
            enqueued: AtomicU64::new(0),
        });
        let weak = Arc::downgrade(&out);
        let _ = std::thread::Builder::new()
            .name("direct-stall-guard".into())
            .spawn(move || stall_guard(weak));
        out
    }

    /// Install (or with null remove) the host's `AVSampleBufferDisplayLayer*`.
    /// Applies from the next pipeline build. Installing a layer also re-arms
    /// direct mode after a failure: the OS fails the layer when the app
    /// goes to the background (`-11847 Operation Interrupted`), so the host
    /// re-installs it on return to the foreground and the next pipeline is
    /// direct again (see `Player::set_video_output_layer`).
    pub fn set_layer(&self, layer: *mut std::ffi::c_void) {
        let mut inner = self.inner.lock().unwrap();
        if inner.layer.is_some() {
            inner.flush();
            inner.layer = None;
        }
        if layer.is_null() {
            log::info!("[direct] video layer removed");
            return;
        }
        let Some(obj) = (unsafe { Retained::retain(layer as *mut AnyObject) }) else { return };
        if !ffi::responds_to(&obj, sel!(enqueueSampleBuffer:)) || !ffi::responds_to(&obj, sel!(setControlTimebase:)) {
            log::warn!("[direct] the object handed over is not an AVSampleBufferDisplayLayer — ignored");
            return;
        }
        if let Some(tb) = &inner.timebase {
            tb.stop();
            ffi::layer_set_control_timebase(&obj, tb);
        }
        inner.layer = Some(Layer(obj));
        if inner.timebase.is_some() {
            self.failed.store(false, Ordering::Relaxed);
        }
        log::info!("[direct] video layer installed");
    }

    /// A decoder is currently feeding the layer.
    pub fn is_feeding(&self) -> bool {
        self.direct_decoders.load(Ordering::Relaxed) > 0
    }

    pub fn set_display_hdr_types(&self, mask: u32) {
        self.display_hdr_types.store(mask, Ordering::Relaxed);
    }

    /// Seek: drop everything enqueued before the next decoder starts.
    pub fn request_flush(&self) {
        self.inner.lock().unwrap().flush_pending = true;
    }

    /// Should the next decoder use direct mode?
    pub fn eligible(&self) -> bool {
        let has_layer = self.inner.lock().unwrap().layer.is_some();
        self.override_.wants_direct(
            self.display_hdr_types.load(Ordering::Relaxed),
            has_layer,
            self.failed.load(Ordering::Relaxed),
        )
    }

    /// Describe a DV stream of `profile` to the OS as Dolby Vision?
    pub(super) fn wants_dolby_vision(&self, profile: u8) -> bool {
        self.override_.wants_dolby_vision(self.display_hdr_types.load(Ordering::Relaxed), profile)
    }

    /// A decoder's first sample: flush on a seek / discontinuity, else
    /// continue. Returns the PTS up to which this decoder's samples overlap
    /// what is already enqueued (ABR splice) — those decode but don't show.
    pub(super) fn begin_stream(&self, first_pts_us: i64) -> Option<i64> {
        let mut inner = self.inner.lock().unwrap();
        if inner.flush_pending || is_discontinuous(inner.last_enqueued_pts_us, first_pts_us) {
            log::info!(
                "[direct] flush (seek/discontinuity: first pts {} ms, last enqueued {:?} ms)",
                first_pts_us / 1000,
                inner.last_enqueued_pts_us.map(|p| p / 1000)
            );
            inner.flush();
            inner.flush_pending = false;
            return None;
        }
        inner.last_enqueued_pts_us
    }

    pub(super) fn enqueue(&self, sample: &SampleBuffer, pts_us: i64) -> Result<(), DecoderError> {
        let mut inner = self.inner.lock().unwrap();
        let Some(layer) = inner.layer.as_ref() else {
            return Err("direct mode: video layer removed".into());
        };
        let layer = &*layer.0;
        if self.fail_after.is_some_and(|n| self.enqueued.fetch_add(1, Ordering::Relaxed) >= n) {
            self.failed.store(true, Ordering::Relaxed);
            log::error!("[direct] simulated layer failure (RUST_PLAYER_DIRECT_FAIL_AFTER) — falling back");
            return Err("direct mode failed (simulated)".into());
        }
        if ffi::layer_status(layer) == LayerStatus::Failed {
            let (code, description) = ffi::layer_error(layer);
            let interrupted = ffi::layer_wants_flush(layer);
            inner.flush();
            if interrupted {
                log::warn!("[direct] layer interrupted ({code}: {description}) — flushed, rebuilding");
                return Err(format!("direct mode interrupted ({code})").into());
            }
            self.failed.store(true, Ordering::Relaxed);
            log::warn!(
                "[direct] layer failed ({code}: {description}) — the player's own renderer takes over \
                 until the host re-installs the layer (foreground)"
            );
            return Err(format!("direct mode failed ({code}: {description})").into());
        }
        ffi::layer_enqueue(layer, sample);
        inner.last_enqueued_pts_us = Some(inner.last_enqueued_pts_us.map_or(pts_us, |m| m.max(pts_us)));
        Ok(())
    }

    /// The sync loop "shows" the frame at `pts_us` at CLOCK_MONOTONIC
    /// `present_ns` (0 = now): make the layer's timebase agree.
    pub fn anchor(&self, pts_us: i64, present_ns: i64) {
        let mut inner = self.inner.lock().unwrap();
        let (Some(tb), true) = (inner.timebase.as_ref(), inner.layer.is_some()) else { return };
        let now = clock_monotonic_ns();
        let present_ns = if present_ns > 0 { present_ns } else { now };
        let lead_ns = present_ns - now;
        let expected_now_ns = pts_us * 1000 - lead_ns;
        if needs_reanchor(tb.rate(), tb.time_ns(), expected_now_ns) {
            tb.run_from(pts_us, lead_ns);
        }
        let interval_ns = frame_interval_ns(inner.anchor.map(|a| (a.pts_us, a.interval_ns)), pts_us);
        inner.anchor = Some(Anchor { pts_us, present_ns, interval_ns });
        inner.anchor_generation += 1;
        if inner.anchor_generation % 240 == 1 {
            self.log_health(&inner, lead_ns);
        }
        drop(inner);
        self.stamped.notify_one();
    }

    /// Field diagnostics every ~5-10 s: is the layer rendering, and does
    /// its clock sit where the sync loop says it should?
    fn log_health(&self, inner: &Inner, lead_ns: i64) {
        let (Some(layer), Some(tb), Some(anchor)) = (&inner.layer, &inner.timebase, inner.anchor) else { return };
        log::info!(
            "[direct] layer status={:?} timebase={} ms, frame pts={} ms (lead {} ms)",
            ffi::layer_status(&layer.0),
            tb.time_ns() / 1_000_000,
            anchor.pts_us / 1000,
            lead_ns / 1_000_000
        );
    }

    pub(super) fn direct_decoder_started(&self) {
        self.direct_decoders.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn direct_decoder_stopped(&self) {
        self.direct_decoders.fetch_sub(1, Ordering::Relaxed);
    }

    /// Debug HUD line while a decoder feeds the layer; `None` otherwise.
    pub fn debug_output(&self) -> Option<String> {
        self.is_feeding().then(|| "AVSampleBufferDisplayLayer (direct)".to_string())
    }
}

impl Drop for AppleDirectOutput {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutdown = true;
        inner.flush();
        inner.layer = None;
    }
}

/// Stop the timebase when the sync loop stops stamping frames (pause,
/// buffering, teardown) — otherwise the layer would keep showing the
/// already-enqueued frames ahead of the clock.
fn stall_guard(out: Weak<AppleDirectOutput>) {
    loop {
        let Some(out) = out.upgrade() else { return };
        let mut inner = out.inner.lock().unwrap();
        if inner.shutdown {
            return;
        }
        let Some(anchor) = inner.anchor else {
            let _ = out.stamped.wait_timeout(inner, Duration::from_millis(200));
            continue;
        };
        let deadline = stall_deadline_ns(anchor.present_ns, anchor.interval_ns);
        let now = clock_monotonic_ns();
        if now >= deadline {
            if let Some(tb) = &inner.timebase {
                tb.stop();
            }
            inner.anchor = None;
            continue;
        }
        let generation = inner.anchor_generation;
        let _ = out.stamped.wait_timeout_while(inner, Duration::from_nanos((deadline - now) as u64), |i| {
            i.anchor_generation == generation && !i.shutdown
        });
    }
}

/// What the sync loop "renders" in direct mode: re-anchors the layer's
/// timebase (see [`AppleDirectOutput::anchor`]). Holds no picture.
pub struct AppleDirectFrame {
    pub output: Arc<AppleDirectOutput>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_parses() {
        assert_eq!(DirectOverride::parse(None), DirectOverride::Auto);
        assert_eq!(DirectOverride::parse(Some("")), DirectOverride::Auto);
        assert_eq!(DirectOverride::parse(Some("1")), DirectOverride::Force);
        assert_eq!(DirectOverride::parse(Some(" Force ")), DirectOverride::Force);
        assert_eq!(DirectOverride::parse(Some("off")), DirectOverride::Off);
    }

    #[test]
    fn direct_needs_a_layer_and_an_hdr_display_unless_forced() {
        let auto = DirectOverride::Auto;
        assert!(auto.wants_direct(DISPLAY_HDR10, true, false));
        assert!(auto.wants_direct(DISPLAY_DOLBY_VISION, true, false));
        assert!(!auto.wants_direct(0, true, false), "SDR display");
        assert!(!auto.wants_direct(DISPLAY_HDR10, false, false), "no layer");
        assert!(!auto.wants_direct(DISPLAY_HDR10, true, true), "retired after a failure");
        assert!(DirectOverride::Force.wants_direct(0, true, false));
        assert!(!DirectOverride::Force.wants_direct(0, false, false), "forcing can't invent a layer");
        assert!(!DirectOverride::Off.wants_direct(DISPLAY_HDR10, true, false));
    }

    #[test]
    fn dolby_vision_description_follows_display_and_profile() {
        let auto = DirectOverride::Auto;
        assert!(auto.wants_dolby_vision(DISPLAY_DOLBY_VISION | DISPLAY_HDR10, 8));
        assert!(!auto.wants_dolby_vision(DISPLAY_HDR10, 8), "HDR10-only display: play the base layer");
        assert!(auto.wants_dolby_vision(DISPLAY_HDR10, 5), "profile 5 has no base layer");
        assert!(DirectOverride::Force.wants_dolby_vision(0, 8));
    }

    #[test]
    fn continuity_flushes_only_on_jumps() {
        assert!(!is_discontinuous(None, 0));
        assert!(!is_discontinuous(Some(10_000_000), 10_040_000), "next frame");
        assert!(!is_discontinuous(Some(10_000_000), 9_200_000), "splice overlap");
        assert!(!is_discontinuous(Some(9_592_000), 6_089_000), "splice from the segment start (iPhone SE run)");
        assert!(is_discontinuous(Some(10_000_000), 60_000_000), "jump forward");
        assert!(is_discontinuous(Some(60_101_000), 83_000), "replay from the start");
    }

    #[test]
    fn reanchor_when_stopped_or_drifted() {
        assert!(needs_reanchor(0.0, 0, 0));
        assert!(!needs_reanchor(1.0, 5_000_000_000, 5_002_000_000));
        assert!(needs_reanchor(1.0, 5_000_000_000, 5_010_000_000));
    }

    #[test]
    fn frame_interval_tracks_cadence_not_gaps() {
        assert_eq!(frame_interval_ns(None, 0), DEFAULT_FRAME_INTERVAL_NS);
        assert_eq!(frame_interval_ns(Some((1_000_000, 0)), 1_041_666), 41_666_000);
        assert_eq!(frame_interval_ns(Some((1_000_000, 41_666_000)), 9_000_000), 41_666_000, "seek gap");
        assert_eq!(frame_interval_ns(Some((1_000_000, 41_666_000)), 500_000), 41_666_000, "backwards");
    }

    #[test]
    fn stall_deadline_has_a_floor() {
        assert_eq!(stall_deadline_ns(1_000, 41_666_667), 1_000 + 83_333_334);
        assert_eq!(stall_deadline_ns(1_000, 8_333_333), 1_000 + MIN_STALL_SLACK_NS);
    }

    #[test]
    fn output_without_a_layer_is_never_eligible() {
        let out = AppleDirectOutput::new();
        if out.override_ != DirectOverride::Auto {
            return; // env override set by the caller
        }
        out.set_display_hdr_types(DISPLAY_HDR10);
        assert!(!out.eligible());
        assert!(out.debug_output().is_none());
    }

    #[test]
    fn installing_a_layer_re_arms_direct_mode_after_a_failure() {
        let out = AppleDirectOutput::new();
        if out.override_ != DirectOverride::Auto {
            return;
        }
        out.set_display_hdr_types(DISPLAY_HDR10);
        let layer: Retained<AnyObject> =
            unsafe { objc2::msg_send![objc2::class!(AVSampleBufferDisplayLayer), new] };
        let ptr = Retained::as_ptr(&layer).cast_mut().cast::<std::ffi::c_void>();
        out.set_layer(ptr);
        assert!(out.eligible());
        out.failed.store(true, Ordering::Relaxed);
        assert!(!out.eligible(), "retired after the OS failed the layer");
        out.set_layer(ptr);
        assert!(out.eligible(), "host re-installed the layer on foreground");
        out.set_layer(std::ptr::null_mut());
        assert!(!out.eligible());
    }

    #[test]
    fn rejects_objects_that_are_not_a_display_layer() {
        let out = AppleDirectOutput::new();
        let obj: Retained<AnyObject> = unsafe { objc2::msg_send![objc2::class!(NSObject), new] };
        out.set_layer(Retained::as_ptr(&obj).cast_mut().cast::<std::ffi::c_void>());
        assert!(!out.inner.lock().unwrap().layer.is_some());
    }
}
