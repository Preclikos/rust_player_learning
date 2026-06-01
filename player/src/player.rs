mod abr;
mod crypto;
mod decoders;
mod events;
mod ffmpeg_log;
mod manifest;
mod net;
mod parsers;
mod renderers;
mod tracks;
mod utils;

// Public re-exports so downstream consumers (BlackZone Console etc.) can
// implement RequestInterceptor / LicenseResolver against the player's
// canonical types — see PLAYER_INTEGRATION.md.
pub use abr::AbrStrategy;
pub use events::{
    BufferingReason, Fps, PlayerErrorKind, PlayerEvent, TrackInfo, TrackKind,
};
pub use ffmpeg_log::{set_log_level, LogLevel};
pub use net::{
    BoxError, HttpClient, LicenseResolver, NoopInterceptor, PreparedRequest,
    RequestInterceptor, RequestKind, RetryPolicy,
};
/// Physical (device-pixel) size of the render target. A tiny owned type so the
/// player crate doesn't depend on winit; mirrors the subset of
/// `winit::dpi::PhysicalSize` the player uses (`new`, `.width`, `.height`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PhysicalSize<P = u32> {
    pub width: P,
    pub height: P,
}

impl<P> PhysicalSize<P> {
    pub const fn new(width: P, height: P) -> Self {
        Self { width, height }
    }
}

// Re-exported so hosts can name the handle types they pass to
// `Player::new_from_raw_handle` without pinning their own raw-window-handle.
pub use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use crypto::{
    kid_short, parse_aac_config, parse_hvcc_nalus, parse_senc, parse_tenc, ClearKeyDecryptor,
    Decryptor, TrackCrypto,
};
use decoders::{
    AudioCodec, AudioDecoder, AudioDecoderParams, DecodedAudioFrame, DecodedVideoFrame,
    HwVideoDecoder, VideoCodec, VideoDecoderParams,
};
use parsers::mp4::aac_sampling_frequency_index_to_u32;
use pollster::FutureExt;
use re_mp4::Mp4;
use renderers::audio::AudioRenderer;
use renderers::video::VideoRenderer;
use renderers::{AudioSink, VideoSink};

use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
#[cfg(target_os = "android")]
use libc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::{broadcast, Notify, RwLock};
use tokio::time::Instant;
use tokio::{join, sync::mpsc::Sender};
use tracks::audio::{AudioAdaptation, AudioRepresentation};
use tracks::{
    segment::Segment,
    video::{VideoAdaptation, VideoRepresenation},
    Tracks,
};
use url::Url;

use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver};
use tokio::task::{self, JoinHandle};

use manifest::Manifest;

/// Default target buffer in seconds — how far ahead the download path is
/// allowed to run from the renderer. Higher = more resilience against
/// network jitter, lower = less RAM (each queued segment holds ~1-4 MB).
/// Configurable per Player via `set_buffer_target_secs`.
const DEFAULT_BUFFER_TARGET_SECS: u32 = 8;

/// Assumed average segment duration when converting buffer-target-seconds
/// into segments-in-flight capacity. DASH segments are typically 2-4 s;
/// 2 is a conservative floor that biases the cap upward.
const ASSUMED_SEGMENT_SECS: u32 = 2;

/// Result of a starvation-state update — exposed by the helper so the
/// caller can react to combined-state transitions (the moment EITHER
/// side starts stalling, or the moment BOTH have recovered).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StarvationTransition {
    /// Combined state didn't change — either still healthy or still
    /// buffering. No action required.
    Unchanged,
    /// Healthy → buffering. Pause audio sink, emit
    /// `PlayerEvent::Buffering { Stall }`.
    EnteredBuffering,
    /// Buffering → healthy. Unpause audio sink (unless user-paused),
    /// emit `PlayerEvent::Playing`.
    ExitedBuffering,
}

/// Which sync-loop is reporting the starvation transition.
#[derive(Clone, Copy, Debug)]
enum StallSide {
    Video,
    Audio,
}

/// Shared counters surfaced via `PlayerEvent::Stats`. Created once in
/// `Player::new` and cloned into every play() pipeline so the stats keep
/// accumulating across seek / track-switch boundaries.
#[derive(Default)]
struct StatsState {
    video_frames_decoded: AtomicU64,
    video_frames_dropped: AtomicU64,
    audio_underruns: AtomicU64,
    /// Wall-clock ms the download path was blocked waiting on network in
    /// the trailing second.
    net_stall_ms: AtomicU64,
    /// EWMA of segment download throughput in bits-per-second, surfaced via
    /// `Position.bandwidth_bps` and consumed by the ABR engine.
    bandwidth_bps_ewma: AtomicU64,
    /// Highest media-time PTS (in ms) currently available locally for
    /// **video** — bumped both when `download_task` finishes a segment
    /// (segment.end_time) and when `video_decoder_task` produces a
    /// frame. Including the downloaded-but-not-yet-decoded segments
    /// matches the documented semantics of `buffered_ahead_secs`:
    /// "amount of media that's safe to play through if the network
    /// drops *right now*". Decode is local CPU work that completes
    /// without network, so a downloaded segment is just as safe as a
    /// decoded one. Without the download-side update the gauge
    /// effectively topped out at `frame_sender` capacity (~2.7 s at
    /// 24 fps), regardless of how large the consumer set
    /// `buffer_target_secs`.
    last_decoded_pts_ms: std::sync::atomic::AtomicI64,
    /// Same as `last_decoded_pts_ms` but for the audio pipeline.
    /// Read together with the video field to compute
    /// `Position.buffered_ahead_secs = min(video, audio)` — whichever
    /// runs out first stalls playback. Audio segments are smaller and
    /// decode faster than video, so audio is normally well ahead, but
    /// a slow audio download or a very small initial buffer can flip
    /// that on startup.
    audio_last_decoded_pts_ms: std::sync::atomic::AtomicI64,
    /// Name of the currently-active video decoder backend
    /// (`"D3D11VA (FFmpeg)"`, `"MediaCodec"`, …). Plumbed in at play().
    decoder_name: StdMutex<String>,

    /// Set by `video_sync_loop` when its decoder pipeline hasn't
    /// produced a frame for >300 ms (download stall, decode hang, …).
    /// Read by `audio_sync_loop` so an audio-only-healthy side parks
    /// instead of draining its cpal queue and showing the user video
    /// frozen with audio still playing.
    video_starving: AtomicBool,
    /// Symmetric to `video_starving` — set by `audio_sync_loop` when
    /// IT hasn't received a frame for >300 ms. Video parks on its
    /// current frame instead of marching forward over silence.
    audio_starving: AtomicBool,
}

pub struct Player<V: VideoSink = VideoRenderer, A: AudioSink = AudioRenderer> {
    base_url: Option<String>,
    manifest: Option<Manifest>,
    tracks: Arc<StdMutex<Option<Tracks>>>,

    /// Shared HTTP transport used by every manifest / segment / license
    /// fetch. Owns the single `reqwest::Client` connection pool and
    /// applies the configured `RequestInterceptor` + `RetryPolicy`.
    http: Arc<HttpClient>,

    /// Broadcast(64) sender for `PlayerEvent`s. Cloning a `Player` shares
    /// the same channel, so every subscriber sees every event regardless
    /// of which Player handle emitted it.
    events: Arc<broadcast::Sender<PlayerEvent>>,

    /// Pause flag — checked in both video_sync_loop and audio_sync_loop
    /// inner ticks. Toggled by `pause()` / `resume()`. While set, both
    /// loops park on `pause_notify` and PTS does not advance.
    paused: Arc<AtomicBool>,
    pause_notify: Arc<Notify>,

    video_adaptation: Arc<StdMutex<Option<VideoAdaptation>>>,
    video_representation: Arc<StdMutex<Option<VideoRepresenation>>>,

    audio_adaptation: Arc<StdMutex<Option<AudioAdaptation>>>,
    audio_representation: Arc<StdMutex<Option<AudioRepresentation>>>,

    start_time: Arc<Instant>,
    video_ready: Arc<Notify>,
    audio_ready: Arc<Notify>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,

    seek_target: Arc<RwLock<Option<Duration>>>,
    position_ms: Arc<AtomicU64>,

    /// ClearKey decryptor — single shared instance so cached keys and
    /// the attached `LicenseResolver` survive across `play()` / `seek()`
    /// cycles. Lazily created on first `set_clearkey` /
    /// `set_license_resolver` call.
    decryptor: Arc<StdMutex<Option<Arc<ClearKeyDecryptor>>>>,

    /// Counters + decoder-name surface for `PlayerEvent::Stats` and the
    /// ABR engine. Single instance shared by every play() pipeline.
    stats: Arc<StatsState>,

    /// ABR policy. `Manual` by default. Mutated by `set_abr_strategy`,
    /// and reset to `Manual` whenever the consumer explicitly calls
    /// `change_video_track` so a user override always sticks.
    abr_strategy: Arc<ArcSwap<AbrStrategy>>,

    /// Watch channel the running `play()` supervisor listens on for
    /// mid-flight representation swaps. Each `play()` call installs a
    /// fresh sender; sending `Some(repr)` triggers a soft swap (tear
    /// down current video pipeline, spin up a new one from the next
    /// segment after current playback PTS, audio keeps playing).
    /// `None` between play() calls — the setter is a no-op then.
    video_switch_tx: Arc<StdMutex<Option<tokio::sync::watch::Sender<Option<VideoRepresenation>>>>>,

    /// How many seconds of media the player tries to keep buffered ahead
    /// of the renderer. Affects the segments-in-flight capacity of the
    /// download → decode channel. Takes effect at the next `play()` call
    /// — the running pipeline holds whatever it was given at spawn time.
    buffer_target_secs: Arc<AtomicU32>,

    /// Currently-selected subtitle representation. `None` means
    /// subtitles disabled — text_play won't spawn. Consumer toggles via
    /// `set_subtitle_track` / `clear_subtitle_track`.
    subtitle_representation: Arc<StdMutex<Option<tracks::text::TextRepresenation>>>,

    video_renderer: Arc<V>,
    audio_renderer: Arc<A>,

    /// Handle to the Tokio runtime the player was constructed in. Control
    /// methods (`resize`, `seek`, track switches) spawn fire-and-forget tasks
    /// through this instead of the ambient `tokio::spawn`, so they're safe to
    /// call from a host thread that isn't itself inside the runtime — e.g. the
    /// iOS UIKit layout callback or the Android JNI thread. Captured via
    /// `Handle::current()` at construction (every ctor runs inside a runtime).
    rt: tokio::runtime::Handle,
}

impl<V: VideoSink, A: AudioSink> Clone for Player<V, A> {
    fn clone(&self) -> Self {
        Player {
            base_url: self.base_url.clone(),
            manifest: self.manifest.clone(),
            tracks: Arc::clone(&self.tracks),
            http: Arc::clone(&self.http),
            events: Arc::clone(&self.events),
            paused: Arc::clone(&self.paused),
            pause_notify: Arc::clone(&self.pause_notify),
            video_adaptation: Arc::clone(&self.video_adaptation),
            video_representation: Arc::clone(&self.video_representation),
            audio_adaptation: Arc::clone(&self.audio_adaptation),
            audio_representation: Arc::clone(&self.audio_representation),
            start_time: Arc::clone(&self.start_time),
            video_ready: Arc::clone(&self.video_ready),
            audio_ready: Arc::clone(&self.audio_ready),
            stop: Arc::clone(&self.stop),
            stop_flag: Arc::clone(&self.stop_flag),
            seek_target: Arc::clone(&self.seek_target),
            position_ms: Arc::clone(&self.position_ms),
            decryptor: Arc::clone(&self.decryptor),
            stats: Arc::clone(&self.stats),
            abr_strategy: Arc::clone(&self.abr_strategy),
            video_switch_tx: Arc::clone(&self.video_switch_tx),
            buffer_target_secs: Arc::clone(&self.buffer_target_secs),
            subtitle_representation: Arc::clone(&self.subtitle_representation),
            video_renderer: Arc::clone(&self.video_renderer),
            audio_renderer: Arc::clone(&self.audio_renderer),
            rt: self.rt.clone(),
        }
    }
}

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
fn report_starvation(
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
        StarvationTransition::EnteredBuffering
    } else if was_buffering && !is_buffering {
        StarvationTransition::ExitedBuffering
    } else {
        StarvationTransition::Unchanged
    }
}

// Returns current CLOCK_MONOTONIC time in nanoseconds.
// Used to compute absolute presentation timestamps for eglPresentationTimeANDROID.
#[cfg(target_os = "android")]
fn clock_monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}
#[cfg(not(target_os = "android"))]
fn clock_monotonic_ns() -> i64 { 0 }

async fn video_sync_loop<V: VideoSink, A: AudioSink>(
    start_time: Arc<Instant>,
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
    let mut emitted_playing = false;
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
    // BMDT (base media decode time) offset: DASH content timestamps are absolute
    // (e.g. ~7979ms for segment 0) while our wall clock starts at 0. Calibrated
    // on the first frame as (raw_pts - elapsed), making frame 0 render immediately
    // and all subsequent frames at their correct relative positions.
    let mut pts_base: Option<u64> = None;
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        // Park here if pause() was called. Note: when stop fires during
        // a pause, we still want to exit cleanly.
        if paused.load(Ordering::Relaxed) {
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
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                    _ = stop.notified() => return,
                }
                pause_skew += park_started.elapsed();
                continue;
            }
            let starvation_wait = tokio::time::sleep(Duration::from_millis(300));
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
                        // Treat the time we were starving as paused
                        // time so the recovered frame's pts_ms isn't
                        // declared LATE by however many ms we sat
                        // waiting for the network.
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
        let raw_pts_ms = (frame.pts_us / 1000) as u64;
        let elapsed = start_time
            .elapsed()
            .saturating_sub(pause_skew)
            .as_millis() as u64;

        let base = *pts_base.get_or_insert(raw_pts_ms.saturating_sub(elapsed));
        let mut pts_ms = raw_pts_ms.saturating_sub(base);

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
                let pts_before_drain = pts_ms;
                let max_drain = (late_ms / 42).saturating_sub(1) as usize;
                let mut drained = 0usize;
                loop {
                    if drained >= max_drain { break; }
                    match input_rx.try_recv() {
                        Ok(newer) => {
                            frame = newer;
                            pts_ms = ((frame.pts_us / 1000) as u64).saturating_sub(base);
                            drained += 1;
                        }
                        Err(_) => break,
                    }
                }
                if drained > 0 {
                    stats
                        .video_frames_dropped
                        .fetch_add(drained as u64, Ordering::Relaxed);
                    // Drop the matching amount of audio so the speaker
                    // jumps forward in lock-step with video. Without
                    // this, audio kept playing the pre-drop samples
                    // queued in cpal — so every dropped batch shifted
                    // A/V sync by ~drained × frame_interval ms in the
                    // audio-leads direction, and the shift accumulated
                    // visibly across multiple late events.
                    let audio_skip_ms = pts_ms.saturating_sub(pts_before_drain);
                    if audio_skip_ms > 0 {
                        audio_sink.drop_ms(audio_skip_ms);
                    }
                }
            }
        } else {
            // Sleep until RENDER_BUDGET_MS before the target PTS so the GPU
            // draws the frame early. The compositor then displays it at exactly
            // pts_ms thanks to eglPresentationTimeANDROID.
            let target_wake_ms = pts_ms.saturating_sub(RENDER_BUDGET_MS);
            if target_wake_ms > elapsed {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(target_wake_ms - elapsed)) => {}
                    _ = stop.notified() => break,
                }
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
            }
        }

        // Compute the absolute CLOCK_MONOTONIC time at which this frame should
        // appear on screen. The renderer passes this to eglPresentationTimeANDROID
        // so the compositor schedules the frame at the correct VSync even if the
        // GPU finishes slightly earlier or later than expected.
        // Use microseconds (not ms) to preserve the sub-millisecond fraction —
        // 23.976fps frames are 41.708µs apart, and ms-truncation here would drift
        // across VSync boundaries every ~24 frames, causing irregular pulldown.
        let raw_pts_us = frame.pts_us;
        let base_us = base as i64 * 1_000;
        let pts_us_rel = (raw_pts_us - base_us).max(0);
        let elapsed_us = start_time
            .elapsed()
            .saturating_sub(pause_skew)
            .as_micros() as i64;
        let pts_to_go_ns = (pts_us_rel - elapsed_us).max(0) * 1_000;
        frame.desired_present_ns = clock_monotonic_ns() + pts_to_go_ns;

        let render_start = start_time
            .elapsed()
            .saturating_sub(pause_skew)
            .as_millis() as u64;
        let interval_ms = if last_render_elapsed > 0 { render_start - last_render_elapsed } else { 0 };
        let delta_pts = if pts_ms >= last_pts_ms { pts_ms - last_pts_ms } else { 0 };

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
        // loop (re)starts. Buffering→Playing transition.
        if !emitted_playing {
            let _ = events.send(PlayerEvent::Playing);
            emitted_playing = true;
        }

        let frame_w = frame.width;
        let frame_h = frame.height;
        renderer.render_frame(frame).await;

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
            let decoder_name = stats.decoder_name.lock().unwrap().clone();
            let _ = events.send(PlayerEvent::Stats {
                video_frames_decoded: stats.video_frames_decoded.load(Ordering::Relaxed),
                video_frames_dropped: stats.video_frames_dropped.load(Ordering::Relaxed),
                audio_underruns: stats.audio_underruns.load(Ordering::Relaxed),
                net_stall_ms: stats.net_stall_ms.swap(0, Ordering::Relaxed),
                decoder_name,
                current_resolution: Some((frame_w, frame_h)),
                audio_peak_db: audio_sink.last_peak_db(),
            });
            last_stats_emit = Instant::now();
        }

        log::trace!("[vsync] #{} pts={}ms wall={}ms render={}ms interval={}ms Δpts={}ms",
            frame_idx - 1, pts_ms, render_start, render_ms, interval_ms, delta_pts);
    }
}

async fn audio_sync_loop<A: AudioSink>(
    mut input_rx: mpsc::Receiver<DecodedAudioFrame>,
    sink: Arc<A>,
    target_pts_ms: i64,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    stats: Arc<StatsState>,
    events: Arc<broadcast::Sender<PlayerEvent>>,
    paused: Arc<AtomicBool>,
) {
    // Align the FIRST audible sample with `target_pts_ms` (= video's
    // snapped seek offset). DASH audio and video segments rarely share
    // boundaries: a seek that lands cleanly on a video segment boundary
    // will pick the audio segment that CONTAINS that PTS — and that
    // segment typically starts up to ~1 s before. Without correction,
    // audio_sync_loop would push those pre-target samples to cpal first,
    // delaying every subsequent sample by that gap and producing
    // perceptible audio-lag-behind-video for the rest of playback.
    //
    // We trim the leading samples here (or pad with silence if the
    // audio segment instead starts AFTER the target) so cpal's first
    // emitted sample corresponds to media-time `target_pts_ms`, the
    // same anchor video_sync_loop uses for its first rendered frame.
    let sample_rate = sink.sample_rate() as i64;
    let mut aligned = false;
    let mut starving = false;
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
            let starvation_wait = tokio::time::sleep(Duration::from_millis(300));
            tokio::pin!(starvation_wait);
            tokio::select! {
                maybe = input_rx.recv() => {
                    let f = match maybe {
                        Some(f) => f,
                        None => return,
                    };
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
        // Build the slice/owned buffer to actually hand off to cpal.
        // Stereo interleaved: samples.len() / 2 = per-channel frames.
        let trimmed: std::borrow::Cow<'_, [f32]> = if aligned {
            std::borrow::Cow::Borrowed(&frame.samples)
        } else {
            let frames_per_chan = (frame.samples.len() / 2) as i64;
            let dur_ms = if sample_rate > 0 {
                frames_per_chan * 1000 / sample_rate
            } else {
                0
            };
            let frame_end_ms = frame.pts_ms + dur_ms;
            if frame_end_ms <= target_pts_ms {
                // Whole frame lies before the target — drop it.
                continue;
            } else if frame.pts_ms < target_pts_ms {
                // Frame straddles the target — trim leading samples.
                let drop_ms = target_pts_ms - frame.pts_ms;
                let drop_chan = (drop_ms * sample_rate / 1000) as usize;
                let drop_idx = (drop_chan * 2).min(frame.samples.len());
                aligned = true;
                if drop_idx >= frame.samples.len() {
                    continue;
                }
                std::borrow::Cow::Borrowed(&frame.samples[drop_idx..])
            } else {
                // Frame starts at/after target — pad silence so the
                // first audible sample lands on target.
                let pad_ms = frame.pts_ms - target_pts_ms;
                let pad_chan = (pad_ms * sample_rate / 1000) as usize;
                aligned = true;
                if pad_chan == 0 {
                    std::borrow::Cow::Borrowed(&frame.samples)
                } else {
                    let mut buf = Vec::with_capacity(pad_chan * 2 + frame.samples.len());
                    buf.resize(pad_chan * 2, 0.0_f32);
                    buf.extend_from_slice(&frame.samples);
                    std::borrow::Cow::Owned(buf)
                }
            }
        };
        if trimmed.is_empty() {
            continue;
        }
        tokio::select! {
            _ = sink.put_samples(&trimmed) => {}
            _ = stop.notified() => return,
        }
    }
}

async fn av_sync_handler<V: VideoSink, A: AudioSink>(
    seek_offset: Duration,
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
) {
    // Emit Buffering{Initial} immediately so the consumer can show "buffering"
    // while the first segments download.
    let _ = events.send(PlayerEvent::Buffering {
        reason: BufferingReason::Initial,
    });

    // Wait for both decoders to produce their first output before setting
    // start_time. This ensures A/V sync is established from a common wall-clock
    // origin. The audio channel is sized large enough (256 frames) that
    // the audio decoder task won't block even if video download is slow.
    tokio::select! {
        _ = async { tokio::join!(video_ready.notified(), audio_ready.notified()); } => {}
        _ = stop.notified() => {
            stop.notify_waiters();
            return;
        }
    }
    let now = Instant::now();
    let start_time = Arc::new(now.checked_sub(seek_offset).unwrap_or(now));
    // Unpause the audio device in lock-step with the wall-clock origin.
    // AudioRenderer starts paused at construction (cpal would otherwise
    // pull from an empty mpsc and play silence while the audio decoder
    // warmed up, then "catch up" once real samples arrived — perceived
    // as audio leading or lagging by an unpredictable amount). seek()
    // re-pauses + flushes; this restores playback for the new pipeline.
    if !paused.load(Ordering::Relaxed) {
        audio_sink.set_paused(false);
    }
    let stats_audio = Arc::clone(&stats);
    let events_audio = Arc::clone(&events);
    let paused_audio = Arc::clone(&paused);
    let (_, _) = tokio::join!(
        tokio::spawn(video_sync_loop(
            start_time.clone(),
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
        tokio::spawn(audio_sync_loop(
            audio_rx,
            audio_sink,
            seek_offset.as_millis() as i64,
            stop.clone(),
            stop_flag.clone(),
            stats_audio,
            events_audio,
            paused_audio,
        )),
    );
    // Both loops returning naturally (channels closed by decoder EOF) means
    // we hit end-of-stream. If we were stopped explicitly, the consumer is
    // tearing down and doesn't care about EndOfStream — but emitting it
    // regardless is harmless and idempotent.
    if !stop_flag.load(Ordering::Relaxed) {
        let _ = events.send(PlayerEvent::EndOfStream);
    }
    stop.notify_waiters();
}

// ---------------------------------------------------------------------------
// Decoder tasks (platform-generic, communicate via channels)
// ---------------------------------------------------------------------------

async fn video_decoder_task(
    mut receiver: Receiver<DataSegment>,
    sender: Sender<DecodedVideoFrame>,
    mut decoder: Box<dyn HwVideoDecoder>,
    init_data: Vec<u8>,
    video_ready: Arc<Notify>,
    track_crypto: Option<TrackCrypto>,
    stats: Arc<StatsState>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut first_frame_signaled = false;

    // Reorder buffer to fix two sources of non-monotonic PTS:
    //   1. HEVC B-frames: hardware decoders may output in decode order, not
    //      display order, so composition timestamps are non-monotonic.
    //   2. Segment boundaries: the last N frames of segment K may arrive from
    //      the MediaCodec pipeline *after* the first frames of segment K+1,
    //      causing PTS to jump backward every ~4 seconds.
    // We always emit the lowest-PTS frame from the buffer. video_ready fires
    // on the first send so start_time is calibrated to when frames are
    // actually available (avoids a timing hole at startup).
    const REORDER_DEPTH: usize = 4;
    let mut reorder_buf: Vec<DecodedVideoFrame> = Vec::with_capacity(REORDER_DEPTH + 1);

    while let Some(segment) = receiver.recv().await {
        log::debug!("[dec] consuming video segment: {}", segment.id);

        let mut data_vec = init_data.clone();
        data_vec.extend_from_slice(&segment.data[..]);
        decrypt_segment_in_place(&mut data_vec, track_crypto.as_ref())?;

        let sample_info: Vec<(usize, usize, i64, u64)> = {
            let mp4 = Mp4::read_bytes(&data_vec)
                .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("mp4: {}", e).into() })?;
            let (_id, track) = mp4
                .tracks()
                .first_key_value()
                .ok_or_else(|| -> Box<dyn Error + Send + Sync> { "no track".into() })?;
            track
                .samples
                .iter()
                .map(|s| (s.offset as usize, s.size as usize, s.composition_timestamp, s.timescale))
                .collect()
        };

        let mut first_pts_us: Option<i64> = None;
        let mut last_pts_us: i64 = 0;
        for (offset, size, ts, ts_scale) in sample_info {
            if offset + size > data_vec.len() {
                continue;
            }
            let sample_data = &data_vec[offset..offset + size];
            let pts_us = if ts_scale > 0 { ts * 1_000_000 / ts_scale as i64 } else { 0 };
            if first_pts_us.is_none() { first_pts_us = Some(pts_us); }
            last_pts_us = pts_us;

            decoder.submit(sample_data, pts_us)?;

            loop {
                match decoder.try_recv()? {
                    Some(frame) => {
                        // Track the high-water-mark of decoded PTS so the
                        // av_sync loop can publish `buffered_ahead_secs`.
                        // Includes frames still in the reorder buffer — they
                        // are already decoded and will be rendered shortly.
                        let pts_ms = frame.pts_us / 1000;
                        let prev = stats.last_decoded_pts_ms.load(Ordering::Relaxed);
                        if pts_ms > prev {
                            stats
                                .last_decoded_pts_ms
                                .store(pts_ms, Ordering::Relaxed);
                        }
                        reorder_buf.push(frame);
                        if reorder_buf.len() > REORDER_DEPTH {
                            let min_idx = reorder_buf.iter().enumerate()
                                .min_by_key(|(_, f)| f.pts_us)
                                .map(|(i, _)| i)
                                .unwrap();
                            let to_send = reorder_buf.swap_remove(min_idx);
                            if !first_frame_signaled {
                                video_ready.notify_one();
                                first_frame_signaled = true;
                            }
                            if sender.send(to_send).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    None => break,
                }
            }
        }
        if let Some(first) = first_pts_us {
            log::info!("[dec] seg done: pts {}..{}ms", first / 1000, last_pts_us / 1000);
        }
    }

    // Flush remaining frames in PTS order.
    reorder_buf.sort_by_key(|f| f.pts_us);
    for frame in reorder_buf.drain(..) {
        if !first_frame_signaled {
            video_ready.notify_one();
            first_frame_signaled = true;
        }
        if sender.send(frame).await.is_err() {
            return Ok(());
        }
    }

    Ok(())
}

async fn audio_decoder_task(
    mut receiver: Receiver<DataSegment>,
    sender: Sender<DecodedAudioFrame>,
    mut decoder: Box<dyn AudioDecoder>,
    init_data: Vec<u8>,
    audio_ready: Arc<Notify>,
    track_crypto: Option<TrackCrypto>,
    stats: Arc<StatsState>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Mirror the video pattern: fire audio_ready only once we've
    // actually produced + queued a real PCM frame, not on the first
    // submit. Firing too early made av_sync_handler set start_time
    // before any audio was in cpal's queue, so the speaker only
    // started hearing real samples 20-200 ms after video began
    // rendering — perceived as constant audio lag.
    let mut first_audio_signaled = false;
    while let Some(segment) = receiver.recv().await {
        log::debug!("[dec] consuming audio segment: {}", segment.id);

        let mut data_vec = init_data.clone();
        data_vec.extend_from_slice(&segment.data[..]);
        decrypt_segment_in_place(&mut data_vec, track_crypto.as_ref())?;

        let sample_info: Vec<(usize, usize, i64, u64)> = {
            let mp4 = Mp4::read_bytes(&data_vec)
                .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("mp4: {}", e).into() })?;
            let (_id, track) = mp4
                .tracks()
                .first_key_value()
                .ok_or_else(|| -> Box<dyn Error + Send + Sync> { "no track".into() })?;
            track
                .samples
                .iter()
                .map(|s| (s.offset as usize, s.size as usize, s.composition_timestamp, s.timescale))
                .collect()
        };

        for (offset, size, ts, ts_scale) in sample_info {
            if offset + size > data_vec.len() {
                continue;
            }
            let sample_data = &data_vec[offset..offset + size];
            let pts_us = if ts_scale > 0 { ts * 1_000_000 / ts_scale as i64 } else { 0 };

            decoder.submit(sample_data, pts_us)?;

            loop {
                match decoder.try_recv()? {
                    Some(frame) => {
                        let pts_ms = frame.pts_ms;
                        let prev = stats
                            .audio_last_decoded_pts_ms
                            .load(Ordering::Relaxed);
                        if pts_ms > prev {
                            stats
                                .audio_last_decoded_pts_ms
                                .store(pts_ms, Ordering::Relaxed);
                        }
                        if sender.send(frame).await.is_err() {
                            return Ok(());
                        }
                        if !first_audio_signaled {
                            audio_ready.notify_one();
                            first_audio_signaled = true;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Download + decode pipeline builders (renderer-agnostic)
// ---------------------------------------------------------------------------

async fn video_play(
    video_representation: VideoRepresenation,
    start_index: usize,
    video_ready: Arc<Notify>,
    sender: Sender<DecodedVideoFrame>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    mut decoder: Box<dyn HwVideoDecoder>,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    segments_in_flight: usize,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(segments_in_flight);

    let init_dl = video_representation
        .segment_init
        .download(&http, RequestKind::InitSegment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("init download: {}", e).into() })?;
    let init_data = init_dl.data;

    let hvcc_nalus = parse_hvcc_nalus(&init_data)
        .ok_or_else(|| -> Box<dyn Error + Send + Sync> { "no hvcC in init segment".into() })?;

    let track_crypto = match parse_tenc(&init_data) {
        Some(tenc) => {
            log::info!(
                "video: CENC encrypted, KID={} iv_size={}",
                kid_short(&tenc.default_kid),
                tenc.default_iv_size
            );
            let dec = decryptor.ok_or(
                "Track is CENC-encrypted but no decryptor configured (call Player::set_clearkey or set_license_resolver)",
            )?;
            // Resolve the key NOW (async, possibly via LicenseResolver).
            // decrypt_sample is on the hot path and stays sync — it just
            // hits the cache populated here.
            dec.ensure_key_for(tenc.default_kid).await.map_err(
                |e| -> Box<dyn Error + Send + Sync> {
                    format!("license resolve (video kid={}): {}", kid_short(&tenc.default_kid), e).into()
                },
            )?;
            Some(TrackCrypto {
                decryptor: dec,
                kid: tenc.default_kid,
                iv_size: tenc.default_iv_size as usize,
            })
        }
        None => {
            log::info!("video: clear (no tenc box)");
            None
        }
    };

    decoder.configure(VideoDecoderParams {
        codec: VideoCodec::Hevc,
        width: video_representation.width,
        height: video_representation.height,
        hvcc_nalus,
    })?;

    let segments = video_representation.segments.clone();
    let dl_stats = Arc::clone(&stats);
    let on_video_dl: SegmentDoneCallback = Arc::new(move |pts_ms| {
        let prev = dl_stats.last_decoded_pts_ms.load(Ordering::Relaxed);
        if pts_ms > prev {
            dl_stats.last_decoded_pts_ms.store(pts_ms, Ordering::Relaxed);
        }
    });
    let download_task = task::spawn(download_task(
        segments,
        start_index,
        download_tx,
        stop,
        stop_flag,
        http,
        Some(Arc::clone(&stats)),
        Some(on_video_dl),
    ));
    let decoder_task = task::spawn(video_decoder_task(
        download_rx,
        sender,
        decoder,
        init_data,
        video_ready,
        track_crypto,
        stats,
    ));

    let (dl_res, dec_res) = join!(download_task, decoder_task);
    log_task_result("video download_task", dl_res);
    log_task_result("video decoder_task", dec_res);
    Ok(())
}

async fn audio_play(
    audio_representation: AudioRepresentation,
    start_index: usize,
    audio_ready: Arc<Notify>,
    sender: Sender<DecodedAudioFrame>,
    output_sample_rate: u32,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    mut decoder: Box<dyn AudioDecoder>,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    segments_in_flight: usize,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(segments_in_flight);

    let init_dl = audio_representation
        .segment_init
        .download(&http, RequestKind::InitSegment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("audio init download: {}", e).into() })?;
    let init_data = init_dl.data;

    let track_crypto = match parse_tenc(&init_data) {
        Some(tenc) => {
            log::info!(
                "audio: CENC encrypted, KID={} iv_size={}",
                kid_short(&tenc.default_kid),
                tenc.default_iv_size
            );
            let dec = decryptor.ok_or(
                "Audio track is CENC-encrypted but no decryptor configured (call Player::set_clearkey or set_license_resolver)",
            )?;
            dec.ensure_key_for(tenc.default_kid).await.map_err(
                |e| -> Box<dyn Error + Send + Sync> {
                    format!("license resolve (audio kid={}): {}", kid_short(&tenc.default_kid), e).into()
                },
            )?;
            Some(TrackCrypto {
                decryptor: dec,
                kid: tenc.default_kid,
                iv_size: tenc.default_iv_size as usize,
            })
        }
        None => {
            log::info!("audio: clear (no tenc)");
            None
        }
    };

    let codecs_str = audio_representation.codecs.as_str();
    let codec = if codecs_str.starts_with("mp4a") {
        AudioCodec::Aac
    } else if codecs_str == "ec-3" {
        AudioCodec::Eac3
    } else if codecs_str == "ac-3" {
        AudioCodec::Ac3
    } else {
        return Err(format!("Unsupported audio codec: {}", codecs_str).into());
    };

    // AAC carries its codec params in `esds` (AudioSpecificConfig), and both
    // FFmpeg and MediaCodec want those 2 bytes as extradata / csd-0 to open
    // the decoder. AC-3 and EAC-3 are self-describing (each frame begins with
    // a syncinfo header), so the decoder just needs the MIME plus the
    // sample-rate/channel hints from the DASH manifest.
    let (input_sample_rate, input_channels, codec_specific_data) = match codec {
        AudioCodec::Aac => {
            let aac_config = parse_aac_config(&init_data)
                .ok_or("Audio codec not supported (no AAC config in init segment)")?;
            let rate = aac_sampling_frequency_index_to_u32(aac_config.freq_index);
            let ch = aac_config.chan_conf as u16;
            let dsi: [u8; 2] = [
                (aac_config.profile << 3) | (aac_config.freq_index >> 1),
                ((aac_config.freq_index & 0x01) << 7) | (aac_config.chan_conf << 3),
            ];
            log::info!(
                "audio: AAC profile={} freq_index={} (={}Hz) chan_conf={}",
                aac_config.profile, aac_config.freq_index, rate, aac_config.chan_conf
            );
            (rate, ch, dsi.to_vec())
        }
        AudioCodec::Ac3 | AudioCodec::Eac3 => {
            let rate = audio_representation.audio_sampling_rate;
            let ch = audio_representation.channels.unwrap_or(2) as u16;
            log::info!("audio: {:?} {}Hz {}ch", codec, rate, ch);
            (rate, ch, Vec::new())
        }
    };

    decoder.configure(AudioDecoderParams {
        codec,
        input_sample_rate,
        input_channels,
        output_sample_rate,
        codec_specific_data,
    })?;

    let segments = audio_representation.segments.clone();
    let dl_stats = Arc::clone(&stats);
    let on_audio_dl: SegmentDoneCallback = Arc::new(move |pts_ms| {
        let prev = dl_stats.audio_last_decoded_pts_ms.load(Ordering::Relaxed);
        if pts_ms > prev {
            dl_stats
                .audio_last_decoded_pts_ms
                .store(pts_ms, Ordering::Relaxed);
        }
    });
    let download_task = task::spawn(download_task(
        segments,
        start_index,
        download_tx,
        stop,
        stop_flag,
        http,
        Some(Arc::clone(&stats)),
        Some(on_audio_dl),
    ));
    let decoder_task = task::spawn(audio_decoder_task(
        download_rx,
        sender,
        decoder,
        init_data,
        audio_ready,
        track_crypto,
        stats,
    ));

    let (dl_res, dec_res) = join!(download_task, decoder_task);
    log_task_result("audio download_task", dl_res);
    log_task_result("audio decoder_task", dec_res);
    Ok(())
}

// ---------------------------------------------------------------------------
// Subtitle (WebVTT) pipeline
// ---------------------------------------------------------------------------

/// Fetch + parse the selected subtitle representation, push cues into
/// the video sink so the wgpu overlay can render them.
///
/// Two delivery patterns are supported:
///   1. Single-file VTT (the common "external .vtt per language"
///      pattern) — `single_file_url` set, segments empty. Download
///      whole file once, parse as raw WebVTT, hand off, done.
///   2. ISO BMFF VTT in CMAF — `segment_init` + `segments` populated.
///      Stream segments through the normal download path, parse each
///      as `vttc` boxes, push cues incrementally.
///
/// Both paths skip silently if the representation isn't decodable
/// (TTML for now).
/// `active` is the live cell holding the currently-selected subtitle
/// representation. If the consumer flips it (via clear_subtitle_track
/// or set_subtitle_track to a different track) the task notices between
/// operations and exits so stale downloads stop wasting bandwidth.
async fn text_play<V: VideoSink>(
    text_representation: tracks::text::TextRepresenation,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    http: Arc<HttpClient>,
    video_sink: Arc<V>,
    active: Arc<StdMutex<Option<tracks::text::TextRepresenation>>>,
    target_id: u32,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Helper: did the consumer change subtitle selection out from under us?
    let still_selected = |active: &Arc<StdMutex<Option<tracks::text::TextRepresenation>>>| -> bool {
        active
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.id == target_id)
            .unwrap_or(false)
    };

    if !text_representation.is_webvtt() {
        log::info!(
            "[subs] representation {} is not WebVTT ({}/{}) — skipping",
            text_representation.id,
            text_representation.codecs,
            text_representation.mime_type,
        );
        return Ok(());
    }

    // ---- single-file delivery ----
    if let Some(url) = &text_representation.single_file_url {
        log::info!("[subs] downloading single-file VTT: {}", url);
        let dl_fut = http.get(url.clone(), RequestKind::InitSegment);
        let bytes = tokio::select! {
            r = dl_fut => match r {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("[subs] single-file download failed: {}", e);
                    return Ok(());
                }
            },
            _ = stop.notified() => return Ok(()),
        };
        if !still_selected(&active) {
            return Ok(());
        }
        let cues = crate::parsers::vtt::parse_segment(&bytes, 0);
        log::info!(
            "[subs] parsed {} cues from {} bytes (single VTT file)",
            cues.len(),
            bytes.len()
        );
        // Diagnostic for "Czech (or any non-ASCII) renders wrong" reports:
        // if the source isn't UTF-8 (some Windows-1250 / ISO-8859-2 VTT
        // files exist in the wild despite the spec) we'd see U+FFFD
        // replacement chars in the text. If the source IS UTF-8 but the
        // consumer's font lacks Latin Extended-A glyphs, the text here
        // looks fine and the problem is the font. The byte preview lets
        // us tell which.
        if let Some(first) = cues.first() {
            let head = first.text.chars().take(80).collect::<String>();
            let raw_head: String = bytes
                .iter()
                .take(160)
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(" ");
            log::debug!(
                "[subs] first cue text {:?} (chars={}); raw bytes head: {}",
                head,
                first.text.chars().count(),
                raw_head
            );
        }
        if cues.is_empty() {
            // Dump a hex+ASCII preview of the first 256 bytes so future
            // "0 cues" reports show what we actually got — line endings,
            // BOM, or some upstream format we don't recognise yet.
            let preview_len = bytes.len().min(256);
            let mut hex_dump = String::new();
            let mut ascii_dump = String::new();
            for &b in &bytes[..preview_len] {
                hex_dump.push_str(&format!("{:02x} ", b));
                ascii_dump.push(if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else if b == b'\n' {
                    '↵'
                } else if b == b'\r' {
                    '⏎'
                } else {
                    '.'
                });
            }
            log::warn!(
                "[subs] no cues parsed — preview ({} bytes):\n  hex: {}\n  txt: {}",
                preview_len, hex_dump, ascii_dump
            );
        }
        video_sink.queue_subtitle_cues(cues);
        return Ok(());
    }

    // ---- ISO BMFF CMAF delivery ----
    let init = match &text_representation.segment_init {
        Some(s) => s,
        None => {
            log::info!(
                "[subs] representation {} has no segments and no single-file URL",
                text_representation.id
            );
            return Ok(());
        }
    };
    let _ = init.download(&http, RequestKind::InitSegment).await;

    for (i, seg) in text_representation.segments.iter().enumerate() {
        if stop_flag.load(Ordering::Relaxed) || !still_selected(&active) {
            break;
        }
        let dl_fut = seg.download(&http, RequestKind::Segment);
        let dl = tokio::select! {
            r = dl_fut => r,
            _ = stop.notified() => break,
        };
        match dl {
            Ok(d) => {
                let pts_ms = seg.start_time().as_millis() as i64;
                let cues = crate::parsers::vtt::parse_segment(&d.data, pts_ms);
                if !cues.is_empty() {
                    log::debug!("[subs] segment {} produced {} cues", i, cues.len());
                    video_sink.queue_subtitle_cues(cues);
                }
            }
            Err(e) => {
                log::warn!("[subs] segment {} download failed: {}", i, e);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Video supervisor — owns the video pipeline lifecycle within one play()
// ---------------------------------------------------------------------------

/// Factory closure that produces a fresh `HwVideoDecoder`. The supervisor
/// invokes it once per spawned `video_play` (i.e. once per representation),
/// so the platform-specific decoder type stays out of this module.
type VideoDecoderFactory = Arc<dyn Fn() -> Box<dyn HwVideoDecoder> + Send + Sync>;

/// Long-lived task that owns the video pipeline for a single `play()` call.
/// It spawns one `video_play` at a time and respawns on representation swap.
///
/// Soft-switch flow on receiving a new representation:
///   1. Tell the current `video_play` to stop via a local notify+flag pair
///      (separate from the play-level `stop` so audio + av_sync survive).
///   2. Await its termination, dropping its decoder cleanly.
///   3. Compute the next segment in the new representation that's *after*
///      the current playback PTS, so the first new frame can't land before
///      the last old frame on the timeline.
///   4. Spawn a fresh `video_play` with the new representation, the same
///      shared frame `Sender` (so av_sync keeps consuming from one source).
///
/// Audio keeps playing throughout — only the video pipeline is touched.
async fn video_supervisor(
    initial_repr: VideoRepresenation,
    initial_start_index: usize,
    frame_sender: Sender<DecodedVideoFrame>,
    video_ready: Arc<Notify>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    decoder_factory: VideoDecoderFactory,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    mut switch_rx: tokio::sync::watch::Receiver<Option<VideoRepresenation>>,
    position_ms: Arc<AtomicU64>,
    events: Arc<broadcast::Sender<PlayerEvent>>,
    segments_in_flight: usize,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut current_repr = initial_repr;
    let mut current_start = initial_start_index;
    loop {
        // Per-iteration stop signal — only fires on representation swap,
        // not on play-level stop. Keeping it separate means `seek()`
        // semantics (play-level stop) still work as before.
        let local_stop = Arc::new(Notify::new());
        let local_stop_flag = Arc::new(AtomicBool::new(false));

        let vp_handle = task::spawn(video_play(
            current_repr.clone(),
            current_start,
            video_ready.clone(),
            frame_sender.clone(),
            local_stop.clone(),
            local_stop_flag.clone(),
            decryptor.clone(),
            decoder_factory(),
            Arc::clone(&http),
            Arc::clone(&stats),
            segments_in_flight,
        ));
        tokio::pin!(vp_handle);

        // Race three outcomes: play-level stop, video_play finishing on
        // its own (natural EOF), or a switch_rx update requesting a soft
        // swap. The natural-EOF arm is essential: if we kept the
        // keepalive frame_sender alive forever after the last segment
        // was decoded, the av_sync video_rx would never observe channel
        // close and `PlayerEvent::EndOfStream` would never fire.
        let new_repr: VideoRepresenation = loop {
            tokio::select! {
                _ = stop.notified() => {
                    local_stop_flag.store(true, Ordering::Relaxed);
                    local_stop.notify_waiters();
                    let _ = vp_handle.await;
                    return Ok(());
                }
                _ = &mut vp_handle => {
                    // video_play returned without us asking. Either it
                    // ran out of segments (EOF) or it errored. Either
                    // way, exit the supervisor so the keepalive
                    // frame_sender drops, the video channel closes, and
                    // av_sync can fire EndOfStream.
                    log::info!("[video] supervisor: video_play exited naturally; closing pipeline");
                    return Ok(());
                }
                _ = async {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let _ = switch_rx.changed().await;
                } => {
                    if stop_flag.load(Ordering::Relaxed) {
                        local_stop_flag.store(true, Ordering::Relaxed);
                        local_stop.notify_waiters();
                        let _ = vp_handle.await;
                        return Ok(());
                    }
                    if let Some(new) = switch_rx.borrow_and_update().clone() {
                        break new;
                    }
                    // Spurious None — keep waiting.
                }
            }
        };

        // Avoid swapping to the same representation (the ABR engine guards
        // this too, but explicit is cheap and idempotent).
        if new_repr.id == current_repr.id {
            continue;
        }

        // Tear down the OLD pipeline cleanly.
        local_stop_flag.store(true, Ordering::Relaxed);
        local_stop.notify_waiters();
        let _ = vp_handle.await;

        // Pick the segment in the new representation that *starts after*
        // the current playback position. This guarantees the first frame
        // emitted by the NEW pipeline has PTS > the last frame from the
        // OLD pipeline, so the av_sync receiver doesn't see backward PTS.
        let pos = Duration::from_millis(position_ms.load(Ordering::Relaxed));
        let mut new_start = find_segment_index(&new_repr.segments, pos);
        if new_start + 1 < new_repr.segments.len() {
            new_start = new_start.saturating_add(1);
        }
        log::info!(
            "[abr] soft switch: repr {} -> {} from seg {} (pos {}ms)",
            current_repr.id, new_repr.id, new_start, pos.as_millis()
        );
        let _ = events.send(PlayerEvent::TrackChanged {
            kind: TrackKind::Video,
            info: video_track_info(&new_repr),
        });
        current_repr = new_repr;
        current_start = new_start;
        // Next iteration spawns the new video_play.
    }
}

// ---------------------------------------------------------------------------
// Player — constructs default (platform-native) sinks
// ---------------------------------------------------------------------------

impl Player<VideoRenderer, AudioRenderer> {
    /// Host-owned-surface path for desktop (or any host that can hand over raw
    /// window + display handles, e.g. winit). The player never touches winit
    /// itself — the host keeps the underlying window alive for the player's
    /// lifetime and forwards layout changes via `resize`.
    pub fn new_from_raw_handle(
        window_handle: RawWindowHandle,
        display_handle: RawDisplayHandle,
        width: u32,
        height: u32,
    ) -> Self {
        let video_renderer = Arc::new(
            VideoRenderer::new_from_raw_handle(window_handle, display_handle, width, height)
                .block_on(),
        );
        let audio_renderer = Arc::new(AudioRenderer::new());
        Self::from_renderers(video_renderer, audio_renderer)
    }

    /// Install a hook invoked right before each frame is presented. Desktop
    /// hosts wire this to `winit::window::Window::pre_present_notify` so the
    /// compositor still gets its frame-pacing hint; embedded hosts (iOS/Android)
    /// leave it unset (`CAMetalLayer` / `ANativeWindow` need no pre-notify).
    pub fn set_pre_present_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        self.video_renderer.set_pre_present_hook(hook);
    }

    /// Embedded Apple path: render into a host-provided `CAMetalLayer*`.
    /// Mirror of `new` for an app that owns `UIApplicationMain` itself (no
    /// winit). The host keeps the layer alive for the player's lifetime and
    /// drives layout changes through `Player::resize`.
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub fn new_from_metal_layer(layer: *mut std::ffi::c_void, width: u32, height: u32) -> Self {
        let video_renderer =
            Arc::new(VideoRenderer::new_from_metal_layer(layer, width, height).block_on());
        let audio_renderer = Arc::new(AudioRenderer::new());
        Self::from_renderers(video_renderer, audio_renderer)
    }

    /// Embedded Android path: render into a host-provided `ANativeWindow*`
    /// (obtained from a Java `Surface`). Mirror of `new` for a host Activity
    /// that owns the `SurfaceView` (no winit `NativeActivity`).
    #[cfg(target_os = "android")]
    pub fn new_from_android_surface(
        native_window: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> Self {
        let video_renderer = Arc::new(
            VideoRenderer::new_from_android_surface(native_window, width, height).block_on(),
        );
        let audio_renderer = Arc::new(AudioRenderer::new());
        Self::from_renderers(video_renderer, audio_renderer)
    }

    /// Assemble a `Player` from already-built renderers. Shared tail of every
    /// constructor above — the only difference between the winit and embedded
    /// paths is how the `VideoRenderer` obtained its surface.
    fn from_renderers(
        video_renderer: Arc<VideoRenderer>,
        audio_renderer: Arc<AudioRenderer>,
    ) -> Self {
        let start_time = Arc::new(Instant::now());

        let video_ready = Arc::new(Notify::new());
        let audio_ready = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());

        let (events_tx, _) = broadcast::channel::<PlayerEvent>(64);
        let events = Arc::new(events_tx);
        // Emit the initial Idle state so freshly-constructed subscribers
        // see something on their first recv() if they happen to subscribe
        // before any state transition.
        let _ = events.send(PlayerEvent::Idle);

        Player {
            base_url: None,
            manifest: None,
            tracks: Arc::new(StdMutex::new(None)),
            http: Arc::new(HttpClient::new()),
            events,
            paused: Arc::new(AtomicBool::new(false)),
            pause_notify: Arc::new(Notify::new()),
            video_adaptation: Arc::new(StdMutex::new(None)),
            video_representation: Arc::new(StdMutex::new(None)),
            audio_adaptation: Arc::new(StdMutex::new(None)),
            audio_representation: Arc::new(StdMutex::new(None)),

            video_ready,
            audio_ready,

            stop,
            stop_flag: Arc::new(AtomicBool::new(false)),

            start_time,

            seek_target: Arc::new(RwLock::new(None)),
            position_ms: Arc::new(AtomicU64::new(0)),

            decryptor: Arc::new(StdMutex::new(None)),

            stats: Arc::new(StatsState::default()),
            abr_strategy: Arc::new(ArcSwap::from_pointee(AbrStrategy::default())),
            video_switch_tx: Arc::new(StdMutex::new(None)),
            buffer_target_secs: Arc::new(AtomicU32::new(DEFAULT_BUFFER_TARGET_SECS)),
            subtitle_representation: Arc::new(StdMutex::new(None)),

            video_renderer,
            audio_renderer,

            // Every constructor runs inside a Tokio runtime (desktop:
            // `#[tokio::main]`; iOS/Android: the host enters the runtime before
            // calling in), so a current handle is always available here. Storing
            // it lets `resize`/`seek`/track-switch spawn from any thread later.
            rt: tokio::runtime::Handle::current(),
        }
    }
}

// ---------------------------------------------------------------------------
// Player — generic methods, work with any VideoSink + AudioSink
// ---------------------------------------------------------------------------

impl<V: VideoSink, A: AudioSink> Player<V, A> {
    /// Pre-seed the ClearKey cache with `(kid_hex → key_hex)` pairs.
    /// Backwards-compatible API: if a `LicenseResolver` is also installed,
    /// these keys take precedence (cache hit wins).
    pub fn set_clearkey(&self, keys: HashMap<String, String>) -> Result<(), Box<dyn Error>> {
        let from_hex = ClearKeyDecryptor::from_hex(keys)?;
        // ClearKeyDecryptor::from_hex returns a fresh instance with the
        // parsed keys; merge them into our shared decryptor (creating it
        // if this is the first crypto call).
        let mut slot = self.decryptor.lock().unwrap();
        let dec = slot.get_or_insert_with(|| {
            Arc::new(ClearKeyDecryptor::new(HashMap::new()))
        });
        // Move keys out of the temporary decryptor into the shared one.
        let parsed = from_hex.into_keys();
        dec.add_keys(parsed);
        Ok(())
    }

    /// Install a `LicenseResolver` to fetch keys lazily on first encounter
    /// of an unknown KID. May be combined with `set_clearkey` — pre-seeded
    /// keys win on cache hit, the resolver is only asked on cache miss.
    pub fn set_license_resolver(&self, resolver: Arc<dyn LicenseResolver>) {
        let mut slot = self.decryptor.lock().unwrap();
        let dec = slot.get_or_insert_with(|| {
            Arc::new(ClearKeyDecryptor::new(HashMap::new()))
        });
        dec.set_resolver(resolver);
    }

    fn parse_base_url(full_url: &str) -> Result<String, Box<dyn Error>> {
        let mut url = Url::parse(full_url)?;
        url.path_segments_mut()
            .expect("Cannot modify path segments")
            .pop();
        Ok(url.to_string() + "/")
    }

    pub async fn open_url(&mut self, url: &str) -> Result<(), Box<dyn Error>> {
        let base_url = Self::parse_base_url(url)?;
        self.base_url = Some(base_url);
        let url = url.to_string();
        let manifest = match Manifest::new(url, &self.http).await {
            Ok(m) => m,
            Err(e) => {
                self.emit_error(PlayerErrorKind::ManifestParse, format!("manifest: {}", e));
                return Err(e);
            }
        };
        // Multi-period MPDs are explicitly out of scope (see
        // PLAYER_INTEGRATION.md §11). Reject upfront rather than silently
        // playing only the first period.
        if manifest.mpd.periods.len() > 1 {
            let detail = format!(
                "multi-period MPD not supported (got {} periods)",
                manifest.mpd.periods.len()
            );
            self.emit_error(PlayerErrorKind::ManifestParse, detail.clone());
            return Err(detail.into());
        }

        // Pre-count tracks for the ManifestLoaded event. The duration
        // string is parsed inside `Tracks::new`, but we emit a coarse
        // duration here from the MPD already.
        let dur_str = &manifest.mpd.media_presentation_duration;
        let duration = dur_str
            .parse::<iso8601_duration::Duration>()
            .ok()
            .map(|d| crate::utils::time::iso_to_std_duration(&d))
            .unwrap_or(Duration::ZERO);
        let (mut video, mut audio, mut text) = (0usize, 0usize, 0usize);
        if let Some(period) = manifest.mpd.periods.first() {
            for a in &period.adaptation_sets {
                match a.content_type.as_str() {
                    "video" => video += 1,
                    "audio" => audio += 1,
                    "text" => text += 1,
                    _ => {}
                }
            }
        }
        let _ = self.events.send(PlayerEvent::ManifestLoaded {
            duration,
            video_tracks: video,
            audio_tracks: audio,
            subtitle_tracks: text,
        });
        self.manifest = Some(manifest);
        Ok(())
    }

    pub async fn prepare(&mut self) -> Result<(), Box<dyn Error>> {
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        ffmpeg_next::init()?;

        let manifest = match &self.manifest {
            Some(m) => m,
            None => return Err("Manifest not loaded!".into()),
        };
        let base_url = match &self.base_url {
            Some(u) => u.to_string(),
            None => return Err("BaseUrl not loaded!".into()),
        };
        let tracks = match Tracks::new(base_url, &manifest.mpd, &manifest.content, &self.http).await {
            Ok(t) => t,
            Err(e) => {
                self.emit_error(PlayerErrorKind::ManifestParse, format!("tracks: {}", e));
                return Err(e);
            }
        };
        *self.tracks.lock().unwrap() = Some(tracks);
        let _ = self.events.send(PlayerEvent::Prepared);
        Ok(())
    }

    /// Subscribe to the event stream. Each subscriber gets every event
    /// from the moment of subscription forward (broadcast semantics).
    /// The channel buffer holds 64 events; a slow subscriber that falls
    /// behind by more receives `RecvError::Lagged(n)` and continues
    /// from the newest event.
    pub fn events(&self) -> broadcast::Receiver<PlayerEvent> {
        self.events.subscribe()
    }

    /// Emit an `Error` event. Internal helper — surfaces both via
    /// `events()` and as a `Result::Err` on the originating call.
    fn emit_error(&self, kind: PlayerErrorKind, detail: impl Into<String>) {
        let _ = self.events.send(PlayerEvent::Error {
            kind,
            detail: detail.into(),
        });
    }

    /// Pause playback. The video sync loop parks on the next iteration;
    /// audio output stops feeding the device. PTS does not advance.
    /// `Paused` event is emitted; no-op if already paused.
    pub fn pause(&self) {
        if !self.paused.swap(true, Ordering::Relaxed) {
            self.audio_renderer.set_paused(true);
            let _ = self.events.send(PlayerEvent::Paused);
        }
    }

    /// Resume playback after `pause()`. Wakes both sync loops and the
    /// audio output. `Playing` is emitted by the first rendered frame
    /// after resume. No-op if not paused.
    pub fn resume(&self) {
        if self.paused.swap(false, Ordering::Relaxed) {
            self.audio_renderer.set_paused(false);
            self.pause_notify.notify_waiters();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Install a `RequestInterceptor` (auth headers, URL rewrites). Replaces
    /// the default `NoopInterceptor`. Subsequent requests use the new
    /// interceptor; in-flight requests keep the previous one.
    pub fn set_request_interceptor(&self, interceptor: Arc<dyn RequestInterceptor>) {
        self.http.set_interceptor(interceptor);
    }

    /// Override the default `RetryPolicy` (3 attempts, 250ms × 2, cap 4s,
    /// ±20% jitter). Affects every subsequent request.
    pub fn set_retry_policy(&self, policy: RetryPolicy) {
        self.http.set_retry_policy(policy);
    }

    /// How long the player waits on an interceptor or license-resolver call
    /// before giving up (default 10s). On timeout the corresponding request
    /// surfaces an Interceptor / LicenseResolver error.
    pub fn set_callback_timeout(&self, timeout: Duration) {
        self.http.set_callback_timeout(timeout);
    }

    pub fn get_tracks(&self) -> Result<Tracks, Box<dyn Error>> {
        match self.tracks.lock().unwrap().as_ref() {
            Some(t) => Ok(t.clone()),
            None => Err("No parsed tracks - player not prepared".into()),
        }
    }

    pub fn set_video_track(
        &self,
        adaptation: &VideoAdaptation,
        representation: &VideoRepresenation,
    ) {
        *self.video_adaptation.lock().unwrap() = Some(adaptation.clone());
        *self.video_representation.lock().unwrap() = Some(representation.clone());
        let size = PhysicalSize::new(representation.width, representation.height);
        self.change_frame_size(size);
    }

    pub fn set_audio_track(
        &self,
        adaptation: &AudioAdaptation,
        representation: &AudioRepresentation,
    ) {
        *self.audio_adaptation.lock().unwrap() = Some(adaptation.clone());
        *self.audio_representation.lock().unwrap() = Some(representation.clone());
    }

    pub fn current_video_representation(&self) -> Option<VideoRepresenation> {
        self.video_representation.lock().unwrap().clone()
    }

    pub fn current_audio_representation(&self) -> Option<AudioRepresentation> {
        self.audio_representation.lock().unwrap().clone()
    }

    /// User-facing track switch. Treated as an explicit override:
    ///   - Flips the ABR strategy back to `Manual` so the user's pick
    ///     sticks until they re-arm ABR.
    ///   - Performs a **hard** switch: tears the whole pipeline down via
    ///     `seek(current_position)` and restarts on the new representation
    ///     from the current playback PTS. The user paid for a click — they
    ///     should see the chosen quality NOW, not at the next segment
    ///     boundary like the soft path does. Brief A/V resync (~100-200ms)
    ///     is the price, intentional.
    pub fn change_video_track(&self, representation: &VideoRepresenation) {
        let already = self
            .video_representation
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.id == representation.id)
            .unwrap_or(false);
        if already {
            // Still flip strategy — calling change_video_track on the
            // current rung is the natural way to "lock in" the manual mode.
            self.abr_strategy.store(Arc::new(AbrStrategy::Manual));
            return;
        }
        self.abr_strategy.store(Arc::new(AbrStrategy::Manual));
        *self.video_representation.lock().unwrap() = Some(representation.clone());
        let size = PhysicalSize::new(representation.width, representation.height);
        self.change_frame_size(size);
        let _ = self.events.send(PlayerEvent::TrackChanged {
            kind: TrackKind::Video,
            info: video_track_info(representation),
        });
        // Hard restart — seek() flips stop_flag, the user-level play()
        // loop respawns with the freshly-stored representation.
        self.seek(self.position());
    }

    /// ABR-driven swap. Soft: hands the new representation to the running
    /// video supervisor over a watch channel; only the video sub-pipeline
    /// restarts, audio + av_sync stay alive. Falls back to a no-op when
    /// the supervisor isn't running (between play() calls) — the stored
    /// representation gets picked up by the next play().
    ///
    /// Never called for user-driven switches: those go through
    /// `change_video_track` which is intentionally hard so the user sees
    /// the picked quality immediately.
    fn apply_video_representation_soft(&self, representation: &VideoRepresenation) {
        let already = self
            .video_representation
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.id == representation.id)
            .unwrap_or(false);
        if already {
            return;
        }
        *self.video_representation.lock().unwrap() = Some(representation.clone());
        let size = PhysicalSize::new(representation.width, representation.height);
        self.change_frame_size(size);

        let guard = self.video_switch_tx.lock().unwrap();
        if let Some(tx) = guard.as_ref() {
            // Supervisor running — hands over without tearing audio down.
            // Supervisor emits TrackChanged itself once the handover lands.
            let _ = tx.send(Some(representation.clone()));
        } else {
            // No live pipeline; just emit the event so consumers see the
            // selection update. Next play() will use the stored repr.
            let _ = self.events.send(PlayerEvent::TrackChanged {
                kind: TrackKind::Video,
                info: video_track_info(representation),
            });
        }
    }

    /// Install an ABR strategy. `Manual` (the default) leaves track
    /// selection entirely to the consumer. `BandwidthEwma` runs a 1Hz
    /// reconsideration against the measured throughput EWMA.
    ///
    /// `change_video_track` resets this back to `Manual` so user picks
    /// always win — call `set_abr_strategy` again to re-arm ABR.
    pub fn set_abr_strategy(&self, strategy: AbrStrategy) {
        self.abr_strategy.store(Arc::new(strategy));
    }

    /// Returns the active ABR strategy. Useful for UIs that want to render
    /// an "Auto" indicator next to the manually-picked rung.
    pub fn abr_strategy(&self) -> AbrStrategy {
        **self.abr_strategy.load()
    }

    /// How many seconds of media the player tries to keep buffered ahead
    /// of the renderer. Default is `DEFAULT_BUFFER_TARGET_SECS` (8s).
    /// Bumping this trades RAM for resilience against network jitter
    /// — each queued segment holds roughly bandwidth × segment-duration
    /// bytes. Takes effect at the next `play()`; the running pipeline
    /// keeps its current capacity until it restarts.
    ///
    /// Clamped to at least 2s so the channel always has room for one
    /// segment ahead of the decoder.
    pub fn set_buffer_target_secs(&self, secs: u32) {
        self.buffer_target_secs.store(secs.max(2), Ordering::Relaxed);
    }

    pub fn buffer_target_secs(&self) -> u32 {
        self.buffer_target_secs.load(Ordering::Relaxed)
    }

    /// Provide a TTF/OTF font for subtitle rendering. Until this is
    /// called, the cue pipeline still parses and tracks cues but the
    /// wgpu overlay draws nothing. Reasonable choices per platform:
    /// `/system/fonts/Roboto-Regular.ttf` on Android, any system font
    /// on desktop, or bundle one with your app.
    pub fn set_subtitle_font(&self, bytes: Vec<u8>) -> Result<(), Box<dyn Error>> {
        self.video_renderer
            .set_subtitle_font(bytes)
            .map_err(|e| -> Box<dyn Error> { format!("subtitle font: {}", e).into() })?;
        Ok(())
    }

    /// Select a subtitle track. Spawns the text_play pipeline
    /// immediately — works regardless of whether `play()` is currently
    /// running, has finished, or hasn't been called yet. Single-file
    /// VTT downloads once then exits; CMAF streaming runs until the
    /// segment list is exhausted or `clear_subtitle_track` fires.
    pub fn set_subtitle_track(&self, representation: &tracks::text::TextRepresenation) {
        *self.subtitle_representation.lock().unwrap() = Some(representation.clone());
        // Wipe any cues from the previous track so they don't bleed
        // across the switch.
        self.video_renderer.clear_subtitles();
        log::info!(
            "[subs] selected representation id={} codecs={} mime={}",
            representation.id, representation.codecs, representation.mime_type
        );

        // Spawn text_play right now. We don't track the handle — when
        // clear_subtitle_track flips subtitle_representation to None
        // the running task checks the flag between segments and exits.
        let repr = representation.clone();
        let stop = self.stop.clone();
        let stop_flag = self.stop_flag.clone();
        let http = Arc::clone(&self.http);
        let sink = self.video_renderer.clone();
        let active = Arc::clone(&self.subtitle_representation);
        let target_id = representation.id;
        self.rt.spawn(async move {
            let res = text_play(repr, stop, stop_flag, http, sink, active, target_id).await;
            if let Err(e) = res {
                log::warn!("[subs] text_play exited: {}", e);
            }
        });
    }

    /// Disable subtitles. Wipes the overlay's queued cues; any running
    /// `text_play` notices the cleared selection on its next segment
    /// boundary and exits cleanly.
    pub fn clear_subtitle_track(&self) {
        *self.subtitle_representation.lock().unwrap() = None;
        self.video_renderer.clear_subtitles();
    }

    pub fn current_subtitle_representation(&self) -> Option<tracks::text::TextRepresenation> {
        self.subtitle_representation.lock().unwrap().clone()
    }

    /// Convert the configured `buffer_target_secs` into a channel capacity
    /// (segments-in-flight) using the conservative segment-duration estimate.
    /// Floored at 2 so even with a tiny buffer target the decoder has room
    /// for the next segment behind the one currently being processed.
    fn segments_in_flight(&self) -> usize {
        let secs = self.buffer_target_secs.load(Ordering::Relaxed).max(2);
        ((secs / ASSUMED_SEGMENT_SECS) as usize).max(2)
    }

    /// One ABR reconsideration. Called from the per-second tick spawned in
    /// `play()`. No-op when the strategy is `Manual` or when the current
    /// adaptation has fewer than two representations to choose between.
    fn abr_tick(&self) {
        let strategy = **self.abr_strategy.load();
        let safety = match strategy {
            AbrStrategy::Manual => return,
            AbrStrategy::BandwidthEwma { safety_factor } => safety_factor,
        };

        let adaptation = match self.video_adaptation.lock().unwrap().clone() {
            Some(a) => a,
            None => return,
        };
        if adaptation.representations.len() < 2 {
            return;
        }
        let current_id = self
            .video_representation
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.id);

        let ewma_bps = self.stats.bandwidth_bps_ewma.load(Ordering::Relaxed);
        // Warmup: don't switch until we have *some* sample. Otherwise the
        // very first tick would always pick the lowest rung.
        if ewma_bps == 0 {
            return;
        }

        let bws: Vec<u64> = adaptation
            .representations
            .iter()
            .map(|r| r.bandwidth)
            .collect();
        let pick_idx = match abr::pick_representation(&bws, ewma_bps, safety) {
            Some(i) => i,
            None => return,
        };
        let picked = &adaptation.representations[pick_idx];
        if Some(picked.id) == current_id {
            return;
        }
        log::info!(
            "[abr] switch repr {:?} -> {} (ewma={}bps safety={})",
            current_id, picked.id, ewma_bps, safety
        );
        self.apply_video_representation_soft(picked);
    }

    pub fn change_audio_track(
        &self,
        adaptation: &AudioAdaptation,
        representation: &AudioRepresentation,
    ) {
        *self.audio_adaptation.lock().unwrap() = Some(adaptation.clone());
        *self.audio_representation.lock().unwrap() = Some(representation.clone());
        self.seek(self.position());
    }

    /// Start playback. Creates platform-specific decoders and feeds them into
    /// the generic A/V sync loop. Only the concrete decoder types differ per
    /// platform; the rest of the pipeline is identical.
    pub fn play(&self) -> Result<JoinHandle<()>, Box<dyn Error>> {
        let video_representation = match self.video_representation.lock().unwrap().as_ref() {
            Some(r) => r.clone(),
            None => return Err("Video Track not set".into()),
        };
        let audio_representation = match self.audio_representation.lock().unwrap().as_ref() {
            Some(r) => r.clone(),
            None => return Err("Audio Track not set".into()),
        };

        let video_ready = self.video_ready.clone();
        let audio_ready = self.audio_ready.clone();
        let stop = self.stop.clone();
        let stop_flag = self.stop_flag.clone();
        let video_sink = self.video_renderer.clone();
        let audio_sink = self.audio_renderer.clone();
        let seek_target = self.seek_target.clone();
        let position_ms = self.position_ms.clone();
        // Upcast Arc<ClearKeyDecryptor> → Arc<dyn Decryptor> for the
        // generic pipeline. Cache + resolver are owned by the
        // ClearKeyDecryptor and persist across play() cycles.
        let decryptor_snapshot: Option<Arc<dyn Decryptor>> = self
            .decryptor
            .lock()
            .unwrap()
            .clone()
            .map(|d| d as Arc<dyn Decryptor>);
        let http_video = Arc::clone(&self.http);
        let http_audio = Arc::clone(&self.http);
        let events = Arc::clone(&self.events);
        let paused = Arc::clone(&self.paused);
        let pause_notify = Arc::clone(&self.pause_notify);
        let media_duration = self
            .tracks
            .lock()
            .unwrap()
            .as_ref()
            .map(|t| t.duration)
            .unwrap_or(Duration::ZERO);

        // Video decoder factory — the supervisor calls this once per spawned
        // video_play (one for the initial repr, again for every ABR swap).
        let video_decoder_factory: VideoDecoderFactory = {
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                Arc::new(|| Box::new(decoders::ffmpeg_hw::FfmpegHwDecoder::new()))
            }
            #[cfg(target_os = "android")]
            {
                Arc::new(|| Box::new(decoders::mediacodec::MediaCodecDecoder::new()))
            }
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                Arc::new(|| {
                    let d = decoders::videotoolbox::VideoToolboxDecoder::new()
                        .expect("VideoToolboxDecoder::new");
                    Box::new(d) as Box<dyn HwVideoDecoder>
                })
            }
        };

        // Capture the decoder name once per play() cycle. The first
        // factory() call is cheap (no platform resources allocated until
        // configure() runs inside video_play) so we drop the temporary.
        {
            let probe = video_decoder_factory();
            *self.stats.decoder_name.lock().unwrap() = probe.name().to_string();
        }

        // macOS + iOS both use FFmpeg for audio (AudioToolbox AAC is broken when
        // fed access units packet-by-packet); video stays native VideoToolbox.
        #[cfg(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "macos",
            target_os = "ios"
        ))]
        let audio_decoder: Box<dyn AudioDecoder> =
            Box::new(decoders::ffmpeg_audio::FfmpegAudioDecoder::new());
        #[cfg(target_os = "android")]
        let audio_decoder: Box<dyn AudioDecoder> =
            Box::new(decoders::mediacodec_audio::MediaCodecAudioDecoder::new());

        // Install a fresh per-play switch channel so apply_video_representation
        // can route ABR swaps into the supervisor. Dropped at end of play().
        let (switch_tx, switch_rx) =
            tokio::sync::watch::channel::<Option<VideoRepresenation>>(None);
        *self.video_switch_tx.lock().unwrap() = Some(switch_tx);
        let video_switch_slot = Arc::clone(&self.video_switch_tx);

        let stats = Arc::clone(&self.stats);
        let abr_player = self.clone();
        // Capture the configured buffer target at play() time so the
        // spawned pipeline stays consistent across its lifetime even if
        // the consumer flips set_buffer_target_secs mid-play.
        let seg_in_flight = self.segments_in_flight();
        log::info!(
            "play(): buffer target {}s -> {} segments in flight",
            self.buffer_target_secs.load(Ordering::Relaxed), seg_in_flight
        );
        // Snapshot the subtitle representation at play() time. Mid-play
        // track switches go through set_subtitle_track which already
        // calls clear_subtitles + stores the new repr; the next play()
        // cycle picks it up.
        let subtitle_repr_snapshot = self.subtitle_representation.lock().unwrap().clone();
        let http_subs = Arc::clone(&self.http);
        let video_sink_for_subs = self.video_renderer.clone();

        let play = tokio::spawn(async move {
            // ABR tick: runs alongside the playback pipeline. Each second,
            // if the strategy is BandwidthEwma, re-evaluate which video
            // representation fits the measured throughput. Quiet on Manual.
            let abr_stop_flag = abr_player.stop_flag.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(1));
                ticker.set_missed_tick_behavior(
                    tokio::time::MissedTickBehavior::Skip,
                );
                loop {
                    ticker.tick().await;
                    if abr_stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    abr_player.abr_tick();
                }
            });
            let seek_offset = {
                let mut target = seek_target.write().await;
                stop_flag.store(false, Ordering::Relaxed);
                target.take().unwrap_or(Duration::ZERO)
            };

            // Hard reset the audio sink before the new pipeline starts
            // pumping samples. Without this, a play() that follows a
            // natural EndOfStream inherits the previous pipeline's
            // residual cpal-channel contents — the bounded mpsc holds
            // up to ~2 s of f32 samples — and the next audio_sync_loop
            // blocks on `send().await` until cpal drains the stale
            // buffer at real-time rate. The downstream effect was that
            // the second video in a loop appeared silent for the first
            // few seconds and could deadlock if upstream backpressure
            // stalled the decoder before any new sample landed in cpal.
            // seek() already does this for user-initiated restarts;
            // doing it here covers the EOS → next-play() case too.
            audio_sink.flush();
            audio_sink.set_paused(true);

            let video_start_index =
                find_segment_index(&video_representation.segments, seek_offset);
            let audio_start_index =
                find_segment_index(&audio_representation.segments, seek_offset);

            let snapped_seek_offset = video_representation
                .segments
                .get(video_start_index)
                .map(|s| s.start_time())
                .unwrap_or(seek_offset);
            position_ms.store(snapped_seek_offset.as_millis() as u64, Ordering::Relaxed);
            // Snap the decode high-water-mark to the new position so the
            // buffer gauge reads 0 until the fresh pipeline produces its
            // first frames. Otherwise a stale value from the previous
            // pipeline would inflate (or after a backward seek, mislead)
            // the gauge for the first second.
            stats
                .last_decoded_pts_ms
                .store(snapped_seek_offset.as_millis() as i64, Ordering::Relaxed);
            stats
                .audio_last_decoded_pts_ms
                .store(snapped_seek_offset.as_millis() as i64, Ordering::Relaxed);
            // Clear any stale starvation state from a previous play()
            // — a play that ended in Buffering would otherwise leave
            // the flags set, and the next pipeline would never emit
            // the initial Playing event because is_buffering would
            // already read true.
            stats.video_starving.store(false, Ordering::Relaxed);
            stats.audio_starving.store(false, Ordering::Relaxed);

            let (frame_sender, frame_receiver) = mpsc::channel::<DecodedVideoFrame>(64);
            let (sample_sender, sample_receiver) = mpsc::channel::<DecodedAudioFrame>(256);

            let events_for_supervisor = Arc::clone(&events);
            let video = tokio::spawn(video_supervisor(
                video_representation,
                video_start_index,
                frame_sender,
                video_ready.clone(),
                stop.clone(),
                stop_flag.clone(),
                decryptor_snapshot.clone(),
                video_decoder_factory,
                http_video,
                Arc::clone(&stats),
                switch_rx,
                position_ms.clone(),
                events_for_supervisor,
                seg_in_flight,
            ));

            let sample_rate = audio_sink.sample_rate();
            let audio = tokio::spawn(audio_play(
                audio_representation,
                audio_start_index,
                audio_ready.clone(),
                sample_sender,
                sample_rate,
                stop.clone(),
                stop_flag.clone(),
                decryptor_snapshot,
                audio_decoder,
                http_audio,
                Arc::clone(&stats),
                seg_in_flight,
            ));

            // Subtitles: text_play is spawned from set_subtitle_track,
            // not here. It runs independently of play() so seeks or
            // track switches don't tear it down — the loaded cues live
            // in the overlay until clear_subtitle_track or a new
            // set_subtitle_track replaces them.
            let _ = (&http_subs, &video_sink_for_subs, &subtitle_repr_snapshot);

            av_sync_handler(
                snapped_seek_offset,
                video_ready.clone(),
                frame_receiver,
                video_sink,
                position_ms,
                audio_ready.clone(),
                sample_receiver,
                audio_sink,
                stop.clone(),
                stop_flag.clone(),
                events,
                media_duration,
                paused,
                pause_notify,
                stats,
            )
            .await;

            let (play_res, audio_res) = join!(video, audio);
            log_task_result("video_supervisor", play_res);
            log_task_result("audio_play", audio_res);

            // Drop the watch sender so a stale apply_video_representation
            // between play() calls becomes a no-op until the next play().
            *video_switch_slot.lock().unwrap() = None;
        });
        Ok(play)
    }

    pub fn seek(&self, target: Duration) {
        let seek_target = self.seek_target.clone();
        let stop = self.stop.clone();
        let stop_flag = self.stop_flag.clone();
        let audio_sink = self.audio_renderer.clone();
        self.rt.spawn(async move {
            {
                let mut slot = seek_target.write().await;
                *slot = Some(target);
                stop_flag.store(true, Ordering::Relaxed);
            }
            audio_sink.flush();
            // Pause cpal so the NEXT pipeline's first audio sample can
            // be re-aligned with the new start_time by av_sync_handler.
            // Otherwise cpal would consume whatever lands in the mpsc
            // first, racing ahead of the rebuilt video pipeline.
            audio_sink.set_paused(true);
            stop.notify_waiters();
        });
    }

    pub fn seek_relative(&self, delta_ms: i64) {
        let current = self.position_ms.load(Ordering::Relaxed) as i64;
        let target_ms = (current + delta_ms).max(0) as u64;
        self.seek(Duration::from_millis(target_ms));
    }

    pub fn position(&self) -> Duration {
        Duration::from_millis(self.position_ms.load(Ordering::Relaxed))
    }

    /// Stop the current `play()` pipeline. Re-callable: the Player can
    /// be driven through another `open_url` / `play()` cycle afterwards
    /// without rebuilding the audio device.
    ///
    /// Previously this also called `audio_renderer.stop()`, which sent
    /// the AudioRenderer's `Stop` command and tore down the underlying
    /// cpal output stream. That made the renderer single-use: any
    /// subsequent `play()` would push samples into a closed channel,
    /// the cpal callback was no longer firing, and audio stayed silent
    /// forever. Consumers that drive multiple movies through a single
    /// Player (BlackZone Console picks a movie, leaves the player
    /// screen via stop(), then picks another) hit this on movie #2.
    /// The cpal stream now stays alive across `stop()` and is only
    /// torn down when the AudioRenderer itself is dropped — i.e. when
    /// the last Player handle is dropped.
    pub async fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        self.stop.notify_waiters();
        // Drain any samples still queued for cpal and park the device
        // until the next play() unpauses it. Mirrors what seek() and
        // play()-startup do for the same "switching pipelines" reason.
        self.audio_renderer.flush();
        self.audio_renderer.set_paused(true);
    }

    /// Relative volume nudge — adds `volume_diff` to the current value and
    /// clamps to 0.0..=1.0. Convenience for hotkey-driven adjustments.
    pub fn volume(&self, volume_diff: f32) {
        self.audio_renderer.volume(volume_diff);
    }

    /// Absolute volume in 0.0..=1.0. The UI layer should call this on
    /// startup with any persisted user value, and whenever the user
    /// drags a volume slider.
    pub fn set_volume(&self, volume: f32) {
        self.audio_renderer.set_volume(volume);
    }

    /// Current volume in 0.0..=1.0.
    pub fn get_volume(&self) -> f32 {
        self.audio_renderer.get_volume()
    }

    pub fn resize(&self, size: PhysicalSize<u32>) {
        let video_sink = self.video_renderer.clone();
        // self.rt.spawn (not tokio::spawn) so this works when called from a host
        // thread outside the runtime (iOS UIKit layout / Android JNI).
        self.rt.spawn(async move {
            video_sink.resize(size).await;
        });
    }

    fn change_frame_size(&self, size: PhysicalSize<u32>) {
        let video_sink = self.video_renderer.clone();
        self.rt.spawn(async move {
            video_sink.change_frame_size(size).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Callback fired by `download_task` after each successful segment
/// download, passed the segment's end-PTS in milliseconds. Both
/// `video_play` and `audio_play` plug this in to push the
/// `Position.buffered_ahead_secs` gauge forward as media lands in the
/// local download buffer — rather than only when the decoder finally
/// gets around to producing a frame.
type SegmentDoneCallback = Arc<dyn Fn(i64) + Send + Sync>;

async fn download_task(
    segments: Vec<Segment>,
    start_index: usize,
    segment_sender: Sender<DataSegment>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    http: Arc<HttpClient>,
    stats: Option<Arc<StatsState>>,
    on_segment_done: Option<SegmentDoneCallback>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    /// How long `download_task` keeps retrying a single failing
    /// segment before giving up and ending the pipeline. The inner
    /// `HttpClient::get` already does ~3 short retries (~2 s); this
    /// outer cap covers extended outages, e.g. a Wi-Fi drop while a
    /// movie is playing. Long enough for a typical reconnect, short
    /// enough that the player doesn't sit silently forever when the
    /// network is genuinely gone.
    const SEGMENT_RETRY_TOTAL: Duration = Duration::from_secs(30);

    let segment_slice = &segments[..];
    for i in start_index..segment_slice.len() {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let seg = &segment_slice[i];
        let mut backoff = Duration::from_millis(500);
        // Outer retry loop: keep trying the same segment until it
        // succeeds, the user stops playback, the seek target changes,
        // or `SEGMENT_RETRY_TOTAL` elapses. Previously download_task
        // broke out on the first error — so a brief network blip tore
        // the whole pipeline down. Now a Wi-Fi blip rides through
        // transparently, and only a genuine extended outage ends the
        // pipeline.
        let retry_started = Instant::now();
        let started = retry_started;
        let mut should_break = false;
        let mut last_err: Option<Box<dyn Error + Send + Sync>> = None;
        loop {
            if stop_flag.load(Ordering::Relaxed) {
                should_break = true;
                break;
            }
            if retry_started.elapsed() > SEGMENT_RETRY_TOTAL {
                log::error!(
                    "[dl] segment {} gave up after {:?} of retries: {}",
                    i,
                    SEGMENT_RETRY_TOTAL,
                    last_err
                        .as_ref()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "(no error captured)".to_string())
                );
                should_break = true;
                break;
            }
            let sender = segment_sender.clone();
            let outcome = tokio::select! {
                res = download_and_queue(i, seg, sender, &http, stats.as_ref()) => Some(res),
                _ = stop.notified() => None,
            };
            match outcome {
                Some(Ok(())) => {
                    log::debug!("[dl] produced segment {}", i);
                    if let Some(cb) = &on_segment_done {
                        cb(seg.end_time().as_millis() as i64);
                    }
                    break;
                }
                Some(Err(e)) => {
                    log::warn!(
                        "[dl] segment {} failed, retrying in {:?}: {}",
                        i, backoff, e
                    );
                    last_err = Some(e);
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = stop.notified() => {
                            should_break = true;
                            break;
                        }
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(8));
                }
                None => {
                    should_break = true;
                    break;
                }
            }
        }
        if let Some(s) = stats.as_ref() {
            // Anything longer than ~250 ms blocked on a single segment counts
            // as net stall — the decoder downstream is starving.
            let elapsed_ms = started.elapsed().as_millis() as u64;
            if elapsed_ms > 250 {
                s.net_stall_ms.fetch_add(elapsed_ms - 250, Ordering::Relaxed);
            }
        }
        if should_break {
            break;
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct DataSegment {
    id: usize,
    data: Vec<u8>,
}

fn log_task_result<T, E: std::fmt::Display>(
    name: &str,
    result: Result<Result<T, E>, tokio::task::JoinError>,
) {
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => log::error!("{}: {}", name, e),
        Err(e) => log::error!("{}: join error: {}", name, e),
    }
}

fn decrypt_segment_in_place(
    data_vec: &mut Vec<u8>,
    track_crypto: Option<&TrackCrypto>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let tc = match track_crypto {
        Some(t) => t,
        None => return Ok(()),
    };

    // No senc box = this segment is clear, even though the track's `tenc`
    // box advertises encryption. Common Encryption explicitly supports
    // mixed-protection tracks — some samples / segments in the clear,
    // others encrypted with the cached KID. Skipping decryption is the
    // correct behaviour here; treating it as fatal stalled playback the
    // first time a clear segment landed.
    let senc_entries = match parse_senc(data_vec, tc.iv_size) {
        Some(e) => e,
        None => {
            log::debug!(
                "[crypto] no senc in segment — treating as clear (track kid={})",
                kid_short(&tc.kid)
            );
            return Ok(());
        }
    };

    let sample_ranges: Vec<(usize, usize)> = {
        let mp4 = Mp4::read_bytes(&data_vec[..])
            .map_err(|e| format!("Decrypt: mp4 parse error {}", e))?;
        let (_id, track) = mp4
            .tracks()
            .first_key_value()
            .ok_or("Decrypt: no track in segment")?;
        track
            .samples
            .iter()
            .map(|s| (s.offset as usize, s.size as usize))
            .collect()
    };

    for ((offset, size), entry) in sample_ranges.iter().zip(senc_entries.iter()) {
        let end = offset + size;
        if end > data_vec.len() {
            continue;
        }
        // Per-sample "clear" entries also exist within an encrypted senc:
        //   - IV all-zeros AND no subsamples → sample is clear
        //   - subsamples list present but every entry has encrypted=0
        // In both cases applying the keystream is a no-op anyway (CTR with
        // IV=0 still XORs against a real keystream, breaking the data), so
        // we must detect and skip.
        let iv_is_zero = entry.iv.iter().all(|&b| b == 0);
        let no_encrypted_bytes = !entry.subsamples.is_empty()
            && entry.subsamples.iter().all(|&(_, enc)| enc == 0);
        if iv_is_zero || no_encrypted_bytes {
            continue;
        }
        tc.decryptor
            .decrypt_sample(&tc.kid, &entry.iv, &mut data_vec[*offset..end], &entry.subsamples)?;
    }
    Ok(())
}

/// Build a `TrackInfo` snapshot from a video representation. Used by
/// `TrackChanged` events on both user-driven and ABR-driven switches.
fn video_track_info(repr: &VideoRepresenation) -> TrackInfo {
    TrackInfo {
        representation_id: repr.id,
        codec: repr.codec_short().to_string(),
        bitrate_bps: repr.bandwidth,
        width: Some(repr.width),
        height: Some(repr.height),
        fps: None,
        channels: None,
        sample_rate_hz: None,
        language: None,
        label: repr.label(),
        hdr10: repr.is_hdr10(),
        dolby_vision: repr.is_dolby_vision(),
    }
}

fn find_segment_index(segments: &[Segment], target: Duration) -> usize {
    if segments.is_empty() {
        return 0;
    }
    for (i, seg) in segments.iter().enumerate() {
        if seg.end_time() > target {
            return i;
        }
    }
    segments.len() - 1
}

async fn download_and_queue(
    index: usize,
    segment: &Segment,
    sender: Sender<DataSegment>,
    http: &HttpClient,
    stats: Option<&Arc<StatsState>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let dl = segment
        .download(http, RequestKind::Segment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> {
            format!("segment download: {}", e).into()
        })?;
    if let Some(s) = stats {
        update_bandwidth_ewma(&s.bandwidth_bps_ewma, dl.data.len(), dl.elapsed);
    }
    let data_segment = DataSegment {
        id: index,
        data: dl.data,
    };
    if let Err(e) = sender.send(data_segment).await {
        return Err(format!("downstream receiver dropped: {:?}", e).into());
    }
    Ok(())
}

/// EWMA with smoothing factor ~1/8 (last 8 samples weight equivalent). The
/// instantaneous rate per segment is `bytes * 8 / elapsed_secs`; we fold it
/// into the running estimate so single fast/slow segments don't whipsaw
/// ABR. `Relaxed` is fine — readers tolerate stale-by-one values.
fn update_bandwidth_ewma(ewma: &AtomicU64, bytes: usize, elapsed: Duration) {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 || bytes == 0 {
        return;
    }
    let instant_bps = (bytes as f64 * 8.0 / secs) as u64;
    let prev = ewma.load(Ordering::Relaxed);
    let next = if prev == 0 {
        instant_bps
    } else {
        // alpha = 1/8
        ((prev as u128 * 7 + instant_bps as u128) / 8) as u64
    };
    ewma.store(next, Ordering::Relaxed);
}
