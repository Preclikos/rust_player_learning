//! A/V sync: the per-track sync loops and the handler that owns a
//! generation of the pipeline and keeps the two in step.

use super::*;

// ---------------------------------------------------------------------------
// A/V sync loop — identical on all platforms, generic over sink traits
// ---------------------------------------------------------------------------

/// Toggle the per-side starvation flag and report what changed about
/// the COMBINED `video_starving || audio_starving` state. Callers use
/// the returned transition to decide whether to pause / unpause the
/// audio sink and emit a `PlayerEvent::Buffering` or `Playing` to the
/// consumer. The transition is computed read-modify-read-style (no
/// CAS) because each side only writes its own flag, so there's a
/// single writer per atomic.
pub(super) fn report_starvation(
    stats: &StatsState,
    side: StallSide,
    starving: bool,
) -> StarvationTransition {
    let other_starving = match side {
        StallSide::Video => stats.audio_starving.load(Ordering::Relaxed),
        StallSide::Audio => stats.video_starving.load(Ordering::Relaxed),
    };
    let was_buffering = other_starving
        || match side {
            StallSide::Video => stats.video_starving.swap(starving, Ordering::Relaxed),
            StallSide::Audio => stats.audio_starving.swap(starving, Ordering::Relaxed),
        };
    let is_buffering = other_starving || starving;
    if !was_buffering && is_buffering {
        stats.stall_events.fetch_add(1, Ordering::Relaxed);
        StarvationTransition::EnteredBuffering
    } else if was_buffering && !is_buffering {
        StarvationTransition::ExitedBuffering
    } else {
        StarvationTransition::Unchanged
    }
}



pub(super) async fn video_sync_loop<V: VideoSink, A: AudioSink>(
    // DIAG: pipeline generation id (one per play-loop (re)build). Tags HEALTH +
    // start/exit so concurrent vsync loops (a superseded generation that didn't
    // tear down) are visible in logcat — see ABR_REBUILD_ORPHANED_DOWNLOADER.
    gen: u64,
    start_time: Arc<Instant>,
    // 0-based media position playback starts at (the requested seek/resume
    // TARGET, NOT segment-snapped). The audio-master clock (`MediaClock`)
    // is rebased by this so `position_ms` reports the ABSOLUTE stream position,
    // not the audio device's free-running played_ms. Without it, position
    // collapses to play-time-since-(seek/resume) — breaking the seekbar,
    // relative seeks, and the ABR soft-switch's restart-segment pick.
    seek_offset: Duration,
    // Content origin (absolute pts of the first segment, µs). Frames are
    // scheduled on the 0-based media axis `pts − origin` — the SAME axis the
    // audio is trimmed on and the clock counts in — instead of being anchored
    // to whatever the clock happened to read when the first frame arrived.
    origin_us: i64,
    // Frame-accurate seek: the decoder feeds from the segment-start keyframe,
    // which can be up to a segment before the target. Frames whose absolute
    // pts_us is below this threshold are dropped (codec buffer released) WITHOUT
    // pacing/rendering, so playback begins exactly at the target instead of the
    // segment boundary. 0 = no discard (start of content / segment-aligned).
    discard_below_us: i64,
    mut input_rx: mpsc::Receiver<DecodedVideoFrame>,
    renderer: Arc<V>,
    audio_sink: Arc<A>,
    position_ms: Arc<AtomicU64>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    events: Arc<broadcast::Sender<PlayerEvent>>,
    media_duration: Duration,
    paused: Arc<AtomicBool>,
    pause_notify: Arc<Notify>,
    stats: Arc<StatsState>,
) {
    // While paused, real time keeps advancing but media time must NOT.
    // We accumulate the wall-clock duration spent paused and subtract
    // it from `start_time.elapsed()` everywhere we compute "current
    // playback time", so resuming after a 10s pause doesn't make every
    // frame look "10s late" and trigger the drain-to-catch-up path.
    let mut pause_skew = Duration::ZERO;
    // Frame-accurate-seek discard gate: false until the first frame at/after
    // the target is seen; pre-target frames are dropped (see `discard_below_us`).
    let mut reached_target = discard_below_us <= 0;
    let mut discarded = 0u32;
    // Start rendering this many ms before the target PTS so the GPU has time
    // to finish before the VSync deadline. The compositor (via
    // eglPresentationTimeANDROID) then holds the frame until the exact VSync.
    const RENDER_BUDGET_MS: u64 = 20;

    let mut last_pts_ms = 0u64;
    let mut frame_idx: u64 = 0;
    let mut last_render_elapsed: u64 = 0;
    // Position event rate-limit: ≤ 4 Hz per PLAYER_INTEGRATION.md §4.2.
    let mut last_position_emit = Instant::now() - Duration::from_secs(1);
    // Stats event rate-limit: ≤ 1 Hz per PLAYER_INTEGRATION.md §4.1.
    let mut last_stats_emit = Instant::now() - Duration::from_secs(1);
    // A/V drift measurement: (video elapsed_ms, audio played_ms) at the
    // first stats tick — subsequent ticks compare ADVANCES from here.
    let mut drift_baseline: Option<(u64, u64)> = None;
    let mut last_drift_warn: Option<Instant> = None;
    // Min A/V drift over the current 1Hz stats window (least-stale read of
    // the chunky audio-played counter ≈ the true offset).
    let mut drift_min_window = i64::MAX;
    let mut emitted_playing = false;
    // Have we presented at least one frame on this pipeline run? Drives the
    // seek-while-paused preview: the pause gate below only parks AFTER one
    // frame is on screen, so a fresh pipeline (cold start or post-seek)
    // always paints its first at-target frame even when paused — the user
    // sees where they landed instead of a stale frozen frame. Pauses during
    // playback (presented_frame already true) park immediately as before.
    let mut presented_frame = false;
    // Per-second HEALTH heartbeat baselines: previous cumulative counters,
    // so the stats tick can log frame DROPS and DECODES as a per-second
    // delta (a climbing cumulative number is hard to read live). See the
    // stats emit block below.
    let mut last_health_dropped = 0u64;
    let mut last_health_decoded = 0u64;
    // Starvation tracking — flips when the decoder hasn't produced a
    // frame for >300 ms (typically a network outage hitting the
    // download side and propagating through the empty decoder queue).
    // While set, audio_sink is paused and the consumer sees a
    // Buffering{Stall} event; resetting on the next frame emits
    // Playing again. Without this, video would freeze (recv blocked)
    // while audio kept emptying its ~2 s cpal queue — exactly the
    // "audio kept going, no buffering UI" symptom from the network-
    // disconnect repro.
    let mut starving = false;
    let mut starvation_started: Option<Instant> = None;
    // Frames presented more than this far after their clock time — visible as
    // a per-frame lip-sync error even though nothing was dropped (the LATE
    // drain only kicks in past 80 ms). Conformance gauge.
    const LATE_FRAME_MS: u64 = 45;
    // Startup diagnostics: how long the first frame waited for the audio clock
    // to start moving, and where it landed relative to the target.
    let loop_started = Instant::now();
    let mut logged_first_frame = false;
    // De-judder smoother state: (present_ns, pts_us_rel) of the last frame, so
    // the next frame's present time can be snapped to the smooth media cadence
    // (last + Δpts) instead of inheriting the audio clock's frame-to-frame
    // wobble. Bounded to ±PRESENT_SMOOTH_NS of the raw value (see present block).
    let mut last_present: Option<(i64, i64)> = None;
    // Browser: present on the display's vsync (see VsyncCadence).
    #[cfg(target_arch = "wasm32")]
    let mut vsync = crate::av_sync::VsyncCadence::new();
    #[cfg(target_arch = "wasm32")]
    let mut drew_this_tick = false;
    // Max deviation of the smoothed present time from the raw `now + (pts −
    // clock)` value. ≥ the audio clock's per-callback quantization (~one cpal
    // buffer) so steady-state wobble is fully absorbed, yet small enough that a
    // seek / LATE / resume jump (raw moves further than this) is followed at
    // once and the cadence re-bases — so this can never schedule meaningfully
    // further from reality than the proven raw formula already did.
    const PRESENT_SMOOTH_NS: i64 = 40_000_000;
    // Cap on how far in the future a frame's present stamp may point (see the
    // clamp in the present block). 250 ms ≈ 6 frames at 24 fps — well inside
    // the direct codec's 8-buffer output pool, so SurfaceFlinger can never be
    // handed enough far-future frames to drain it.
    const MAX_PRESENT_LEAD_NS: i64 = 250_000_000;
    // Playback master clock: audio-disciplined, 0-based, rebased to this
    // pipeline's timeline. Video paces to it; the same seam serves passthrough
    // / multichannel / other renderers (see MediaClock).
    let clock = MediaClock::new(
        audio_sink.clone(),
        start_time.clone(),
        seek_offset,
        paused.clone(),
        stats.clone(),
    );
    log::info!(
        "[vsync gen {}] loop start (seek_offset={}ms origin={}ms)",
        gen,
        seek_offset.as_millis(),
        origin_us / 1000
    );
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        // Park here if pause() was called. Note: when stop fires during
        // a pause, we still want to exit cleanly. Gated on `presented_frame`
        // so a seek/start while paused still paints one frame (see above)
        // before we honour the pause.
        if paused.load(Ordering::Relaxed) && presented_frame {
            let pause_started = Instant::now();
            tokio::select! {
                _ = pause_notify.notified() => {}
                _ = stop.notified() => break,
            }
            pause_skew += pause_started.elapsed();
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            // Re-arm Playing emission: pause() emits Paused, and resume()
            // relies on the first post-resume frame to emit Playing — but
            // emitted_playing was sticky-true from startup, so the gate
            // below would swallow the Paused→Playing transition.
            emitted_playing = false;
        }
        // Race recv against a 300 ms timeout so we can detect that
        // the decoder pipeline has stopped producing frames (network
        // outage, slow segment download, decoder hang, …) and pause
        // the audio sink + surface `Buffering{Stall}` to the consumer.
        // ALSO park here while the AUDIO side is starving: if audio
        // has gone silent we don't want video to keep ploughing ahead
        // through frames, the user is supposed to see "buffering"
        // not "video playing without sound". `pause_skew` accumulates
        // the wait so once both sides recover the next frame still
        // renders at its proper PTS instead of being declared LATE
        // by the wall-clock-based catch-up logic.
        let mut frame = loop {
            // If the AUDIO side is currently stalled, park video here
            // until it recovers. Recv on input_rx with no timeout so
            // we observe stop / forward channel closure normally.
            if stats.audio_starving.load(Ordering::Relaxed) {
                let park_started = Instant::now();
                tokio::select! {
                    _ = crate::rt::sleep(Duration::from_millis(100)) => {}
                    _ = stop.notified() => return,
                }
                pause_skew += park_started.elapsed();
                continue;
            }
            let starvation_wait = crate::rt::sleep(Duration::from_millis(300));
            tokio::pin!(starvation_wait);
            tokio::select! {
                maybe = input_rx.recv() => {
                    let f = match maybe {
                        Some(f) => f,
                        None => return,
                    };
                    if starving {
                        let waited = starvation_started
                            .map(|t| t.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        log::info!("[vsync] starvation recovered after {}ms", waited);
                        stats.stall_ms_total.fetch_add(waited, Ordering::Relaxed);
                        // Roll the wall clock back by the time we sat
                        // starving so the recovering frame's pts_ms
                        // isn't declared LATE by however many ms we
                        // were waiting. Audio was `set_paused(true)`
                        // for the whole starvation window (see the
                        // starvation-wait arm below), so it didn't
                        // advance either — no separate `drop_ms`
                        // needed to keep A/V locked.
                        if let Some(t) = starvation_started.take() {
                            pause_skew += t.elapsed();
                        }
                        starving = false;
                        if let StarvationTransition::ExitedBuffering =
                            report_starvation(&stats, StallSide::Video, false)
                        {
                            let _ = events.send(PlayerEvent::Playing);
                            if !paused.load(Ordering::Relaxed) {
                                audio_sink.set_paused(false);
                            }
                        }
                    }
                    break f;
                }
                _ = &mut starvation_wait => {
                    // Planned ABR/manual track swap: OLD's decoder is torn
                    // down and NEW's first GOP is still decoding. Not a
                    // network stall — hold the last frame and keep audio
                    // rolling instead of flashing the buffering UI; the LATE
                    // drain below re-aligns video once NEW's frames land.
                    let in_swap_grace = stats
                        .swap_grace_deadline
                        .lock()
                        .unwrap()
                        .is_some_and(|d| Instant::now() < d);
                    if in_swap_grace && !starving {
                        continue;
                    }
                    if !starving {
                        starving = true;
                        starvation_started = Some(Instant::now());
                        log::warn!("[vsync] no frame for 300ms — entering buffering");
                        if let StarvationTransition::EnteredBuffering =
                            report_starvation(&stats, StallSide::Video, true)
                        {
                            let _ = events.send(PlayerEvent::Buffering {
                                reason: BufferingReason::Stall,
                            });
                            audio_sink.set_paused(true);
                        }
                    }
                    continue;
                }
                _ = stop.notified() => return,
            }
        };
        // Frame-accurate seek: drop frames decoded from the segment-start
        // keyframe that fall before the requested target. Dropping `frame`
        // releases its codec output buffer (direct mode), keeping the decoder
        // flowing; we neither pace nor render until the first at/after-target
        // frame, which then anchors the clock exactly at the target.
        if !reached_target {
            if frame.pts_us < discard_below_us {
                discarded += 1;
                continue;
            }
            log::debug!(
                "[vsync] seek target reached: pts={}ms (dropped {} pre-target frames)",
                frame.pts_us / 1000, discarded
            );
            reached_target = true;
        }
        let raw_pts_ms = (frame.pts_us / 1000) as u64;
        // Master clock — audio-disciplined (already output-latency corrected
        // so it reads the media time currently AUDIBLE), wall fallback. See
        // MediaClock.
        let elapsed = (clock.now_us(pause_skew) / 1_000) as u64;

        // The frame's 0-based media time — the axis the clock counts in and
        // the audio was trimmed on. No first-frame anchoring: a frame is due
        // exactly when the clock reads its pts, whatever the clock read when
        // it happened to arrive.
        let mut pts_ms = crate::av_sync::media_pts_ms(frame.pts_us, origin_us);
        if !logged_first_frame {
            logged_first_frame = true;
            log::info!(
                "[vsync gen {}] first frame pts={}ms target={}ms clock={}ms audio_since_flush={:?}ms {}ms after loop start",
                gen,
                pts_ms,
                seek_offset.as_millis(),
                elapsed,
                audio_sink.played_since_flush_ms(),
                loop_started.elapsed().as_millis()
            );
        }

        if pts_ms < last_pts_ms {
            log::warn!("[vsync] BACKWARD #{} pts={}ms last={}ms Δ=-{}ms elapsed={}ms",
                frame_idx, pts_ms, last_pts_ms, last_pts_ms - pts_ms, elapsed);
        }

        if elapsed > pts_ms {
            let late_ms = elapsed - pts_ms;
            if late_ms > 80 {
                log::warn!("[vsync] LATE #{} pts={}ms elapsed={}ms late={}ms",
                    frame_idx, pts_ms, elapsed, late_ms);
                // Drain only as many frames as needed to catch up.
                // Draining ALL buffered frames would skip seconds of content
                // (the channel can hold 64 frames = 2.7 s) causing a jarring
                // jump. Draining exactly late_ms/frame_interval frames brings
                // pts ≈ elapsed with no overshoot and no content skip.
                let max_drain = (late_ms / 42).saturating_sub(1) as usize;
                let mut drained = 0usize;
                loop {
                    if drained >= max_drain { break; }
                    match input_rx.try_recv() {
                        Ok(newer) => {
                            frame = newer;
                            pts_ms = crate::av_sync::media_pts_ms(frame.pts_us, origin_us);
                            drained += 1;
                        }
                        Err(_) => break,
                    }
                }
                if drained > 0 {
                    stats
                        .video_frames_dropped
                        .fetch_add(drained as u64, Ordering::Relaxed);
                    // Audio is deliberately NOT skipped in lock-step. The cpal
                    // sink consumes samples at device rate on its own timer,
                    // so when video falls behind the wall clock (decoder
                    // hiccup, ABR-swap decoder spin-up) audio is still exactly
                    // AT the clock — draining stale video frames re-aligns
                    // video to that same clock and the two meet. Skipping
                    // audio by the drained span too (the old drop_ms call)
                    // pushed audio AHEAD of the clock by that span on every
                    // LATE event: each ABR swap leaked ~150-250 ms of
                    // audio-leads offset (the swap gap minus the frame-channel
                    // cushion), and repeated auto-ABR switches accumulated it
                    // into a gross lip-sync error while fixed-quality playback
                    // stayed clean.
                }
            }
        } else {
            // Sleep until shortly before the target PTS. GL path: the GPU
            // draws the frame early and eglPresentationTimeANDROID holds it
            // until exactly pts_ms. Direct mode: the release timestamp does
            // the same, and a LARGER lead is the point — every released
            // frame returns its output buffer to MediaCodec (the channel +
            // reorder window only hold ~4), so queueing 2-3 frames ahead in
            // SurfaceFlinger is what bridges the segment-boundary decoder
            // warmup that the GL path bridged with its 32-image pool.
            #[cfg(target_os = "android")]
            let render_budget_ms = if matches!(
                frame.native,
                crate::decoders::PlatformFrame::MediaCodecDirect(_)
            ) {
                100
            } else {
                RENDER_BUDGET_MS
            };
            #[cfg(not(target_os = "android"))]
            let render_budget_ms = RENDER_BUDGET_MS;
            #[cfg(not(target_arch = "wasm32"))]
            {
                let target_wake_ms = pts_ms.saturating_sub(render_budget_ms);
                if target_wake_ms > elapsed {
                    tokio::select! {
                        _ = crate::rt::sleep(Duration::from_millis(target_wake_ms - elapsed)) => {}
                        _ = stop.notified() => break,
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
            // Browser: the canvas is composited on the display's vsync, so a
            // frame drawn between two vsyncs shows at the next one — timer
            // pacing lands each frame on a random side of a vsync (±16 ms of
            // judder the render-interval numbers never show). Wait for
            // animation-frame ticks instead and draw in the tick whose
            // upcoming vsync is nearest the frame's clock time; the draw
            // below happens inside that tick, before the browser composites.
            // A hidden tab stops animation frames — the 250 ms fallback keeps
            // the pipeline moving (the LATE drain catches up when visible).
            #[cfg(target_arch = "wasm32")]
            {
                let _ = render_budget_ms;
                let mut stopped = false;
                // One frame per tick: a second draw in the same tick only
                // overwrites the first before the browser composites it (a
                // frame the viewer never sees, but a "render" in the stats).
                let mut need_tick = drew_this_tick;
                loop {
                    if !need_tick {
                        let now_ms = (clock.now_us(pause_skew) / 1_000) as u64;
                        if vsync.frame_due(pts_ms, now_ms) {
                            break;
                        }
                    }
                    tokio::select! {
                        t = crate::rt::animation_frame() => { vsync.observe_tick(t); need_tick = false; }
                        _ = crate::rt::sleep(Duration::from_millis(250)) => { need_tick = false; }
                        _ = stop.notified() => { stopped = true; break; }
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        stopped = true;
                        break;
                    }
                }
                if stopped {
                    break;
                }
                drew_this_tick = true;
            }
        }

        // Compute the absolute CLOCK_MONOTONIC time at which this frame should
        // appear on screen. The renderer passes this to eglPresentationTimeANDROID
        // so the compositor schedules the frame at the correct VSync even if the
        // GPU finishes slightly earlier or later than expected.
        // Use microseconds (not ms) to preserve the sub-millisecond fraction —
        // 23.976fps frames are 41.708µs apart, and ms-truncation here would drift
        // across VSync boundaries every ~24 frames, causing irregular pulldown.
        let pts_us_rel = (frame.pts_us - origin_us).max(0);
        let elapsed_us = clock.now_us(pause_skew);
        // Conformance gauge: presented visibly after its clock time (the
        // LATE drain above only intervenes past 80 ms; 45–80 ms late frames
        // are shown late — a per-frame lip-sync error the viewer can see).
        if elapsed_us / 1_000 > pts_ms as i64 + LATE_FRAME_MS as i64 {
            stats.video_late_frames.fetch_add(1, Ordering::Relaxed);
        }
        let pts_to_go_ns = (pts_us_rel - elapsed_us).max(0) * 1_000;
        let raw_present_ns = clock_monotonic_ns() + pts_to_go_ns;
        // De-judder: `raw_present_ns` carries the audio master clock's
        // frame-to-frame wobble (it's a quantized per-callback staircase, wall-
        // interpolated), enough to land a frame on the wrong VSync = visible
        // judder, worst right after resume when the clock anchor + output
        // latency lurch. The ideal present time advances by exactly the media
        // delta from the previous frame; snap to it, but only within
        // ±PRESENT_SMOOTH_NS of raw so a seek / LATE-drain / resume jump (raw
        // moves further than the window) is followed at once and the cadence
        // re-bases on the next frame. The raw formula above is untouched, so the
        // sleep gate / LATE drain / clock all behave exactly as before.
        let present_ns = match last_present {
            Some((last_ns, last_pts_us)) => {
                let ideal_ns = last_ns + (pts_us_rel - last_pts_us) * 1_000;
                ideal_ns.clamp(
                    raw_present_ns - PRESENT_SMOOTH_NS,
                    raw_present_ns + PRESENT_SMOOTH_NS,
                )
            }
            None => raw_present_ns,
        };
        // HARD CAP on how far in the future a frame may be stamped. The media
        // clock can step BACKWARD between the pacing sleep and this stamp (the
        // wall→audio handover at startup, underrun/seek re-anchors), which used
        // to produce eglPresentationTime stamps seconds ahead — SurfaceFlinger
        // held those frames, the direct codec's 8-buffer output pool drained,
        // and playback wedged into a ~1 fps convoy ("no present fence for
        // frame N"). Clamping keeps the buffer flowing through SF regardless of
        // any clock discontinuity; pacing still comes from the sleep above.
        let present_ns = present_ns.min(clock_monotonic_ns() + MAX_PRESENT_LEAD_NS);
        last_present = Some((present_ns, pts_us_rel));
        frame.desired_present_ns = present_ns;

        // DIAG (#23): first few frames' pacing — tells us whether the direct
        // pipeline renders promptly (releasing codec buffers) or schedules
        // present far in the future (buffers stay captive → dequeue_input stall).
        if frame_idx < 3 {
            log::debug!(
                "[vsync] frame #{} pts_ms={} elapsed_ms={} pts_rel_ms={} pts_to_go_ms={} origin={}",
                frame_idx, pts_ms, elapsed_us / 1000, pts_us_rel / 1000,
                pts_to_go_ns / 1_000_000, origin_us / 1000
            );
        }

        let render_start = start_time
            .elapsed()
            .saturating_sub(pause_skew)
            .as_millis() as u64;
        // Per-frame A/V drift, min-accumulated over the 1Hz stats window.
        // Sampling the chunky audio-played counter gives a noisy sawtooth;
        // the per-window MIN is the least-stale read = the true video-ahead
        // offset (see the stats emit below).
        // Baseline only once THIS pipeline's audio is actually running:
        // before that the wall clock advances while the device (and the
        // audio-mastered picture) hold, and the whole start delay would read
        // as a permanent "drift" even though lip-sync is exact.
        if let Some(played) = audio_sink.played_ms() {
            match drift_baseline {
                None if audio_sink.played_since_flush_ms().unwrap_or(1) == 0 => {}
                None => drift_baseline = Some((render_start, played)),
                Some((e0, p0)) => {
                    let d = render_start.saturating_sub(e0) as i64
                        - played.saturating_sub(p0) as i64;
                    drift_min_window = drift_min_window.min(d);
                }
            }
        }
        let interval_ms = if last_render_elapsed > 0 { render_start - last_render_elapsed } else { 0 };
        let delta_pts = pts_ms.saturating_sub(last_pts_ms);
        // Conformance cadence gauges: max render gap = freeze/swap-hole depth,
        // sub-5ms renders = catch-up bursts (a few per LATE drain are normal,
        // hundreds mean flicker/pacing regressions).
        if last_render_elapsed > 0 {
            stats.render_gap_max_ms.fetch_max(interval_ms, Ordering::Relaxed);
            if interval_ms < 5 {
                stats.render_burst_frames.fetch_add(1, Ordering::Relaxed);
            }
            // Judder: the wall interval should track the media delta. Guard to
            // steady-state (sane consecutive deltas) so seeks, splices and
            // segment boundaries don't count as stutter.
            if delta_pts > 0 && delta_pts < 100 {
                let jitter = interval_ms as i64 - delta_pts as i64;
                if jitter.abs() > 10 {
                    stats.judder_frames.fetch_add(1, Ordering::Relaxed);
                }
                let bucket = if interval_ms < 25 {
                    &stats.int_lt25
                } else if interval_ms <= 41 {
                    &stats.int_25_41
                } else if interval_ms <= 58 {
                    &stats.int_42_58
                } else {
                    &stats.int_gt58
                };
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }

        // DIAG: per-frame pacing. interval_ms = wall ms between renders (~41 at
        // 24fps; jitter here = judder), elapsed = master clock, pts_to_go = the
        // scheduled lead. Reveals clock-jitter judder that doesn't trip LATE.
        if frame_idx.is_multiple_of(60) {
            log::info!(
                "[vsync] f#{} pts={}ms elapsed={}ms interval={}ms dpts={}ms pts_to_go={}ms",
                frame_idx, pts_ms, elapsed_us / 1000, interval_ms, delta_pts,
                pts_to_go_ns / 1_000_000
            );
        }

        last_pts_ms = pts_ms;
        last_render_elapsed = render_start;
        frame_idx += 1;

        position_ms.store(pts_ms, Ordering::Relaxed);
        // Subtitle overlay needs media-timeline-relative PTS (0-based
        // from start of content) so it can pick the right cue from the
        // VTT timestamps. Use `pts_ms` here, NOT frame.pts_us — the
        // latter still carries the DASH BMDT offset.
        renderer.set_subtitle_pts(pts_ms as i64);

        // Emit `Playing` once on the first rendered frame after a sync
        // loop (re)starts (Buffering→Playing transition) — but NOT for a
        // preview frame painted while paused: that must keep the consumer's
        // state Paused. Leaving emitted_playing false means the first frame
        // after resume makes the transition.
        if !emitted_playing && !paused.load(Ordering::Relaxed) {
            let _ = events.send(PlayerEvent::Playing);
            emitted_playing = true;
        }

        let frame_w = frame.width;
        let frame_h = frame.height;
        renderer.render_frame(frame).await;
        stats.diag_video_ren.fetch_add(1, Ordering::Relaxed);
        // One frame is now on screen — from here the pause gate parks.
        presented_frame = true;

        let render_done = start_time
            .elapsed()
            .saturating_sub(pause_skew)
            .as_millis() as u64;
        let render_ms = render_done - render_start;

        stats.video_frames_decoded.fetch_add(1, Ordering::Relaxed);

        // Rate-limited Position emission (≤ 4 Hz). The buffer head is the
        // min of video + audio decode high-water-marks — whichever runs
        // out first stalls playback, so that's what "safe to play" really
        // means. Clamped at zero so an out-of-order frame can't briefly
        // drive the gauge negative.
        if last_position_emit.elapsed() >= Duration::from_millis(250) {
            let video_decoded = stats.last_decoded_pts_ms.load(Ordering::Relaxed);
            let audio_decoded = stats.audio_last_decoded_pts_ms.load(Ordering::Relaxed);
            let bottleneck = video_decoded.min(audio_decoded);
            let ahead_ms = (bottleneck - pts_ms as i64).max(0);
            let _ = events.send(PlayerEvent::Position {
                position: Duration::from_millis(pts_ms),
                duration: media_duration,
                buffered_ahead_secs: ahead_ms as f32 / 1000.0,
                bandwidth_bps: stats.bandwidth_bps_ewma.load(Ordering::Relaxed),
            });
            last_position_emit = Instant::now();
        }

        // Rate-limited Stats emission (≤ 1 Hz). net_stall_ms is swap-reset
        // so consumers see "ms blocked in the last second", not cumulative.
        if last_stats_emit.elapsed() >= Duration::from_secs(1) {
            // A/V drift bookkeeping: both clocks freeze together across
            // pauses and starvation windows (cpal consumes nothing while
            // paused; pause_skew stops the video timeline), so the
            // difference of ADVANCES since the baseline isolates pure
            // clock-rate mismatch plus any sync bug.
            // A/V drift = the MIN video-ahead offset over the window. The
            // 1Hz-sampled raw drift beats against the chunky audio-played
            // counter (a ±100ms sampling sawtooth); the per-window minimum
            // is the least-stale read and tracks the true offset. Both
            // clocks freeze together across pause/starvation, so this
            // isolates genuine desync. Warn past 150ms.
            let mut drift_out: Option<i64> = None;
            if drift_min_window != i64::MAX {
                stats.av_drift_ms.store(drift_min_window, Ordering::Relaxed);
                stats.av_drift_max_ms.fetch_max(drift_min_window.abs(), Ordering::Relaxed);
                drift_out = Some(drift_min_window);
                if drift_min_window.abs() > 150
                    && last_drift_warn
                        .map(|t| t.elapsed() > Duration::from_secs(20))
                        .unwrap_or(true)
                {
                    log::warn!(
                        "[vsync] A/V drift {}ms (+ = audio behind video)",
                        drift_min_window
                    );
                    last_drift_warn = Some(Instant::now());
                }
            }
            drift_min_window = i64::MAX;
            let decoder_name = stats.decoder_name.lock().unwrap().clone();
            // Hoist the counters out of the event literal: net_stall_ms is
            // swap-reset on read, and the HEALTH line below reuses all of them.
            let decoded_total = stats.video_frames_decoded.load(Ordering::Relaxed);
            let dropped_total = stats.video_frames_dropped.load(Ordering::Relaxed);
            let net_stall = stats.net_stall_ms.swap(0, Ordering::Relaxed);
            // Per-side buffer depth relative to the frame being rendered
            // (absolute media pts on both sides). Negative = decoder behind
            // the picture (imminent starvation).
            let v_ahead =
                stats.last_decoded_pts_ms.load(Ordering::Relaxed) - raw_pts_ms as i64;
            let a_ahead =
                stats.audio_last_decoded_pts_ms.load(Ordering::Relaxed) - raw_pts_ms as i64;
            let _ = events.send(PlayerEvent::Stats {
                video_frames_decoded: decoded_total,
                video_frames_dropped: dropped_total,
                video_late_frames: stats.video_late_frames.load(Ordering::Relaxed),
                audio_underruns: stats.audio_underruns.load(Ordering::Relaxed),
                net_stall_ms: net_stall,
                decoder_name,
                current_resolution: Some((frame_w, frame_h)),
                audio_peak_db: audio_sink.last_peak_db(),
                av_drift_ms: drift_out,
                video_buffer_ahead_ms: v_ahead,
                audio_buffer_ahead_ms: a_ahead,
                video_segment: stats.video_segment_id.load(Ordering::Relaxed),
                stall_events: stats.stall_events.load(Ordering::Relaxed),
                pipeline_retries: stats.pipeline_retries.load(Ordering::Relaxed),
                render_gap_max_ms: stats.render_gap_max_ms.load(Ordering::Relaxed),
                judder_frames: stats.judder_frames.load(Ordering::Relaxed),
                interval_hist: [
                    stats.int_lt25.load(Ordering::Relaxed),
                    stats.int_25_41.load(Ordering::Relaxed),
                    stats.int_42_58.load(Ordering::Relaxed),
                    stats.int_gt58.load(Ordering::Relaxed),
                ],
                bandwidth_bps: stats.bandwidth_bps_ewma.load(Ordering::Relaxed),
            });

            // HEALTH heartbeat: a single warn line per second WHEN something
            // is off — frames dropped this second, A/V drift past ~2 frames,
            // or the decoder blocked on the network. Deltas (not cumulative)
            // so a glance at logcat shows the pattern: e.g. `drops=+2/s` every
            // ~2s points at the segment-boundary LATE drain. Silent when
            // healthy so it doesn't drown the log.
            let dropped_delta = dropped_total.saturating_sub(last_health_dropped);
            let decoded_delta = decoded_total.saturating_sub(last_health_decoded);
            last_health_dropped = dropped_total;
            last_health_decoded = decoded_total;
            let drift = drift_out.unwrap_or(0);
            if dropped_delta > 0 || drift.abs() > 80 || net_stall > 0 {
                log::warn!(
                    "[vsync gen {}] HEALTH drops=+{}/s decoded={}/s drift={}ms net_stall={}ms res={}x{}",
                    gen, dropped_delta, decoded_delta, drift, net_stall, frame_w, frame_h
                );
            }
            last_stats_emit = Instant::now();
        }

        log::trace!("[vsync] #{} pts={}ms wall={}ms render={}ms interval={}ms Δpts={}ms",
            frame_idx - 1, pts_ms, render_start, render_ms, interval_ms, delta_pts);
    }
}

pub(super) async fn audio_sync_loop<A: AudioSink>(
    mut input_rx: mpsc::Receiver<DecodedAudioFrame>,
    sink: Arc<A>,
    // 0-based seek target (ms) and the content origin (ms) — the first
    // audible sample must be the one at ABSOLUTE media time
    // `origin + target`. Decoded audio pts are absolute (composition
    // timestamps), so the trim compares on that axis; comparing them against
    // the 0-based target (the old code) padded `origin` ms of silence in
    // front of every start on content with a non-zero timeline origin —
    // audio permanently late by the origin.
    target_pts_ms: i64,
    origin_ms: i64,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    stats: Arc<StatsState>,
    events: Arc<broadcast::Sender<PlayerEvent>>,
    paused: Arc<AtomicBool>,
) {
    // Keep the PCM handed to the sink continuous on the media axis (see
    // `crate::av_sync::AudioAligner`):
    //  * start: DASH audio and video segments rarely share boundaries — the
    //    audio segment containing the target typically starts up to ~1 s
    //    before it. The leading samples are trimmed (or silence padded when
    //    audio starts AFTER the target) so the first sample the sink plays is
    //    media time `origin + target` — the point the clock starts counting
    //    from. Without it every later sample is late by that gap.
    //  * steady state: a gap (a swallowed corrupt AU, an edit-list
    //    discontinuity) is padded and an overlap trimmed, so a dropped 32 ms
    //    frame cannot shift all subsequent audio 32 ms early for good.
    let sample_rate = sink.sample_rate();
    // Interleaved channel count of the sink's PCM (the decoders mix to it).
    let channels = sink.channels().max(1) as usize;
    let mut aligner = crate::av_sync::AudioAligner::new(sample_rate, origin_ms + target_pts_ms);
    let mut gap_events = 0u32;
    let mut starving = false;
    // Output starts once PREROLL_MS of PCM is queued (or the source goes
    // quiet first) — never against an empty queue.
    let mut preroll = crate::av_sync::PrerollGate::new(sample_rate, sink.is_passthrough());
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        // Mirror video_sync_loop's recv-with-timeout: if the audio
        // pipeline stops producing frames (decoder hang, network
        // outage on the audio adaptation set, …), flip into
        // Buffering state so the consumer sees "stalled" and the
        // audio sink stops emitting whatever's left in its cpal
        // queue. Combined with `stats.audio_starving` being checked
        // in video_sync_loop, this also stalls video on this frame
        // — preventing the asymmetric "audio silent but video keeps
        // playing" state.
        let frame = loop {
            let starvation_wait = crate::rt::sleep(Duration::from_millis(300));
            tokio::pin!(starvation_wait);
            tokio::select! {
                maybe = input_rx.recv() => {
                    let f = match maybe {
                        Some(f) => f,
                        None => return,
                    };
                    stats.diag_audio_sync.fetch_add(1, Ordering::Relaxed);
                    if starving {
                        starving = false;
                        if let StarvationTransition::ExitedBuffering =
                            report_starvation(&stats, StallSide::Audio, false)
                        {
                            let _ = events.send(PlayerEvent::Playing);
                            if !paused.load(Ordering::Relaxed) {
                                sink.set_paused(false);
                            }
                        }
                    }
                    break f;
                }
                _ = &mut starvation_wait => {
                    if preroll.force_open() && !paused.load(Ordering::Relaxed) {
                        // A trickle that never filled the pre-roll: play
                        // what there is rather than hold the start.
                        sink.set_paused(false);
                    }
                    if !starving {
                        starving = true;
                        log::warn!("[async] no audio frame for 300ms — entering buffering");
                        if let StarvationTransition::EnteredBuffering =
                            report_starvation(&stats, StallSide::Audio, true)
                        {
                            let _ = events.send(PlayerEvent::Buffering {
                                reason: BufferingReason::Stall,
                            });
                            sink.set_paused(true);
                        }
                    }
                    continue;
                }
                _ = stop.notified() => return,
            }
        };
        if frame.samples.is_empty() {
            continue;
        }
        // Interleaved: samples.len() / channels = per-channel frames.
        let frames_per_chan = frame.samples.len() / channels;
        let was_aligned = aligner.is_aligned();
        let (skip_frames, pad_frames) = match aligner.plan(frame.pts_ms, frames_per_chan) {
            crate::av_sync::AlignAction::Drop => continue,
            crate::av_sync::AlignAction::Emit { skip_frames, pad_frames } => {
                (skip_frames, pad_frames)
            }
        };
        if !was_aligned {
            log::info!(
                "[async] aligned first audio: pts={}ms target={}ms skip={}ms pad={}ms",
                frame.pts_ms,
                origin_ms + target_pts_ms,
                skip_frames as u64 * 1000 / sample_rate.max(1) as u64,
                pad_frames as u64 * 1000 / sample_rate.max(1) as u64
            );
        } else if skip_frames > 0 || pad_frames > 0 {
            gap_events += 1;
            // Rate-limit: the first few, then every 50th.
            if gap_events <= 5 || gap_events.is_multiple_of(50) {
                log::warn!(
                    "[async] audio discontinuity #{} at pts={}ms: trimmed {}ms / padded {}ms to stay contiguous",
                    gap_events,
                    frame.pts_ms,
                    skip_frames as u64 * 1000 / sample_rate.max(1) as u64,
                    pad_frames as u64 * 1000 / sample_rate.max(1) as u64
                );
            }
        }
        let skip_idx = (skip_frames * channels).min(frame.samples.len());
        let trimmed: std::borrow::Cow<'_, [f32]> = if pad_frames == 0 {
            std::borrow::Cow::Borrowed(&frame.samples[skip_idx..])
        } else {
            let mut buf = Vec::with_capacity(pad_frames * channels + frame.samples.len() - skip_idx);
            buf.resize(pad_frames * channels, 0.0_f32);
            buf.extend_from_slice(&frame.samples[skip_idx..]);
            std::borrow::Cow::Owned(buf)
        };
        if trimmed.is_empty() {
            continue;
        }
        tokio::select! {
            _ = sink.put_samples(&trimmed) => {
                stats.diag_audio_sunk.fetch_add(1, Ordering::Relaxed);
            }
            _ = stop.notified() => return,
        }
        if preroll.queued(trimmed.len() / channels) {
            log::debug!(
                "[async] audio pre-roll of {} ms queued — output starts",
                crate::av_sync::PrerollGate::PREROLL_MS
            );
            if !paused.load(Ordering::Relaxed) {
                sink.set_paused(false);
            }
        }
    }
}


pub(super) async fn av_sync_handler<V: VideoSink, A: AudioSink>(
    // DIAG: pipeline generation id (see video_sync_loop).
    gen: u64,
    seek_offset: Duration,
    // Content origin (first segment's absolute presentation time): both sync
    // loops work on the 0-based axis `pts − origin`.
    origin: Duration,
    // Absolute pts_us below which video frames are discarded (frame-accurate
    // seek: decode from the segment keyframe, render from the target). 0 = none.
    video_discard_below_us: i64,
    video_ready: Arc<Notify>,
    video_rx: mpsc::Receiver<DecodedVideoFrame>,
    video_sink: Arc<V>,
    position_ms: Arc<AtomicU64>,
    audio_ready: Arc<Notify>,
    audio_rx: mpsc::Receiver<DecodedAudioFrame>,
    audio_sink: Arc<A>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    events: Arc<broadcast::Sender<PlayerEvent>>,
    media_duration: Duration,
    paused: Arc<AtomicBool>,
    pause_notify: Arc<Notify>,
    stats: Arc<StatsState>,
    pipeline_live: Arc<AtomicBool>,
    rebuild_reason: Arc<StdMutex<BufferingReason>>,
) {
    // Opening Buffering, tagged with WHY this pipeline is being built, so a
    // consumer can show a spinner for a cold start or a deliberate switch and
    // stay quiet for churn it did not ask for. Consumed here: the next build
    // is an ordinary start again unless its trigger says otherwise.
    let reason = std::mem::replace(
        &mut *rebuild_reason.lock().unwrap(),
        BufferingReason::Initial,
    );
    log::info!("[pipeline gen {}] opening buffering, reason={:?}", gen, reason);
    let _ = events.send(PlayerEvent::Buffering { reason });

    // Pipeline watchdog: a plain OS thread (immune to any async-runtime state)
    // that logs the per-stage progress counters every 3 s. When playback stalls,
    // the first counter that stops advancing names the wedged stage directly —
    // no more guessing which of download/decode/sync/sink/render died.
    {
        let stats = Arc::clone(&stats);
        let stop_flag = Arc::clone(&stop_flag);
        let position = Arc::clone(&position_ms);
        std::thread::Builder::new()
            .name("bz-watchdog".into())
            .spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(3));
                    log::info!(
                        "[watchdog gen {}] a_seg={} a_dec={} a_sync={} a_sunk={} | v_seg={} v_dec={} v_ren={} | pos={}ms",
                        gen,
                        stats.diag_audio_seg.load(Ordering::Relaxed),
                        stats.diag_audio_dec.load(Ordering::Relaxed),
                        stats.diag_audio_sync.load(Ordering::Relaxed),
                        stats.diag_audio_sunk.load(Ordering::Relaxed),
                        stats.diag_video_seg.load(Ordering::Relaxed),
                        stats.video_frames_decoded.load(Ordering::Relaxed),
                        stats.diag_video_ren.load(Ordering::Relaxed),
                        position.load(Ordering::Relaxed),
                    );
                }
            })
            .ok();
    }

    // Wait for both decoders to produce their first output before setting
    // start_time. This ensures A/V sync is established from a common wall-clock
    // origin. The audio channel is sized large enough (256 frames) that
    // the audio decoder task won't block even if video download is slow.
    // Video MUST produce its first frame: the sync loop needs it to anchor the
    // clock base, and in direct mode the sync loop is also what releases the
    // codec's output buffers back to it.
    tokio::select! {
        _ = video_ready.notified() => {}
        _ = stop.notified() => {
            stop.notify_waiters();
            return;
        }
    }
    log::debug!("[av_sync] video_ready passed (seek_offset={}ms)", seek_offset.as_millis());
    // First frame is out: the pipeline is live. Lets the ABR tick resume —
    // any switch from here rebuilds a pipeline that's actually producing,
    // not a half-started one.
    pipeline_live.store(true, Ordering::Relaxed);
    // Audio readiness is BOUNDED, not a hard gate: if it gated the sync loop's
    // start, a slow audio decoder after a seek/start-at-offset would keep the
    // loop from running — and in direct mode that means the video codec's
    // output buffers never get released, so it backpressure-stalls on
    // dequeue_input forever. A/V sync self-aligns once audio starts flowing.
    tokio::select! {
        _ = audio_ready.notified() => {}
        _ = crate::rt::sleep(Duration::from_secs(3)) => {
            log::warn!("[vsync] audio not ready after 3s — starting playback without waiting (guards the direct-mode video codec against a backpressure stall)");
        }
        _ = stop.notified() => {
            stop.notify_waiters();
            return;
        }
    }
    // The PCM output stays paused until `audio_sync_loop` has queued a short
    // pre-roll (see `PrerollGate`): unpausing here, on the first decoded
    // frame, ran the device against a near-empty queue and crackled at every
    // start and seek. Bitstream passthrough primes its own track and is
    // unpaused now, as before.
    if !paused.load(Ordering::Relaxed) && audio_sink.is_passthrough() {
        audio_sink.set_paused(false);
    }
    // Universal start alignment (no per-device constants): anchor the video
    // clock to the instant audio ACTUALLY starts flowing — i.e. when the
    // output device's played-sample position first advances — not to the
    // earlier "first frame decoded" instant. Otherwise video starts at the
    // wall clock while the audio output buffer is still filling, so video
    // leads audio at every (re)start; the LATE-drain then yanks video back
    // with a visible skip. Because seek() and track/ABR switches rebuild
    // the pipeline through here, that transient recurred on every switch —
    // the "audio delayed after switching" the user reported. Bounded so a
    // sink that never reports a position (mocks) or genuine leading silence
    // still starts. Skipped while paused.
    // A bitstream passthrough AudioTrack only starts its head once the feed has
    // primed it past the start threshold (~2.5 s on the HDMI AVR) — far longer
    // than a cpal PCM head (which advances on the first callback). With the old
    // 500 ms cap the gate timed out, start_time anchored to the wall, and video
    // paced on the wall fallback through the prime, then re-synced to the head —
    // a brief frame slowdown at every (re)start. Waiting for the head to begin
    // (it breaks as soon as played_ms ticks, so the wait is ~the prime, not the
    // full cap) anchors video to real audio start so it begins in step — no
    // wall-paced slowdown. cpal keeps the 500 ms cap.
    //
    // PCM path: no gate at all any more. The PCM sink only receives samples
    // from `audio_sync_loop`, which is spawned BELOW — so waiting here for its
    // position to move could only ever time out (500 ms added to every start
    // and seek). It is also unnecessary: the master clock is
    // `played_since_flush_ms`, which reads a frozen `seek_offset` until the
    // new audio is really being presented, so video simply holds its first
    // frame until then instead of racing ahead on the wall clock.
    //
    // Passthrough keeps the gate: its feed writes to the bitstream track
    // directly (not through the loop below) and the sink reports NO clock
    // until the head moves — i.e. video would run on the wall fallback
    // through the ~1–2 s prime and then snap back to the head.
    let gate = Instant::now();
    if !paused.load(Ordering::Relaxed) && audio_sink.is_passthrough() {
        let gate_cap = Duration::from_millis(4000);
        // `None` = first AU not written yet, `Some(0)` = written, head not
        // moving yet — wait through both (bounded).
        while matches!(audio_sink.played_since_flush_ms(), None | Some(0)) {
            if gate.elapsed() > gate_cap {
                log::debug!("[vsync] audio-start gate timed out ({}ms); anchoring anyway", gate_cap.as_millis());
                break;
            }
            tokio::select! {
                _ = crate::rt::sleep(Duration::from_millis(4)) => {}
                _ = stop.notified() => {
                    stop.notify_waiters();
                    return;
                }
            }
        }
    }
    let now = Instant::now();
    let start_time = Arc::new(now.checked_sub(seek_offset).unwrap_or(now));
    log::info!(
        "[av_sync gen {}] spawning sync loops: target={}ms origin={}ms audio_since_flush={:?}ms (audio start waited {}ms)",
        gen,
        seek_offset.as_millis(),
        origin.as_millis(),
        audio_sink.played_since_flush_ms(),
        gate.elapsed().as_millis()
    );
    let stats_audio = Arc::clone(&stats);
    let events_audio = Arc::clone(&events);
    let paused_audio = Arc::clone(&paused);
    let end_position = Arc::clone(&position_ms);
    let (_, _) = tokio::join!(
        crate::rt::spawn(video_sync_loop(
            gen,
            start_time.clone(),
            seek_offset,
            origin.as_micros() as i64,
            video_discard_below_us,
            video_rx,
            video_sink,
            audio_sink.clone(),
            position_ms,
            stop.clone(),
            stop_flag.clone(),
            events.clone(),
            media_duration,
            paused,
            pause_notify,
            stats,
        )),
        crate::rt::spawn(audio_sync_loop(
            audio_rx,
            audio_sink,
            seek_offset.as_millis() as i64,
            origin.as_millis() as i64,
            stop.clone(),
            stop_flag.clone(),
            stats_audio,
            events_audio,
            paused_audio,
        )),
    );
    // Both loops returning naturally (channels closed by decoder EOF) means
    // we hit end-of-stream. If we were stopped explicitly, the consumer is
    // tearing down and doesn't care about EndOfStream.
    //
    // Positional gate: "both loops ended" is also what a silently dead
    // pipeline looks like (e.g. an outage path that slipped past the
    // supervisor). A genuine end plays out to the media duration, so
    // anything that stops well short of it is reported as a network error
    // instead of a fake EndOfStream.
    if !stop_flag.load(Ordering::Relaxed) {
        const PREMATURE_END_GUARD_MS: u64 = 10_000;
        let pos = end_position.load(Ordering::Relaxed);
        let dur = media_duration.as_millis() as u64;
        if dur == 0 || pos + PREMATURE_END_GUARD_MS >= dur {
            let _ = events.send(PlayerEvent::EndOfStream);
        } else {
            log::error!(
                "[av_sync] stream ended prematurely at {}ms of {}ms — reporting as network error",
                pos, dur
            );
            let _ = events.send(PlayerEvent::Error {
                kind: PlayerErrorKind::Network,
                detail: format!(
                    "stream ended prematurely at {}s of {}s (network outage?)",
                    pos / 1000,
                    dur / 1000
                ),
            });
        }
    }
    stop.notify_waiters();
}


