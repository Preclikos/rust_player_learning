//! Audio-output liveness watchdog.
//!
//! Split out of `player.rs`: it is one task with one job, and the rules about
//! what does and does not count as a dead output are subtle enough to want
//! their tests next to them.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, Notify, RwLock};

use crate::events::{BufferingReason, PlayerErrorKind, PlayerEvent};
use crate::renderers::AudioSink;

use super::{report_starvation, StallSide, StarvationTransition, StatsState};

/// The audio output's reported position may stand still this long, while
/// playback is running and nothing else has declared a stall, before the
/// watchdog calls the output dead. Above the coarsest device update burst
/// (~256 ms on Android deep buffer) with room to spare, and deliberately
/// EARLIER than [`AUDIO_CLOCK_STALE_MS`] so the watchdog — which can actually
/// fix the problem — always acts before the clock's last-resort guard.
const AUDIO_OUTPUT_DEAD_MS: u64 = 1_500;

/// Pipeline rebuilds spent trying to bring the output back before the player
/// reports the failure instead of playing on without sound.
const AUDIO_OUTPUT_MAX_REBUILDS: u32 = 1;

/// The output must keep advancing this long for the rebuild budget to reset,
/// so a device that dies again hours later still gets a full ladder.
const AUDIO_OUTPUT_HEALTHY_MS: u64 = 15_000;

/// Audio-output liveness watchdog — one task per pipeline generation.
///
/// The output can die under a running pipeline without anything noticing: on TV
/// standby the HDMI sink goes away and AudioFlinger kills the `IAudioTrack`
/// server-side, after which the sink still reports a position — it just never
/// moves again. The master clock is audio-disciplined, so a standing position
/// standing means a standing clock, and the sync loop paces each frame against
/// it: frame k waits one frame period longer than k-1 and the picture decays
/// into an ever-slower crawl (measured on a Google TV Streamer: 0.2 fps, media
/// advancing at 0.8 % of real time, for as long as the stream was left up).
///
/// What the viewer should get instead is what they would get from any other
/// player: a moment of loading while the output is brought back, then sound.
/// So on a standing position this parks the picture behind `Buffering` and
/// rebuilds the pipeline at the current position — a fresh pipeline builds a
/// fresh output device, which is what actually recovers HDMI audio. If the
/// output is still dead after the rebuild budget, the failure is reported
/// ([`PlayerErrorKind::AudioOutput`]) rather than papered over: a movie
/// playing silently is a failure, not a degraded success.
///
/// It only ever judges an output it has seen working: the death timer arms
/// after the first observed advance. A position that has never moved means
/// the pipeline is still starting, which is [`AUDIO_CLOCK_START_GRACE_MS`]'s
/// business, not a device to rebuild.
pub(crate) async fn audio_output_watchdog<A: AudioSink>(
    gen: u64,
    audio_sink: Arc<A>,
    stats: Arc<StatsState>,
    events: Arc<broadcast::Sender<PlayerEvent>>,
    paused: Arc<AtomicBool>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    position_ms: Arc<AtomicU64>,
    seek_target: Arc<RwLock<Option<Duration>>>,
) {
    // (last position seen, wall instant it FIRST read that value).
    // tokio's clock, not std's: it is the one  can drive,
    // which is what makes the liveness thresholds testable without sleeping.
    let mut seen: Option<(u64, tokio::time::Instant)> = None;
    // When the output started advancing again after a death (budget reset).
    let mut live_since: Option<tokio::time::Instant> = None;
    // True once this generation has actually SEEN the head move. Until then
    // there is no dead output to diagnose: a position standing at 0 is how a
    // sink says "not audible yet", and the gap between opening the device and
    // the first sample reaching it is unbounded — it spans the first
    // segment's fetch, decrypt and decode, which on desktop after a resume
    // seek is comfortably several seconds. Arming the death timer before the
    // first advance turns every start-up into a phantom rebuild and then an
    // AudioOutput error on a machine whose sound was never broken. Start-up
    // has its own guard: MediaClock holds the picture for
    // [`AUDIO_CLOCK_START_GRACE_MS`] while the position has never advanced.
    let mut ever_advanced = false;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            _ = stop.notified() => return,
        }
        if stop_flag.load(Ordering::Relaxed) {
            return;
        }
        // Paused: the head stops on purpose.
        if paused.load(Ordering::Relaxed) {
            seen = None;
            live_since = None;
            continue;
        }
        // Someone else already declared a stall — the consumer is seeing
        // Buffering and the head is standing for a reason we must not mistake
        // for a dead device.
        //
        // BOTH sides matter, not just the audio one: when VIDEO starves, the
        // sync loop pauses the audio sink on purpose so the two stay locked,
        // and the position then stands still exactly like a dead output. This
        // watchdog read that as a death and rebuilt the pipeline on top of a
        // pipeline that was merely waiting for frames — measured once at 18 s
        // of starvation before it recovered.
        if stats.audio_starving.load(Ordering::Relaxed)
            || stats.video_starving.load(Ordering::Relaxed)
        {
            seen = None;
            live_since = None;
            continue;
        }
        // No clock at all: the MediaClock is already on the wall and there is
        // no output position to watch.
        let Some(played) = audio_sink.played_since_flush_ms() else {
            seen = None;
            live_since = None;
            continue;
        };

        match seen {
            None => {
                seen = Some((played, tokio::time::Instant::now()));
                continue;
            }
            Some((p0, _)) if played > p0 => {
                seen = Some((played, tokio::time::Instant::now()));
                ever_advanced = true;
                let since = *live_since.get_or_insert_with(tokio::time::Instant::now);
                if stats.audio_output_rebuilds.load(Ordering::Relaxed) > 0
                    && since.elapsed() >= Duration::from_millis(AUDIO_OUTPUT_HEALTHY_MS)
                {
                    log::info!(
                        "[audio-watchdog gen {gen}] output healthy again — resetting the rebuild budget"
                    );
                    stats.audio_output_rebuilds.store(0, Ordering::Relaxed);
                }
                continue;
            }
            // Never started: a start-up problem, not a device that died.
            Some(_) if !ever_advanced => {
                continue;
            }
            Some((_, w0)) if w0.elapsed() < Duration::from_millis(AUDIO_OUTPUT_DEAD_MS) => {
                continue;
            }
            Some((_, w0)) => {
                // ---- the output is dead ----
                let stood_ms = w0.elapsed().as_millis();
                let spent = stats.audio_output_rebuilds.load(Ordering::Relaxed);
                if spent >= AUDIO_OUTPUT_MAX_REBUILDS {
                    log::error!(
                        "[audio-watchdog gen {gen}] audio output still dead at {played}ms after \
                         {spent} rebuild(s) — giving up rather than playing on without sound"
                    );
                    let _ = events.send(PlayerEvent::Error {
                        kind: PlayerErrorKind::AudioOutput,
                        detail: format!(
                            "audio output stopped ({played}ms, standing {stood_ms}ms) and could \
                             not be restored by a pipeline rebuild"
                        ),
                    });
                    return;
                }
                log::warn!(
                    "[audio-watchdog gen {gen}] audio output position stuck at {played}ms for \
                     {stood_ms}ms while playing — rebuilding the pipeline to get sound back"
                );
                stats.audio_output_rebuilds.fetch_add(1, Ordering::Relaxed);

                // Park the picture behind a spinner first: the user should see
                // that something is being fixed, not a movie that went mute.
                // Parking also suppresses MediaClock's last-resort wall-clock
                // guard, so the two mechanisms never fight over the picture.
                if let StarvationTransition::EnteredBuffering =
                    report_starvation(&stats, StallSide::Audio, true)
                {
                    let _ = events.send(PlayerEvent::Buffering {
                        reason: BufferingReason::Stall,
                    });
                }

                // Rebuild at the current position. Same handshake as `seek()`:
                // the play loop sees a seek_target and respawns the pipeline,
                // which builds a brand-new output device.
                let target = Duration::from_millis(position_ms.load(Ordering::Relaxed));
                {
                    let mut slot = seek_target.write().await;
                    *slot = Some(target);
                    stop_flag.store(true, Ordering::Relaxed);
                }
                audio_sink.flush();
                audio_sink.set_paused(true);
                stop.notify_waiters();
                // This generation is over; the next one gets a fresh watchdog.
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestSink;

    // ---- audio-output watchdog ----------------------------------------------
    //
    // The ladder the viewer should experience when the output dies under a
    // running pipeline (TV standby kills the HDMI track): loading, a rebuild
    // that brings sound back, and an honest error if it cannot.

    struct WatchdogRig {
        sink: Arc<TestSink>,
        stats: Arc<StatsState>,
        events: Arc<broadcast::Sender<PlayerEvent>>,
        rx: broadcast::Receiver<PlayerEvent>,
        paused: Arc<AtomicBool>,
        stop: Arc<Notify>,
        stop_flag: Arc<AtomicBool>,
        position_ms: Arc<AtomicU64>,
        seek_target: Arc<RwLock<Option<Duration>>>,
    }

    fn rig() -> WatchdogRig {
        let (tx, rx) = broadcast::channel(64);
        WatchdogRig {
            sink: Arc::new(TestSink::new(1_000)),
            stats: Arc::new(StatsState::default()),
            events: Arc::new(tx),
            rx,
            paused: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(Notify::new()),
            stop_flag: Arc::new(AtomicBool::new(false)),
            position_ms: Arc::new(AtomicU64::new(42_000)),
            seek_target: Arc::new(RwLock::new(None)),
        }
    }

    fn spawn_watchdog(r: &WatchdogRig) -> tokio::task::JoinHandle<()> {
        tokio::spawn(audio_output_watchdog(
            7,
            r.sink.clone(),
            r.stats.clone(),
            r.events.clone(),
            r.paused.clone(),
            r.stop.clone(),
            r.stop_flag.clone(),
            r.position_ms.clone(),
            r.seek_target.clone(),
        ))
    }

    fn drain(rx: &mut broadcast::Receiver<PlayerEvent>) -> Vec<PlayerEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Play the output for a second so the watchdog sees a live head; the
    /// death it is meant to catch is a device that stops, and it deliberately
    /// does not judge one that never started.
    async fn play_a_while(r: &WatchdogRig) {
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            r.sink.played_ms.fetch_add(250, Ordering::Relaxed);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dead_output_shows_loading_and_rebuilds_the_pipeline() {
        let mut r = rig();
        let wd = spawn_watchdog(&r);
        play_a_while(&r).await;
        // ...and now it never moves again. Bounding the wait is what makes the
        // liveness threshold load-bearing: without it the test would pass even
        // if the watchdog took a day to notice.
        let t0 = tokio::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), wd)
            .await
            .expect("watchdog did not act on a dead output within 5s")
            .unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5), "acted only after {:?}", t0.elapsed());

        assert_eq!(
            *r.seek_target.read().await,
            Some(Duration::from_millis(42_000)),
            "the pipeline must be rebuilt at the current position"
        );
        assert!(r.stop_flag.load(Ordering::Relaxed), "the old pipeline must be torn down");
        assert_eq!(r.stats.audio_output_rebuilds.load(Ordering::Relaxed), 1);
        assert!(
            r.stats.audio_starving.load(Ordering::Relaxed),
            "the picture must be parked, not left ploughing on silently"
        );
        let events = drain(&mut r.rx);
        assert!(
            events.iter().any(|e| matches!(e, PlayerEvent::Buffering { .. })),
            "the viewer must see loading; got {events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn output_that_stays_dead_after_the_rebuild_is_reported_not_papered_over() {
        let mut r = rig();
        // Budget already spent by the previous generation.
        r.stats
            .audio_output_rebuilds
            .store(AUDIO_OUTPUT_MAX_REBUILDS, Ordering::Relaxed);
        let wd = spawn_watchdog(&r);
        play_a_while(&r).await;
        let t0 = tokio::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), wd)
            .await
            .expect("watchdog did not report the dead output within 5s")
            .unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5), "acted only after {:?}", t0.elapsed());

        assert!(
            r.seek_target.read().await.is_none(),
            "must not rebuild forever"
        );
        let events = drain(&mut r.rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                PlayerEvent::Error { kind: PlayerErrorKind::AudioOutput, .. }
            )),
            "a movie that cannot have sound is a failure, not a silent success; got {events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_output_is_left_alone() {
        let mut r = rig();
        let wd = spawn_watchdog(&r);
        // Advance the head at real time for a few seconds.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            r.sink.played_ms.fetch_add(250, Ordering::Relaxed);
        }
        assert!(!wd.is_finished(), "watchdog fired on a healthy output");
        assert!(r.seek_target.read().await.is_none());
        assert_eq!(r.stats.audio_output_rebuilds.load(Ordering::Relaxed), 0);
        assert!(drain(&mut r.rx).is_empty(), "a healthy output needs no events");
        wd.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn pause_and_starvation_are_not_mistaken_for_a_dead_output() {
        // Paused: the head stops on purpose.
        let r = rig();
        r.paused.store(true, Ordering::Relaxed);
        let wd = spawn_watchdog(&r);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!wd.is_finished(), "watchdog fired while paused");
        assert!(r.seek_target.read().await.is_none());
        wd.abort();

        // Starving: the audio sync loop already declared a stall and paused the
        // sink; the consumer is looking at a spinner for a different reason.
        let r = rig();
        r.stats.audio_starving.store(true, Ordering::Relaxed);
        let wd = spawn_watchdog(&r);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!wd.is_finished(), "watchdog fired during ordinary starvation");
        assert!(r.seek_target.read().await.is_none());
        wd.abort();

        // VIDEO starving pauses the audio sink too, so the head stands still
        // for a reason that has nothing to do with the output device. Missing
        // this rebuilt the pipeline under a merely-waiting one on a real
        // device, turning a hiccup into 18 s of starvation.
        //
        // The output has to have been RUNNING first, or the watchdog would
        // hold off anyway and the test would pass for the wrong reason.
        let r = rig();
        let wd = spawn_watchdog(&r);
        play_a_while(&r).await;
        r.stats.video_starving.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(
            !wd.is_finished(),
            "watchdog mistook a video stall for a dead audio output"
        );
        assert!(r.seek_target.read().await.is_none());
        wd.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn an_output_that_never_started_is_not_a_dead_output() {
        // Desktop (WASAPI) reports a position from the moment the device is
        // opened, which is long before the first sample arrives: the first
        // segment still has to be fetched, decrypted and decoded, and after a
        // resume seek that is several seconds. Treating that standing 0 as a
        // death rebuilt the pipeline on every start and then reported
        // AudioOutput on machines whose sound was fine.
        let mut r = rig();
        r.sink.played_ms.store(0, Ordering::Relaxed);
        let wd = spawn_watchdog(&r);
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(!wd.is_finished(), "watchdog fired before audio ever started");
        assert!(r.seek_target.read().await.is_none(), "no phantom rebuild");
        assert_eq!(r.stats.audio_output_rebuilds.load(Ordering::Relaxed), 0);
        assert!(drain(&mut r.rx).is_empty(), "start-up is not an error");

        // Once it HAS started, a stop is judged as usual.
        play_a_while(&r).await;
        tokio::time::timeout(Duration::from_secs(5), wd)
            .await
            .expect("watchdog did not act after the output died for real")
            .unwrap();
        assert!(r.seek_target.read().await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_sink_with_no_clock_is_not_a_dead_output() {
        // No clock at all means the MediaClock is already on the wall; there is
        // no output position to watch and nothing to rebuild.
        let r = rig();
        r.sink.has_clock.store(false, Ordering::Relaxed);
        let wd = spawn_watchdog(&r);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!wd.is_finished());
        assert!(r.seek_target.read().await.is_none());
        wd.abort();
    }


}
