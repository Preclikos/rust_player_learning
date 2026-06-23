mod abr;
mod capabilities;
mod crypto;
mod decoders;
mod events;
mod ffmpeg_log;
mod hdr_tonemap;
mod manifest;
mod net;
mod parsers;
mod renderers;
mod subtitle_style;
mod tracks;
mod utils;

// Public re-exports so downstream consumers (BlackZone Console etc.) can
// implement RequestInterceptor / LicenseResolver against the player's
// canonical types — see PLAYER_INTEGRATION.md.
pub use abr::{AbrStrategy, AbrVideoProfile};
pub use capabilities::{capabilities, probe_capabilities, PlayerCapabilities};
pub use events::{
    BufferingReason, Fps, PlayerErrorKind, PlayerEvent, TrackInfo, TrackKind,
};
pub use ffmpeg_log::{set_log_level, LogLevel};
pub use hdr_tonemap::HdrTonemapParams;
pub use subtitle_style::SubtitleStyle;
pub use net::{
    tls_client, BoxError, HttpClient, LicenseResolver, NoopInterceptor, PreparedRequest,
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
    kid_short, parse_aac_config, parse_hvcc_bit_depth, parse_hvcc_nalus, parse_senc, parse_tenc,
    ClearKeyDecryptor,
    Decryptor, TrackCrypto,
};
use decoders::{
    AudioCodec, AudioDecoder, AudioDecoderParams, DecodedAudioFrame, DecodedVideoFrame,
    HwVideoDecoder, VideoCodec, VideoColorInfo, VideoDecoderParams,
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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
    /// Measured A/V clock drift in ms: how far the video wall clock has
    /// run ahead of the audio device clock since this pipeline started
    /// (negative = audio ahead). Written ~1 Hz by video_sync_loop when
    /// the sink supports `played_ms`; surfaced via PlayerEvent::Stats.
    /// The device crystal vs CLOCK_MONOTONIC disagree by 10–100 ppm, so
    /// multi-hour sessions are expected to show a slow linear trend —
    /// this is the measurement that tells us when an active servo is
    /// warranted on a given device class.
    av_drift_ms: std::sync::atomic::AtomicI64,
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

    /// HDR / bit-depth filter applied to ABR-eligible representations.
    /// `Adaptive` by default (no filtering). Mutated by
    /// `set_abr_video_profile`. Orthogonal to `abr_strategy` — `Manual`
    /// ignores this profile, but `BandwidthEwma` consults it before
    /// running the bitrate selector.
    abr_video_profile: Arc<ArcSwap<AbrVideoProfile>>,

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

    /// Android direct mode: the dedicated video-plane `ANativeWindow` the
    /// decoder renders into (0 = classic renderer path). Set by the host before
    /// play(); consumed at pipeline build. Wrapped in [`DirectWindow`] so we
    /// hold an acquired ref for the player's lifetime (the AFR/Surface UAF).
    video_output_window: Arc<DirectWindow>,

    /// Adaptive frame rate (Android direct mode): when set, the player hints
    /// the video plane's content fps to the OS via `ANativeWindow_setFrameRate`
    /// so the display can switch to a matching refresh rate (24 -> 24/48/120
    /// Hz) and avoid judder. Default on; the host can disable it via
    /// `set_adaptive_frame_rate(false)` to own display-mode policy itself.
    adaptive_frame_rate: Arc<std::sync::atomic::AtomicBool>,
    /// Audio passthrough (bitstream): when enabled AND the platform output
    /// supports it AND the selected track is a passthrough codec (E-AC-3 /
    /// AC-3 / DTS), the compressed audio is sent to the device's audio sink
    /// untouched (HDMI → AVR/soundbar decodes it) instead of being decoded to
    /// PCM. Default OFF — the host opts in via `set_audio_passthrough(true)`,
    /// and it transparently falls back to PCM decode when unsupported.
    audio_passthrough: Arc<std::sync::atomic::AtomicBool>,

    /// True once the current pipeline has produced its first frame (set in
    /// av_sync_handler after video_ready, reset to false on every pipeline
    /// (re)build). The ABR tick consults it so the FIRST auto-switch can't
    /// fire into a just-started / resuming pipeline — that rebuild-on-top-of-a-
    /// rebuild is what collided with resume and could stall the direct-mode
    /// MediaCodec. An event-based gate (first frame landed), not a timer.
    pipeline_live: Arc<std::sync::atomic::AtomicBool>,

    /// Set when a play() ends on exhausted pipeline retries: the position
    /// the NEXT play() resumes from, so the consumer's manual retry
    /// continues where playback stopped instead of starting over.
    pending_resume: Arc<StdMutex<Option<Duration>>>,

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
            abr_video_profile: Arc::clone(&self.abr_video_profile),
            video_switch_tx: Arc::clone(&self.video_switch_tx),
            buffer_target_secs: Arc::clone(&self.buffer_target_secs),
            subtitle_representation: Arc::clone(&self.subtitle_representation),
            video_output_window: Arc::clone(&self.video_output_window),
            adaptive_frame_rate: Arc::clone(&self.adaptive_frame_rate),
            audio_passthrough: Arc::clone(&self.audio_passthrough),
            pipeline_live: Arc::clone(&self.pipeline_live),
            pending_resume: Arc::clone(&self.pending_resume),
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

/// Playback master clock — 0-based media time, audio-disciplined.
///
/// Mastered by the audio sink's real playback position so video (and any other
/// renderer) cannot drift from audio (crystal mismatch / underruns). Between
/// the sink's coarse position updates it interpolates with the wall clock for
/// smooth pacing; when the sink reports no position (mocks / not-yet-started)
/// it falls back to the wall clock. Rebased onto THIS pipeline's 0-based
/// timeline (media = seek_offset + (played − audio_base)) so the reported
/// position is absolute, not the audio device's free-running counter.
///
/// The audio sink is the only clock source today and the seam for tomorrow: an
/// AudioTrack passthrough sink reports `played_ms` via getTimestamp, so the
/// clock serves bitstream (Dolby/DTS passthrough) and multichannel without
/// video / subtitles knowing the difference. Shareable (interior-mutable
/// anchor) so future consumers can pace to the same clock.
struct MediaClock<A: AudioSink> {
    audio_sink: Arc<A>,
    // Wall anchor (= now − seek_offset): the fallback when the sink has no clock.
    start_time: Arc<Instant>,
    // seek_offset_us − audio_base_us: rebases the cumulative audio counter onto
    // this pipeline's 0-based timeline. Constant per pipeline; cancels out of
    // the frame-pacing delta, so it only affects the *reported* position.
    rebase_us: i64,
    // (last observed played_ms, wall instant then) for sub-update interpolation.
    anchor: std::sync::Mutex<Option<(u64, Instant)>>,
}

impl<A: AudioSink> MediaClock<A> {
    fn new(
        audio_sink: Arc<A>,
        start_time: Arc<Instant>,
        seek_offset: Duration,
        audio_base_ms: u64,
    ) -> Self {
        Self {
            audio_sink,
            start_time,
            rebase_us: seek_offset.as_micros() as i64 - audio_base_ms as i64 * 1_000,
            anchor: std::sync::Mutex::new(None),
        }
    }

    /// Audio-disciplined position (µs), or None when the sink reports no clock.
    /// `played_ms` advances at the device rate and freezes on pause/starvation,
    /// so it already subsumes pause skew; `output_latency_ms` folds in so the
    /// picture lands when its audio is audible, not merely consumed.
    fn audio_now_us(&self) -> Option<i64> {
        let played = self.audio_sink.played_ms()?;
        let lat_us = self.audio_sink.output_latency_ms() as i64 * 1_000;
        let now = Instant::now();
        let mut anchor = self.anchor.lock().unwrap();
        let (p0, w0) = match *anchor {
            Some((p0, w0)) if played <= p0 => (p0, w0),
            _ => {
                *anchor = Some((played, now));
                (played, now)
            }
        };
        let since = now.duration_since(w0);
        // Interpolate with wall time between the sink's ~per-callback updates;
        // if it hasn't ticked for >80ms the audio is paused/starving — freeze.
        let pos_us = if since < Duration::from_millis(80) {
            p0 as i64 * 1_000 + since.as_micros() as i64
        } else {
            p0 as i64 * 1_000
        };
        Some((pos_us + self.rebase_us - lat_us).max(0))
    }

    /// Current 0-based media time (µs): audio when available, else the wall
    /// clock. Only the wall fallback applies `pause_skew` — the audio clock
    /// freezes during pause on its own.
    fn now_us(&self, pause_skew: Duration) -> i64 {
        if let Some(us) = self.audio_now_us() {
            return us;
        }
        self.start_time
            .elapsed()
            .saturating_sub(pause_skew)
            .saturating_sub(Duration::from_millis(self.audio_sink.output_latency_ms()))
            .as_micros() as i64
    }
}

async fn video_sync_loop<V: VideoSink, A: AudioSink>(
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
    // Snapshot of the audio sink's cumulative `played_ms` at this pipeline's
    // anchor instant. `samples_consumed` is NOT reset on flush(), so played_ms
    // is session-cumulative; subtracting this baseline yields the per-pipeline
    // elapsed, to which `seek_offset` is added → absolute media time.
    audio_base_ms: u64,
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
    // BMDT (base media decode time) offset: DASH content timestamps are absolute
    // (e.g. ~7979ms for segment 0) while our wall clock starts at 0. Calibrated
    // on the first frame as (raw_pts - elapsed), making frame 0 render immediately
    // and all subsequent frames at their correct relative positions.
    let mut pts_base: Option<u64> = None;
    // De-judder smoother state: (present_ns, pts_us_rel) of the last frame, so
    // the next frame's present time can be snapped to the smooth media cadence
    // (last + Δpts) instead of inheriting the audio clock's frame-to-frame
    // wobble. Bounded to ±PRESENT_SMOOTH_NS of the raw value (see present block).
    let mut last_present: Option<(i64, i64)> = None;
    // Max deviation of the smoothed present time from the raw `now + (pts −
    // clock)` value. ≥ the audio clock's per-callback quantization (~one cpal
    // buffer) so steady-state wobble is fully absorbed, yet small enough that a
    // seek / LATE / resume jump (raw moves further than this) is followed at
    // once and the cadence re-bases — so this can never schedule meaningfully
    // further from reality than the proven raw formula already did.
    const PRESENT_SMOOTH_NS: i64 = 40_000_000;
    // Playback master clock: audio-disciplined, 0-based, rebased to this
    // pipeline's timeline. Video paces to it; the same seam serves passthrough
    // / multichannel / other renderers (see MediaClock).
    let clock = MediaClock::new(
        audio_sink.clone(),
        start_time.clone(),
        seek_offset,
        audio_base_ms,
    );
    log::info!("[vsync gen {}] loop start (seek_offset={}ms)", gen, seek_offset.as_millis());
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
        // Audio output latency (device buffer + DAC): the sink consumes a
        // sample this many ms before it's audible. Subtract it from the
        // video clock so a frame reaches the screen exactly when its audio
        // reaches the speaker — without it video leads audio by the output
        // latency at every (re)start, which surfaces as "audio delayed"
        // after seeks/track-switches. Re-read each iteration: it's ~0 until
        // the first callback, then stabilises, so it applies as a one-time
        // hold early in playback. `pts_ms` itself stays the frame's true
        // media time, so subtitles (keyed off pts_ms) and the picture move
        // together against this same audio-anchored clock.
        // Master clock — audio-disciplined, wall fallback. See MediaClock.
        let elapsed = (clock.now_us(pause_skew) / 1_000) as u64;

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
            let target_wake_ms = pts_ms.saturating_sub(render_budget_ms);
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
        let elapsed_us = clock.now_us(pause_skew);
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
        last_present = Some((present_ns, pts_us_rel));
        frame.desired_present_ns = present_ns;

        // DIAG (#23): first few frames' pacing — tells us whether the direct
        // pipeline renders promptly (releasing codec buffers) or schedules
        // present far in the future (buffers stay captive → dequeue_input stall).
        if frame_idx < 3 {
            log::debug!(
                "[vsync] frame #{} pts_ms={} elapsed_ms={} pts_rel_ms={} pts_to_go_ms={} base={}",
                frame_idx, pts_ms, elapsed_us / 1000, pts_us_rel / 1000,
                pts_to_go_ns / 1_000_000, base
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
        if let Some(played) = audio_sink.played_ms() {
            match drift_baseline {
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

        // DIAG: per-frame pacing. interval_ms = wall ms between renders (~41 at
        // 24fps; jitter here = judder), elapsed = master clock, pts_to_go = the
        // scheduled lead. Reveals clock-jitter judder that doesn't trip LATE.
        if frame_idx % 60 == 0 {
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
            let _ = events.send(PlayerEvent::Stats {
                video_frames_decoded: decoded_total,
                video_frames_dropped: dropped_total,
                audio_underruns: stats.audio_underruns.load(Ordering::Relaxed),
                net_stall_ms: net_stall,
                decoder_name,
                current_resolution: Some((frame_w, frame_h)),
                audio_peak_db: audio_sink.last_peak_db(),
                av_drift_ms: drift_out,
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
    // DIAG: pipeline generation id (see video_sync_loop).
    gen: u64,
    seek_offset: Duration,
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
        _ = tokio::time::sleep(Duration::from_secs(3)) => {
            log::warn!("[vsync] audio not ready after 3s — starting playback without waiting (guards the direct-mode video codec against a backpressure stall)");
        }
        _ = stop.notified() => {
            stop.notify_waiters();
            return;
        }
    }
    // Unpause the audio device. AudioRenderer starts paused at construction
    // (cpal would otherwise pull from an empty mpsc and play silence while
    // the audio decoder warmed up, then "catch up" once real samples
    // arrived). seek() re-pauses + flushes; this restores playback.
    if !paused.load(Ordering::Relaxed) {
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
    if !paused.load(Ordering::Relaxed) && audio_sink.played_ms().is_some() {
        let gate = Instant::now();
        while audio_sink.played_ms().unwrap_or(1) == 0 {
            if gate.elapsed() > Duration::from_millis(500) {
                log::debug!("[vsync] audio-start gate timed out; anchoring anyway");
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(4)) => {}
                _ = stop.notified() => {
                    stop.notify_waiters();
                    return;
                }
            }
        }
    }
    let now = Instant::now();
    let start_time = Arc::new(now.checked_sub(seek_offset).unwrap_or(now));
    // Baseline the cumulative audio clock at the SAME instant as start_time so
    // the video sync loop can rebase played_ms onto this pipeline's 0-based
    // media timeline (position = seek_offset + (played - audio_base)).
    // Passthrough's AudioTrack played_ms is ALREADY 0-based from this
    // pipeline's start (fresh track playing seek_offset content from 0), and
    // it has typically been playing for the video-startup duration by now — so
    // its base is 0, not the (late, non-zero) anchor snapshot, which would make
    // the clock lag by that pre-anchor playback (audio running seconds ahead).
    let audio_base_ms = if audio_sink.is_passthrough() {
        0
    } else {
        audio_sink.played_ms().unwrap_or(0)
    };
    log::debug!("[av_sync] spawning sync loops (audio_base={}ms)", audio_base_ms);
    let stats_audio = Arc::clone(&stats);
    let events_audio = Arc::clone(&events);
    let paused_audio = Arc::clone(&paused);
    let (_, _) = tokio::join!(
        tokio::spawn(video_sync_loop(
            gen,
            start_time.clone(),
            seek_offset,
            audio_base_ms,
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
    // Local stop signal so the supervisor can abort the decoder MID-QUEUE
    // on an ABR swap. Without this, decoder_task drains every segment
    // already in download_rx before exiting — at 1× consumption that's
    // up to `segments_in_flight` × segment_duration ≈ 24 s of OLD content
    // playing after the swap request, then a PTS jump back to NEW's
    // start. Reacting to stop here cuts the swap-to-NEW-frame latency
    // to one in-flight segment + the few frames already buffered in
    // `sender`.
    stop_flag: Arc<AtomicBool>,
    // `Some(..)` on an ABR-swap pipeline: trims NEW's overlap with OLD's tail
    // (frames at/below `skip_below_pts_us`) so the splice is forward-contiguous,
    // and stamps the first-frame-after-teardown timing log. `None` initially.
    splice: Option<SwapSplice>,
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
    // Direct mode holds codec output buffers captive in this window — keep
    // it shallow there (see the frame-channel capacity comment in play()).
    let reorder_depth: usize = if decoder.is_direct() { 2 } else { 4 };
    let mut reorder_buf: Vec<DecodedVideoFrame> = Vec::with_capacity(reorder_depth + 1);

    // Segment preparation (init concat + CENC decrypt + mp4 parse) runs on
    // a BLOCKING thread, overlapped with feeding the previous segment.
    // Software AES on 32-bit devices costs ~0.5 s per 4K segment — done
    // inline (the old shape) that was a guaranteed codec starvation +
    // LATE drain at every segment boundary; overlapped it disappears into
    // the ~6 s feed window of the segment before it.
    struct PreparedSegment {
        id: usize,
        data_vec: Vec<u8>,
        sample_info: Vec<(usize, usize, i64, u64)>,
    }
    let init_data = Arc::new(init_data);
    let prepare = {
        let init_data = Arc::clone(&init_data);
        let crypto = track_crypto.clone();
        move |segment: DataSegment| {
            let init_data = Arc::clone(&init_data);
            let crypto = crypto.clone();
            tokio::task::spawn_blocking(
                move || -> Result<PreparedSegment, Box<dyn Error + Send + Sync>> {
                    let mut data_vec =
                        Vec::with_capacity(init_data.len() + segment.data.len());
                    data_vec.extend_from_slice(&init_data);
                    data_vec.extend_from_slice(&segment.data[..]);
                    decrypt_segment_in_place(&mut data_vec, crypto.as_ref())?;
                    let sample_info: Vec<(usize, usize, i64, u64)> = {
                        let mp4 = Mp4::read_bytes(&data_vec).map_err(
                            |e| -> Box<dyn Error + Send + Sync> { format!("mp4: {}", e).into() },
                        )?;
                        let (_id, track) = mp4.tracks().first_key_value().ok_or_else(
                            || -> Box<dyn Error + Send + Sync> { "no track".into() },
                        )?;
                        track
                            .samples
                            .iter()
                            .map(|s| {
                                (
                                    s.offset as usize,
                                    s.size as usize,
                                    s.composition_timestamp,
                                    s.timescale,
                                )
                            })
                            .collect()
                    };
                    Ok(PreparedSegment {
                        id: segment.id,
                        data_vec,
                        sample_info,
                    })
                },
            )
        }
    };

    let mut pending_prepare: Option<
        tokio::task::JoinHandle<Result<PreparedSegment, Box<dyn Error + Send + Sync>>>,
    > = None;
    loop {
        let boundary_t0 = Instant::now();
        let prepared = match pending_prepare.take() {
            // Steady state: the next segment was prepared while the
            // previous one fed — this await is ~0.
            Some(handle) => handle
                .await
                .map_err(|e| -> Box<dyn Error + Send + Sync> {
                    format!("prepare task: {}", e).into()
                })??,
            // Startup / buffer-dry case: wait for a download, prepare it
            // (nothing to overlap with).
            None => {
                let Some(segment) = receiver.recv().await else {
                    break;
                };
                if stop_flag.load(Ordering::Relaxed) {
                    log::debug!("[dec] stop signal received between segments; aborting drain");
                    break;
                }
                prepare(segment)
                    .await
                    .map_err(|e| -> Box<dyn Error + Send + Sync> {
                        format!("prepare task: {}", e).into()
                    })??
            }
        };
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        // Kick off the NEXT segment's preparation before feeding this one
        // — downloads run ~4 segments ahead, so it's normally buffered.
        if let Ok(next) = receiver.try_recv() {
            pending_prepare = Some(prepare(next));
        }
        let boundary_ms = boundary_t0.elapsed().as_millis();
        if boundary_ms > 50 {
            log::info!(
                "[dec] segment {} boundary stall {}ms ({} samples, {} KiB)",
                prepared.id, boundary_ms,
                prepared.sample_info.len(), prepared.data_vec.len() / 1024
            );
        }
        log::debug!("[dec] consuming video segment: {}", prepared.id);
        let data_vec = prepared.data_vec;
        let sample_info = prepared.sample_info;

        let mut first_pts_us: Option<i64> = None;
        let mut last_pts_us: i64 = 0;
        for (offset, size, ts, ts_scale) in sample_info {
            // Check stop_flag INSIDE the segment-processing loop too.
            // Without this, an ABR swap fired mid-segment would wait for
            // the current segment's frames to finish pacing through the
            // 8-slot frame_sender at 1× (~6 s for a 144-frame segment),
            // and only then notice the supervisor's stop signal. That's
            // the "60 s before ABR takes effect" the user reported.
            // Bailing mid-segment loses the rest of this segment but
            // keeps the swap responsive.
            if stop_flag.load(Ordering::Relaxed) {
                log::debug!("[dec] stop signal received mid-segment; aborting drain");
                return Ok(());
            }
            if offset + size > data_vec.len() {
                continue;
            }
            let sample_data = &data_vec[offset..offset + size];
            let pts_us = if ts_scale > 0 { ts * 1_000_000 / ts_scale as i64 } else { 0 };
            if first_pts_us.is_none() { first_pts_us = Some(pts_us); }
            last_pts_us = pts_us;

            // Drain the frames the codec ALREADY has ready BEFORE feeding it
            // more input. A direct MediaCodec whose output buffers are all
            // dequeued-but-undrained refuses new input — submit() then spins on
            // a full codec while the very frames that would free it sit waiting
            // to be pulled, so the old feed-then-drain order deadlocked once the
            // pool filled (#23 backpressure variant: produced>0, dequeue_input
            // stall forever, video starves). Draining first keeps the codec's
            // output pool flowing so submit doesn't wedge.
            if !drain_video_decoder(
                &mut decoder,
                &mut reorder_buf,
                reorder_depth,
                &splice,
                &mut first_frame_signaled,
                &video_ready,
                &stats,
                &sender,
                &stop_flag,
            )
            .await?
            {
                return Ok(());
            }

            decoder.submit(sample_data, pts_us)?;
        }
        if let Some(first) = first_pts_us {
            log::info!("[dec] seg done: pts {}..{}ms", first / 1000, last_pts_us / 1000);
        }
    }

    // Final drain: with drain-before-submit, the last submitted sample's output
    // is still inside the codec — pull it before the reorder flush so the tail
    // frames aren't lost.
    if !drain_video_decoder(
        &mut decoder,
        &mut reorder_buf,
        reorder_depth,
        &splice,
        &mut first_frame_signaled,
        &video_ready,
        &stats,
        &sender,
        &stop_flag,
    )
    .await?
    {
        return Ok(());
    }

    // Flush remaining frames in PTS order.
    reorder_buf.sort_by_key(|f| f.pts_us);
    for frame in reorder_buf.drain(..) {
        if let Some(sp) = &splice {
            if frame.pts_us <= sp.skip_below_pts_us {
                continue;
            }
        }
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

/// Pull every frame the video decoder currently has ready into the reorder
/// buffer and forward the lowest-PTS ones downstream. MUST run before each
/// `submit()` in the decode loop (direct mode): a MediaCodec whose output
/// buffers are all dequeued-but-undrained refuses new input, so feeding before
/// draining deadlocks. Returns `false` when the pipeline should stop (the
/// frame channel closed, or a teardown was signalled mid-send).
async fn drain_video_decoder(
    decoder: &mut Box<dyn HwVideoDecoder>,
    reorder_buf: &mut Vec<DecodedVideoFrame>,
    reorder_depth: usize,
    splice: &Option<SwapSplice>,
    first_frame_signaled: &mut bool,
    video_ready: &Arc<Notify>,
    stats: &Arc<StatsState>,
    sender: &Sender<DecodedVideoFrame>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    loop {
        match decoder.try_recv()? {
            Some(frame) => {
                // Track the high-water-mark of decoded PTS so av_sync can
                // publish `buffered_ahead_secs` (reorder-buffered frames are
                // already decoded and render shortly).
                let pts_ms = frame.pts_us / 1000;
                let prev = stats.last_decoded_pts_ms.load(Ordering::Relaxed);
                if pts_ms > prev {
                    stats.last_decoded_pts_ms.store(pts_ms, Ordering::Relaxed);
                }
                reorder_buf.push(frame);
                // Startup fast-path: emit the very first frame as soon as it
                // is decoded instead of waiting for the reorder buffer to fill
                // (reorder_depth + 1 frames). At a cold start / post-seek the
                // first decoded frame is the segment-start IDR keyframe — the
                // lowest PTS in its GOP — so emitting it early can't reorder
                // ahead of an earlier frame. This is exactly the wait that
                // gates time-to-first-frame (video_ready → av_sync). Restricted
                // to `splice.is_none()`: an ABR swap mid-play needs the full
                // reorder discipline (its first kept frame is mid-GOP, not an
                // IDR) and doesn't show startup latency anyway.
                let ready = if !*first_frame_signaled && splice.is_none() {
                    !reorder_buf.is_empty()
                } else {
                    reorder_buf.len() > reorder_depth
                };
                if ready {
                    let min_idx = reorder_buf
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, f)| f.pts_us)
                        .map(|(i, _)| i)
                        .unwrap();
                    let to_send = reorder_buf.swap_remove(min_idx);
                    // ABR splice trim: drop NEW frames at/below the PTS OLD last
                    // rendered so NEW joins forward-contiguous (no rewind, no
                    // future-PTS frame av_sync would sit waiting for).
                    if let Some(sp) = splice {
                        if to_send.pts_us <= sp.skip_below_pts_us {
                            continue;
                        }
                    }
                    if !*first_frame_signaled {
                        video_ready.notify_one();
                        *first_frame_signaled = true;
                        if let Some(sp) = splice {
                            log::info!(
                                "[abr] NEW first frame {}ms after OLD teardown (pts={}ms)",
                                sp.started.elapsed().as_millis(),
                                to_send.pts_us / 1000
                            );
                        }
                    }
                    if sender.send(to_send).await.is_err() {
                        return Ok(false);
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        log::debug!("[dec] stop signal received after send; aborting");
                        return Ok(false);
                    }
                }
            }
            None => break,
        }
    }
    Ok(true)
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

/// The download-side product of one representation's pipeline, ready to be
/// handed to [`run_decode`]. Produced by [`video_prefetch`].
///
/// Splitting the pipeline into a download half (this) and a decode half lets
/// the ABR supervisor fetch the NEW representation's first segments *while the
/// OLD representation's decoder is still running* — the OLD HW decoder slot is
/// untouched, so there's no second-instance allocation conflict, and av_sync
/// keeps getting OLD frames the whole time. Only once NEW is buffered locally
/// does OLD tear down and NEW's decode start, so the gap that used to trip the
/// 300 ms starvation pause (the visible buffering freeze) collapses to a fast
/// local configure + first-GOP decode.
struct VideoPrefetch {
    width: u32,
    height: u32,
    init_data: Vec<u8>,
    hvcc_nalus: Vec<Vec<u8>>,
    color: VideoColorInfo,
    dovi_profile: Option<u8>,
    track_crypto: Option<TrackCrypto>,
    download_rx: mpsc::Receiver<DataSegment>,
    download_handle: JoinHandle<Result<(), Box<dyn Error + Send + Sync>>>,
    /// Fired (once) when `prime_target` segments have been buffered into
    /// `download_rx`. The supervisor awaits this before tearing OLD down.
    primed: Arc<Notify>,
}

/// Download half of a video pipeline: fetch + parse the init segment, resolve
/// the CENC key, and spawn [`download_task`] streaming media segments into a
/// bounded channel. Touches the network only — never the HW decoder — so it is
/// safe to run concurrently with another representation's live decoder.
#[allow(clippy::too_many_arguments)]
async fn video_prefetch(
    repr: &VideoRepresenation,
    start_index: usize,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    segments_in_flight: usize,
    soft_end_exclusive: Arc<AtomicUsize>,
    // Notify `primed` once this many segments are buffered. `usize::MAX` means
    // "never signal" — used for the initial pipeline, which has no OLD to
    // overlap and so decodes immediately.
    prime_target: usize,
) -> Result<VideoPrefetch, Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(segments_in_flight);

    let init_dl = repr
        .segment_init
        .download(&http, RequestKind::InitSegment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("init download: {}", e).into() })?;
    let init_data = init_dl.data;

    let hvcc_nalus = parse_hvcc_nalus(&init_data)
        .ok_or_else(|| -> Box<dyn Error + Send + Sync> { "no hvcC in init segment".into() })?;

    // Dolby Vision policy: profiles 7/8 carry a decodable HEVC base layer
    // (HDR10/SDR/HLG-compatible, correctly signalled in the SPS VUI), so
    // they can always play through the normal Main10 + tonemap path with
    // the RPU NAL dropped. On Android the decoder additionally tries the
    // platform `video/dolby-vision` codec (full DV, RPU kept) and only
    // falls back to the base layer. Profile 5 (IPTPQc2) has NO compatible
    // base layer — it is only playable where a real DV decoder exists
    // (Android direct mode); elsewhere refuse up front with an actionable
    // error instead of rendering green/purple garbage.
    let dovi_profile = crate::crypto::parse_dovi_config(&init_data).map(|dovi| {
        log::info!(
            "video: Dolby Vision profile {}.{} (rpu={} el={} bl={} compat_id={})",
            dovi.profile,
            dovi.level,
            dovi.rpu_present,
            dovi.el_present,
            dovi.bl_present,
            dovi.bl_signal_compatibility_id
        );
        if dovi.el_present {
            log::warn!(
                "video: DV enhancement layer present — ignored unless the platform DV decoder picks it up"
            );
        }
        dovi.profile
    });
    if let Some(p) = dovi_profile {
        if !matches!(p, 7 | 8) && !cfg!(target_os = "android") {
            return Err(format!(
                "Dolby Vision profile {} has no backward-compatible base layer \
                 (needs a platform DV decoder) — unsupported on this target",
                p
            )
            .into());
        }
    }

    // Colour info comes from the SPS VUI — the MPD is not trustworthy here
    // (our test stream signals BT.709 on PQ representations). Fall back to
    // the hvcC bit depth when the SPS doesn't parse.
    let sps_color = crate::parsers::hevc::parse_sps_color_info(&hvcc_nalus);
    let color = VideoColorInfo::from_sps(sps_color, parse_hvcc_bit_depth(&init_data));
    if color.bit_depth != 8 || color.is_hdr() {
        log::info!(
            "video: {}-bit, transfer={:?}, bt2020={}, full_range={} (SPS VUI {})",
            color.bit_depth,
            color.transfer,
            color.bt2020,
            color.full_range,
            if sps_color.is_some() { "parsed" } else { "missing — hvcC fallback" },
        );
    }

    let track_crypto = setup_track_crypto(&init_data, decryptor, "video").await?;

    let segments = repr.segments.clone();
    let primed = Arc::new(Notify::new());
    let dl_stats = Arc::clone(&stats);
    let cb_primed = Arc::clone(&primed);
    let downloaded = Arc::new(AtomicUsize::new(0));
    // Only the initial / OLD pipeline (prime_target == MAX) advances the
    // download-time decoded-PTS high-water. A swap-prefetch must NOT touch it:
    // the supervisor reads last_decoded_pts_ms at OLD teardown as the splice
    // point, and NEW's downloaded-ahead segments would otherwise inflate it.
    let track_dl_pts = prime_target == usize::MAX;
    let on_video_dl: SegmentDoneCallback = Arc::new(move |pts_ms| {
        if track_dl_pts {
            let prev = dl_stats.last_decoded_pts_ms.load(Ordering::Relaxed);
            if pts_ms > prev {
                dl_stats.last_decoded_pts_ms.store(pts_ms, Ordering::Relaxed);
            }
        }
        // `notify_one` (not `notify_waiters`) so the permit survives even if
        // the supervisor hasn't reached its `.notified()` await yet — avoids a
        // lost-wakeup race when a small segment finishes downloading fast.
        let n = downloaded.fetch_add(1, Ordering::Relaxed) + 1;
        if n == prime_target {
            cb_primed.notify_one();
        }
    });
    let download_handle = task::spawn(download_task(
        segments,
        start_index,
        download_tx,
        stop,
        stop_flag,
        http,
        Some(Arc::clone(&stats)),
        Some(on_video_dl),
        soft_end_exclusive,
    ));

    Ok(VideoPrefetch {
        width: repr.width,
        height: repr.height,
        init_data,
        hvcc_nalus,
        color,
        dovi_profile,
        track_crypto,
        download_rx,
        download_handle,
        primed,
    })
}

/// Carried into the decode half on an ABR swap so NEW splices cleanly onto
/// OLD's tail. NEW is started on the segment *containing* the current playback
/// position (its keyframe is therefore at/before the splice), then the decoder
/// drops every frame at/below `skip_below_pts_us` — the absolute PTS OLD last
/// rendered — so NEW's first *emitted* frame is the next one forward. That
/// avoids both a backward rewind AND a future-PTS frame that av_sync would
/// sit and wait for (the multi-second freeze seen on a 1080→4K upswitch, where
/// NEW used to start a whole segment ahead). `started` is for the timing log.
/// Switch the video plane's display refresh rate to match the content fps
/// (adaptive frame rate). Prefers `ANativeWindow_setFrameRateWithChangeStrategy`
/// (API 31) with the ALWAYS strategy so the panel *actually* changes mode
/// (e.g. 60 -> 24 Hz) — plain `ANativeWindow_setFrameRate` is seamless-only,
/// and on most TVs a 60->24 switch is NOT a seamless transition, so it would
/// silently never change. ALWAYS costs a brief blink at start/stop, which is
/// the expected "match content frame rate" behaviour. Falls back to the
/// seamless `setFrameRate` on API 30. Both symbols live in libnativewindow.so
/// and are dlsym'd (ndk-sys doesn't link it; a build-time extern broke the
/// 32-bit Streamer .so load — same lesson as `ANativeWindow_setBuffersDataSpace`).
/// No-op below API 30.
#[cfg(target_os = "android")]
fn set_window_frame_rate(window: usize, fps: f32) {
    use std::sync::OnceLock;
    if window == 0 || !(fps > 0.0) {
        return;
    }
    unsafe fn resolve(name: &[u8]) -> *mut libc::c_void {
        // libEGL usually pulled libnativewindow into the process already, so
        // RTLD_DEFAULT finds the symbol without bumping refcounts; else dlopen.
        let mut s = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const _);
        if s.is_null() {
            let lib = libc::dlopen(
                b"libnativewindow.so\0".as_ptr() as *const _,
                libc::RTLD_NOW | libc::RTLD_GLOBAL,
            );
            if !lib.is_null() {
                s = libc::dlsym(lib, name.as_ptr() as *const _);
            }
        }
        s
    }
    // int32_t (ANativeWindow*, float rate, int8_t compatibility, int8_t strategy)
    type WithStrategyFn = unsafe extern "C" fn(*mut std::ffi::c_void, f32, i8, i8) -> i32;
    // int32_t (ANativeWindow*, float rate, int8_t compatibility)
    type BasicFn = unsafe extern "C" fn(*mut std::ffi::c_void, f32, i8) -> i32;
    enum FrameRateFn {
        WithStrategy(WithStrategyFn),
        Basic(BasicFn),
    }
    static SYM: OnceLock<Option<FrameRateFn>> = OnceLock::new();
    let f = SYM.get_or_init(|| unsafe {
        let with_strategy = resolve(b"ANativeWindow_setFrameRateWithChangeStrategy\0");
        if !with_strategy.is_null() {
            return Some(FrameRateFn::WithStrategy(
                std::mem::transmute::<*mut libc::c_void, WithStrategyFn>(with_strategy),
            ));
        }
        let basic = resolve(b"ANativeWindow_setFrameRate\0");
        if basic.is_null() {
            log::warn!("[afr] ANativeWindow_setFrameRate* unavailable (API < 30) — adaptive frame rate off");
            None
        } else {
            Some(FrameRateFn::Basic(
                std::mem::transmute::<*mut libc::c_void, BasicFn>(basic),
            ))
        }
    });
    // compatibility = 1 (FIXED_SOURCE): the source has a fixed inherent rate.
    // strategy = 1 (ALWAYS): switch even when not seamless.
    let win = window as *mut std::ffi::c_void;
    match f {
        Some(FrameRateFn::WithStrategy(f)) => {
            let rc = unsafe { f(win, fps, 1, 1) };
            if rc == 0 {
                log::info!("[afr] switching display to {:.3} fps (fixed-source, always)", fps);
            } else {
                log::warn!("[afr] setFrameRateWithChangeStrategy({:.3}) returned {}", fps, rc);
            }
        }
        Some(FrameRateFn::Basic(f)) => {
            let rc = unsafe { f(win, fps, 1) };
            if rc == 0 {
                log::info!("[afr] hinted {:.3} fps (seamless-only, API 30)", fps);
            } else {
                log::warn!("[afr] setFrameRate({:.3}) returned {}", fps, rc);
            }
        }
        None => {}
    }
}

/// Owns the direct-mode video-plane `ANativeWindow` for the player's lifetime.
///
/// The host hands us a raw `ANativeWindow*` via `set_video_output_window`. We
/// take an `ANativeWindow_acquire` reference on it and only release when the
/// last `Player` clone drops (or the host installs a different window). Without
/// that owned ref the host's `SurfaceView` teardown could free the window — and
/// its internal mutex — while a pipeline rebuild is still asserting AFR on it,
/// crashing in `ANativeWindow_setFrameRate*` → `Surface::hook_query` with
/// "pthread_mutex_lock on a destroyed mutex" (SIGABRT). Holding the ref keeps
/// the window object alive, so a stale `setFrameRate` is at worst a no-op, not a
/// use-after-free. The pointer is still stored as a `usize` for the lock-free
/// reads on the build path; this type just bolts lifetime onto it.
struct DirectWindow(AtomicUsize);

impl DirectWindow {
    fn new() -> Self {
        DirectWindow(AtomicUsize::new(0))
    }

    /// Current window pointer (0 = none / classic renderer path).
    fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Install `window` (0 to clear), acquiring it and releasing the previous.
    /// Balanced per call: each `set` releases exactly the ref the prior `set`
    /// acquired, so repeated identical sets don't leak or over-release.
    fn set(&self, window: usize) {
        #[cfg(target_os = "android")]
        unsafe {
            if window != 0 {
                ndk_sys::ANativeWindow_acquire(window as *mut ndk_sys::ANativeWindow);
            }
            let old = self.0.swap(window, Ordering::Relaxed);
            if old != 0 {
                ndk_sys::ANativeWindow_release(old as *mut ndk_sys::ANativeWindow);
            }
        }
        #[cfg(not(target_os = "android"))]
        self.0.store(window, Ordering::Relaxed);
    }
}

impl Drop for DirectWindow {
    fn drop(&mut self) {
        #[cfg(target_os = "android")]
        unsafe {
            let w = self.0.load(Ordering::Relaxed);
            if w != 0 {
                ndk_sys::ANativeWindow_release(w as *mut ndk_sys::ANativeWindow);
            }
        }
    }
}

struct SwapSplice {
    started: Instant,
    skip_below_pts_us: i64,
}

/// Decode half: configure the HW decoder and run [`video_decoder_task`] against
/// the segments [`video_prefetch`] is already streaming, joining both halves to
/// completion. `decoder` (the scarce HW slot) must be created by the caller
/// only *after* any previous representation's decoder has been dropped.
async fn run_decode(
    pf: VideoPrefetch,
    sender: Sender<DecodedVideoFrame>,
    video_ready: Arc<Notify>,
    mut decoder: Box<dyn HwVideoDecoder>,
    stats: Arc<StatsState>,
    decoder_stop_flag: Arc<AtomicBool>,
    // `Some(..)` on an ABR swap (splice trim + timing), `None` initially.
    splice: Option<SwapSplice>,
    // Android direct mode video window (0 = renderer path).
    direct_window: usize,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    decoder.configure(VideoDecoderParams {
        codec: VideoCodec::Hevc,
        width: pf.width,
        height: pf.height,
        hvcc_nalus: pf.hvcc_nalus,
        color: pf.color,
        direct_window,
        dovi_profile: pf.dovi_profile,
    })?;
    // Direct mode: let the input-buffer spin observe teardown so a seek /
    // track-switch can't strand the decode task in the spin (see [B] in
    // mediacodec submit_direct).
    decoder.set_stop_signal(decoder_stop_flag.clone());

    let decoder_task = task::spawn(video_decoder_task(
        pf.download_rx,
        sender,
        decoder,
        pf.init_data,
        video_ready,
        pf.track_crypto,
        stats,
        decoder_stop_flag,
        splice,
    ));

    let (dl_res, dec_res) = join!(pf.download_handle, decoder_task);
    log_task_result("video download_task", dl_res);
    log_task_result("video decoder_task", dec_res);
    Ok(())
}

async fn video_play(
    video_representation: VideoRepresenation,
    start_index: usize,
    video_ready: Arc<Notify>,
    sender: Sender<DecodedVideoFrame>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    decoder: Box<dyn HwVideoDecoder>,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    segments_in_flight: usize,
    // See `download_task::soft_end_exclusive`. Plumbed through so the
    // supervisor can softly cap an old pipeline mid-flight without
    // discarding its already-decoded tail.
    soft_end_exclusive: Arc<AtomicUsize>,
    // Android direct mode video window (0 = renderer path).
    direct_window: usize,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Initial pipeline: nothing to overlap with, so download and decode run
    // back to back. `prime_target = MAX` → the readiness signal never fires
    // (only the supervisor's swap path consumes it).
    let pf = video_prefetch(
        &video_representation,
        start_index,
        stop,
        Arc::clone(&stop_flag),
        decryptor,
        http,
        Arc::clone(&stats),
        segments_in_flight,
        soft_end_exclusive,
        usize::MAX,
    )
    .await?;
    run_decode(
        pf,
        sender,
        video_ready,
        decoder,
        stats,
        stop_flag,
        None,
        direct_window,
    )
    .await
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

    let track_crypto = setup_track_crypto(&init_data, decryptor, "audio").await?;

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
        // Audio doesn't ABR-switch in this player (single audio rep per
        // session), so the soft-end mechanism is unused — usize::MAX
        // disables the early break.
        Arc::new(AtomicUsize::new(usize::MAX)),
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

/// Audio passthrough feed: download + decrypt the audio segments, slice the
/// compressed access units out of the mp4 samples and write them straight to
/// the bitstream sink (no decode, no PCM channel). Mirrors `audio_play`'s
/// download path; the decoder + `audio_sync_loop` are bypassed. Pre-target AUs
/// are dropped so audio begins at the seek target, matching video's
/// frame-accurate discard, so A/V line up under the passthrough clock.
#[cfg(target_os = "android")]
async fn audio_passthrough_play(
    audio_representation: AudioRepresentation,
    start_index: usize,
    sink: Arc<dyn crate::renderers::AudioPassthrough>,
    audio_ready: Arc<Notify>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    segments_in_flight: usize,
    discard_below_us: i64,
    pipeline_live: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(segments_in_flight);

    let init_dl = audio_representation
        .segment_init
        .download(&http, RequestKind::InitSegment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> {
            format!("audio init download: {}", e).into()
        })?;
    let init_data = init_dl.data;
    let track_crypto = setup_track_crypto(&init_data, decryptor, "audio").await?;

    let segments = audio_representation.segments.clone();
    let download = task::spawn(download_task(
        segments,
        start_index,
        download_tx,
        stop,
        stop_flag.clone(),
        http,
        Some(Arc::clone(&stats)),
        None,
        Arc::new(AtomicUsize::new(usize::MAX)),
    ));
    let feed = task::spawn(audio_passthrough_task(
        download_rx,
        sink,
        init_data,
        audio_ready,
        track_crypto,
        stop_flag,
        discard_below_us,
        pipeline_live,
    ));
    let (dl_res, feed_res) = join!(download, feed);
    log_task_result("audio download_task (passthrough)", dl_res);
    log_task_result("audio passthrough_task", feed_res);
    Ok(())
}

#[cfg(target_os = "android")]
async fn audio_passthrough_task(
    mut receiver: Receiver<DataSegment>,
    sink: Arc<dyn crate::renderers::AudioPassthrough>,
    init_data: Vec<u8>,
    audio_ready: Arc<Notify>,
    track_crypto: Option<TrackCrypto>,
    stop_flag: Arc<AtomicBool>,
    discard_below_us: i64,
    pipeline_live: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut first_au_written = false;
    let mut au_count = 0u64;
    let mut base_pts_ms: i64 = 0;
    while let Some(segment) = receiver.recv().await {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
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
            if stop_flag.load(Ordering::Relaxed) {
                return Ok(());
            }
            if offset + size > data_vec.len() {
                continue;
            }
            let pts_us = if ts_scale > 0 { ts * 1_000_000 / ts_scale as i64 } else { 0 };
            // Frame-accurate seek: drop AUs before the target.
            if pts_us < discard_below_us {
                continue;
            }
            let au_ms = pts_us / 1000;
            if !first_au_written {
                // Start audio only once video is live (pipeline_live), so the
                // two begin together. Otherwise the feed plays the first AU
                // immediately while video is still ~1-2s from its first frame,
                // and audio runs that far ahead for the whole pipeline.
                while !pipeline_live.load(Ordering::Relaxed) {
                    if stop_flag.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                base_pts_ms = au_ms;
            }
            // Two-phase feed. PRIME: an E-AC-3 *direct* AudioTrack does not
            // begin output — `getTimestamp` stays false / `played_ms` reads 0 —
            // until enough compressed data is buffered to cross its start
            // threshold (empirically well over the old 200 ms window; ~1–2 s of
            // media on the Google TV Streamer + HDMI AVR). So until the playback
            // head first moves we DON'T gate on it: we just keep writing, and
            // `AudioTrack.write` (blocking, STREAM mode) back-pressures on the
            // track's own buffer once it fills. Gating on the head before it has
            // started is the deadlock we hit: 200 ms never reached the threshold
            // → head never moved → feed waited forever → the video clock (which
            // reads `played_ms`) froze and the decoder back-pressured to ~1 fps.
            //
            // STEADY: once the head is moving, pace to keep the write at most
            // AHEAD_MS of media ahead of the real playback position, so the head
            // stays a usable clock and seek teardown doesn't strand seconds of
            // queued audio. (Wall-pacing instead buffered the whole startup gap
            // ahead — write_ahead≈1970 ms — which is why we pace on the head.)
            const AHEAD_MS: i64 = 750;
            // Upper bound on PRIME (NOT while paused — see below). Catches the
            // runaway: a stale duplicate feed left over from a pipeline rebuild
            // whose sink never becomes the active output, so its head stays 0
            // and its write() never blocks → it buffers forever (write_ahead →
            // minutes, the observed leak after an audio-track switch). Generous
            // because a *live* but slowly-locking output (an HDMI AVR waking
            // from standby) also shows head 0 with non-blocking writes for a
            // while — seen >10 s on a cold soundbar — and abandoning a live
            // pipeline strands it with no recovery. 30 s clears realistic AVR
            // wake yet still bounds a dead sink to seconds, not minutes.
            const PRIME_MAX_AHEAD_MS: i64 = 30_000;
            // While paused before the head starts, buffer only this far then
            // idle: a paused/not-yet-started track's write() does NOT block, so
            // without a cap the feed would run the buffer away (and a head still
            // at 0 *because we're paused* would trip the dead-sink abandon
            // below). This is enough to cross the start threshold so resume
            // plays immediately.
            const PRIME_PAUSED_TARGET_MS: i64 = 3_000;
            // Stall watchdog: in steady playback the head should drain the
            // buffer at ~real time. If over STALL_WINDOW_MS it advances less
            // than STALL_MIN_ADVANCE_MS (well under 25% of real time) while we
            // hold data and aren't paused, the output has wedged — the head, and
            // the video clock paced to it, would crawl until the next seek
            // rebuilds the track. (Observed on 4K/ec-3: head ~2% of real time,
            // video at 1 fps, only a seek recovered it.)
            const STALL_WINDOW_MS: u64 = 3_000;
            const STALL_MIN_ADVANCE_MS: i64 = 750;
            let mut chk_played = sink.played_ms().unwrap_or(0) as i64;
            let mut chk_wall = Instant::now();
            loop {
                if stop_flag.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let played = sink.played_ms().unwrap_or(0) as i64;
                // Prime phase: head not started yet → write now (write() blocks
                // on the full track buffer for backpressure; the buffer is sized
                // above the start threshold so we can reach it without blocking).
                if played == 0 {
                    // Paused before the head started: head reads 0 because we're
                    // paused, not because the sink is dead. Buffer a little for a
                    // prompt resume, then wait — do NOT abandon.
                    if sink.is_paused() {
                        if au_ms - base_pts_ms >= PRIME_PAUSED_TARGET_MS {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            continue;
                        }
                        break;
                    }
                    if au_ms - base_pts_ms > PRIME_MAX_AHEAD_MS {
                        log::warn!(
                            "[audio-pt] playback head never started after {}ms buffered — \
                             abandoning passthrough feed (stale pipeline or unsupported output)",
                            au_ms - base_pts_ms
                        );
                        return Ok(());
                    }
                    break;
                }
                // Steady phase: within the ahead budget → write; else wait for
                // playback to drain it.
                if au_ms - (base_pts_ms + played) <= AHEAD_MS {
                    break;
                }
                // We have data buffered but the head isn't taking it. Measure the
                // head rate over STALL_WINDOW_MS; if it's crawling (and we aren't
                // paused) the track wedged — nudge it and log the ground truth.
                if chk_wall.elapsed() >= Duration::from_millis(STALL_WINDOW_MS) {
                    let advanced = played - chk_played;
                    let paused = sink.is_paused();
                    if !paused && advanced < STALL_MIN_ADVANCE_MS {
                        let (have_ts, frame_pos) = sink.head_debug();
                        log::warn!(
                            "[audio-pt] STALL: head +{}ms in {}ms (played={}ms write_ahead={}ms \
                             paused={} have_ts={} frame_pos={}) — nudging (pause→play)",
                            advanced, chk_wall.elapsed().as_millis(), played,
                            au_ms - (base_pts_ms + played), paused, have_ts, frame_pos
                        );
                        sink.recover_stall();
                    }
                    chk_played = played;
                    chk_wall = Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // `block_in_place`: hand this worker's other tasks (the video decode
            // pipeline!) to a sibling worker while the JNI write runs.
            let au = &data_vec[offset..offset + size];
            tokio::task::block_in_place(|| sink.write(au));
            au_count += 1;
            if !first_au_written {
                log::debug!("[audio-pt] first AU written (au_ms={}), play() armed", au_ms);
                audio_ready.notify_one();
                first_au_written = true;
            }
            if au_count % 48 == 0 {
                let head = base_pts_ms + sink.played_ms().unwrap_or(0) as i64;
                log::info!(
                    "[audio-pt] au={}ms head={}ms write_ahead={}ms",
                    au_ms, head, au_ms - head
                );
            }
        }
    }
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
/// It runs one representation's decode at a time and switches on ABR request.
///
/// Make-before-break soft-switch flow on receiving a new representation:
///   1. Compute the next segment in the NEW representation that starts after
///      the current playback PTS, so the first new frame can't land before the
///      last old frame on the timeline.
///   2. Prefetch NEW via [`video_prefetch`] — download its init + first
///      segment(s) into a buffer — WHILE the OLD pipeline keeps decoding and
///      feeding av_sync. Prefetch touches only the network, never the HW
///      decoder, so it can't collide with OLD's live decoder slot.
///   3. Once NEW has buffered `PRIME_TARGET` segments (or a timeout elapses),
///      tear OLD down: drop `soft_end`/stop_flag so its download + decode exit
///      and the single HW decoder slot frees.
///   4. Start NEW's decode ([`run_decode`]) from the prefetched buffer. Decode
///      is still sequential w.r.t. OLD — some HW decoder paths (Intel D3D11VA,
///      certain MediaCodec drivers) won't allocate a second instance while the
///      first is live — but because the segments are already local, NEW's first
///      frame is just a configure + first-GOP decode away, well inside OLD's
///      ~8-frame (~333 ms) buffer drain. So av_sync never starves and the swap
///      no longer shows the buffering freeze it did when the whole NEW startup
///      (download included) happened only after OLD stopped.
///
/// Audio keeps playing throughout — only the video pipeline is touched.
async fn video_supervisor(
    // DIAG: pipeline generation id (see video_sync_loop).
    gen: u64,
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
    // Content origin (first segment's absolute presentation time). position_ms
    // is 0-based, so we add this back to locate segments on a soft swap.
    origin: Duration,
    // Android direct mode video window (0 = renderer path).
    direct_window: usize,
    // Resume slot written when retries are exhausted: the NEXT play() call
    // starts from this position instead of zero ("continue where we
    // stopped" semantics for the consumer's manual retry).
    pending_resume: Arc<StdMutex<Option<Duration>>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Pipeline failures (network death mid-stream, decoder errors) are
    // retried from the current playback position with backoff. The
    // counter resets once playback makes real progress, so a long movie
    // surviving three separate hiccups hours apart keeps recovering, while
    // a hard failure (dead URL, broken stream) exhausts quickly and
    // surfaces as PlayerEvent::Error instead of a fake EndOfStream.
    const MAX_PIPELINE_RETRIES: u32 = 3;
    const PROGRESS_RESET_MS: u64 = 10_000;
    let mut retry_attempt: u32 = 0;
    let mut last_fail_pos_ms: u64 = 0;
    let spawn_pipeline = |repr: VideoRepresenation,
                          start_index: usize,
                          local_stop: Arc<Notify>,
                          local_stop_flag: Arc<AtomicBool>|
     -> (
        tokio::task::JoinHandle<Result<(), Box<dyn Error + Send + Sync>>>,
        Arc<AtomicUsize>,
    ) {
        let soft_end = Arc::new(AtomicUsize::new(usize::MAX));
        let handle = task::spawn(video_play(
            repr,
            start_index,
            video_ready.clone(),
            frame_sender.clone(),
            local_stop,
            local_stop_flag,
            decryptor.clone(),
            decoder_factory(),
            Arc::clone(&http),
            Arc::clone(&stats),
            segments_in_flight,
            soft_end.clone(),
            direct_window,
        ));
        (handle, soft_end)
    };

    // The pipeline currently feeding av_sync. `cur_*` are reassigned on each
    // swap. An ABR switch prefetches the NEW representation (network only — no
    // HW decoder) WHILE this one keeps decoding, then tears OLD down and brings
    // NEW's decode up from the prefetched buffer. That keeps av_sync fed across
    // the swap so it never hits its 300 ms starvation pause (the visible
    // "buffering" freeze): the gap shrinks from a full download + decode to a
    // local configure + first-GOP decode.
    let mut current_repr = initial_repr;
    let mut cur_stop = Arc::new(Notify::new());
    let mut cur_flag = Arc::new(AtomicBool::new(false));
    log::info!("[video gen {}] supervisor start (repr={} idx={})", gen, current_repr.id, initial_start_index);
    let (mut cur_handle, mut cur_soft_end) = spawn_pipeline(
        current_repr.clone(),
        initial_start_index,
        cur_stop.clone(),
        cur_flag.clone(),
    );

    // Segments to buffer in NEW before tearing OLD down. One segment is
    // seconds of frames, which a HW decoder chews through far faster than
    // real-time, so NEW's first frame reaches av_sync inside OLD's ~8-frame
    // (~333 ms) buffer drain.
    const PRIME_TARGET: usize = 1;
    // Cap on how long to wait for that prefetch. Past this we swap anyway and
    // accept (for this one swap) the old freeze rather than stalling forever on
    // a dead/slow segment.
    const PRIME_TIMEOUT: Duration = Duration::from_secs(8);

    loop {
        // Race: play-level stop, the current pipeline finishing on its own
        // (natural EOF — must propagate so the keepalive frame_sender drops,
        // the channel closes, and av_sync fires EndOfStream), or an ABR switch.
        let new_repr: VideoRepresenation = loop {
            tokio::select! {
                _ = stop.notified() => {
                    cur_flag.store(true, Ordering::Relaxed);
                    cur_stop.notify_waiters();
                    let _ = cur_handle.await;
                    return Ok(());
                }
                res = &mut cur_handle => {
                    let detail = match res {
                        Ok(Ok(())) => {
                            // Natural EOF — propagate so the keepalive
                            // frame_sender drops, the channel closes, and
                            // av_sync fires EndOfStream.
                            log::info!("[video] supervisor: pipeline exited naturally; closing");
                            return Ok(());
                        }
                        Ok(Err(e)) => e.to_string(),
                        Err(e) => format!("pipeline task panicked: {}", e),
                    };
                    if stop_flag.load(Ordering::Relaxed) {
                        // Teardown raced the failure — not an error.
                        return Ok(());
                    }
                    log::error!("[video] supervisor: pipeline failed: {}", detail);

                    // Bounded retry-with-resume. Progress since the last
                    // failure resets the budget.
                    let pos_now = position_ms.load(Ordering::Relaxed);
                    if pos_now.saturating_sub(last_fail_pos_ms) > PROGRESS_RESET_MS {
                        retry_attempt = 0;
                    }
                    last_fail_pos_ms = pos_now;
                    retry_attempt += 1;
                    if retry_attempt > MAX_PIPELINE_RETRIES {
                        log::error!(
                            "[video] supervisor: {} consecutive pipeline failures — giving up at {}ms",
                            retry_attempt - 1,
                            pos_now
                        );
                        // Park the position for the consumer's next play()
                        // ("continue where we stopped"), surface the error,
                        // and stop WITHOUT the fake EndOfStream (av_sync
                        // checks stop_flag before emitting EOS).
                        *pending_resume.lock().unwrap() =
                            Some(Duration::from_millis(pos_now));
                        let _ = events.send(PlayerEvent::Error {
                            kind: PlayerErrorKind::Decoder,
                            detail,
                        });
                        stop_flag.store(true, Ordering::Relaxed);
                        stop.notify_waiters();
                        return Err("video pipeline retries exhausted".into());
                    }
                    log::warn!(
                        "[video] supervisor: retrying pipeline ({}/{}) from {}ms",
                        retry_attempt,
                        MAX_PIPELINE_RETRIES,
                        pos_now
                    );
                    // Backoff, abortable by stop. av_sync's starvation
                    // detection keeps the consumer in Buffering meanwhile.
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(retry_attempt as u64)) => {}
                        _ = stop.notified() => return Ok(()),
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        return Ok(());
                    }

                    // Respawn from the segment containing the current
                    // position; the splice trims re-decoded frames at/below
                    // the last rendered PTS so playback continues forward
                    // (no rewind). The retry task includes the prefetch, so
                    // a network failure during recovery lands back in this
                    // arm and consumes another attempt.
                    let pos_abs = Duration::from_millis(pos_now) + origin;
                    let resume_idx = find_segment_index(&current_repr.segments, pos_abs);
                    cur_stop = Arc::new(Notify::new());
                    cur_flag = Arc::new(AtomicBool::new(false));
                    cur_soft_end = Arc::new(AtomicUsize::new(usize::MAX));
                    cur_handle = task::spawn({
                        let repr = current_repr.clone();
                        let stop = cur_stop.clone();
                        let flag = cur_flag.clone();
                        let soft_end = cur_soft_end.clone();
                        let decryptor = decryptor.clone();
                        let http = Arc::clone(&http);
                        let stats = Arc::clone(&stats);
                        let sender = frame_sender.clone();
                        let video_ready = video_ready.clone();
                        let decoder_factory = decoder_factory.clone();
                        let splice_pts_us = pos_abs.as_micros() as i64;
                        async move {
                            let pf = video_prefetch(
                                &repr,
                                resume_idx,
                                stop,
                                Arc::clone(&flag),
                                decryptor,
                                http,
                                Arc::clone(&stats),
                                segments_in_flight,
                                soft_end,
                                usize::MAX,
                            )
                            .await?;
                            run_decode(
                                pf,
                                sender,
                                video_ready,
                                decoder_factory(),
                                stats,
                                flag,
                                Some(SwapSplice {
                                    started: Instant::now(),
                                    skip_below_pts_us: splice_pts_us,
                                }),
                                direct_window,
                            )
                            .await
                        }
                    });
                    continue;
                }
                _ = async {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let _ = switch_rx.changed().await;
                } => {
                    if stop_flag.load(Ordering::Relaxed) {
                        cur_flag.store(true, Ordering::Relaxed);
                        cur_stop.notify_waiters();
                        let _ = cur_handle.await;
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

        // Switch at the NEXT segment boundary after the current position
        // (position_ms is 0-based; segment start_times are absolute → add
        // origin). NEW starts on that keyframe-aligned boundary and OLD plays
        // up to it (step 2.5), so the two are contiguous: NEW's first frame
        // isn't in the future (no forward-PTS wait) and there's only a tiny
        // ~buffer-sized overlap for the decoder to trim. Starting on the
        // *current* segment instead made NEW decode seconds of throwaway frames
        // to reach the splice and starved av_sync into buffering.
        let pos = Duration::from_millis(position_ms.load(Ordering::Relaxed)) + origin;
        let mut new_start = find_segment_index(&new_repr.segments, pos);
        if new_start + 1 < new_repr.segments.len() {
            new_start += 1;
        }
        log::info!(
            "[abr] soft switch: repr {} -> {} from seg {} (pos {}ms)",
            current_repr.id, new_repr.id, new_start, pos.as_millis()
        );
        let swap_t0 = Instant::now();

        // --- Make-before-break, step 1: prefetch NEW while OLD keeps playing.
        // `video_prefetch` only downloads — it never allocates the HW decoder,
        // so it can't collide with OLD's live decoder slot.
        let new_stop = Arc::new(Notify::new());
        let new_flag = Arc::new(AtomicBool::new(false));
        let new_soft_end = Arc::new(AtomicUsize::new(usize::MAX));
        let new_pf = tokio::select! {
            r = video_prefetch(
                &new_repr,
                new_start,
                new_stop.clone(),
                new_flag.clone(),
                decryptor.clone(),
                Arc::clone(&http),
                Arc::clone(&stats),
                segments_in_flight,
                new_soft_end.clone(),
                PRIME_TARGET,
            ) => match r {
                Ok(pf) => pf,
                Err(e) => {
                    // NEW couldn't even begin downloading — keep OLD playing.
                    log::error!(
                        "[abr] prefetch of repr {} failed; staying on {}: {}",
                        new_repr.id, current_repr.id, e
                    );
                    continue;
                }
            },
            _ = stop.notified() => {
                cur_flag.store(true, Ordering::Relaxed);
                cur_stop.notify_waiters();
                let _ = cur_handle.await;
                return Ok(());
            }
            res = &mut cur_handle => {
                // OLD ended before NEW even started; nothing to swap into.
                if let Ok(Err(e)) = res {
                    log::error!("[video] supervisor: pipeline failed during prefetch: {}", e);
                }
                return Ok(());
            }
        };

        // --- step 2: wait until NEW has buffered enough to decode without a
        // network wait. OLD keeps feeding av_sync throughout.
        let primed = Arc::clone(&new_pf.primed);
        let mut old_done = false;
        tokio::select! {
            _ = primed.notified() => {
                log::info!("[abr] NEW primed {}ms after switch", swap_t0.elapsed().as_millis());
            }
            _ = tokio::time::sleep(PRIME_TIMEOUT) => {
                log::warn!(
                    "[abr] prefetch prime timed out after {}ms; swapping anyway",
                    swap_t0.elapsed().as_millis()
                );
            }
            _ = stop.notified() => {
                // Tear NEW's prefetch down (the flag makes download_task exit;
                // dropping new_pf closes its channel too) and OLD, then exit.
                new_flag.store(true, Ordering::Relaxed);
                new_stop.notify_waiters();
                cur_flag.store(true, Ordering::Relaxed);
                cur_stop.notify_waiters();
                let _ = cur_handle.await;
                return Ok(());
            }
            res = &mut cur_handle => {
                // OLD reached EOF while NEW was priming — bring NEW up anyway.
                if let Ok(Err(e)) = res {
                    log::error!("[video] supervisor: pipeline failed during prime: {}", e);
                }
                old_done = true;
            }
        }

        // --- step 2.5: let OLD play up to the chosen boundary so NEW (which
        // starts there) isn't in the future. OLD keeps rendering at its current
        // quality the whole time — no freeze; the switch just lands on a
        // segment boundary, like every production DASH player. Cap OLD's
        // download at the boundary so it can't run on while we wait.
        cur_soft_end.store(new_start, Ordering::Relaxed);
        let boundary_ms = new_repr
            .segments
            .get(new_start)
            .map(|s| s.start_time().as_millis() as u64)
            .unwrap_or(0);
        // Begin the swap a touch before the boundary so NEW's first GOP is
        // decoded by the time OLD's buffered tail drains.
        const BOUNDARY_LEAD_MS: u64 = 150;
        // Never wait forever (e.g. OLD stalls) — cap and switch anyway.
        const MAX_BOUNDARY_WAIT: Duration = Duration::from_secs(15);
        let wait_start = Instant::now();
        while !old_done && boundary_ms != 0 {
            let rendered_abs =
                position_ms.load(Ordering::Relaxed) + origin.as_millis() as u64;
            if rendered_abs + BOUNDARY_LEAD_MS >= boundary_ms {
                break;
            }
            if wait_start.elapsed() >= MAX_BOUNDARY_WAIT {
                log::warn!("[abr] boundary wait hit cap; switching mid-segment");
                break;
            }
            let remaining = boundary_ms - (rendered_abs + BOUNDARY_LEAD_MS);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(remaining.min(120))) => {}
                _ = stop.notified() => {
                    new_flag.store(true, Ordering::Relaxed);
                    new_stop.notify_waiters();
                    cur_flag.store(true, Ordering::Relaxed);
                    cur_stop.notify_waiters();
                    let _ = cur_handle.await;
                    return Ok(());
                }
                res = &mut cur_handle => {
                    if let Ok(Err(e)) = res {
                        log::error!("[video] supervisor: pipeline failed during boundary wait: {}", e);
                    }
                    old_done = true;
                }
            }
        }
        log::info!(
            "[abr] reached boundary {}ms after switch (pos≈{}ms boundary={}ms)",
            swap_t0.elapsed().as_millis(),
            position_ms.load(Ordering::Relaxed) + origin.as_millis() as u64,
            boundary_ms
        );

        let _ = events.send(PlayerEvent::TrackChanged {
            kind: TrackKind::Video,
            info: video_track_info(&new_repr),
        });

        // --- step 3: tear OLD down (frees the single HW decoder slot), then
        // start NEW's decode from the already-downloaded buffer. soft_end is
        // belt-and-braces alongside the flag + notify. `cur_handle` is only
        // awaited if it didn't already finish above (a JoinHandle must not be
        // polled twice).
        if !old_done {
            cur_soft_end.store(new_start, Ordering::Relaxed);
            cur_flag.store(true, Ordering::Relaxed);
            cur_stop.notify_waiters();
            log::info!("[video gen {}] soft-swap: awaiting OLD repr {} decode teardown", gen, current_repr.id);
            let _ = cur_handle.await;
            log::info!("[video gen {}] soft-swap: OLD repr {} decode joined", gen, current_repr.id);
        } else {
            log::info!("[video gen {}] soft-swap: OLD repr {} already done", gen, current_repr.id);
        }
        // Splice NEW onto OLD's tail at the last RENDERED pts so NEW resumes on
        // the very next frame — forward-contiguous, no rewind and no future-PTS
        // frame to wait on.
        //
        // This used to read `stats.last_decoded_pts_ms`, but that is the
        // DOWNLOAD high-water (set in video_prefetch's segment-done callback),
        // not the last frame OLD actually showed. On a low-bitrate OLD the
        // downloader races many seconds ahead of the picture, so trimming NEW to
        // it discarded every frame between the boundary and the download head —
        // the renderer then had no frame at the current position and stalled
        // ("[vsync] no frame → buffering") for seconds on every swap while audio
        // (own clock) and the downloader kept running (the "everything plays but
        // no frames reach the screen" freeze, on every platform). The render
        // position (position_ms + origin, same expression the boundary wait
        // above uses) is where OLD's picture actually is, so NEW splices there
        // cleanly.
        let rendered_abs_ms = position_ms.load(Ordering::Relaxed) + origin.as_millis() as u64;
        let splice_pts_us = rendered_abs_ms as i64 * 1000;
        log::info!(
            "[abr] OLD torn down {}ms after switch; NEW trimmed to pts>{}ms",
            swap_t0.elapsed().as_millis(),
            splice_pts_us / 1000
        );

        // OLD's decoder is dropped now — safe to allocate NEW's.
        let decode_t0 = Instant::now();
        let decoder = decoder_factory();
        cur_handle = task::spawn(run_decode(
            new_pf,
            frame_sender.clone(),
            video_ready.clone(),
            decoder,
            Arc::clone(&stats),
            new_flag.clone(),
            Some(SwapSplice {
                started: decode_t0,
                skip_below_pts_us: splice_pts_us,
            }),
            direct_window,
        ));
        cur_stop = new_stop;
        cur_flag = new_flag;
        cur_soft_end = new_soft_end;
        current_repr = new_repr;
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
            abr_video_profile: Arc::new(ArcSwap::from_pointee(AbrVideoProfile::default())),
            video_switch_tx: Arc::new(StdMutex::new(None)),
            buffer_target_secs: Arc::new(AtomicU32::new(DEFAULT_BUFFER_TARGET_SECS)),
            subtitle_representation: Arc::new(StdMutex::new(None)),
            video_output_window: Arc::new(DirectWindow::new()),
            adaptive_frame_rate: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            audio_passthrough: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pipeline_live: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_resume: Arc::new(StdMutex::new(None)),

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

        // A new manifest is a fresh playback session — never inherit the
        // `paused` flag from a prior session on this Player instance.
        // Without this reset, calling open_url() while paused would leave
        // the next play() parked on the very first frame until the host
        // explicitly called resume(). That's surprising UX: switching
        // channels / streams shouldn't carry transport state across.
        // `audio_renderer.set_paused(false)` matches what `resume()` does
        // so the audio output is ready when the new play() spins up.
        if self.paused.swap(false, Ordering::Relaxed) {
            self.audio_renderer.set_paused(false);
        }

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
        // loop respawns with the freshly-stored representation. Gated on
        // pipeline_live for the same reason as change_audio_track: a seek
        // before the first frame would clobber a parked start position
        // (pending_resume); the play() loop re-reads this cell on start.
        if self.pipeline_live.load(Ordering::Relaxed) {
            self.seek(self.position());
        }
    }

    /// Trigger a soft (ABR-style) video representation swap directly,
    /// bypassing the ABR engine. Primarily for testing the supervisor's
    /// soft-swap path without simulating bandwidth changes. Unlike
    /// `change_video_track`, this does NOT flip the ABR strategy back
    /// to Manual — the next ABR tick can immediately re-evaluate.
    pub fn change_video_track_soft(&self, representation: &VideoRepresenation) {
        self.apply_video_representation_soft(representation);
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

    /// Set the HDR / bit-depth policy applied by ABR. Takes effect on the
    /// next ABR tick (≤ 1s). Default is `Adaptive` (no filtering — every
    /// representation in the adaptation set is eligible).
    ///
    /// The host UI should query `PlayerCapabilities` first: if `hdr10` is
    /// `false` on the active device, force `SdrOnly` here so a manifest
    /// with HDR reps doesn't auto-switch into an unrenderable one.
    pub fn set_abr_video_profile(&self, profile: AbrVideoProfile) {
        self.abr_video_profile.store(Arc::new(profile));
    }

    /// Returns the active HDR / bit-depth profile.
    pub fn abr_video_profile(&self) -> AbrVideoProfile {
        **self.abr_video_profile.load()
    }

    /// Push new HDR→SDR tonemap parameters to the active video sink.
    /// Values are sanitised into the safe rendering range (see
    /// `HdrTonemapParams::sanitised`) so a typo'd extreme can't break
    /// the output. On platforms where the OS owns HDR conversion
    /// (macOS / iOS via VideoToolbox), this is a no-op — check
    /// `PlayerCapabilities::hdr_tonemap_tunable` to decide whether to
    /// show the setting in the UI at all.
    ///
    /// The player does **not** persist the value across runs; the host
    /// is responsible for round-tripping the user's choice through its
    /// own settings storage and calling this on every Player init.
    pub fn set_hdr_tonemap(&self, params: HdrTonemapParams) {
        self.video_renderer.set_hdr_tonemap_params(params.sanitised());
    }

    /// Tell the renderer which HDR formats the active display can present
    /// natively (bitmask in `Display.HdrCapabilities` order: bit 0 = Dolby
    /// Vision, 1 = HDR10, 2 = HLG, 3 = HDR10+). On Android the GLES sink
    /// then passes PQ streams through to the display (BT2020_PQ surface
    /// dataspace, no tonemap) instead of tonemapping to SDR. 0 (default) =
    /// SDR display, tonemap in-shader.
    pub fn set_display_hdr_types(&self, mask: u32) {
        self.video_renderer.set_display_hdr_types(mask);
    }

    /// Bottom safe-area inset, in **device pixels of the overlay surface**,
    /// that subtitles must stay above. The cue's bottom edge is anchored here
    /// instead of at the surface's physical bottom — this is how subtitles
    /// clear TV overscan and system bars without the player guessing a
    /// per-device margin.
    ///
    /// The host should pass the real bottom inset from `WindowInsets`
    /// (system bars / display cutout / reported overscan). On Android TV,
    /// where HDMI overscan is usually invisible to the app, the host should
    /// pass `max(windowInsets.bottom, 0.10 * surfaceHeight)` so the
    /// title-safe margin still applies. 0 (default, host never called this)
    /// makes the renderers fall back to a 10% title-safe margin.
    ///
    /// Takes effect on the next presented frame. Re-call it on every
    /// inset/size change.
    pub fn set_subtitle_safe_insets(&self, bottom_px: u32) {
        self.video_renderer.set_subtitle_safe_bottom_px(bottom_px);
    }

    /// Android direct playback mode: hand the decoder a dedicated video
    /// `ANativeWindow*` to render into. Decoded frames then ride a HW
    /// video plane — HDR10/HDR10+/Dolby Vision signals (incl. dynamic
    /// metadata in the bitstream) reach the display exactly as the OS
    /// video pipeline delivers them, and the renderer surface only
    /// carries subtitles/UI. Takes effect at the next `play()`.
    ///
    /// The player takes its own `ANativeWindow_acquire` ref on the window and
    /// holds it until the player is dropped or a different window is installed,
    /// so it stays alive even if the host releases its `Surface` (preventing the
    /// AFR `setFrameRate`-on-destroyed-Surface crash). Pass null to release the
    /// player's ref — call this from the host's `surfaceDestroyed` before it
    /// releases the Surface.
    pub fn set_video_output_window(&self, window: *mut std::ffi::c_void) {
        self.video_output_window.set(window as usize);
    }

    /// Adaptive frame rate (Android direct mode). When enabled (the default),
    /// the player hints the content's frame rate to the OS via
    /// `ANativeWindow_setFrameRate` on the video plane at each pipeline build,
    /// so the display can switch to a matching refresh rate (e.g. 24 ->
    /// 24/48/120 Hz) and play judder-free. It's a per-surface *hint*, not a
    /// forced mode switch — the system chooses. Requires API 30+ (no-op
    /// below). Disable it if the host wants to drive display-mode policy
    /// itself (`Surface.setFrameRate` / `preferredDisplayModeId`). Takes
    /// effect at the next `play()`.
    pub fn set_adaptive_frame_rate(&self, enabled: bool) {
        self.adaptive_frame_rate
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Opt into audio passthrough (bitstream): when enabled, a passthrough
    /// codec track (E-AC-3 / AC-3 / DTS) is sent to the audio output untouched
    /// for an HDMI AVR/soundbar to decode, instead of being decoded to PCM
    /// here. Default OFF. It self-gates: passthrough only engages if the
    /// platform sink reports support for the codec AND a passthrough track is
    /// selected; otherwise it transparently falls back to PCM decode. Takes
    /// effect at the next `play()`. NB: E-AC-3 needs HDMI — optical (S/PDIF)
    /// carries only AC-3/DTS core.
    pub fn set_audio_passthrough(&self, enabled: bool) {
        self.audio_passthrough
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Initial playback position for the next `play()` (resume). Unlike
    /// `seek()` — which is fire-and-forget and races `play()`'s read of the
    /// seek target — this stores the position **synchronously**, so a
    /// consumer can just `set_start_position(Some(pos)); play();` and the
    /// pipeline deterministically starts there (priority over the start of
    /// content, below an explicit pending `seek()`). One-shot: `play()`
    /// consumes it. `None` clears it (start from the beginning).
    ///
    /// The initial ABR auto-switch is held until the pipeline produces its
    /// first frame, so it can't collide with this resume start — consumers
    /// no longer need to defer ABR or seek-after-Playing to resume safely.
    pub fn set_start_position(&self, pos: Option<Duration>) {
        *self.pending_resume.lock().unwrap() = pos;
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

    /// Set the subtitle overlay's visual style — text/outline colour and a
    /// size multiplier (see [`SubtitleStyle`]). Values are sanitised into
    /// the safe rendering range before being applied, and the change takes
    /// effect on the next cue draw (any cached rasterization is dropped).
    ///
    /// Like `set_hdr_tonemap`, the player does **not** persist this across
    /// runs; the host round-trips the user's choice through its own
    /// settings and calls this on every init. No-op on sinks that don't
    /// own subtitle rendering. A future libass backend will read the same
    /// struct, so styling set here survives that migration.
    pub fn set_subtitle_style(&self, style: SubtitleStyle) {
        self.video_renderer.set_subtitle_style(style.sanitised());
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
    ///
    /// Two-stage selection: the `abr_video_profile` first filters the
    /// candidate set (e.g. `SdrOnly` drops HDR10 reps), then the bitrate
    /// selector picks the highest-bandwidth survivor that fits the EWMA.
    fn abr_tick(&self) {
        let strategy = **self.abr_strategy.load();
        let safety = match strategy {
            AbrStrategy::Manual => return,
            AbrStrategy::BandwidthEwma { safety_factor } => safety_factor,
        };

        // Don't switch until the current pipeline has produced its first frame.
        // Otherwise the first auto-switch (~1s in, once a bandwidth sample
        // exists) can fire into a just-started or resuming pipeline — a codec
        // rebuild on top of a rebuild that collided with resume and could stall
        // the direct-mode MediaCodec. Reset on every (re)build; set after the
        // first frame. This is the safe, event-based replacement for consumers
        // deferring ABR by a fixed delay after a resume seek.
        if !self.pipeline_live.load(Ordering::Relaxed) {
            return;
        }

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

        // Stage 1: filter by HDR / bit-depth policy.
        let profile = **self.abr_video_profile.load();
        let candidate_indices = profile.filter_indices(&adaptation.representations);
        if candidate_indices.is_empty() {
            // Profile filtered everything out (e.g. SdrOnly on an HDR-only
            // adaptation). Keep the currently-playing rep rather than
            // picking one the policy forbids.
            return;
        }

        // Stage 2: bitrate selector against the filtered set.
        let bws: Vec<u64> = candidate_indices
            .iter()
            .map(|&i| adaptation.representations[i].bandwidth)
            .collect();
        let pick_local = match abr::pick_representation(&bws, ewma_bps, safety) {
            Some(i) => i,
            None => return,
        };
        let pick_idx = candidate_indices[pick_local];
        let picked = &adaptation.representations[pick_idx];
        if Some(picked.id) == current_id {
            return;
        }

        // Buffer gate for UP-switches. A higher rung means a heavier segment +
        // a codec reconfigure (e.g. 1080p -> 4K); at the cold start or right
        // after a (re)build the buffer is near-empty, so the make-before-break
        // swap starves -> a visible buffering hitch + a LATE cascade (the rough
        // start, no seek involved). Defer until a cushion exists. Down-switches
        // are NOT gated — dropping a rung is how we AVOID starvation when
        // bandwidth falls, so it must fire even on a thin buffer.
        let cur_bw = self
            .video_representation
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.bandwidth)
            .unwrap_or(0);
        if picked.bandwidth > cur_bw {
            const MIN_UPSWITCH_BUFFER_MS: i64 = 4_000;
            let pos = self.position_ms.load(Ordering::Relaxed) as i64;
            let decoded = self.stats.last_decoded_pts_ms.load(Ordering::Relaxed);
            let buffered_ahead_ms = (decoded - pos).max(0);
            if buffered_ahead_ms < MIN_UPSWITCH_BUFFER_MS {
                log::debug!(
                    "[abr] up-switch to {} deferred: buffer {}ms < {}ms",
                    picked.id, buffered_ahead_ms, MIN_UPSWITCH_BUFFER_MS
                );
                return;
            }
        }
        log::info!(
            "[abr] switch repr {:?} -> {} (ewma={}bps safety={} profile={:?})",
            current_id, picked.id, ewma_bps, safety, profile
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
        // Only an already-running pipeline needs a seek to restart at the
        // current position with the new track. BEFORE the pipeline goes live,
        // a seek here is destructive: position() is still 0 and seek(0) parks
        // seek_target=Some(0), which the play() loop takes ahead of (shadows)
        // a parked start position — set_start_position's `pending_resume` —
        // via `take().or_else(pending_resume)`, silently killing resume. The
        // play() loop re-reads the representation cells on (re)start, so the
        // new track is honored at startup WITHOUT a seek. This fires in the
        // wild because consumers apply a saved audio-language preference right
        // after prepare() (e.g. BlackZone's applyLanguagePreference), i.e.
        // exactly between set_start_position() and the pipeline's first frame.
        if self.pipeline_live.load(Ordering::Relaxed) {
            self.seek(self.position());
        }
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

        // Robustness: a representation with zero media segments would leave the
        // pipeline waiting forever for frames that never arrive — a silent hang
        // (kind=2 = 0 requests, eternal buffering, no error). This happens when
        // a SegmentBase/sidx yields no subsegments (e.g. a mis-parsed sidx).
        // Fail loud instead of wedging so the cause is visible.
        if video_representation.segments.is_empty() {
            let msg = format!(
                "video representation {} has no media segments (SegmentBase/sidx yielded none)",
                video_representation.id
            );
            self.emit_error(PlayerErrorKind::ManifestParse, msg.clone());
            return Err(msg.into());
        }
        if audio_representation.segments.is_empty() {
            let msg = format!(
                "audio representation {} has no media segments (SegmentBase/sidx yielded none)",
                audio_representation.id
            );
            self.emit_error(PlayerErrorKind::ManifestParse, msg.clone());
            return Err(msg.into());
        }

        let video_ready = self.video_ready.clone();
        let audio_ready = self.audio_ready.clone();
        let stop = self.stop.clone();
        let stop_flag = self.stop_flag.clone();
        let video_sink = self.video_renderer.clone();
        let audio_sink = self.audio_renderer.clone();
        let seek_target = self.seek_target.clone();
        let position_ms = self.position_ms.clone();
        // Cells re-read on every (re)start of the pipeline so a seek /
        // track-switch picks up the latest selection + decryptor without the
        // consumer re-calling play().
        let http = Arc::clone(&self.http);
        let video_repr_cell = Arc::clone(&self.video_representation);
        let audio_repr_cell = Arc::clone(&self.audio_representation);
        let decryptor_cell = Arc::clone(&self.decryptor);
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
                // Create the hw-device ONCE and share it across the initial
                // decoder + every ABR-swap decoder, so a swap re-opens only the
                // codec instead of recreating the D3D11/VAAPI device. Recreating
                // it is slow and on Windows stalls the wgpu present + the DWM
                // compositor, hitching the whole UI for a moment on each switch.
                // Fall back to a per-decoder device if creation fails.
                let factory: VideoDecoderFactory =
                    match decoders::ffmpeg_hw::SharedHwDevice::new() {
                        Ok(dev) => {
                            log::info!("[video] shared hw-device created; ABR swaps reuse it");
                            Arc::new(move || {
                                Box::new(decoders::ffmpeg_hw::FfmpegHwDecoder::new_shared(
                                    dev.clone(),
                                )) as Box<dyn HwVideoDecoder>
                            })
                        }
                        Err(e) => {
                            log::warn!(
                                "[video] shared hw-device init failed ({e}); per-decoder device fallback"
                            );
                            Arc::new(|| {
                                Box::new(decoders::ffmpeg_hw::FfmpegHwDecoder::new())
                                    as Box<dyn HwVideoDecoder>
                            })
                        }
                    };
                factory
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

        // The per-play switch channel (ABR soft-swap) is (re)installed inside
        // the pipeline loop below; keep a handle to the slot for cleanup.
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
        // Oneshot used to kill the abr_tick task when this play() invocation
        // ends FOR REAL (outer loop break — either no track selected or a
        // genuine stop()). Critically NOT tied to `stop_flag`: that gets
        // flipped by every `seek()` / `change_video_track()` to tear the
        // sub-pipeline down, and if the abr_tick exited on it then a
        // user-driven manual switch + `set_abr_strategy(BandwidthEwma)`
        // afterwards would silently never fire any ticks again.
        let (abr_kill_tx, mut abr_kill_rx) = tokio::sync::oneshot::channel::<()>();
        let video_output_window = Arc::clone(&self.video_output_window);
        // Adaptive-frame-rate inputs (Android direct mode only): the toggle and
        // the content fps (MPD @frameRate on the selected adaptation, captured
        // once per play() — ABR swaps keep the same content fps).
        #[cfg(target_os = "android")]
        let adaptive_frame_rate = Arc::clone(&self.adaptive_frame_rate);
        #[cfg(target_os = "android")]
        let video_fps = self
            .video_adaptation
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|a| a.fps());
        let pending_resume = Arc::clone(&self.pending_resume);
        let pipeline_live = Arc::clone(&self.pipeline_live);
        let audio_passthrough = Arc::clone(&self.audio_passthrough);
        let play = tokio::spawn(async move {
            // ABR tick runs once for the whole play() lifetime (survives
            // every seek/track-switch restart below). On Manual it's a
            // no-op each tick.
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(1));
                ticker.set_missed_tick_behavior(
                    tokio::time::MissedTickBehavior::Skip,
                );
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            abr_player.abr_tick();
                        }
                        _ = &mut abr_kill_rx => break,
                    }
                }
            });

            // Self-restarting pipeline. seek() / change_video_track() /
            // change_audio_track() tear the current pipeline down (set
            // stop_flag + seek_target) — we rebuild here instead of relying on
            // the consumer to call play() again in a loop. A real stop() (no
            // seek_target) or a natural EndOfStream (stop_flag stays false)
            // leaves seek_target None after the pipeline ends → we exit.
            loop {
                let seek_offset = {
                    let mut target = seek_target.write().await;
                    stop_flag.store(false, Ordering::Relaxed);
                    // Priority: explicit seek > resume position parked by an
                    // exhausted-retries stop ("continue where we stopped" on
                    // the consumer's next play()) > start of content.
                    target
                        .take()
                        .or_else(|| pending_resume.lock().unwrap().take())
                        .unwrap_or(Duration::ZERO)
                };

                // Release any passthrough sink from the previous pipeline
                // BEFORE this iteration creates a new one — only one compressed
                // AudioTrack may own the HDMI output at a time, so creating the
                // next while the old is alive conflicts (the seek crash). The
                // previous audio task has already joined (join! below), so this
                // drops the last ref → old AudioTrack stop+release. Also
                // un-pauses the cpal path for a possible PCM track this round.
                audio_sink.set_passthrough(None);

                // Hard reset the audio sink before the (re)started pipeline
                // pumps samples — drops the previous pipeline's residual
                // cpal-channel contents (see seek()/EOS rationale).
                audio_sink.flush();
                audio_sink.set_paused(true);

                // Re-read the current selection so a track switch that arrived
                // alongside the seek takes effect on restart.
                let video_representation = match video_repr_cell.lock().unwrap().clone() {
                    Some(v) => v,
                    None => break,
                };
                let audio_representation = match audio_repr_cell.lock().unwrap().clone() {
                    Some(a) => a,
                    None => break,
                };
                // A zero-segment representation would buffer forever — fail loud
                // instead of wedging (also guards a restart onto a broken rep).
                if video_representation.segments.is_empty()
                    || audio_representation.segments.is_empty()
                {
                    let _ = events.send(PlayerEvent::Error {
                        kind: PlayerErrorKind::ManifestParse,
                        detail: "selected representation has no media segments".to_string(),
                    });
                    break;
                }

                // Content origin = the first segment's presentation time. The
                // media PTS / sidx EPT are absolute (non-zero
                // baseMediaDecodeTime), so subtracting this exposes a 0-based
                // position/seek to consumers (matches the 0-based duration).
                let origin = video_representation
                    .segments
                    .first()
                    .map(|s| s.start_time())
                    .unwrap_or(Duration::ZERO);

                // seek_offset is 0-based; map to the absolute media timeline to
                // locate the segment.
                let abs_offset = seek_offset + origin;
                let video_start_index =
                    find_segment_index(&video_representation.segments, abs_offset);
                let audio_start_index =
                    find_segment_index(&audio_representation.segments, abs_offset);

                // Frame-accurate seek: decode entry is the segment START (a
                // keyframe — CMAF segments are independently decodable), but we
                // RENDER from the requested target. Video discards frames below
                // the target (`discard_below_us`, absolute pts) and audio trims
                // to it, so playback lands exactly where the user aimed instead
                // of snapping back to the segment boundary.
                let entry_seg = video_representation.segments.get(video_start_index);
                let seg_start = entry_seg
                    .map(|s| s.start_time())
                    .unwrap_or(abs_offset)
                    .saturating_sub(origin);
                // Discard ceiling = the entry segment's END (absolute pts). The
                // target is always within this segment (find_segment_index), so
                // clamping here guarantees the discard terminates inside the
                // segment we decode from — fps-independent, no frame-count
                // guess. Worst case (a bad target past the segment) renders from
                // the next segment boundary instead of spinning forever.
                // Only discard when the target is genuinely PAST the segment
                // start — at a segment-aligned start (cold start / resume on a
                // boundary) there's nothing to trim, and trimming there risks
                // dropping the first displayable frame if its composition pts
                // dips just under the segment start (B-frame reorder).
                let seg_end = entry_seg.map(|s| s.end_time()).unwrap_or(abs_offset);
                let discard_below_us = if seek_offset > seg_start {
                    abs_offset.min(seg_end).as_micros() as i64
                } else {
                    0
                };
                log::debug!(
                    "[play] start: target={}ms seg_start={}ms discard_below={}ms vidx={} aidx={} segs={}",
                    seek_offset.as_millis(), seg_start.as_millis(), discard_below_us / 1000,
                    video_start_index, audio_start_index,
                    video_representation.segments.len()
                );
                // Anchor position/clock to the TARGET (video discards to it,
                // audio trims to it) — not the segment start.
                position_ms.store(seek_offset.as_millis() as u64, Ordering::Relaxed);
                stats
                    .last_decoded_pts_ms
                    .store(seek_offset.as_millis() as i64, Ordering::Relaxed);
                stats
                    .audio_last_decoded_pts_ms
                    .store(seek_offset.as_millis() as i64, Ordering::Relaxed);
                // Clear stale starvation state so the fresh pipeline can emit
                // its initial Playing event.
                stats.video_starving.store(false, Ordering::Relaxed);
                stats.audio_starving.store(false, Ordering::Relaxed);

                // Per-iteration decryptor snapshot (shared ClearKey state).
                let decryptor_snapshot: Option<Arc<dyn Decryptor>> = decryptor_cell
                    .lock()
                    .unwrap()
                    .clone()
                    .map(|d| d as Arc<dyn Decryptor>);

                // Per-iteration audio decoder (consumed by audio_play).
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

                // Fresh per-iteration switch channel for ABR soft-swaps.
                let (switch_tx, switch_rx) =
                    tokio::sync::watch::channel::<Option<VideoRepresenation>>(None);
                *video_switch_slot.lock().unwrap() = Some(switch_tx);

                // Capacity 8: keeps concurrent D3D11VA surfaces (DPB ~7 + pipeline)
                // well under Intel Arc A750's driver limit of ~21 individual
                // decoder surfaces. hevc_d3d11va2 auto-allocates a dynamic pool;
                // this bound prevents the pipeline from accumulating too many refs.
                //
                // Android direct mode: every queued frame PINS one of the
                // codec's scarce output buffers (typically ~8 for HEVC) —
                // a deep channel deadlocks the decoder outright. 2 in the
                // channel + 2 in the reorder buffer + 1 rendering leaves
                // the codec breathing room.
                let direct_window = video_output_window.get();
                // Adaptive frame rate: hint the content fps to the video plane
                // so the display can match its refresh rate. Idempotent — fine
                // to re-assert on every (re)build (seek / ABR swap).
                #[cfg(target_os = "android")]
                if direct_window != 0
                    && adaptive_frame_rate.load(std::sync::atomic::Ordering::Relaxed)
                {
                    if let Some(fps) = video_fps {
                        set_window_frame_rate(direct_window, fps.as_f32());
                    }
                }
                let frame_cap = if direct_window != 0 { 2 } else { 8 };
                let (frame_sender, frame_receiver) =
                    mpsc::channel::<DecodedVideoFrame>(frame_cap);
                let (sample_sender, sample_receiver) = mpsc::channel::<DecodedAudioFrame>(256);

                // DIAG: one generation id per play-loop (re)build, shared by this
                // iteration's video_supervisor + av_sync_handler so logcat shows
                // if a superseded generation keeps running (orphaned pipeline).
                static PIPELINE_GEN: AtomicU64 = AtomicU64::new(0);
                let gen = PIPELINE_GEN.fetch_add(1, Ordering::Relaxed);
                let video = tokio::spawn(video_supervisor(
                    gen,
                    video_representation,
                    video_start_index,
                    frame_sender,
                    video_ready.clone(),
                    stop.clone(),
                    stop_flag.clone(),
                    decryptor_snapshot.clone(),
                    video_decoder_factory.clone(),
                    Arc::clone(&http),
                    Arc::clone(&stats),
                    switch_rx,
                    position_ms.clone(),
                    Arc::clone(&events),
                    seg_in_flight,
                    origin,
                    direct_window,
                    Arc::clone(&pending_resume),
                ));

                let sample_rate = audio_sink.sample_rate();
                // Audio passthrough decision: host opted in AND the selected
                // track is a passthrough codec. The sink create self-gates
                // (None on unsupported → PCM). When engaged, feed raw AUs to
                // the bitstream sink and let av_sync's audio_sync_loop no-op
                // (its sample channel is dropped → recv None → returns).
                let want_passthrough = audio_passthrough.load(Ordering::Relaxed)
                    && matches!(audio_representation.codecs.as_str(), "ec-3" | "ac-3");
                let audio;
                #[cfg(target_os = "android")]
                {
                    let pt_sink: Option<Arc<dyn crate::renderers::AudioPassthrough>> =
                        if want_passthrough {
                            let enc = if audio_representation.codecs == "ac-3" {
                                renderers::audio_passthrough::ENCODING_AC3
                            } else {
                                renderers::audio_passthrough::ENCODING_E_AC3
                            };
                            renderers::audio_passthrough::AudioTrackSink::new(
                                enc,
                                audio_representation.audio_sampling_rate,
                                audio_representation.channels.unwrap_or(6) as u16,
                            )
                            .map(|s| Arc::new(s) as Arc<dyn crate::renderers::AudioPassthrough>)
                        } else {
                            None
                        };
                    audio_sink.set_passthrough(pt_sink.clone());
                    // Re-apply the user's current pause intent onto the freshly
                    // installed sink. A pause()/resume() that landed during the
                    // rebuild (while passthrough was momentarily None) only
                    // touched the old sink; without re-asserting it here the new
                    // bitstream sink starts un-paused and its first AU lazily
                    // play()s regardless — audio would run under a user pause
                    // while video stays parked, and the clock would then race
                    // ahead (the "audio plays, video frozen, then fast-forward
                    // on resume" seek-while-buffering bug).
                    if pt_sink.is_some() {
                        audio_sink.set_paused(paused.load(Ordering::Relaxed));
                    }
                    audio = if let Some(sink) = pt_sink {
                        drop(sample_sender);
                        log::info!("[audio] passthrough engaged ({})", audio_representation.codecs);
                        tokio::spawn(audio_passthrough_play(
                            audio_representation,
                            audio_start_index,
                            sink,
                            audio_ready.clone(),
                            stop.clone(),
                            stop_flag.clone(),
                            decryptor_snapshot,
                            Arc::clone(&http),
                            Arc::clone(&stats),
                            seg_in_flight,
                            discard_below_us,
                            Arc::clone(&pipeline_live),
                        ))
                    } else {
                        tokio::spawn(audio_play(
                            audio_representation,
                            audio_start_index,
                            audio_ready.clone(),
                            sample_sender,
                            sample_rate,
                            stop.clone(),
                            stop_flag.clone(),
                            decryptor_snapshot,
                            audio_decoder,
                            Arc::clone(&http),
                            Arc::clone(&stats),
                            seg_in_flight,
                        ))
                    };
                }
                #[cfg(not(target_os = "android"))]
                {
                    let _ = want_passthrough;
                    audio = tokio::spawn(audio_play(
                        audio_representation,
                        audio_start_index,
                        audio_ready.clone(),
                        sample_sender,
                        sample_rate,
                        stop.clone(),
                        stop_flag.clone(),
                        decryptor_snapshot,
                        audio_decoder,
                        Arc::clone(&http),
                        Arc::clone(&stats),
                        seg_in_flight,
                    ));
                }

                // New pipeline: not "live" until it produces its first frame.
                // Gates the ABR tick off this fragile startup window.
                pipeline_live.store(false, Ordering::Relaxed);
                av_sync_handler(
                    gen,
                    seek_offset,
                    discard_below_us,
                    video_ready.clone(),
                    frame_receiver,
                    video_sink.clone(),
                    position_ms.clone(),
                    audio_ready.clone(),
                    sample_receiver,
                    audio_sink.clone(),
                    stop.clone(),
                    stop_flag.clone(),
                    Arc::clone(&events),
                    media_duration,
                    paused.clone(),
                    pause_notify.clone(),
                    Arc::clone(&stats),
                    Arc::clone(&pipeline_live),
                )
                .await;

                let (play_res, audio_res) = join!(video, audio);
                log_task_result("video_supervisor", play_res);
                log_task_result("audio_play", audio_res);

                // Drop the watch sender so a stale apply_video_representation
                // between pipelines becomes a no-op.
                *video_switch_slot.lock().unwrap() = None;

                // Restart in-process iff a seek arrived (seek_target set again
                // by seek()/change_*_track). A real stop()/EOS leaves it None.
                if seek_target.read().await.is_none() {
                    break;
                }
            }
            // Outer loop ended — this play() invocation is truly over.
            // Kick the abr_tick task off the executor.
            let _ = abr_kill_tx.send(());
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
    // Upper-exclusive segment index. Default `usize::MAX` means "no soft
    // limit, run to natural EOF". The supervisor lowers this on ABR
    // swaps so the OLD pipeline drains its already-downloaded tail
    // cleanly (segments < new_start) while the NEW pipeline downloads
    // from new_start in parallel — no PTS overlap because the two
    // ranges are disjoint, and av_sync sees a continuous frame stream.
    soft_end_exclusive: Arc<AtomicUsize>,
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
        // A dropped receiver means the consumer (this pipeline's decode side) is
        // gone — a rebuild/ABR swap tore it down. Stop immediately; this is not
        // a network failure and must NOT hit the retry path, or an orphaned
        // downloader spams "downstream receiver dropped" for 30 s and wedges the
        // rebuild (see ABR_REBUILD_ORPHANED_DOWNLOADER handoff).
        if stop_flag.load(Ordering::Relaxed) || segment_sender.is_closed() {
            break;
        }
        // Soft end: supervisor sets this to the NEW pipeline's start
        // index on an ABR swap so the OLD pipeline exits naturally at
        // the swap boundary instead of needing a hard stop_flag.
        // Re-read every iteration so an in-flight swap takes effect
        // promptly.
        if i >= soft_end_exclusive.load(Ordering::Relaxed) {
            log::debug!(
                "[dl] soft end at segment {} reached (limit={}); pipeline draining for swap",
                i,
                soft_end_exclusive.load(Ordering::Relaxed),
            );
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
            // stop, or the receiver vanished (pipeline torn down) → terminate,
            // never retry a dropped channel.
            if stop_flag.load(Ordering::Relaxed) || segment_sender.is_closed() {
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
                    // Receiver dropped mid-send = consumer gone, not a transport
                    // error. Stop now instead of retrying (the SendError-loop wedge).
                    if segment_sender.is_closed() {
                        log::debug!("[dl] segment {} receiver gone — stopping (pipeline torn down)", i);
                        should_break = true;
                        break;
                    }
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

/// Parse the `tenc` box from a track's init segment and, if the track is
/// CENC-encrypted, resolve its content key up front (async — possibly via a
/// `LicenseResolver`) so the per-sample `decrypt_sample` stays sync on the hot
/// path. Returns `None` for a clear track. `label` ("video"/"audio") only
/// flavours the log lines and the error text.
async fn setup_track_crypto(
    init_data: &[u8],
    decryptor: Option<Arc<dyn Decryptor>>,
    label: &str,
) -> Result<Option<TrackCrypto>, Box<dyn Error + Send + Sync>> {
    let tenc = match parse_tenc(init_data) {
        Some(t) => t,
        None => {
            log::info!("{}: clear (no tenc box)", label);
            return Ok(None);
        }
    };

    log::info!(
        "{}: CENC encrypted, KID={} iv_size={}",
        label,
        kid_short(&tenc.default_kid),
        tenc.default_iv_size
    );
    let dec = decryptor.ok_or_else(|| -> Box<dyn Error + Send + Sync> {
        format!(
            "{} track is CENC-encrypted but no decryptor configured \
             (call Player::set_clearkey or set_license_resolver)",
            label
        )
        .into()
    })?;
    dec.ensure_key_for(tenc.default_kid)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> {
            format!("license resolve ({} kid={}): {}", label, kid_short(&tenc.default_kid), e).into()
        })?;
    Ok(Some(TrackCrypto {
        decryptor: dec,
        kid: tenc.default_kid,
        iv_size: tenc.default_iv_size as usize,
    }))
}

fn decrypt_segment_in_place(
    data_vec: &mut [u8],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracks::segment::Segment;

    /// Build a segment whose `start_time` / `end_time` work out to the
    /// passed-in milliseconds — handy because `find_segment_index` only
    /// reads `end_time`. Uses timescale=1000 so the base values ARE the
    /// ms values directly.
    fn seg_ms(start_ms: u64, end_ms: u64) -> Segment {
        Segment::new(
            &String::new(),
            &String::new(),
            0,
            0,
            Some(start_ms),
            Some(end_ms),
            Some(1000),
        )
        .expect("stub segment")
    }

    // ---------------- find_segment_index ----------------

    #[test]
    fn find_segment_index_empty_returns_zero() {
        // Empty slice — the function returns 0 as a "no-op safe" sentinel.
        // Callers gate their use of the result on the segments being
        // non-empty in practice, but the function itself shouldn't panic.
        assert_eq!(find_segment_index(&[], Duration::from_secs(5)), 0);
    }

    #[test]
    fn find_segment_index_picks_segment_containing_target() {
        let segs = vec![
            seg_ms(0, 5000),
            seg_ms(5000, 10000),
            seg_ms(10000, 15000),
        ];
        // 0 ms → segment 0 (end_time 5000 > 0)
        assert_eq!(find_segment_index(&segs, Duration::ZERO), 0);
        // 4999 ms → segment 0 (still inside)
        assert_eq!(find_segment_index(&segs, Duration::from_millis(4999)), 0);
        // 5000 ms → boundary; end_time>target so segment 1 (the one
        // starting at 5000), not segment 0 (whose end_time IS 5000 — fails
        // the strict `>` check).
        assert_eq!(find_segment_index(&segs, Duration::from_millis(5000)), 1);
        // 7500 ms → segment 1
        assert_eq!(find_segment_index(&segs, Duration::from_millis(7500)), 1);
        // 12500 ms → segment 2
        assert_eq!(find_segment_index(&segs, Duration::from_millis(12500)), 2);
    }

    #[test]
    fn find_segment_index_target_past_end_returns_last() {
        let segs = vec![seg_ms(0, 5000), seg_ms(5000, 10000)];
        // Target way past the last segment's end_time — falls through
        // the loop, returns segments.len()-1 so the caller can still
        // index into the slice without bounds checks.
        assert_eq!(find_segment_index(&segs, Duration::from_secs(3600)), 1);
    }

    #[test]
    fn find_segment_index_single_segment() {
        let segs = vec![seg_ms(0, 5000)];
        assert_eq!(find_segment_index(&segs, Duration::ZERO), 0);
        assert_eq!(find_segment_index(&segs, Duration::from_millis(2500)), 0);
        // Past end → still returns 0 (the last/only index).
        assert_eq!(find_segment_index(&segs, Duration::from_secs(100)), 0);
    }

    #[test]
    fn find_segment_index_real_dash_timings() {
        // 6-second segments (real-world DASH cadence — matches the
        // user's manifest where segment 0 spans pts 83..5964ms).
        let segs: Vec<Segment> = (0..10)
            .map(|i| seg_ms(i * 6000, (i + 1) * 6000))
            .collect();

        // Playback at 14.5 s → segment 2 (12-18 s window).
        assert_eq!(find_segment_index(&segs, Duration::from_millis(14_500)), 2);
        // Right after a segment boundary (24.001 s) → segment 4.
        assert_eq!(find_segment_index(&segs, Duration::from_millis(24_001)), 4);
    }

    // ---------------- update_bandwidth_ewma ----------------

    #[test]
    fn ewma_seeds_with_instant_value_when_zero() {
        let ewma = AtomicU64::new(0);
        // 1 MB in 1 second = 8 Mbps.
        update_bandwidth_ewma(&ewma, 1_000_000, Duration::from_secs(1));
        assert_eq!(ewma.load(Ordering::Relaxed), 8_000_000);
    }

    #[test]
    fn ewma_skips_zero_and_negative_inputs() {
        let ewma = AtomicU64::new(5_000_000);
        // Zero bytes — should not change EWMA.
        update_bandwidth_ewma(&ewma, 0, Duration::from_secs(1));
        assert_eq!(ewma.load(Ordering::Relaxed), 5_000_000);
        // Zero elapsed — should not change EWMA (divide-by-zero guard).
        update_bandwidth_ewma(&ewma, 1_000_000, Duration::ZERO);
        assert_eq!(ewma.load(Ordering::Relaxed), 5_000_000);
    }

    #[test]
    fn ewma_converges_with_repeated_samples() {
        let ewma = AtomicU64::new(0);
        // Seed with 1 Mbps.
        update_bandwidth_ewma(&ewma, 125_000, Duration::from_secs(1)); // 1 Mbps
        let seeded = ewma.load(Ordering::Relaxed);
        assert_eq!(seeded, 1_000_000);

        // Drive with 5 Mbps samples — EWMA should rise toward 5 Mbps.
        for _ in 0..20 {
            update_bandwidth_ewma(&ewma, 625_000, Duration::from_secs(1));
        }
        let after = ewma.load(Ordering::Relaxed);
        // Analytical: y_n = 5_000_000 − (5_000_000 − 1_000_000) × (7/8)^n.
        // After 20 samples that's ≈ 4_718_500 — 94% of the way to target.
        // Lower bound below leaves a safety margin for integer-truncation
        // jitter; upper bound asserts we never overshoot.
        assert!(
            (4_700_000..=5_000_000).contains(&after),
            "EWMA didn't converge: {} (expected ~4.7M..5M after 20 samples)",
            after
        );
    }

    #[test]
    fn ewma_steady_state_holds() {
        // Once converged, repeated identical samples shouldn't drift.
        let ewma = AtomicU64::new(5_000_000);
        for _ in 0..50 {
            update_bandwidth_ewma(&ewma, 625_000, Duration::from_secs(1));
        }
        let after = ewma.load(Ordering::Relaxed);
        // Stay within 1% of seed value.
        assert!(
            (4_950_000..=5_050_000).contains(&after),
            "EWMA drifted at steady state: {}",
            after
        );
    }

    #[test]
    fn ewma_smooths_single_spike() {
        // Steady 5 Mbps, then one 50 Mbps spike — EWMA shouldn't blow up.
        let ewma = AtomicU64::new(0);
        update_bandwidth_ewma(&ewma, 625_000, Duration::from_secs(1));
        for _ in 0..30 {
            update_bandwidth_ewma(&ewma, 625_000, Duration::from_secs(1));
        }
        let stable = ewma.load(Ordering::Relaxed);
        assert!(stable > 4_900_000 && stable <= 5_000_000);

        // One 50 Mbps sample.
        update_bandwidth_ewma(&ewma, 6_250_000, Duration::from_secs(1));
        let after_spike = ewma.load(Ordering::Relaxed);
        // EWMA moves toward 50 Mbps by alpha=1/8, so jumps to ~5+(50-5)/8 ≈ 10.6 Mbps.
        // Crucially it does NOT just snap to 50 Mbps.
        assert!(
            after_spike > 10_000_000 && after_spike < 12_000_000,
            "spike absorption broken: stable={} after_spike={}",
            stable,
            after_spike
        );
    }
}
