//! The player: its public handle, the pipeline that feeds it, and the tasks
//! that keep the two in step.

use crate::{
    AudioRenderer,
    VideoRenderer,
    AbrStrategy,
    AbrVideoProfile,
    AudioSink,
    BufferingReason,
    DecodedVideoFrame,
    HdrTonemapParams,
    HttpClient,
    LicenseResolver,
    PlayerErrorKind,
    PlayerEvent,
    RawDisplayHandle,
    RawWindowHandle,
    RequestInterceptor,
    RequestKind,
    RetryPolicy,
    SubtitleStyle,
    TrackInfo,
    TrackKind,
    Tracks,
    VideoSink,
};

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


use crate::crypto::{
    kid_short, parse_aac_config, parse_hvcc_bit_depth, parse_hvcc_nalus, parse_senc, parse_tenc,
    ClearKeyDecryptor,
    Decryptor, TrackCrypto,
};
use crate::decoders::{
    AudioCodec, AudioDecoder, AudioDecoderParams, DecodedAudioFrame,
    HwVideoDecoder, VideoCodec, VideoColorInfo, VideoDecoderParams,
};
use crate::parsers::mp4::aac_sampling_frequency_index_to_u32;
use pollster::FutureExt;
use re_mp4::Mp4;

pub type OffscreenPlayer = Player<VideoRenderer, AudioRenderer>;

use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
#[cfg(target_os = "android")]
use libc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::{broadcast, Notify, RwLock};
use crate::rt::Instant;
use tokio::{join, sync::mpsc::Sender};
use crate::tracks::audio::{AudioAdaptation, AudioRepresentation};
use crate::tracks::{
    segment::Segment,
    video::{VideoAdaptation, VideoRepresenation},
};
use url::Url;

use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver};
use crate::rt::JoinHandle;

use crate::manifest::Manifest;

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
/// Session-cumulative playback-quality gauges. See
/// [`Player::conformance_summary`]. All counters monotonically accumulate for
/// the Player's lifetime; thresholds belong to the harness, not here.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConformanceSummary {
    /// Buffering{Stall} transitions (either A/V side): user-visible spinners.
    pub stall_events: u64,
    /// Total wall ms spent in video-side starvation.
    pub stall_ms_total: u64,
    /// Mid-play pipeline rebuilds after failures — >0 means playback died
    /// and self-healed, a regression even when barely visible.
    pub pipeline_retries: u64,
    /// Max wall gap between consecutive rendered frames, ms (pause- and
    /// accounted-stall-corrected): freeze / swap-hole depth.
    pub render_gap_max_ms: u64,
    /// Frames rendered <5 ms apart (catch-up bursts). A few per LATE drain
    /// are normal; hundreds mean flicker/pacing regressions.
    pub render_burst_frames: u64,
    /// Max |A/V drift| observed, ms.
    pub av_drift_max_ms: i64,
    /// Frames whose render interval deviated from their media delta by
    /// >±10 ms (micro-stutter / judder).
    pub judder_frames: u64,
    /// Render-interval histogram, ms: [<25, 25–41, 42–58, >58].
    pub interval_hist: [u64; 4],
    pub video_frames_decoded: u64,
    pub video_frames_dropped: u64,
    /// Frames presented >45 ms after their master-clock time (visible
    /// per-frame lip-sync error; the LATE drain only drops past 80 ms).
    pub video_late_frames: u64,
    pub audio_underruns: u64,
}

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
pub(crate) struct StatsState {
    video_frames_decoded: AtomicU64,
    video_frames_dropped: AtomicU64,
    /// Frames presented >45 ms after their master-clock time (shown, not
    /// dropped — a visible per-frame lip-sync error). Conformance gauge.
    video_late_frames: AtomicU64,
    audio_underruns: AtomicU64,
    /// Wall-clock ms the download path was blocked waiting on network in
    /// the trailing second.
    net_stall_ms: AtomicU64,
    /// EWMA of segment download throughput in bits-per-second, surfaced via
    /// `Position.bandwidth_bps` and consumed by the ABR engine.
    bandwidth_bps_ewma: AtomicU64,
    /// Total media bytes downloaded this session — the ABR engine does not
    /// trust the estimate before `ABR_MIN_TOTAL_BYTES` (Shaka's
    /// `abr.minTotalBytes`).
    bandwidth_bytes_total: AtomicU64,
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
    /// Deadline of the current planned track-swap "hole": the video
    /// supervisor arms this right before tearing OLD's decoder down, while
    /// NEW's first GOP is still being configured/decoded. While unexpired,
    /// `video_sync_loop`'s 300 ms starvation timeout does NOT flip into
    /// Buffering — the last frame stays on screen, audio keeps rolling, and
    /// the LATE drain re-aligns video to the clock once NEW's frames land.
    /// Without it every ABR switch whose configure+first-GOP exceeded 300 ms
    /// flashed the buffering spinner and hiccuped audio. Expires on its own
    /// so a genuinely wedged swap still surfaces as Buffering.
    swap_grace_deadline: StdMutex<Option<Instant>>,
    /// Symmetric to `video_starving` — set by `audio_sync_loop` when
    /// IT hasn't received a frame for >300 ms. Video parks on its
    /// current frame instead of marching forward over silence.
    audio_starving: AtomicBool,
    /// Pipeline rebuilds spent trying to bring a dead audio output back.
    /// Lives on the per-Player stats (not the per-generation locals) so the
    /// budget survives the very rebuild it is counting.
    audio_output_rebuilds: AtomicU32,
    /// Measured A/V clock drift in ms: how far the video wall clock has
    /// run ahead of the audio device clock since this pipeline started
    /// (negative = audio ahead). Written ~1 Hz by video_sync_loop when
    /// the sink supports `played_ms`; surfaced via PlayerEvent::Stats.
    /// The device crystal vs CLOCK_MONOTONIC disagree by 10–100 ppm, so
    /// multi-hour sessions are expected to show a slow linear trend —
    /// this is the measurement that tells us when an active servo is
    /// warranted on a given device class.
    av_drift_ms: std::sync::atomic::AtomicI64,

    // ---- pipeline stage counters (diagnostics) ------------------------------
    // Cumulative per-stage progress counters, sampled by the watchdog thread
    // (see `spawn_pipeline_watchdog`) so a stalled playback prints exactly
    // WHICH stage stopped advancing. Cheap relaxed atomics; bumped at each
    // stage's hot point.
    /// Audio segments received by `audio_decoder_task`.
    diag_audio_seg: AtomicU64,
    /// Audio frames pushed into the audio_sync channel by the decoder.
    diag_audio_dec: AtomicU64,
    /// Audio frames received by `audio_sync_loop`.
    diag_audio_sync: AtomicU64,
    /// Audio frames fully handed to the sink (put_samples returned).
    diag_audio_sunk: AtomicU64,
    /// Video segments consumed (post-prepare) by `video_decoder_task`.
    diag_video_seg: AtomicU64,
    /// Video frames rendered by the vsync loop.
    diag_video_ren: AtomicU64,

    // ---- conformance counters (see Player::conformance_summary) -------------
    // Session-cumulative gauges a soak harness asserts thresholds against.
    // Everything user-visible that ALSO surfaces as an event (Buffering,
    // Error, EndOfStream, TrackChanged) is counted by the harness from the
    // event stream instead — these cover what events can't see.
    /// Times a Buffering{Stall} transition fired (either A/V side) — each is
    /// a user-visible spinner + audio pause.
    stall_events: AtomicU64,
    /// Total wall ms spent in video-side starvation (stall depth, not count).
    stall_ms_total: AtomicU64,
    /// Supervisor mid-play pipeline rebuilds after failures. Anything > 0
    /// means playback died and self-healed — a red flag even when the user
    /// barely noticed (e.g. the warm-handoff ENOMEM loop).
    pipeline_retries: AtomicU64,
    /// Max wall gap between consecutive rendered frames, ms (pause- and
    /// accounted-stall-corrected). Freeze / swap-hole detector.
    render_gap_max_ms: AtomicU64,
    /// Frames rendered <5 ms after their predecessor — catch-up bursts.
    /// A handful per LATE drain is normal; hundreds = flicker/pacing bug.
    render_burst_frames: AtomicU64,
    /// Max |A/V drift| observed, ms.
    av_drift_max_ms: std::sync::atomic::AtomicI64,
    /// Sequence number of the video segment most recently handed to the
    /// decoder (Stats/debug-HUD "where we are").
    video_segment_id: AtomicU64,
    /// Frames whose wall render interval deviated from their media delta by
    /// more than ±10 ms (micro-stutter / judder detector). A 24p frame is due
    /// every ~42 ms; rendering it at 30 or 55 ms is exactly the "obraz se
    /// mikrotrhá" a viewer perceives even when nothing is dropped.
    judder_frames: AtomicU64,
    /// Render-interval histogram, ms: <25 | 25–41 | 42–58 | >58. For 24p
    /// content a clean cadence sits in 25–41/42–58; >58 = a skipped slot
    /// the viewer can see. Same buckets as the desktop HUD's UI-layer
    /// histogram so the two are comparable.
    int_lt25: AtomicU64,
    int_25_41: AtomicU64,
    int_42_58: AtomicU64,
    int_gt58: AtomicU64,
}

/// How to interpret the bytes handed to
/// [`Player::add_external_subtitle_track`], and how to present the
/// resulting track. `Default` means "detect everything, no offset".
#[derive(Clone, Debug, Default)]
pub struct ExternalSubtitleOptions {
    /// BCP-47 language tag (`"cs"`, `"en"`). Surfaces as the track's
    /// `lang`, which is what a picker groups by; empty when unknown.
    pub language: Option<String>,
    /// Human-readable name for a picker, typically the file name. Only
    /// logged today — the track list has no label field of its own yet,
    /// so hosts keep their own mapping by representation id.
    pub label: Option<String>,
    /// Source format. Leave as `Auto` unless the host knows better than
    /// the payload does.
    pub format: crate::parsers::sidecar::SubtitleFormat,
    /// Force a character encoding by `encoding_rs` label (e.g.
    /// `"windows-1250"`) instead of detecting one. The escape hatch for
    /// short files where detection guesses wrong.
    pub encoding: Option<String>,
    /// Shift every cue by this many milliseconds — positive makes
    /// subtitles appear later. The usual "subs are two seconds out" fix
    /// for a sidecar file cut for a different release.
    pub time_offset_ms: i64,
    /// Mark the track as forced subtitles (signs and untranslated
    /// dialogue only), the same way a manifest `Role` would.
    pub forced: bool,
}

/// External subtitle ids count down from here, far above any manifest
/// `@id`, so a host can tell the two apart at a glance and the ranges can
/// never collide.
const EXTERNAL_TEXT_ID_BASE: u32 = u32::MAX;

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
    /// Why the NEXT pipeline build is happening, so its opening `Buffering`
    /// can say so. Every rebuild goes through `seek_target` regardless of
    /// cause — a user seek, a track switch, a stall recovery — and a consumer
    /// that cannot tell them apart has to show the same spinner for all of
    /// them. Set by whoever triggers the rebuild, consumed once by the
    /// pipeline that results.
    rebuild_reason: Arc<StdMutex<BufferingReason>>,
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
    /// When the current representation was chosen (pipeline (re)start or
    /// ABR switch). The ABR tick waits `ABR_SWITCH_INTERVAL` from here
    /// before switching again — Shaka's `abr.switchInterval` (8 s): "keeps
    /// us from changing too often and annoying the user", and it covers the
    /// start-up window where the first bandwidth samples are still noisy.
    abr_switch_at: Arc<StdMutex<Option<Instant>>>,

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
    subtitle_representation: Arc<StdMutex<Option<crate::tracks::text::TextRepresenation>>>,

    /// Subtitle tracks the host added from its own bytes via
    /// `add_external_subtitle_track`, kept beside the manifest's rather
    /// than merged into `tracks`: they outlive `prepare()` (a host may
    /// add them before the manifest has even been fetched) and must not
    /// be wiped when the parsed track tree is replaced. `get_tracks`
    /// concatenates the two.
    external_text: Arc<StdMutex<Vec<crate::tracks::text::TextAdaptation>>>,
    /// Source of ids for external tracks. Manifest `@id`s are small
    /// integers, so external tracks count down from `u32::MAX` — no
    /// coordination needed to stay clear of them, and the range is
    /// obvious in a log line.
    next_external_id: Arc<AtomicU32>,

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

    /// Force HDR (PQ/HLG) video to decode to an 8-bit destination; the
    /// in-player tonemap still runs, at 8-bit quantization of the PQ
    /// signal. Debug/compat knob — currently honoured by the VideoToolbox
    /// decoder (macOS/iOS) only. Seeded from the `RUST_PLAYER_VT_FORCE_8BIT`
    /// env var; sampled at every decoder configure (play / retry / ABR
    /// swap), so a change applies from the next (re)configure on.
    hdr_decode_8bit: Arc<std::sync::atomic::AtomicBool>,

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
    rt: crate::rt::Handle,
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
            rebuild_reason: Arc::clone(&self.rebuild_reason),
            position_ms: Arc::clone(&self.position_ms),
            decryptor: Arc::clone(&self.decryptor),
            stats: Arc::clone(&self.stats),
            abr_strategy: Arc::clone(&self.abr_strategy),
            abr_video_profile: Arc::clone(&self.abr_video_profile),
            abr_switch_at: Arc::clone(&self.abr_switch_at),
            video_switch_tx: Arc::clone(&self.video_switch_tx),
            buffer_target_secs: Arc::clone(&self.buffer_target_secs),
            subtitle_representation: Arc::clone(&self.subtitle_representation),
            external_text: Arc::clone(&self.external_text),
            next_external_id: Arc::clone(&self.next_external_id),
            video_output_window: Arc::clone(&self.video_output_window),
            adaptive_frame_rate: Arc::clone(&self.adaptive_frame_rate),
            audio_passthrough: Arc::clone(&self.audio_passthrough),
            hdr_decode_8bit: Arc::clone(&self.hdr_decode_8bit),
            pipeline_live: Arc::clone(&self.pipeline_live),
            pending_resume: Arc::clone(&self.pending_resume),
            video_renderer: Arc::clone(&self.video_renderer),
            audio_renderer: Arc::clone(&self.audio_renderer),
            rt: self.rt.clone(),
        }
    }
}

mod sync;
use sync::*;

mod text;
use text::*;

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

    /// Browser: render into a host-provided `<canvas>` (WebGPU, WebGL2
    /// fallback) and play audio through Web Audio. Async because a WebGPU
    /// adapter/device can only be obtained asynchronously — there is no
    /// `block_on` in the browser. The host keeps the canvas in the DOM for the
    /// player's lifetime and forwards size changes through `Player::resize`.
    /// Construct from a user gesture (a click handler) so the `AudioContext`
    /// is allowed to start.
    #[cfg(target_arch = "wasm32")]
    pub async fn new_from_canvas(canvas: web_sys::HtmlCanvasElement, width: u32, height: u32) -> Self {
        let video_renderer = Arc::new(VideoRenderer::new_from_canvas(canvas, width, height).await);
        let audio_renderer = Arc::new(AudioRenderer::new());
        Self::from_renderers(video_renderer, audio_renderer)
    }

    /// Browser: whether PQ (HDR10) representations render through the
    /// engine's own PQ → SDR tonemap — the same mapping as the native
    /// players — or, when the browser's frame conversion could not be
    /// verified at start-up, through the browser's own conversion.
    #[cfg(target_arch = "wasm32")]
    pub fn web_hdr_tonemap_available(&self) -> bool {
        self.video_renderer.web_hdr_tonemap_available()
    }

    /// Browser: render PQ frames as the browser converts them instead of
    /// the engine tonemap. See [`VideoRenderer::set_web_hdr_passthrough`].
    #[cfg(target_arch = "wasm32")]
    pub fn set_web_hdr_passthrough(&self, passthrough: bool) {
        self.video_renderer.set_web_hdr_passthrough(passthrough)
    }

    /// Assemble a `Player` from already-built renderers. Shared tail of every
    /// constructor above — the only difference between the winit and embedded
    /// paths is how the `VideoRenderer` obtained its surface.
    fn from_renderers(
        video_renderer: Arc<VideoRenderer>,
        audio_renderer: Arc<AudioRenderer>,
    ) -> Self {
        Self::with_sinks(video_renderer, audio_renderer)
    }
}

impl<V: VideoSink, A: AudioSink> Player<V, A> {
    /// Build a player over arbitrary sinks. This is how the platform
    /// constructors assemble the player, exposed so a host (or a test
    /// harness) can wrap the stock renderers — e.g. tap every presented
    /// frame / queued sample to measure lip-sync independently of the
    /// engine's own clock (see `examples/conformance.rs`).
    pub fn with_sinks(video_renderer: Arc<V>, audio_renderer: Arc<A>) -> Self {
        // Windows ships a ~15.6 ms default timer resolution and (since Win10
        // 2004) keeps unfocused/background processes on the coarse timer
        // unless they opt in. Every pacing sleep in the vsync loop then wakes
        // up to ~15 ms late — measured as ~11% of frames >±10 ms off their
        // media cadence (the "obraz se mikrotrhá" report). One process-wide
        // timeBeginPeriod(1) restores millisecond wakeups for the process
        // lifetime (the OS releases it at exit).
        #[cfg(target_os = "windows")]
        {
            static TIMER_RES: std::sync::Once = std::sync::Once::new();
            TIMER_RES.call_once(|| {
                #[link(name = "winmm")]
                extern "system" {
                    fn timeBeginPeriod(u_period: u32) -> u32;
                }
                let r = unsafe { timeBeginPeriod(1) };
                log::info!("[player] timeBeginPeriod(1) -> {}", r);
            });
        }
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
            rebuild_reason: Arc::new(StdMutex::new(BufferingReason::Initial)),
            position_ms: Arc::new(AtomicU64::new(0)),

            decryptor: Arc::new(StdMutex::new(None)),

            stats: Arc::new(StatsState::default()),
            abr_strategy: Arc::new(ArcSwap::from_pointee(AbrStrategy::default())),
            abr_video_profile: Arc::new(ArcSwap::from_pointee(AbrVideoProfile::default())),
            abr_switch_at: Arc::new(StdMutex::new(None)),
            video_switch_tx: Arc::new(StdMutex::new(None)),
            buffer_target_secs: Arc::new(AtomicU32::new(DEFAULT_BUFFER_TARGET_SECS)),
            subtitle_representation: Arc::new(StdMutex::new(None)),
            external_text: Arc::new(StdMutex::new(Vec::new())),
            next_external_id: Arc::new(AtomicU32::new(EXTERNAL_TEXT_ID_BASE)),
            video_output_window: Arc::new(DirectWindow::new()),
            adaptive_frame_rate: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            audio_passthrough: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hdr_decode_8bit: Arc::new(std::sync::atomic::AtomicBool::new(
                std::env::var_os("RUST_PLAYER_VT_FORCE_8BIT").is_some(),
            )),
            pipeline_live: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_resume: Arc::new(StdMutex::new(None)),

            video_renderer,
            audio_renderer,

            // Every constructor runs inside a Tokio runtime (desktop:
            // `#[tokio::main]`; iOS/Android: the host enters the runtime before
            // calling in), so a current handle is always available here. Storing
            // it lets `resize`/`seek`/track-switch spawn from any thread later.
            rt: crate::rt::Handle::current(),
        }
    }
}

// ---------------------------------------------------------------------------
// Additive: in-app (offscreen) construction. Reuses `VideoRenderer` with an
// offscreen target, so the in-app player is the SAME concrete type as the
// windowed desktop player — every existing method applies unchanged. Nothing in
// the windowed path calls these; existing consumers are unaffected.
// ---------------------------------------------------------------------------

impl Player<VideoRenderer, AudioRenderer> {
    /// In-app video path: render decoded frames into an offscreen wgpu texture
    /// on a device the host SHARES with its compositor (Slint/iced/egui), then
    /// pull [`current_video_texture`](Self::current_video_texture) each
    /// frame-ready for zero-copy import. No window, no swapchain.
    ///
    /// `device`/`queue` MUST be the host's own, created with
    /// `TEXTURE_FORMAT_NV12 | P010 | 16BIT_NORM` on desktop (see the feature set
    /// in `video.rs::new_with_surface`) so the hardware-decoded frame import
    /// works. Must be called inside a Tokio runtime.
    pub async fn new_offscreen(
        device: wgpu::Device,
        queue: wgpu::Queue,
        backend: wgpu::Backend,
        width: u32,
        height: u32,
    ) -> Self {
        let video_renderer = Arc::new(VideoRenderer::new_offscreen(
            device, queue, backend, width, height,
        ));
        let audio_renderer = Arc::new(AudioRenderer::new());
        Self::from_renderers(video_renderer, audio_renderer)
    }

    /// The freshest finished video texture, for the host to wrap with its GUI's
    /// texture-import API (e.g. `slint::Image::try_from`). Cheap — clones a
    /// handle to the already-published offscreen texture. Offscreen mode only.
    pub fn current_video_texture(&self) -> wgpu::Texture {
        self.video_renderer
            .offscreen_target()
            .expect("current_video_texture requires offscreen mode")
            .current_texture()
    }

    /// Register a callback fired after each offscreen frame is published, so the
    /// host can request a redraw / re-import the texture. No-op in windowed mode.
    pub fn set_frame_ready_callback<F: Fn() + Send + Sync + 'static>(&self, cb: F) {
        if let Some(t) = self.video_renderer.offscreen_target() {
            t.set_on_ready(cb);
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
    /// Fetch ClearKey content keys WRAPPED from `url`
    /// (`docs/CLEARKEY_WRAPPED_LICENCE.md`): per KID an ephemeral ECDH P-256
    /// exchange, HKDF-SHA256 and AES-256-GCM, so no key crosses the wire in
    /// the clear. The POST goes through the request interceptor
    /// (`RequestKind::License`) for the host's authorisation headers.
    /// Natively the unwrapped key lands in the ClearKey cache; in the browser
    /// it is unwrapped into a non-extractable WebCrypto key and every
    /// decrypt for that KID runs through WebCrypto. `hkdf_info` must match
    /// the server's (`None` = the documented default). Replaces any
    /// previously installed `LicenseResolver`.
    pub fn set_wrapped_licence(&self, url: String, hkdf_info: Option<String>) {
        let resolver = crate::wrapped_licence::WrappedLicenceResolver::new(url, hkdf_info, Arc::clone(&self.http));
        self.set_license_resolver(Arc::new(resolver));
    }

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
        // Sidecar subtitles were picked for the stream being replaced;
        // carrying them over would show cues timed against other media.
        self.clear_external_subtitle_tracks();
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
        let mut tracks = match Tracks::new(base_url, &manifest.mpd, &manifest.content, &self.http).await {
            Ok(t) => t,
            Err(e) => {
                self.emit_error(PlayerErrorKind::ManifestParse, format!("tracks: {}", e));
                return Err(e);
            }
        };
        // Drop what this platform's decoders cannot play before anything
        // selects from the tree (no-op where the platform has no probe).
        crate::decoders::support::prune_unsupported(&mut tracks).await;
        if tracks.video.is_empty() {
            let msg = "no video representation this platform can decode".to_string();
            self.emit_error(PlayerErrorKind::ManifestParse, msg.clone());
            return Err(msg.into());
        }
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

    /// The track tree for the current stream, with any host-added
    /// subtitle tracks appended to `.text` after the manifest's own.
    pub fn get_tracks(&self) -> Result<Tracks, Box<dyn Error>> {
        let mut tracks = match self.tracks.lock().unwrap().as_ref() {
            Some(t) => t.clone(),
            None => return Err("No parsed tracks - player not prepared".into()),
        };
        tracks
            .text
            .extend(self.external_text.lock().unwrap().iter().cloned());
        Ok(tracks)
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
        // A selection the consumer did not make itself is still a selection it
        // has to render: without this event a UI has no way to learn what the
        // default pick was, and its track menu shows nothing as current until
        // the user changes something. Same event as every later switch, so
        // consumers need no separate startup path.
        let _ = self.events.send(PlayerEvent::TrackChanged {
            kind: TrackKind::Video,
            info: video_track_info(representation),
        });
    }

    pub fn set_audio_track(
        &self,
        adaptation: &AudioAdaptation,
        representation: &AudioRepresentation,
    ) {
        *self.audio_adaptation.lock().unwrap() = Some(adaptation.clone());
        *self.audio_representation.lock().unwrap() = Some(representation.clone());
        let _ = self.events.send(PlayerEvent::TrackChanged {
            kind: TrackKind::Audio,
            info: audio_track_info(adaptation, representation),
        });
    }

    /// Session-cumulative conformance gauges for automated soak testing (the
    /// counters run for the Player's lifetime, across seeks and track
    /// switches). A harness plays a scenario, reads this once at the end and
    /// asserts thresholds; everything user-visible that surfaces as an event
    /// (Buffering, Error, EndOfStream, TrackChanged) is expected to be
    /// counted from the event stream instead.
    pub fn conformance_summary(&self) -> ConformanceSummary {
        let s = &self.stats;
        ConformanceSummary {
            stall_events: s.stall_events.load(Ordering::Relaxed),
            stall_ms_total: s.stall_ms_total.load(Ordering::Relaxed),
            pipeline_retries: s.pipeline_retries.load(Ordering::Relaxed),
            render_gap_max_ms: s.render_gap_max_ms.load(Ordering::Relaxed),
            render_burst_frames: s.render_burst_frames.load(Ordering::Relaxed),
            av_drift_max_ms: s.av_drift_max_ms.load(Ordering::Relaxed),
            judder_frames: s.judder_frames.load(Ordering::Relaxed),
            interval_hist: [
                s.int_lt25.load(Ordering::Relaxed),
                s.int_25_41.load(Ordering::Relaxed),
                s.int_42_58.load(Ordering::Relaxed),
                s.int_gt58.load(Ordering::Relaxed),
            ],
            video_frames_decoded: s.video_frames_decoded.load(Ordering::Relaxed),
            video_frames_dropped: s.video_frames_dropped.load(Ordering::Relaxed),
            video_late_frames: s.video_late_frames.load(Ordering::Relaxed),
            audio_underruns: s.audio_underruns.load(Ordering::Relaxed),
        }
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
            *self.rebuild_reason.lock().unwrap() = BufferingReason::TrackSwitch;
            self.seek_internal(self.position());
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
    /// Cues are laid out ExoPlayer-style inside the aspect-fitted picture
    /// (8 % of the picture height above its bottom edge by default — media3
    /// `SubtitleView.DEFAULT_BOTTOM_PADDING_FRACTION`). This inset is the
    /// equivalent of padding that view: it raises the bottom of the layout
    /// box in surface pixels, so pass the real bottom inset from
    /// `WindowInsets` (system bars / display cutout / reported overscan).
    /// On Android TV, where HDMI overscan is usually invisible to the app,
    /// pass `max(windowInsets.bottom, 0.05 * surfaceHeight)` (the Android
    /// TV title-safe margin) so cues clear it. 0 (default) = no extra
    /// padding, i.e. exactly what an ExoPlayer app shows out of the box.
    ///
    /// Takes effect on the next presented frame. Re-call it on every
    /// inset/size change.
    pub fn set_subtitle_safe_insets(&self, bottom_px: u32) {
        log::info!("[subs] bottom safe inset {}px", bottom_px);
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

    /// Debug/compat switch: force HDR (PQ/HLG) video to decode to an
    /// 8-bit destination. The in-player HDR→SDR tonemap still runs — the
    /// picture stays colour-correct, just with 8-bit quantization of the
    /// PQ signal. Currently honoured by the VideoToolbox decoder
    /// (macOS/iOS); other platforms ignore it. Default OFF unless the
    /// `RUST_PLAYER_VT_FORCE_8BIT` env var is set at Player construction.
    ///
    /// Sampled at decoder configure time, so a change applies from the
    /// next pipeline (re)build (play, seek-restart, ABR swap) — not to
    /// frames already in flight. Like `set_hdr_tonemap`, the player does
    /// not persist this across instances.
    pub fn set_hdr_decode_8bit(&self, enabled: bool) {
        self.hdr_decode_8bit
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

    /// Add a subtitle track from bytes the host supplies itself — the
    /// sidecar `.srt` / `.vtt` a desktop user picked next to the movie.
    ///
    /// The player does no file picking and no fetching here: the host owns
    /// that, hands over the bytes, and gets back a representation that
    /// behaves like any other subtitle track. It shows up in
    /// [`Player::get_tracks`]`().text` and is activated with the ordinary
    /// [`Player::set_subtitle_track`] — there is no separate "external
    /// subtitles" mode to special-case in a UI.
    ///
    /// Format (WebVTT / SubRip) and character encoding are detected from
    /// the payload; see [`ExternalSubtitleOptions`] to override either, to
    /// label the track, or to nudge its timing.
    ///
    /// Can be called before `prepare()`: added tracks are held separately
    /// from the manifest's and survive it being parsed (they become
    /// listable through `get_tracks` once it succeeds). They do NOT
    /// survive [`Player::open_url`], since subtitles for the previous
    /// stream are meaningless against a new one.
    ///
    /// Returns the representation, so the caller can select it right away:
    ///
    /// ```no_run
    /// # use player::{ExternalSubtitleOptions, Player};
    /// # fn demo(player: &Player, bytes: Vec<u8>) -> Result<(), Box<dyn std::error::Error>> {
    /// let track = player.add_external_subtitle_track(
    ///     &bytes,
    ///     ExternalSubtitleOptions {
    ///         label: Some("Czech (file)".into()),
    ///         language: Some("cs".into()),
    ///         ..Default::default()
    ///     },
    /// )?;
    /// player.set_subtitle_track(&track);
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_external_subtitle_track(
        &self,
        bytes: &[u8],
        options: ExternalSubtitleOptions,
    ) -> Result<crate::tracks::text::TextRepresenation, Box<dyn Error>> {
        let parsed = crate::parsers::sidecar::parse(
            bytes,
            options.format,
            options.encoding.as_deref(),
            options.time_offset_ms,
        )?;

        // Counting down keeps external ids clear of manifest ones without
        // having to look at what the manifest used.
        let id = self.next_external_id.fetch_sub(1, Ordering::Relaxed);
        log::info!(
            "[subs] added external track id={} format={:?} encoding={} cues={} label={:?}",
            id,
            parsed.format,
            parsed.encoding,
            parsed.cues.len(),
            options.label,
        );
        let (representation, adaptation) = external_track(id, parsed, &options);
        self.external_text.lock().unwrap().push(adaptation);
        Ok(representation)
    }

    /// Drop a previously added external subtitle track. Returns whether
    /// one with that id was found. If it happens to be the selected
    /// track, subtitles are turned off too — otherwise the overlay would
    /// keep showing cues from a track that no longer exists.
    pub fn remove_external_subtitle_track(&self, id: u32) -> bool {
        let removed = {
            let mut external = self.external_text.lock().unwrap();
            let before = external.len();
            external.retain(|a| a.id != id);
            external.len() != before
        };
        if removed {
            let selected = self
                .subtitle_representation
                .lock()
                .unwrap()
                .as_ref()
                .map(|r| r.id == id)
                .unwrap_or(false);
            if selected {
                self.clear_subtitle_track();
            }
        }
        removed
    }

    /// Drop every external subtitle track. Called automatically by
    /// `open_url`.
    pub fn clear_external_subtitle_tracks(&self) {
        let selected_external = self
            .subtitle_representation
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.is_external())
            .unwrap_or(false);
        self.external_text.lock().unwrap().clear();
        if selected_external {
            self.clear_subtitle_track();
        }
    }

    /// Select a subtitle track. Spawns the text_play pipeline
    /// immediately — works regardless of whether `play()` is currently
    /// running, has finished, or hasn't been called yet. Single-file
    /// VTT downloads once then exits; CMAF streaming runs until the
    /// segment list is exhausted or `clear_subtitle_track` fires.
    pub fn set_subtitle_track(&self, representation: &crate::tracks::text::TextRepresenation) {
        *self.subtitle_representation.lock().unwrap() = Some(representation.clone());
        let _ = self.events.send(PlayerEvent::TrackChanged {
            kind: TrackKind::Subtitle,
            info: text_track_info(representation),
        });
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

    pub fn current_subtitle_representation(&self) -> Option<crate::tracks::text::TextRepresenation> {
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
        /// Shaka Player `abr.switchInterval` default.
        const ABR_SWITCH_INTERVAL: Duration = Duration::from_secs(8);
        /// Shaka Player `abr.minTotalBytes` default.
        const ABR_MIN_TOTAL_BYTES: u64 = 128_000;
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
        // Shaka `abr.minTotalBytes` (128 kB): an estimate built on less than
        // this is a guess (one small init segment), not a measurement.
        if self.stats.bandwidth_bytes_total.load(Ordering::Relaxed) < ABR_MIN_TOTAL_BYTES {
            return;
        }
        // Shaka `abr.switchInterval` (8 s), measured from the last choice —
        // the pipeline (re)start included, so the first seconds after a
        // start, seek or switch never see another switch on top.
        if let Some(at) = *self.abr_switch_at.lock().unwrap() {
            if at.elapsed() < ABR_SWITCH_INTERVAL {
                return;
            }
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
        let current_local = current_id.and_then(|cid| {
            candidate_indices
                .iter()
                .position(|&i| adaptation.representations[i].id == cid)
        });
        let pick_local = match crate::abr::pick_representation(&bws, ewma_bps, safety, current_local) {
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
            // ExoPlayer gates up-switches on buffered media too
            // (`minDurationForQualityIncreaseMs`, 10 s against its 50 s
            // buffer); ours is scaled to the 8 s buffer target.
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
        *self.abr_switch_at.lock().unwrap() = Some(Instant::now());
        self.apply_video_representation_soft(picked);
    }

    pub fn change_audio_track(
        &self,
        adaptation: &AudioAdaptation,
        representation: &AudioRepresentation,
    ) {
        *self.audio_adaptation.lock().unwrap() = Some(adaptation.clone());
        *self.audio_representation.lock().unwrap() = Some(representation.clone());
        let _ = self.events.send(PlayerEvent::TrackChanged {
            kind: TrackKind::Audio,
            info: audio_track_info(adaptation, representation),
        });
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
            *self.rebuild_reason.lock().unwrap() = BufferingReason::TrackSwitch;
            self.seek_internal(self.position());
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
        let rebuild_reason = self.rebuild_reason.clone();
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
                    match crate::decoders::ffmpeg_hw::SharedHwDevice::new() {
                        Ok(dev) => {
                            log::info!("[video] shared hw-device created; ABR swaps reuse it");
                            Arc::new(move || {
                                Box::new(crate::decoders::ffmpeg_hw::FfmpegHwDecoder::new_shared(
                                    dev.clone(),
                                )) as Box<dyn HwVideoDecoder>
                            })
                        }
                        Err(e) => {
                            log::warn!(
                                "[video] shared hw-device init failed ({e}); per-decoder device fallback"
                            );
                            Arc::new(|| {
                                Box::new(crate::decoders::ffmpeg_hw::FfmpegHwDecoder::new())
                                    as Box<dyn HwVideoDecoder>
                            })
                        }
                    };
                factory
            }
            #[cfg(target_os = "android")]
            {
                Arc::new(|| Box::new(crate::decoders::mediacodec::MediaCodecDecoder::new()))
            }
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                Arc::new(|| {
                    let d = crate::decoders::videotoolbox::VideoToolboxDecoder::new()
                        .expect("VideoToolboxDecoder::new");
                    Box::new(d) as Box<dyn HwVideoDecoder>
                })
            }
            #[cfg(target_arch = "wasm32")]
            {
                Arc::new(|| {
                    Box::new(crate::decoders::webcodecs::WebCodecsVideoDecoder::new())
                        as Box<dyn HwVideoDecoder>
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
        let abr_switch_at = Arc::clone(&self.abr_switch_at);
        let audio_passthrough = Arc::clone(&self.audio_passthrough);
        let hdr_decode_8bit = Arc::clone(&self.hdr_decode_8bit);
        let play = crate::rt::spawn(async move {
            // ABR tick runs once for the whole play() lifetime (survives
            // every seek/track-switch restart below). On Manual it's a
            // no-op each tick.
            crate::rt::spawn(async move {
                // A plain sleep-then-tick loop (not `tokio::time::interval`):
                // ticks that fall behind are skipped rather than burst, which
                // is exactly `MissedTickBehavior::Skip`, and it runs on the
                // runtime facade so the browser build has it too.
                loop {
                    tokio::select! {
                        _ = crate::rt::sleep(Duration::from_secs(1)) => {
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
                    Box::new(crate::decoders::ffmpeg_audio::FfmpegAudioDecoder::new());
                #[cfg(target_os = "android")]
                let audio_decoder: Box<dyn AudioDecoder> =
                    Box::new(crate::decoders::mediacodec_audio::MediaCodecAudioDecoder::new());
                #[cfg(target_arch = "wasm32")]
                let audio_decoder: Box<dyn AudioDecoder> =
                    Box::new(crate::decoders::webcodecs::WebCodecsAudioDecoder::new());

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
                let video = crate::rt::spawn(video_supervisor(
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
                    Arc::clone(&hdr_decode_8bit),
                    Arc::clone(&pending_resume),
                ));

                let sample_rate = audio_sink.sample_rate();
                let out_channels = audio_sink.channels();
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
                                crate::renderers::audio::audio_passthrough::ENCODING_AC3
                            } else {
                                crate::renderers::audio::audio_passthrough::ENCODING_E_AC3
                            };
                            crate::renderers::audio::audio_passthrough::AudioTrackSink::new(
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
                        crate::rt::spawn(audio_passthrough_play(
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
                        crate::rt::spawn(audio_play(
                            audio_representation,
                            audio_start_index,
                            audio_ready.clone(),
                            sample_sender,
                            sample_rate,
                            out_channels,
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
                    audio = crate::rt::spawn(audio_play(
                        audio_representation,
                        audio_start_index,
                        audio_ready.clone(),
                        sample_sender,
                        sample_rate,
                        out_channels,
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
                // Gates the ABR tick off this fragile startup window, and the
                // (re)start counts as a choice for the switch interval.
                pipeline_live.store(false, Ordering::Relaxed);
                *abr_switch_at.lock().unwrap() = Some(Instant::now());
                // Audio-output liveness watch, scoped to this generation: it
                // rebuilds the pipeline if the output dies under us, so it must
                // not outlive the generation it is watching.
                let audio_watchdog = crate::rt::spawn(audio_output_watchdog(
                    gen,
                    audio_sink.clone(),
                    Arc::clone(&stats),
                    Arc::clone(&events),
                    paused.clone(),
                    stop.clone(),
                    stop_flag.clone(),
                    position_ms.clone(),
                    seek_target.clone(),
                ));
                av_sync_handler(
                    gen,
                    seek_offset,
                    origin,
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
                    Arc::clone(&rebuild_reason),
                )
                .await;
                audio_watchdog.abort();

                let (play_res, audio_res) = join!(video, audio);
                log_task_result("video_supervisor", play_res);
                log_task_result("audio_play", audio_res);
                log::info!("[player] pipeline gen {} tasks joined", gen);

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
            log::info!("[player] play() finished — no pending seek, pipeline released");
        });
        Ok(play)
    }

    pub fn seek(&self, target: Duration) {
        *self.rebuild_reason.lock().unwrap() = BufferingReason::Seek;
        self.seek_internal(target)
    }

    /// `seek()` without claiming the rebuild is a seek — for callers that
    /// already named a more specific cause (a track switch is a seek only
    /// mechanically; the consumer must not be told the user scrubbed).
    fn seek_internal(&self, target: Duration) {
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
    ///
    /// To wait until the pipeline is actually gone, `await` the `JoinHandle`
    /// that `play()` returned: clearing `seek_target` here makes the
    /// self-restarting play loop EXIT (instead of rebuilding) once its tasks
    /// observe the stop, so that handle completes. The bridge must `stop()` +
    /// await that handle on teardown rather than dropping/aborting it —
    /// dropping a tokio `JoinHandle` detaches the task, it does not cancel it,
    /// which is why playback (and audio) survived a host teardown.
    pub async fn stop(&self) {
        // Cancel any pending seek/track-switch FIRST: a `Some` seek_target at the
        // play loop's tail makes it rebuild a fresh pipeline instead of ending.
        // Clearing it (with stop_flag set) makes the loop break and play() return.
        *self.seek_target.write().await = None;
        self.stop_flag.store(true, Ordering::Relaxed);
        self.stop.notify_waiters();
        // Stop audible output immediately — with passthrough engaged this pauses
        // the bitstream AudioTrack (not just cpal).
        self.audio_renderer.set_paused(true);
        // Drain any samples still queued for cpal. Mirrors what seek() and
        // play()-startup do for the same "switching pipelines" reason.
        self.audio_renderer.flush();
        // Release the bitstream passthrough AudioTrack so it stops owning the
        // HDMI output (its feed task also exits on stop_flag and drops its ref;
        // detaching here makes the renderer stop reading a dying sink as the
        // clock). set_passthrough(None) un-pauses cpal, so re-park it after.
        self.audio_renderer.set_passthrough(None);
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

mod audio_watchdog;
mod clock;
mod decode;
mod pipeline;
mod supervisor;
pub(crate) use audio_watchdog::audio_output_watchdog;
pub(crate) use clock::{clock_monotonic_ns, MediaClock};
use decode::*;
use pipeline::*;
use supervisor::{video_supervisor, VideoDecoderFactory};
mod net_io;
use net_io::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SencEntry;
    use crate::tracks::segment::Segment;
    use crate::parsers::sidecar::{self, SubtitleFormat};

    const SRT: &str = "1
00:00:01,000 --> 00:00:04,000
První řádek

2
00:00:05,000 --> 00:00:07,000
Druhý
";

    fn parsed(src: &str) -> sidecar::ParsedSidecar {
        sidecar::parse(src.as_bytes(), SubtitleFormat::Auto, None, 0).unwrap()
    }

    #[test]
    fn external_track_carries_its_cues_and_needs_no_network() {
        let (rep, adaptation) = external_track(
            EXTERNAL_TEXT_ID_BASE,
            parsed(SRT),
            &ExternalSubtitleOptions {
                language: Some("cs".to_string()),
                ..Default::default()
            },
        );

        // The whole point: selecting this must not hit the network, so
        // every URL-ish field has to be empty and the cues present.
        assert!(rep.is_external());
        assert!(rep.single_file_url.is_none());
        assert!(rep.segment_init.is_none());
        assert!(rep.segments.is_empty());
        assert_eq!(rep.external_cues.as_ref().unwrap().len(), 2);

        // ...and it has to survive the gate text_play applies to
        // manifest tracks, or it would be skipped as undecodable.
        assert!(rep.is_webvtt());

        assert_eq!(adaptation.language(), Some("cs"));
        assert_eq!(adaptation.role(), Some("subtitle"));
        assert!(!adaptation.is_forced());
        assert_eq!(adaptation.representations.len(), 1);
    }

    #[test]
    fn external_track_label_reports_cue_count_not_bitrate() {
        let (rep, _) = external_track(1, parsed(SRT), &ExternalSubtitleOptions::default());
        // bandwidth is 0 for a local file; quoting "0 kbps" would be noise.
        assert_eq!(rep.label(), "WebVTT · 2 cues");
    }

    #[test]
    fn external_track_can_be_marked_forced() {
        let (_, adaptation) = external_track(
            1,
            parsed(SRT),
            &ExternalSubtitleOptions {
                forced: true,
                ..Default::default()
            },
        );
        assert!(adaptation.is_forced());
    }

    #[test]
    fn subrip_and_webvtt_get_distinct_mime_types() {
        let (srt, _) = external_track(1, parsed(SRT), &ExternalSubtitleOptions::default());
        assert_eq!(srt.mime_type, "application/x-subrip");

        let vtt_src = "WEBVTT

00:00:01.000 --> 00:00:02.000
hi
";
        let (vtt, _) = external_track(2, parsed(vtt_src), &ExternalSubtitleOptions::default());
        assert_eq!(vtt.mime_type, "text/vtt");
        // Both normalise to wvtt: downstream only ever sees parsed cues.
        assert_eq!(srt.codecs, "wvtt");
        assert_eq!(vtt.codecs, "wvtt");
    }


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


    // ---- buffer-ahead gauge across an ABR swap ------------------------------

    /// A prefetch with nothing behind it but the fields the gauge handover
    /// touches; the download half is a closed channel and a finished task.
    fn swap_prefetch(dl_pts_ms: i64) -> VideoPrefetch {
        let (_tx, rx) = mpsc::channel::<DataSegment>(1);
        VideoPrefetch {
            width: 1920,
            height: 1080,
            init_data: Vec::new(),
            hvcc_nalus: Vec::new(),
            decoder_config_record: Vec::new(),
            color: Default::default(),
            dovi_profile: None,
            track_crypto: None,
            download_rx: rx,
            download_handle: crate::rt::spawn(async { Ok(()) }),
            primed: Arc::new(Notify::new()),
            first_prepared: None,
            dl_pts_ms: Arc::new(std::sync::atomic::AtomicI64::new(dl_pts_ms)),
            // A swap prefetch starts out NOT publishing the shared gauge.
            track_dl: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn the_buffer_gauge_follows_the_rung_that_will_actually_play() {
        let stats = Arc::new(StatsState::default());
        // OLD had fetched 40 s ahead in a representation about to be dropped.
        stats.last_decoded_pts_ms.store(40_000, Ordering::Relaxed);

        let pf = swap_prefetch(12_000);
        assert!(
            !pf.track_dl.load(Ordering::Relaxed),
            "a swap prefetch must not publish the gauge while OLD is still playing"
        );

        pf.take_over_buffer_gauge(&stats);

        assert_eq!(
            stats.last_decoded_pts_ms.load(Ordering::Relaxed),
            12_000,
            "the gauge must drop to what the NEW rung has, not keep OLD's reach"
        );
        assert!(
            pf.track_dl.load(Ordering::Relaxed),
            "NEW must keep the gauge moving from here - leaving it false is what              froze buffered_ahead_secs after the first ABR switch and, through              MIN_UPSWITCH_BUFFER_MS, stuck quality down"
        );
    }
    // ---- parallel CENC decrypt ----------------------------------------------
    //
    // The parallel path hands each worker a disjoint `&mut [u8]` carved out of
    // the segment with split_at_mut, so a mistake in the arithmetic would not
    // be a compile error - it would be silently mis-decrypted video. These
    // tests pin it to the serial result, byte for byte.

    fn crypto_fixture() -> (TrackCrypto, [u8; 16]) {
        let kid = [0x11u8; 16];
        let key = [0x42u8; 16];
        let mut keys = std::collections::HashMap::new();
        keys.insert(kid, key);
        let tc = TrackCrypto {
            decryptor: Arc::new(crate::crypto::ClearKeyDecryptor::new(keys)),
            kid,
            iv_size: 16,
        };
        (tc, kid)
    }

    /// Samples of deliberately uneven size, laid out back to back after a
    /// header gap, the way a real fragment carries them.
    fn sample_layout(sizes: &[usize], header: usize) -> (Vec<(usize, usize)>, Vec<SencEntry>) {
        let mut ranges = Vec::new();
        let mut entries = Vec::new();
        let mut at = header;
        for (i, &sz) in sizes.iter().enumerate() {
            ranges.push((at, sz));
            let mut iv = [0u8; 16];
            iv[15] = (i as u8) + 1; // never all-zero: that means "clear"
            entries.push(SencEntry {
                iv,
                subsamples: Vec::new(),
            });
            at += sz;
        }
        (ranges, entries)
    }

    fn serial_reference(
        buf: &mut [u8],
        tc: &TrackCrypto,
        ranges: &[(usize, usize)],
        entries: &[SencEntry],
    ) {
        for ((offset, size), entry) in ranges.iter().zip(entries.iter()) {
            decrypt_one_sample(tc, entry, &mut buf[*offset..offset + size]).unwrap();
        }
    }

    #[test]
    fn parallel_decrypt_matches_the_serial_result() {
        let (tc, _kid) = crypto_fixture();
        // Uneven sizes, like an IDR followed by B-frames.
        let sizes = [900_000usize, 40_000, 30_000, 700_000, 25_000, 500_000, 15_000];
        let header = 2_048;
        let (ranges, entries) = sample_layout(&sizes, header);
        let total = header + sizes.iter().sum::<usize>() + 64; // trailing slack

        let mut original = vec![0u8; total];
        for (i, b) in original.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }

        let mut expected = original.clone();
        serial_reference(&mut expected, &tc, &ranges, &entries);

        for workers in [2usize, 3, 4] {
            let mut got = original.clone();
            decrypt_samples_parallel(&mut got, &tc, &ranges, &entries, workers)
                .unwrap_or_else(|e| panic!("{workers} workers refused the split: {e}"));
            assert_eq!(got, expected, "parallel result differs with {workers} workers");
        }
    }

    #[test]
    fn parallel_decrypt_leaves_bytes_outside_samples_untouched() {
        let (tc, _kid) = crypto_fixture();
        let sizes = [600_000usize, 600_000, 600_000, 600_000];
        let header = 4_096;
        let (ranges, entries) = sample_layout(&sizes, header);
        let total = header + sizes.iter().sum::<usize>() + 1_024;

        let mut buf = vec![0xABu8; total];
        decrypt_samples_parallel(&mut buf, &tc, &ranges, &entries, 4).unwrap();

        assert!(
            buf[..header].iter().all(|&b| b == 0xAB),
            "the header was decrypted over"
        );
        let tail = header + sizes.iter().sum::<usize>();
        assert!(
            buf[tail..].iter().all(|&b| b == 0xAB),
            "the trailing bytes were decrypted over"
        );
    }

    #[test]
    fn parallel_decrypt_refuses_a_layout_it_cannot_split() {
        let (tc, _kid) = crypto_fixture();
        // Overlapping samples: the split would hand two workers the same bytes.
        let ranges = vec![(0usize, 1_000usize), (500, 1_000), (2_000, 1_000)];
        let entries: Vec<SencEntry> = (0..3)
            .map(|i| {
                let mut iv = [0u8; 16];
                iv[15] = i + 1;
                SencEntry {
                    iv,
                    subsamples: Vec::new(),
                }
            })
            .collect();
        let mut buf = vec![0u8; 4_000];
        assert!(
            decrypt_samples_parallel(&mut buf, &tc, &ranges, &entries, 2).is_err(),
            "an overlapping layout must be refused, not silently mis-decrypted"
        );
    }

    #[test]
    fn a_clear_sample_inside_an_encrypted_segment_is_left_alone() {
        let (tc, _kid) = crypto_fixture();
        let mut entry = SencEntry {
            iv: [0u8; 16], // all-zero IV = the sample is in the clear
            subsamples: Vec::new(),
        };
        let mut buf = vec![7u8; 1_024];
        decrypt_one_sample(&tc, &entry, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 7), "a clear sample was decrypted");

        // Subsamples that encrypt nothing mean the same thing.
        entry.iv = [9u8; 16];
        entry.subsamples = vec![(1_024, 0)];
        let mut buf2 = vec![7u8; 1_024];
        decrypt_one_sample(&tc, &entry, &mut buf2).unwrap();
        assert!(buf2.iter().all(|&b| b == 7), "a zero-encrypted sample was decrypted");
    }
}
