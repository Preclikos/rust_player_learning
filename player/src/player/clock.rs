//! Playback master clock.
//!
//! Split out of `player.rs` unchanged: the clock is self-contained (it reads
//! the audio sink and nothing else) and is where the subtlest timing bugs
//! live, so it earns its own file and its own tests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
// tokio's Instant, matching `player.rs` — `start_time` is shared with the sync
// loop and the two must be the same type.
use crate::rt::Instant;

use crate::renderers::AudioSink;

use super::StatsState;

// Returns current CLOCK_MONOTONIC time in nanoseconds.
// Used to compute absolute presentation timestamps for eglPresentationTimeANDROID.
#[cfg(target_os = "android")]
pub(crate) fn clock_monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}
#[cfg(not(target_os = "android"))]
pub(crate) fn clock_monotonic_ns() -> i64 { 0 }

/// Playback master clock — 0-based media time, audio-disciplined.
///
/// Mastered by the audio sink's real playback position so video (and any other
/// renderer) cannot drift from audio (crystal mismatch / underruns). Between
/// the sink's coarse position updates it interpolates with the wall clock for
/// smooth pacing; when the sink reports no position (mocks / output not open)
/// it falls back to the wall clock.
///
/// The audio position used is `played_since_flush_ms` — how much of THIS
/// pipeline's audio (queued after the seek/switch flush, trimmed to start
/// exactly at `seek_offset`) the device has presented. So
/// `media = seek_offset + played_since_flush − output_latency`, and the clock
/// reads `seek_offset` (frozen) until the new audio is really audible — not
/// while the previous pipeline's tail is still draining from the device
/// buffer. Anchoring to a snapshot of the cumulative counter instead (the
/// old `audio_base`) ran the clock ahead by that tail on every rebuild:
/// 100–300 ms of "video leads audio" after each seek / track switch on
/// Android's AudioTrack.
///
/// The audio sink is the only clock source today and the seam for tomorrow: an
/// AudioTrack passthrough sink reports its head via getTimestamp, so the
/// clock serves bitstream (Dolby/DTS passthrough) and multichannel without
/// video / subtitles knowing the difference. Shareable (interior-mutable
/// anchor) so future consumers can pace to the same clock.
///
/// # Surviving a dead audio output
///
/// The device head can stop for good while the sink still reports a position:
/// HDMI/ARC dropping on TV standby kills the `IAudioTrack` server-side (the
/// framework then loops `restoreTrack_l` on every `getTimestamp`), and a
/// pipeline with no audio at all can prime its track on silence and report a
/// standing `Some(0)`. Both leave the master clock STANDING while playback is
/// supposed to run, and a standing clock is worse than no clock: the sync loop
/// sleeps until each frame's due time, so frame *k* waits one frame period
/// longer than frame *k−1* and the picture degrades into a slower and slower
/// crawl (observed: 0.2 fps, media advancing at 0.8 % of real time, for as long
/// as the stream was left running).
///
/// So a position that stands still while we are neither paused nor starving is
/// treated as a dead output: the wall clock takes over AT THE VALUE the audio
/// clock last read, which makes the handover continuous (no lurch), and the
/// picture keeps real time with no sound instead of crawling. When the output
/// comes back the audio master is re-adopted — seamlessly if it agrees with
/// where the wall clock got to, otherwise with one hold or skip of the gap.
/// Staying on the wall clock instead left the picture permanently offset
/// from the sound by that gap (a passthrough receiver relocking for 2 s
/// after an HDMI mode switch came back 2 s "behind"); one visible correction
/// beats a desync that lasts the rest of the title.
pub(crate) struct MediaClock<A: AudioSink> {
    audio_sink: Arc<A>,
    // Wall anchor (= now − seek_offset): the fallback when the sink has no clock.
    start_time: Arc<Instant>,
    // Media position the post-flush audio starts at (the seek target).
    seek_offset_us: i64,
    pub(crate) state: std::sync::Mutex<ClockState>,
    // Paused by the consumer: the head stops on purpose, so a standing
    // position is not evidence of a dead output.
    paused: Arc<AtomicBool>,
    // Audio starvation (no decoded frame for 300 ms) pauses the sink itself
    // — again a standing position on purpose. Read from the shared stats.
    stats: Arc<StatsState>,
}

/// How long the sink's position may stand still, while playback is neither
/// paused nor starving, before the clock stops believing it.
///
/// This is the LAST RESORT, not the product behaviour. `audio_output_watchdog`
/// notices a dead output at [`AUDIO_OUTPUT_DEAD_MS`] and parks the picture
/// behind a `Buffering` event while it rebuilds the pipeline to get sound
/// back, and parking sets `audio_starving`, which suppresses this guard. So
/// the guard only ever fires when the watchdog is not running or did not act
/// — and all it then does is stop the picture from degrading into the crawl.
/// It is deliberately LATER than the watchdog so the watchdog always wins.
const AUDIO_CLOCK_STALE_MS: u64 = 3_000;

/// The same, but before the audio clock has EVER advanced — i.e. during
/// start-up, where a standing `Some(0)` is the documented way the sink says
/// "not audible yet" and video is meant to hold. A direct/passthrough
/// AudioTrack needs ~2.5 s of buffered audio before its head moves at all, so
/// this grace has to clear that comfortably or every passthrough start would
/// run video ahead of sound.
const AUDIO_CLOCK_START_GRACE_MS: u64 = 5_000;

/// How far a revived audio clock may be from the wall-extrapolated position
/// and still be re-adopted SILENTLY; further away it is re-adopted with a
/// warning (the picture holds or skips by the gap once).
const AUDIO_CLOCK_REJOIN_TOL_MS: i64 = 200;

#[derive(Default)]
pub(crate) struct ClockState {
    /// (last observed `played_since_flush_ms`, wall instant it was FIRST seen
    /// at that value, `pause_skew` then) — the interpolation anchor and the
    /// staleness timer.
    ///
    /// The skew is carried because staleness must be measured in PLAYED time,
    /// not wall time. Nothing polls this clock while playback is paused — the
    /// sync loop is parked — so the first call after a resume sees an anchor
    /// as old as the pause, with the paused flag already cleared. Measured in
    /// wall time that reads as an output that died, and the clock hands over
    /// to the wall having skipped the whole pause: every frame is then late by
    /// exactly the paused span and the picture races to catch up. Reported
    /// from the desktop build, reproduced with a 15 s pause.
    seen: Option<(u64, Instant, Duration)>,
    /// True once the sink's position has moved at least once, i.e. audio is
    /// genuinely audible. Before that a standing position means "still
    /// starting up", not "dead".
    ever_advanced: bool,
    /// Set once the position was declared dead: the media ms the wall-clock
    /// extrapolation continues from, the instant it took over, and the
    /// `pause_skew` at that instant.
    ///
    /// The skew baseline is what keeps this honest across a pause. The audio
    /// clock freezes on its own when the device stops, so the normal path
    /// needs no pause handling — but a WALL extrapolation runs with real time
    /// and would happily count a 30 s pause as 30 s of media, leaving every
    /// frame that far behind on resume and sending the picture racing to catch
    /// up. Only the pause accrued SINCE the handover may be subtracted, hence
    /// a baseline rather than the cumulative figure.
    wall_from: Option<(u64, Instant, Duration)>,
    /// Set while a pause / starvation hold is in force (the position then):
    /// until the position has advanced [`AUDIO_CLOCK_RESUME_ADVANCE_MS`] past
    /// it the start-up grace applies to the staleness guard, because a
    /// passthrough output takes 1.5–2.5 s after `play()` to present again —
    /// and its first reads after the release can creep by an interpolated
    /// few hundred ms before the real position moves.
    resumed_from: Option<u64>,
}

/// How far the position must advance after a hold before the normal
/// staleness guard applies again.
const AUDIO_CLOCK_RESUME_ADVANCE_MS: u64 = 250;

/// Real time between `from` and `now` MINUS the part of it spent paused.
///
/// `skew_now`/`skew_at_start` are the cumulative `pause_skew` readings now and
/// when the window opened; their difference is the pause inside the window.
pub(crate) fn wall_played(
    now: Instant,
    from: Instant,
    skew_now: Duration,
    skew_at_start: Duration,
) -> Duration {
    now.duration_since(from)
        .saturating_sub(skew_now.saturating_sub(skew_at_start))
}

impl<A: AudioSink> MediaClock<A> {
    pub(crate) fn new(
        audio_sink: Arc<A>,
        start_time: Arc<Instant>,
        seek_offset: Duration,
        paused: Arc<AtomicBool>,
        stats: Arc<StatsState>,
    ) -> Self {
        Self {
            audio_sink,
            start_time,
            seek_offset_us: seek_offset.as_micros() as i64,
            state: std::sync::Mutex::new(ClockState::default()),
            paused,
            stats,
        }
    }

    /// Audio-disciplined position (µs), or None when the sink reports no clock.
    /// `played_since_flush_ms` advances at the device rate and freezes on
    /// pause/starvation, so it already subsumes pause skew; `output_latency_ms`
    /// folds in so the picture lands when its audio is audible, not merely
    /// consumed.
    pub(crate) fn audio_now_us(&self, pause_skew: Duration) -> Option<i64> {
        let played = self.audio_sink.played_since_flush_ms()?;
        let lat_us = self.audio_sink.output_latency_ms() as i64 * 1_000;
        let now = Instant::now();
        let to_media_us =
            |ms: i64| -> i64 { (ms * 1_000 + self.seek_offset_us - lat_us).max(0) };
        let mut st = self.state.lock().unwrap();
        let held = self.paused.load(Ordering::Relaxed)
            || self.stats.audio_starving.load(Ordering::Relaxed)
            || self.stats.video_starving.load(Ordering::Relaxed);
        if held {
            st.resumed_from = Some(played);
        }

        if st.seen.map(|(p0, _, _)| played > p0).unwrap_or(true) {
            if st.seen.is_some() {
                st.ever_advanced = true;
                if st.resumed_from.is_some_and(|from| played >= from + AUDIO_CLOCK_RESUME_ADVANCE_MS) {
                    st.resumed_from = None;
                }
            }
            if let Some((wp, wt, skew0)) = st.wall_from {
                // The output is alive again: it is the clock the listener
                // hears, so it takes the master back. Seamlessly when it
                // agrees with where the wall clock carried us; otherwise the
                // picture holds (audio behind) or skips (audio ahead) by the
                // gap once — the alternative, keeping the wall clock, is a
                // permanent A/V offset of exactly that gap.
                let wall_ms = wp as i64
                    + wall_played(now, wt, pause_skew, skew0).as_millis() as i64;
                let gap = played as i64 - wall_ms;
                if gap.abs() <= AUDIO_CLOCK_REJOIN_TOL_MS {
                    log::info!(
                        "[clock] audio position advancing again at {}ms — re-adopting the audio master",
                        played
                    );
                } else {
                    log::warn!(
                        "[clock] audio position advancing again at {}ms, {}ms {} the wall clock — \
                         re-anchoring to the audio master (picture {} once)",
                        played,
                        gap.abs(),
                        if gap < 0 { "behind" } else { "ahead of" },
                        if gap < 0 { "holds" } else { "skips" }
                    );
                }
                st.wall_from = None;
            }
            st.seen = Some((played, now, pause_skew));
        }

        // Already handed over: keep extrapolating at real time, minus whatever
        // of it was spent paused.
        if let Some((wp, wt, skew0)) = st.wall_from {
            let ms = wp as i64 + wall_played(now, wt, pause_skew, skew0).as_millis() as i64;
            return Some(to_media_us(ms));
        }

        let (p0, w0, skew0) = st.seen.expect("set above whenever it was None");
        // PLAYED time since the anchor: a pause must not age it (see `seen`).
        let since = wall_played(now, w0, pause_skew, skew0);

        // Standing position while we are supposed to be playing => dead output
        // (see the type docs). Hand over to the wall clock at exactly the value
        // the audio clock last read, so the handover is continuous.
        let stale_after = if st.ever_advanced && st.resumed_from.is_none() {
            AUDIO_CLOCK_STALE_MS
        } else {
            AUDIO_CLOCK_START_GRACE_MS
        };
        if since >= Duration::from_millis(stale_after) && !held {
            // Hand over at exactly the value the frozen clock last read, so
            // the transition is continuous, then keep real time from here.
            let extra = since.as_millis() as u64 - stale_after;
            let handover = p0 + extra;
            st.wall_from = Some((handover, now, pause_skew));
            self.stats.clock_wall_fallbacks.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "[clock] audio position frozen at {}ms for {}ms while playing                  (ever_advanced={}) — master clock falls back to the wall;                  the audio output is dead or absent",
                p0,
                since.as_millis(),
                st.ever_advanced
            );
            return Some(to_media_us(handover as i64));
        }

        // Interpolate with wall time between the sink's ~per-callback updates;
        // if it hasn't ticked for >80ms the audio is paused/starving — freeze.
        let pos_us = if since < Duration::from_millis(80) {
            p0 as i64 * 1_000 + since.as_micros() as i64
        } else {
            p0 as i64 * 1_000
        };
        Some((pos_us + self.seek_offset_us - lat_us).max(0))
    }

    /// Current 0-based media time (µs): audio when available, else the wall
    /// clock. Only the wall fallback applies `pause_skew` — the audio clock
    /// freezes during pause on its own.
    pub(crate) fn now_us(&self, pause_skew: Duration) -> i64 {
        if let Some(us) = self.audio_now_us(pause_skew) {
            return us;
        }
        self.start_time
            .elapsed()
            .saturating_sub(pause_skew)
            .saturating_sub(Duration::from_millis(self.audio_sink.output_latency_ms()))
            .as_micros() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestSink;

    // ---- MediaClock liveness ------------------------------------------------
    //
    // Regression cover for the TV-standby crawl: HDMI drops, AudioFlinger kills
    // the track, the sink keeps reporting a STANDING position, and the master
    // clock stands with it. Because the sync loop sleeps until each frame's due
    // time, a standing clock makes frame k wait one frame period longer than
    // k-1 - playback degrades into an ever-slower crawl (measured on kirkwood:

    struct ClockFixture {
        clock: MediaClock<TestSink>,
        sink: Arc<TestSink>,
        paused: Arc<AtomicBool>,
        stats: Arc<StatsState>,
    }

    fn fixture(played_ms: u64) -> ClockFixture {
        let sink = Arc::new(TestSink::new(played_ms));
        let paused = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StatsState::default());
        let clock = MediaClock::new(
            sink.clone(),
            Arc::new(Instant::now()),
            Duration::ZERO,
            paused.clone(),
            stats.clone(),
        );
        ClockFixture { clock, sink, paused, stats }
    }

    /// Backdate the anchor so the position looks like it has been standing for
    /// `stood_ms`, without the test having to sleep for it.
    fn stand_still_for(fx: &ClockFixture, played_ms: u64, stood_ms: u64, ever_advanced: bool) {
        let mut st = fx.clock.state.lock().unwrap();
        st.seen = Some((played_ms, Instant::now() - Duration::from_millis(stood_ms), Duration::ZERO));
        st.ever_advanced = ever_advanced;
    }

    #[test]
    fn frozen_audio_clock_hands_over_to_the_wall() {
        let fx = fixture(500);
        assert_eq!(
            fx.clock.audio_now_us(Duration::ZERO),
            Some(500_000),
            "first read is the sink's own position"
        );

        // Audio had been running and then the output died: 1.5 s standing.
        stand_still_for(&fx, 500, 3_500, true);
        let handed = fx.clock.audio_now_us(Duration::ZERO).unwrap();
        // Continuous at the frozen value, then real time for the 500 ms past
        // the stale window - NOT a jump to the free-running wall origin.
        assert!(
            (990_000..=1_020_000).contains(&handed),
            "expected a continuous handover around 1000ms, got {handed}us"
        );

        // And it must keep moving, which is the whole point.
        std::thread::sleep(Duration::from_millis(60));
        let later = fx.clock.audio_now_us(Duration::ZERO).unwrap();
        assert!(
            later - handed >= 50_000,
            "clock stalled after handover: {handed}us -> {later}us"
        );
    }

    #[test]
    fn startup_grace_lets_audio_prime_without_running_video_ahead() {
        // A sink that has never advanced is saying "not audible yet" - video is
        // MEANT to hold. A passthrough AudioTrack needs ~2.5 s before its head
        // moves at all, so a 1.5 s standstill must not trip the fallback.
        let fx = fixture(0);
        assert_eq!(fx.clock.audio_now_us(Duration::ZERO), Some(0));
        stand_still_for(&fx, 0, 1_500, false);
        assert_eq!(
            fx.clock.audio_now_us(Duration::ZERO),
            Some(0),
            "held during the start-up grace"
        );

        // Past the grace, a sink that never came alive is a dead output too.
        stand_still_for(&fx, 0, 6_000, false);
        let handed = fx.clock.audio_now_us(Duration::ZERO).unwrap();
        assert!(
            handed > 0,
            "start-up grace must not last forever, got {handed}us"
        );
    }

    #[test]
    fn pause_and_starvation_do_not_look_like_a_dead_output() {
        // Paused: the head stops on purpose.
        let fx = fixture(700);
        fx.paused.store(true, Ordering::Relaxed);
        stand_still_for(&fx, 700, 30_000, true);
        assert_eq!(
            fx.clock.audio_now_us(Duration::ZERO),
            Some(700_000),
            "paused must freeze, not extrapolate"
        );

        // Starving: audio_sync_loop pauses the sink itself, same deal.
        let fx = fixture(700);
        fx.stats.audio_starving.store(true, Ordering::Relaxed);
        stand_still_for(&fx, 700, 30_000, true);
        assert_eq!(
            fx.clock.audio_now_us(Duration::ZERO),
            Some(700_000),
            "starvation must freeze, not extrapolate"
        );
    }

    #[test]
    fn revived_output_is_re_adopted_even_when_far_behind() {
        // Wall clock has carried us to ~2000ms.
        let fx = fixture(1_000);
        {
            let mut st = fx.clock.state.lock().unwrap();
            st.seen = Some((1_000, Instant::now(), Duration::ZERO));
            st.ever_advanced = true;
            st.wall_from = Some((1_000, Instant::now() - Duration::from_millis(1_000), Duration::ZERO));
        }
        // Output comes back roughly where we are -> re-adopt.
        fx.sink.played_ms.store(2_050, Ordering::Relaxed);
        let _ = fx.clock.audio_now_us(Duration::ZERO);
        assert!(
            fx.clock.state.lock().unwrap().wall_from.is_none(),
            "an agreeing audio clock should take the master back"
        );

        // Output comes back a long way behind (a passthrough receiver that
        // relocked 800 ms later than the wall clock assumed) -> re-adopt
        // anyway: the clock steps back to the audible position once, instead
        // of leaving the picture 800 ms ahead of the sound for good.
        let fx = fixture(1_000);
        {
            let mut st = fx.clock.state.lock().unwrap();
            st.seen = Some((1_000, Instant::now(), Duration::ZERO));
            st.ever_advanced = true;
            st.wall_from = Some((1_000, Instant::now() - Duration::from_millis(1_000), Duration::ZERO));
        }
        fx.sink.played_ms.store(1_200, Ordering::Relaxed);
        let now = fx.clock.audio_now_us(Duration::ZERO).unwrap();
        assert!(
            fx.clock.state.lock().unwrap().wall_from.is_none(),
            "a revived audio clock must take the master back even 800ms behind"
        );
        assert!(
            (1_150_000..=1_250_000).contains(&now),
            "the clock should read the audible position, got {now}us"
        );
    }

    #[test]
    fn sink_without_a_clock_still_uses_the_wall() {
        let fx = fixture(0);
        fx.sink.has_clock.store(false, Ordering::Relaxed);
        assert_eq!(
            fx.clock.audio_now_us(Duration::ZERO),
            None,
            "no clock means no audio master"
        );
        // now_us falls through to the wall clock and keeps advancing.
        let a = fx.clock.now_us(Duration::ZERO);
        std::thread::sleep(Duration::from_millis(40));
        let b = fx.clock.now_us(Duration::ZERO);
        assert!(b - a >= 30_000, "wall fallback stalled: {a}us -> {b}us");
    }


    #[test]
    fn a_pause_is_not_a_dead_audio_output() {
        // The exact desktop report: pause for a while, resume, and the picture
        // races to catch up the paused span. Nothing polls this clock while
        // paused (the sync loop is parked), so the first call after the resume
        // sees an anchor as old as the pause with the paused flag already
        // cleared - which is why the `!paused` guard alone cannot catch it and
        // staleness has to be counted in played time.
        let fx = fixture(500);
        assert_eq!(fx.clock.audio_now_us(Duration::ZERO), Some(500_000));

        // 15 s of wall time passed, every millisecond of it paused.
        let paused_for = Duration::from_millis(15_000);
        stand_still_for(&fx, 500, 15_000, true);
        let after = fx.clock.audio_now_us(paused_for).unwrap();

        // Tolerance, not equality: the sub-80ms branch interpolates with wall
        // time and legitimately adds a microsecond or two. The skip this
        // guards against is 15 SECONDS.
        assert!(
            (after - 500_000).abs() < 5_000,
            "the clock skipped the pause: 500000us -> {after}us"
        );
        assert!(
            fx.clock.state.lock().unwrap().wall_from.is_none(),
            "a pause was mistaken for a dead output"
        );
    }

    #[test]
    fn the_wall_fallback_does_not_run_through_a_pause() {
        // Reported from the desktop build: pause for a while, resume, and the
        // picture races to catch up the paused span. The audio clock freezes on
        // its own when the device stops, so the normal path never had to think
        // about pause - but the wall-clock fallback runs with REAL time, and on
        // a machine with no working audio output (which is what puts the clock
        // there) it counted the whole pause as media.
        let fx = fixture(500);
        stand_still_for(&fx, 500, 4_000, true);
        let entered = fx.clock.now_us(Duration::ZERO);

        // 60 ms pass, all of them paused: media time must not move.
        std::thread::sleep(Duration::from_millis(60));
        let after_pause = fx.clock.now_us(Duration::from_millis(60));
        assert!(
            (after_pause - entered).abs() < 15_000,
            "clock ran through a pause: {entered}us -> {after_pause}us"
        );

        // 60 ms more, this time played: media time must move by about that.
        std::thread::sleep(Duration::from_millis(60));
        let after_play = fx.clock.now_us(Duration::from_millis(60));
        assert!(
            after_play - after_pause >= 40_000,
            "clock stalled while playing: {after_pause}us -> {after_play}us"
        );
    }

}
