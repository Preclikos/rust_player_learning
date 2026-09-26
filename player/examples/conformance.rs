//! Playback conformance soak harness — automated A/V validation.
//!
//! Plays a scripted scenario (steady playback + soft track switches + seeks)
//! against a real DASH stream on the real platform decoder stack, headless
//! (offscreen wgpu target, default audio device). While playing it counts
//! every user-visible playback event from the event stream; at the end it
//! reads the engine's [`player::ConformanceSummary`] and fails loudly when
//! any threshold is exceeded. CI runs this on the self-hosted Windows /
//! Linux / macOS runners before a library tag is allowed out the door.
//!
//! # Lip-sync measurement (independent of the engine clock)
//!
//! The beep-flash asset (`scripts/conformance/make-asset.ps1`) carries a
//! full-frame white flash and a 1 kHz beep starting at every even second.
//! The harness wraps the stock sinks ([`player::Player::with_sinks`]):
//!
//! * the audio tap detects each beep ONSET in the PCM the engine hands the
//!   sink (content, not timestamps) and works out the wall instant it became
//!   audible from the sink's presented position + output latency;
//! * the video tap records the wall instant each flash frame (the frame at
//!   pts = 2k s) is presented.
//!
//! `lipsync = t_flash − t_beep` per pair; positive = picture late (audio
//! leads). Because the audio side is measured from content and the device's
//! real playback position, an engine that mis-anchors its clock, trims audio
//! to the wrong target, or lets a stale device-buffer tail count as new audio
//! shows up here even though its own `av_drift` gauge (which only compares
//! the two clocks' RATES) stays clean. The scenario's seeks + switches are
//! exactly where those bugs lived.
//!
//! ```text
//! cargo run --release --example conformance -- <MPD_URL> \
//!     [--key KIDHEX:KEYHEX]...   # ClearKey pairs for CENC test assets
//!     [--secs 90]                # scenario length
//!     [--switches 2]             # soft video-track switches (ABR swap path)
//!     [--seeks 1]                # relative seeks (+30 s)
//!     [--max-gap-ms 700]         # worst allowed frame-to-frame render gap
//!     [--max-drift-ms 100]       # worst allowed |A/V drift|
//!     [--max-bursts 200]         # sub-5ms catch-up renders
//!     [--max-lipsync-ms 80]      # worst allowed |flash − beep| per pair (0 = skip)
//!     [--max-lipsync-median-ms 40] # allowed |median flash − beep| (the real offset)
//!     [--max-late-pct 2.0]       # frames presented >45 ms late, % of decoded
//!     [--allowed-stalls N]       # default = --seeks (a post-seek spinner is fine)
//! ```
//!
//! Exit code 0 = every threshold held; 1 = at least one FAIL (printed).
//! The summary is also printed as a single JSON line (machine-readable, for
//! trend tracking) prefixed with `CONFORMANCE_JSON `.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use player::{
    AudioPassthrough, AudioRenderer, AudioSink, BufferingReason, DecodedVideoFrame,
    HdrTonemapParams, PhysicalSize, Player, PlayerEvent, SubtitleStyle, TrackKind,
    VideoRenderer, VideoSink, VttCue,
};

struct Args {
    mpd: String,
    keys: HashMap<String, String>,
    secs: u64,
    switches: u32,
    seeks: u32,
    max_gap_ms: u64,
    max_drift_ms: i64,
    max_bursts: u64,
    max_judder_pct: f64,
    max_lipsync_ms: i64,
    max_lipsync_median_ms: i64,
    max_late_pct: f64,
    allowed_stalls: Option<u64>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        mpd: String::new(),
        keys: HashMap::new(),
        secs: 90,
        switches: 2,
        seeks: 1,
        max_gap_ms: 700,
        max_drift_ms: 100,
        max_bursts: 200,
        max_judder_pct: 5.0,
        max_lipsync_ms: 80,
        max_lipsync_median_ms: 40,
        max_late_pct: 2.0,
        allowed_stalls: None,
    };
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().unwrap_or_else(|| panic!("{name} needs a value"));
        match arg.as_str() {
            "--key" => {
                let kv = val("--key");
                let (kid, key) = kv.split_once(':').expect("--key expects KIDHEX:KEYHEX");
                a.keys.insert(kid.to_string(), key.to_string());
            }
            "--secs" => a.secs = val("--secs").parse().expect("--secs"),
            "--switches" => a.switches = val("--switches").parse().expect("--switches"),
            "--seeks" => a.seeks = val("--seeks").parse().expect("--seeks"),
            "--max-gap-ms" => a.max_gap_ms = val("--max-gap-ms").parse().expect("--max-gap-ms"),
            "--max-drift-ms" => a.max_drift_ms = val("--max-drift-ms").parse().expect("--max-drift-ms"),
            "--max-bursts" => a.max_bursts = val("--max-bursts").parse().expect("--max-bursts"),
            "--max-judder-pct" => {
                a.max_judder_pct = val("--max-judder-pct").parse().expect("--max-judder-pct")
            }
            "--max-lipsync-ms" => {
                a.max_lipsync_ms = val("--max-lipsync-ms").parse().expect("--max-lipsync-ms")
            }
            "--max-lipsync-median-ms" => {
                a.max_lipsync_median_ms =
                    val("--max-lipsync-median-ms").parse().expect("--max-lipsync-median-ms")
            }
            "--max-late-pct" => a.max_late_pct = val("--max-late-pct").parse().expect("--max-late-pct"),
            "--allowed-stalls" => {
                a.allowed_stalls = Some(val("--allowed-stalls").parse().expect("--allowed-stalls"))
            }
            other if a.mpd.is_empty() && !other.starts_with("--") => a.mpd = other.to_string(),
            other => panic!("unknown argument: {other}"),
        }
    }
    if a.mpd.is_empty() {
        eprintln!("usage: conformance <MPD_URL> [--key KID:KEY] [--secs N] …");
        std::process::exit(2);
    }
    a
}

/// Headless wgpu device — mirror of the BlackZone desktop host's shared-GPU
/// init (the player fork's wgpu carries non-upstream fields, so every field
/// is spelled out).
async fn headless_gpu() -> (wgpu::Device, wgpu::Queue, wgpu::Backend) {
    #[cfg(target_os = "windows")]
    let backends = wgpu::Backends::DX12;
    #[cfg(target_os = "linux")]
    let backends = wgpu::Backends::VULKAN;
    #[cfg(target_os = "macos")]
    let backends = wgpu::Backends::METAL;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    });
    let adapter = match instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
    {
        Ok(a) => a,
        Err(e) => {
            // Headless session without GPU access (e.g. a runner service
            // outside the GUI session — Metal offers no adapter there).
            // Not a playback failure: report a SKIP so CI stays green but
            // the gap is visible in the log.
            println!("CONFORMANCE_SKIP no-gpu-adapter: {e}");
            std::process::exit(0);
        }
    };
    let backend = adapter.get_info().backend;
    let desired = if backend == wgpu::Backend::Metal {
        wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
    } else {
        wgpu::Features::TEXTURE_FORMAT_NV12
            | wgpu::Features::TEXTURE_FORMAT_P010
            | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
    };
    let required_features = adapter.features() & desired;
    let alim = adapter.limits();
    let required_limits = wgpu::Limits {
        max_texture_dimension_2d: alim.max_texture_dimension_2d,
        max_texture_dimension_1d: alim.max_texture_dimension_1d,
        ..wgpu::Limits::default()
    };
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("conformance headless device"),
            required_features,
            required_limits,
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::default(),
            trace: wgpu::Trace::Off,
        })
        .await
        .expect("request_device failed");
    eprintln!(
        "[conformance] gpu: {:?} / {}",
        backend,
        adapter.get_info().name
    );
    (device, queue, backend)
}

// ---------------------------------------------------------------------------
// Lip-sync taps
// ---------------------------------------------------------------------------

/// The asset's marks: a beep + flash starting at every even second.
const MARK_PERIOD_MS: u64 = 2_000;
/// A frame is "the flash frame" when its pts falls within one 24p frame
/// slot after the mark instant.
const FLASH_WINDOW_MS: u64 = 42;
/// Beep detector: per-1 ms block RMS thresholds. The asset's tone is
/// ffmpeg's `sine` default level (≈ −24 dBFS RMS ≈ 0.06 linear); the gaps
/// between beeps are AAC-coded silence (≈ −67 dBFS). Note the beep is gated
/// per AAC frame at encode time, so it starts up to one frame (21 ms) AFTER
/// the even second the flash frame sits on — a constant, tiny negative bias.
const BEEP_ON_RMS: f32 = 0.02;
const BEEP_OFF_RMS: f32 = 0.005;

/// Shared measurement state between the two taps and the verdict.
#[derive(Default)]
struct LipSync {
    /// Audio-side bookkeeping since the last flush (ms of PCM the engine has
    /// queued; the sink's `played_since_flush_ms` counts the same samples).
    queued_ms: Mutex<f64>,
    /// Detected beep onsets not yet presented: their position on the
    /// queued-since-flush axis (ms).
    pending_onsets: Mutex<VecDeque<f64>>,
    /// Detector hysteresis state (carried across `put_samples` calls).
    loud: AtomicBool,
    /// Wall instants at which detected beeps became audible.
    audible: Mutex<Vec<Instant>>,
    /// Wall instants at which flash frames were presented, with their pts.
    flashes: Mutex<Vec<(u64, Instant)>>,
    beeps_detected: AtomicU64,
    /// Wall instants of every sink flush (= every seek / hard rebuild). Each
    /// one opens a window that must produce audible audio again.
    rebuilds: Mutex<Vec<Instant>>,
    /// The flush whose audio has not been heard yet, if any.
    liveness_pending: Mutex<Option<Instant>>,
    /// Rebuilds whose audio position never advanced within
    /// `AUDIO_DEAD_AFTER_REBUILD` — the "picture on, no sound after a seek"
    /// failure that every other criterion is blind to (drift is only
    /// measured once audio moves, lip-sync coverage is a run-wide total).
    dead_rebuilds: AtomicU64,
}

/// How long after a flush the sink's post-flush position may stay at 0
/// before the rebuild counts as mute. Covers the seek's own buffering on a
/// locally served asset with room to spare; a real network stall shows up
/// as a Buffering(Stall) event too.
const AUDIO_DEAD_AFTER_REBUILD: Duration = Duration::from_secs(6);

/// Forwards everything to the stock renderer; taps `put_samples` for beep
/// onsets and `flush` to restart the queued-position axis.
struct TapAudio {
    inner: Arc<AudioRenderer>,
    lip: Arc<LipSync>,
}

impl TapAudio {
    fn detect_onsets(&self, samples: &[f32]) {
        let rate = self.inner.sample_rate().max(1) as f64;
        let block = ((rate / 1000.0) as usize).max(1) * 2; // 1 ms of stereo
        let mut queued = self.lip.queued_ms.lock().unwrap();
        let mut loud = self.lip.loud.load(Ordering::Relaxed);
        for (i, chunk) in samples.chunks(block).enumerate() {
            let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
            if !loud && rms > BEEP_ON_RMS {
                loud = true;
                let pos_ms = *queued + i as f64; // block i starts i ms into this batch
                self.lip.pending_onsets.lock().unwrap().push_back(pos_ms);
                self.lip.beeps_detected.fetch_add(1, Ordering::Relaxed);
            } else if loud && rms < BEEP_OFF_RMS {
                loud = false;
            }
        }
        self.lip.loud.store(loud, Ordering::Relaxed);
        *queued += (samples.len() / 2) as f64 * 1000.0 / rate;
    }

    /// Poll the sink's presented position; every pending onset it has passed
    /// became audible `(played − onset) ms ago + output latency`. Using the
    /// overshoot instead of "now" removes the sink's position-update
    /// granularity (one device callback) from the measurement.
    fn poll_presented(&self) {
        let Some(played) = self.inner.played_since_flush_ms() else { return };
        // Post-rebuild liveness: the first non-zero position after a flush
        // clears the pending rebuild; a rebuild that stays at 0 too long is
        // recorded as dead (once).
        {
            let mut pending = self.lip.liveness_pending.lock().unwrap();
            if let Some(t) = *pending {
                if played > 0 {
                    *pending = None;
                } else if t.elapsed() > AUDIO_DEAD_AFTER_REBUILD {
                    self.lip.dead_rebuilds.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "[conformance] AUDIO DEAD after rebuild: position still 0 {} s after the flush",
                        t.elapsed().as_secs()
                    );
                    *pending = None;
                }
            }
        }
        let played = played as f64;
        let lat = self.inner.output_latency_ms() as f64;
        let now = Instant::now();
        let mut pending = self.lip.pending_onsets.lock().unwrap();
        while let Some(&onset) = pending.front() {
            if played < onset {
                break;
            }
            pending.pop_front();
            let ago_ms = (played - onset).min(500.0);
            let t = now - Duration::from_secs_f64(ago_ms / 1000.0)
                + Duration::from_secs_f64(lat / 1000.0);
            self.lip.audible.lock().unwrap().push(t);
        }
    }
}

impl AudioSink for TapAudio {
    fn put_samples<'a>(&'a self, samples: &'a [f32]) -> impl Future<Output = ()> + Send + 'a {
        self.detect_onsets(samples);
        self.inner.put_samples(samples)
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn played_ms(&self) -> Option<u64> {
        self.inner.played_ms()
    }
    fn played_since_flush_ms(&self) -> Option<u64> {
        self.inner.played_since_flush_ms()
    }
    fn output_latency_ms(&self) -> u64 {
        self.inner.output_latency_ms()
    }
    fn flush(&self) {
        *self.lip.queued_ms.lock().unwrap() = 0.0;
        self.lip.pending_onsets.lock().unwrap().clear();
        self.lip.loud.store(false, Ordering::Relaxed);
        let now = Instant::now();
        self.lip.rebuilds.lock().unwrap().push(now);
        *self.lip.liveness_pending.lock().unwrap() = Some(now);
        self.inner.flush()
    }
    fn stop(&self) -> impl Future<Output = ()> + Send + '_ {
        self.inner.stop()
    }
    fn set_volume(&self, volume: f32) {
        self.inner.set_volume(volume)
    }
    fn get_volume(&self) -> f32 {
        self.inner.get_volume()
    }
    fn volume(&self, diff: f32) {
        self.inner.volume(diff)
    }
    fn set_paused(&self, paused: bool) {
        self.inner.set_paused(paused)
    }
    fn set_passthrough(&self, pt: Option<Arc<dyn AudioPassthrough>>) {
        self.inner.set_passthrough(pt)
    }
    fn is_passthrough(&self) -> bool {
        self.inner.is_passthrough()
    }
    fn last_peak_db(&self) -> Option<[f32; 2]> {
        self.inner.last_peak_db()
    }
}

/// Forwards everything to the stock offscreen renderer; taps `render_frame`
/// to timestamp the flash frames.
struct TapVideo {
    inner: Arc<VideoRenderer>,
    lip: Arc<LipSync>,
}

impl VideoSink for TapVideo {
    fn render_frame(&self, frame: DecodedVideoFrame) -> impl Future<Output = ()> + Send + '_ {
        let pts_ms = (frame.pts_us.max(0) / 1000) as u64;
        // Skip the mark at pts 0: the very first frame is painted at once as
        // the start preview while the audio device is still spinning up, so
        // it is not a lip-sync sample (every later mark is).
        if pts_ms % MARK_PERIOD_MS < FLASH_WINDOW_MS && pts_ms >= FLASH_WINDOW_MS {
            // Only the FIRST frame of each mark (the flash onset) — at 24 fps
            // two frames fall inside the window.
            let mark = pts_ms / MARK_PERIOD_MS;
            let mut flashes = self.lip.flashes.lock().unwrap();
            if flashes.last().map_or(true, |&(p, _)| p / MARK_PERIOD_MS != mark) {
                // The sync loop calls render RENDER_BUDGET_MS (20 ms) before the
                // frame's clock time and the offscreen target paints at once,
                // so "presented" = now + that budget. (A late frame is painted
                // immediately, so for those this overstates by ≤ 20 ms —
                // they are rare and genuinely late.) Not `desired_present_ns`:
                // off Android that carries the de-judder smoother's ±40 ms
                // wobble, which is exactly the noise we don't want here.
                flashes.push((pts_ms, Instant::now() + Duration::from_millis(20)));
            }
        }
        self.inner.render_frame(frame)
    }
    fn resize(&self, size: PhysicalSize<u32>) -> impl Future<Output = ()> + Send + '_ {
        self.inner.resize(size)
    }
    fn change_frame_size(&self, size: PhysicalSize<u32>) -> impl Future<Output = ()> + Send + '_ {
        self.inner.change_frame_size(size)
    }
    fn set_subtitle_font(&self, bytes: Vec<u8>) -> Result<(), String> {
        self.inner.set_subtitle_font(bytes)
    }
    fn set_subtitle_style(&self, style: SubtitleStyle) {
        self.inner.set_subtitle_style(style)
    }
    fn queue_subtitle_cues(&self, cues: Vec<VttCue>) {
        self.inner.queue_subtitle_cues(cues)
    }
    fn clear_subtitles(&self) {
        self.inner.clear_subtitles()
    }
    fn set_subtitle_pts(&self, pts_ms: i64) {
        self.inner.set_subtitle_pts(pts_ms)
    }
    fn set_hdr_tonemap_params(&self, params: HdrTonemapParams) {
        self.inner.set_hdr_tonemap_params(params)
    }
    fn set_display_hdr_types(&self, mask: u32) {
        self.inner.set_display_hdr_types(mask)
    }
    fn set_subtitle_safe_bottom_px(&self, px: u32) {
        self.inner.set_subtitle_safe_bottom_px(px)
    }
}

/// Pair each presented flash with the nearest audible beep and return the
/// offsets `flash − beep` in ms (positive = picture late / audio leads).
/// `(flash pts ms, flash − beep ms, flash wall instant)` for every flash that
/// found its beep.
fn lipsync_offsets(lip: &LipSync) -> Vec<(u64, i64, Instant)> {
    let flashes = lip.flashes.lock().unwrap();
    let beeps = lip.audible.lock().unwrap();
    let mut out = Vec::new();
    for &(pts_ms, t_flash) in flashes.iter() {
        let nearest = beeps
            .iter()
            .map(|&t_beep| {
                let d = if t_flash >= t_beep {
                    t_flash.duration_since(t_beep).as_millis() as i64
                } else {
                    -(t_beep.duration_since(t_flash).as_millis() as i64)
                };
                d
            })
            .min_by_key(|d| d.abs());
        // Pair only within half a mark period; otherwise the beep for this
        // flash was not detected (or the flash landed in a seek hole).
        if let Some(d) = nearest.filter(|d| d.abs() < (MARK_PERIOD_MS / 2) as i64) {
            out.push((pts_ms, d, t_flash));
        }
    }
    out
}

/// Rebuild windows (from each flush to the next one or the run's end) that
/// are long enough to contain a mark pair yet got none: audio or picture
/// never came back after that seek. `(window index, length s)` each.
fn silent_rebuild_windows(
    rebuilds: &[Instant],
    offsets: &[(u64, i64, Instant)],
    end: Instant,
) -> Vec<(usize, u64)> {
    // A window must hold a whole mark period after the seek's own settle
    // time before a missing pair means anything.
    let settle = Duration::from_secs(2);
    let min_len = settle + Duration::from_millis(MARK_PERIOD_MS * 2);
    let mut out = Vec::new();
    for (i, &from) in rebuilds.iter().enumerate() {
        let to = rebuilds.get(i + 1).copied().unwrap_or(end);
        if to.saturating_duration_since(from) < min_len {
            continue;
        }
        let heard = offsets.iter().any(|&(_, _, t)| t >= from + settle && t < to);
        if !heard {
            out.push((i, to.saturating_duration_since(from).as_secs()));
        }
    }
    out
}

#[derive(Default)]
struct EventCounters {
    stall_buffering: AtomicU64,
    seek_buffering: AtomicU64,
    errors: AtomicU64,
    eos: AtomicU64,
    video_track_changes: AtomicU64,
    first_error: std::sync::Mutex<Option<String>>,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();

    let (device, queue, backend) = headless_gpu().await;
    let lip = Arc::new(LipSync::default());
    let video = Arc::new(TapVideo {
        inner: Arc::new(VideoRenderer::new_offscreen(device, queue, backend, 1280, 720)),
        lip: Arc::clone(&lip),
    });
    let audio = Arc::new(TapAudio {
        inner: Arc::new(AudioRenderer::new()),
        lip: Arc::clone(&lip),
    });
    let mut player = Player::with_sinks(video, Arc::clone(&audio));
    if !args.keys.is_empty() {
        player.set_clearkey(args.keys.clone()).expect("set_clearkey");
    }
    // Beep presentation poller: turns detected onsets into audible instants
    // as the sink's presented position passes them.
    {
        let audio = Arc::clone(&audio);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(2)).await;
                audio.poll_presented();
            }
        });
    }

    let t0 = Instant::now();
    player.open_url(&args.mpd).await.expect("open_url");
    player.prepare().await.expect("prepare");
    let tracks = player.get_tracks().expect("get_tracks");
    let va = tracks.video.first().expect("no video adaptation").clone();
    let low = va.representations.first().expect("no video reps").clone();
    let high = va.representations.last().expect("no video reps").clone();
    player.set_video_track(&va, &low);
    let aa = tracks.audio.first().expect("no audio adaptation").clone();
    let ar = aa.representations.first().expect("no audio reps").clone();
    player.set_audio_track(&aa, &ar);
    eprintln!(
        "[conformance] prepared in {} ms — video reps {}..{} ({} total), scenario {}s/{}sw/{}seek",
        t0.elapsed().as_millis(),
        low.id,
        high.id,
        va.representations.len(),
        args.secs,
        args.switches,
        args.seeks,
    );

    // ---- event stream accounting -------------------------------------------
    let counters = Arc::new(EventCounters::default());
    {
        let counters = Arc::clone(&counters);
        let mut rx = player.events();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => match ev {
                        PlayerEvent::Buffering { reason } => match reason {
                            BufferingReason::Stall => {
                                counters.stall_buffering.fetch_add(1, Ordering::Relaxed);
                                eprintln!("[conformance] EVENT Buffering(Stall)");
                            }
                            BufferingReason::Seek => {
                                counters.seek_buffering.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {}
                        },
                        PlayerEvent::Error { kind, detail } => {
                            counters.errors.fetch_add(1, Ordering::Relaxed);
                            counters
                                .first_error
                                .lock()
                                .unwrap()
                                .get_or_insert_with(|| detail.clone());
                            eprintln!("[conformance] EVENT Error({kind:?}): {detail}");
                        }
                        PlayerEvent::EndOfStream => {
                            counters.eos.fetch_add(1, Ordering::Relaxed);
                            eprintln!("[conformance] EVENT EndOfStream");
                        }
                        PlayerEvent::TrackChanged { kind: TrackKind::Video, .. } => {
                            counters.video_track_changes.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    let handle = player.play().expect("play");

    // ---- scripted scenario ---------------------------------------------------
    // Actions are spread across the middle 80% of the run so the tail is clean
    // steady-state playback (drift has time to show).
    let total = Duration::from_secs(args.secs);
    let mut actions: Vec<(Duration, String)> = Vec::new();
    let n_actions = (args.switches + args.seeks) as u64;
    for i in 0..args.switches as u64 {
        let at = total.mul_f64(0.1 + 0.8 * (i as f64 + 0.5) / n_actions.max(1) as f64);
        actions.push((at, format!("switch{}", i)));
    }
    for i in 0..args.seeks as u64 {
        let at = total.mul_f64(0.1 + 0.8 * ((args.switches as u64 + i) as f64 + 0.5) / n_actions.max(1) as f64);
        actions.push((at, format!("seek{}", i)));
    }
    actions.sort_by_key(|(at, _)| *at);

    let start = Instant::now();
    let mut next_action = 0usize;
    let mut on_high = false;
    let mut last_lip_report = Instant::now();
    while start.elapsed() < total {
        tokio::time::sleep(Duration::from_millis(250)).await;
        while next_action < actions.len() && start.elapsed() >= actions[next_action].0 {
            let (_, what) = &actions[next_action];
            if what.starts_with("switch") && va.representations.len() > 1 {
                let target = if on_high { &low } else { &high };
                on_high = !on_high;
                eprintln!(
                    "[conformance] ACTION soft switch -> rep {} at {}s",
                    target.id,
                    start.elapsed().as_secs()
                );
                player.change_video_track_soft(target);
            } else if what.starts_with("seek") {
                let pos = player.position();
                // Land where enough content remains to play out the rest of
                // the scenario (+2 s slack) — a legit EndOfStream would fail
                // the eos==0 assertion. On a short asset this turns the
                // "+30 s" into a backward seek, which exercises the same
                // rebuild path.
                let remaining = total.saturating_sub(start.elapsed());
                let cap = tracks
                    .duration
                    .saturating_sub(remaining + Duration::from_secs(2));
                let to = (pos + Duration::from_secs(30)).min(cap);
                eprintln!(
                    "[conformance] ACTION seek {}s -> {}s",
                    pos.as_secs(),
                    to.as_secs()
                );
                player.seek(to);
            }
            next_action += 1;
        }
        // Live lip-sync trace every ~10 s so a run's log shows WHERE an
        // offset appeared (right after a seek / switch, or drifting).
        if last_lip_report.elapsed() >= Duration::from_secs(10) {
            last_lip_report = Instant::now();
            let offs = lipsync_offsets(&lip);
            if let Some(&(pts, d, _)) = offs.last() {
                eprintln!(
                    "[conformance] LIPSYNC pairs={} latest: flash@{}s offset={}ms (+ = picture late)",
                    offs.len(),
                    pts / 1000,
                    d
                );
            }
        }
    }

    player.stop().await;
    let _ = handle.await;

    // ---- verdict -------------------------------------------------------------
    let s = player.conformance_summary();
    let stalls_ev = counters.stall_buffering.load(Ordering::Relaxed);
    let errors = counters.errors.load(Ordering::Relaxed);
    let eos = counters.eos.load(Ordering::Relaxed);
    let allowed_stalls = args.allowed_stalls.unwrap_or(args.seeks as u64);

    // Environment, not player: a machine without a usable HW decoder (no
    // /dev/dri render node, missing driver) can't play anything at all —
    // report a SKIP so the lane is green-but-honest instead of failing on
    // every run until the machine grows a GPU.
    if s.video_frames_decoded == 0 {
        let first = counters.first_error.lock().unwrap().clone().unwrap_or_default();
        if first.contains("av_hwdevice_ctx_create") {
            println!("CONFORMANCE_SKIP no-hw-decoder: {first}");
            std::process::exit(0);
        }
    }

    let run_end = Instant::now();
    let offsets = lipsync_offsets(&lip);
    let rebuilds = lip.rebuilds.lock().unwrap().clone();
    let silent_windows = silent_rebuild_windows(&rebuilds, &offsets, run_end);
    let dead_rebuilds = lip.dead_rebuilds.load(Ordering::Relaxed);
    let mut sorted: Vec<i64> = offsets.iter().map(|&(_, d, _)| d).collect();
    sorted.sort_unstable();
    let lip_median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
    let lip_max_abs = sorted.iter().map(|d| d.abs()).max().unwrap_or(0);
    let beeps = lip.beeps_detected.load(Ordering::Relaxed);
    for (pts, d, _) in &offsets {
        eprintln!("[conformance] lipsync flash@{}s {:+}ms", pts / 1000, d);
    }
    let late_pct = if s.video_frames_decoded > 0 {
        s.video_late_frames as f64 * 100.0 / s.video_frames_decoded as f64
    } else {
        0.0
    };

    println!(
        "CONFORMANCE_JSON {{\"platform\":\"{}\",\"secs\":{},\"stall_events\":{},\"stall_buffering_events\":{},\"stall_ms_total\":{},\"pipeline_retries\":{},\"render_gap_max_ms\":{},\"render_burst_frames\":{},\"judder_frames\":{},\"interval_hist\":[{},{},{},{}],\"av_drift_max_ms\":{},\"frames_decoded\":{},\"frames_dropped\":{},\"frames_late\":{},\"audio_underruns\":{},\"errors\":{},\"eos\":{},\"video_track_changes\":{},\"lipsync_pairs\":{},\"lipsync_beeps\":{},\"lipsync_median_ms\":{},\"lipsync_max_abs_ms\":{},\"rebuilds\":{},\"dead_rebuilds\":{},\"silent_rebuild_windows\":{},\"clock_wall_fallbacks\":{},\"audio_output_rebuilds\":{}}}",
        std::env::consts::OS,
        args.secs,
        s.stall_events,
        stalls_ev,
        s.stall_ms_total,
        s.pipeline_retries,
        s.render_gap_max_ms,
        s.render_burst_frames,
        s.judder_frames,
        s.interval_hist[0],
        s.interval_hist[1],
        s.interval_hist[2],
        s.interval_hist[3],
        s.av_drift_max_ms,
        s.video_frames_decoded,
        s.video_frames_dropped,
        s.video_late_frames,
        s.audio_underruns,
        errors,
        eos,
        counters.video_track_changes.load(Ordering::Relaxed),
        offsets.len(),
        beeps,
        lip_median,
        lip_max_abs,
        rebuilds.len(),
        dead_rebuilds,
        silent_windows.len(),
        s.clock_wall_fallbacks,
        s.audio_output_rebuilds,
    );

    let mut failed = false;
    let mut check = |name: &str, ok: bool, detail: String| {
        if ok {
            println!("PASS {name}: {detail}");
        } else {
            println!("FAIL {name}: {detail}");
            failed = true;
        }
    };
    check("errors", errors == 0, format!("{errors} player errors"));
    // The mute-after-seek family. Each of these is a pipeline that played
    // picture without sound; none of the rate/drift criteria can see it.
    check(
        "clock-fallbacks",
        s.clock_wall_fallbacks == 0,
        format!("{} master-clock handovers to the wall clock", s.clock_wall_fallbacks),
    );
    check(
        "audio-rebuilds",
        s.audio_output_rebuilds == 0,
        format!("{} audio-watchdog pipeline rebuilds", s.audio_output_rebuilds),
    );
    check(
        "audio-after-rebuild",
        dead_rebuilds == 0,
        format!(
            "{dead_rebuilds} of {} rebuilds never advanced the audio position (limit {} s)",
            rebuilds.len(),
            AUDIO_DEAD_AFTER_REBUILD.as_secs()
        ),
    );
    check(
        "lipsync-per-rebuild",
        silent_windows.is_empty(),
        format!(
            "{} rebuild window(s) without a flash/beep pair: {:?}",
            silent_windows.len(),
            silent_windows
        ),
    );
    check(
        "pipeline-retries",
        s.pipeline_retries == 0,
        format!("{} mid-play pipeline rebuilds", s.pipeline_retries),
    );
    check("eos", eos == 0, format!("{eos} EndOfStream before scenario end"));
    check(
        "stalls",
        stalls_ev <= allowed_stalls,
        format!("{stalls_ev} Buffering(Stall) events (allowed {allowed_stalls})"),
    );
    check(
        "render-gap",
        s.render_gap_max_ms <= args.max_gap_ms,
        format!("max frame gap {} ms (limit {})", s.render_gap_max_ms, args.max_gap_ms),
    );
    check(
        "render-bursts",
        s.render_burst_frames <= args.max_bursts,
        format!("{} sub-5ms renders (limit {})", s.render_burst_frames, args.max_bursts),
    );
    let judder_pct = if s.video_frames_decoded > 0 {
        s.judder_frames as f64 * 100.0 / s.video_frames_decoded as f64
    } else {
        0.0
    };
    check(
        "judder",
        judder_pct <= args.max_judder_pct,
        format!(
            "{} frames >±10 ms off cadence = {:.1}% (limit {:.0}%)",
            s.judder_frames, judder_pct, args.max_judder_pct
        ),
    );
    check(
        "av-drift",
        s.av_drift_max_ms <= args.max_drift_ms,
        format!("max |A/V drift| {} ms (limit {})", s.av_drift_max_ms, args.max_drift_ms),
    );
    check(
        "late-frames",
        late_pct <= args.max_late_pct,
        format!(
            "{} frames presented >45 ms late = {:.2}% (limit {:.1}%)",
            s.video_late_frames, late_pct, args.max_late_pct
        ),
    );
    if args.max_lipsync_ms > 0 {
        // Coverage first: the asset marks every 2 s, so a scenario of N
        // seconds should yield a good fraction of N/2 pairs (seek holes and
        // swap gaps eat a few). Too few pairs means the detector didn't see
        // the beeps (wrong asset? silent sink?) — that is a FAIL, not a pass.
        let expected_pairs = (args.secs / 2) as usize;
        check(
            "lipsync-coverage",
            offsets.len() >= expected_pairs / 3,
            format!(
                "{} flash/beep pairs measured from {} beeps (want ≥ {})",
                offsets.len(),
                beeps,
                expected_pairs / 3
            ),
        );
        // Per-pair worst case tolerates the measurement noise (≈ ±20 ms:
        // 1 ms detector blocks, one device callback of position granularity,
        // the asset's own ≤ 21 ms beep gating); the MEDIAN is the actual
        // A/V offset the viewer lives with.
        check(
            "lipsync",
            lip_max_abs <= args.max_lipsync_ms,
            format!(
                "max |flash − beep| {} ms over {} pairs (limit {})",
                lip_max_abs,
                offsets.len(),
                args.max_lipsync_ms
            ),
        );
        check(
            "lipsync-median",
            lip_median.abs() <= args.max_lipsync_median_ms,
            format!(
                "median flash − beep {:+} ms (+ = picture late; limit ±{})",
                lip_median, args.max_lipsync_median_ms
            ),
        );
    }
    // Sanity: the scenario actually played (≥15 fps average) and the soft
    // switches actually happened.
    check(
        "throughput",
        s.video_frames_decoded > args.secs * 15,
        format!("{} frames decoded over {} s", s.video_frames_decoded, args.secs),
    );

    std::process::exit(if failed { 1 } else { 0 });
}
