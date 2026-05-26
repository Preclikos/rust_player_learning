mod crypto;
mod decoders;
mod events;
mod manifest;
mod net;
mod parsers;
mod renderers;
mod tracks;
mod utils;
mod video;

// Public re-exports so downstream consumers (BlackZone Console etc.) can
// implement RequestInterceptor / LicenseResolver against the player's
// canonical types — see PLAYER_INTEGRATION.md.
pub use events::{
    BufferingReason, Fps, PlayerErrorKind, PlayerEvent, TrackInfo, TrackKind,
};
pub use net::{
    BoxError, HttpClient, LicenseResolver, NoopInterceptor, PreparedRequest,
    RequestInterceptor, RequestKind, RetryPolicy,
};

use crypto::{
    parse_aac_config, parse_hvcc_nalus, parse_senc, parse_tenc, ClearKeyDecryptor,
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
use winit::dpi::PhysicalSize;
use winit::window::Window;

use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

const MAX_SEGMENTS: usize = 2;

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

    video_renderer: Arc<V>,
    audio_renderer: Arc<A>,
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
            video_renderer: Arc::clone(&self.video_renderer),
            audio_renderer: Arc::clone(&self.audio_renderer),
        }
    }
}

// ---------------------------------------------------------------------------
// A/V sync loop — identical on all platforms, generic over sink traits
// ---------------------------------------------------------------------------

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

async fn video_sync_loop<V: VideoSink>(
    start_time: Arc<Instant>,
    mut input_rx: mpsc::Receiver<DecodedVideoFrame>,
    renderer: Arc<V>,
    position_ms: Arc<AtomicU64>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    events: Arc<broadcast::Sender<PlayerEvent>>,
    media_duration: Duration,
    paused: Arc<AtomicBool>,
    pause_notify: Arc<Notify>,
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
    let mut emitted_playing = false;
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
        }
        let mut frame = tokio::select! {
            maybe = input_rx.recv() => match maybe {
                Some(f) => f,
                None => break,
            },
            _ = stop.notified() => break,
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

        // Emit `Playing` once on the first rendered frame after a sync
        // loop (re)starts. Buffering→Playing transition.
        if !emitted_playing {
            let _ = events.send(PlayerEvent::Playing);
            emitted_playing = true;
        }

        renderer.render_frame(frame).await;

        let render_done = start_time
            .elapsed()
            .saturating_sub(pause_skew)
            .as_millis() as u64;
        let render_ms = render_done - render_start;

        // Rate-limited Position emission (≤ 4 Hz). Bandwidth + buffered
        // ahead are 0 in this slice — populated in P2 when we instrument
        // the download/decoder tasks.
        if last_position_emit.elapsed() >= Duration::from_millis(250) {
            let _ = events.send(PlayerEvent::Position {
                position: Duration::from_millis(pts_ms),
                duration: media_duration,
                buffered_ahead_secs: 0.0,
                bandwidth_bps: 0,
            });
            last_position_emit = Instant::now();
        }

        log::info!("[vsync] #{} pts={}ms wall={}ms render={}ms interval={}ms Δpts={}ms",
            frame_idx - 1, pts_ms, render_start, render_ms, interval_ms, delta_pts);
    }
}

async fn audio_sync_loop<A: AudioSink>(
    mut input_rx: mpsc::Receiver<DecodedAudioFrame>,
    sink: Arc<A>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
) {
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let frame = tokio::select! {
            maybe = input_rx.recv() => match maybe {
                Some(f) => f,
                None => break,
            },
            _ = stop.notified() => break,
        };
        if !frame.samples.is_empty() {
            tokio::select! {
                _ = sink.put_samples(&frame.samples) => {}
                _ = stop.notified() => break,
            }
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
    let (_, _) = tokio::join!(
        tokio::spawn(video_sync_loop(
            start_time.clone(),
            video_rx,
            video_sink,
            position_ms,
            stop.clone(),
            stop_flag.clone(),
            events.clone(),
            media_duration,
            paused,
            pause_notify,
        )),
        tokio::spawn(audio_sync_loop(
            audio_rx,
            audio_sink,
            stop.clone(),
            stop_flag.clone(),
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
        println!("Consuming video segment: {}", segment.id);

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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    while let Some(segment) = receiver.recv().await {
        println!("Consuming audio segment: {}", segment.id);

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
            audio_ready.notify_one();

            loop {
                match decoder.try_recv()? {
                    Some(frame) => {
                        if sender.send(frame).await.is_err() {
                            return Ok(());
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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(MAX_SEGMENTS);

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
            println!(
                "video: CENC encrypted, KID={} iv_size={}",
                hex::encode(tenc.default_kid),
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
                    format!("license resolve (video kid={}): {}", hex::encode(tenc.default_kid), e).into()
                },
            )?;
            Some(TrackCrypto {
                decryptor: dec,
                kid: tenc.default_kid,
                iv_size: tenc.default_iv_size as usize,
            })
        }
        None => {
            println!("video: clear (no tenc box)");
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
    let download_task =
        task::spawn(download_task(segments, start_index, download_tx, stop, stop_flag, http));
    let decoder_task =
        task::spawn(video_decoder_task(download_rx, sender, decoder, init_data, video_ready, track_crypto));

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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(MAX_SEGMENTS);

    let init_dl = audio_representation
        .segment_init
        .download(&http, RequestKind::InitSegment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("audio init download: {}", e).into() })?;
    let init_data = init_dl.data;

    let track_crypto = match parse_tenc(&init_data) {
        Some(tenc) => {
            println!(
                "audio: CENC encrypted, KID={} iv_size={}",
                hex::encode(tenc.default_kid),
                tenc.default_iv_size
            );
            let dec = decryptor.ok_or(
                "Audio track is CENC-encrypted but no decryptor configured (call Player::set_clearkey or set_license_resolver)",
            )?;
            dec.ensure_key_for(tenc.default_kid).await.map_err(
                |e| -> Box<dyn Error + Send + Sync> {
                    format!("license resolve (audio kid={}): {}", hex::encode(tenc.default_kid), e).into()
                },
            )?;
            Some(TrackCrypto {
                decryptor: dec,
                kid: tenc.default_kid,
                iv_size: tenc.default_iv_size as usize,
            })
        }
        None => {
            println!("audio: clear (no tenc)");
            None
        }
    };

    let aac_config = parse_aac_config(&init_data)
        .ok_or("Audio codec not supported (no AAC config in init segment)")?;

    let input_sample_rate = aac_sampling_frequency_index_to_u32(aac_config.freq_index);
    let input_channels = aac_config.chan_conf as u16;
    let dsi: [u8; 2] = [
        (aac_config.profile << 3) | (aac_config.freq_index >> 1),
        ((aac_config.freq_index & 0x01) << 7) | (aac_config.chan_conf << 3),
    ];

    println!(
        "audio: AAC profile={} freq_index={} (={}Hz) chan_conf={}",
        aac_config.profile, aac_config.freq_index, input_sample_rate, aac_config.chan_conf
    );

    decoder.configure(AudioDecoderParams {
        codec: AudioCodec::Aac,
        input_sample_rate,
        input_channels,
        output_sample_rate,
        codec_specific_data: dsi.to_vec(),
    })?;

    let segments = audio_representation.segments.clone();
    let download_task =
        task::spawn(download_task(segments, start_index, download_tx, stop, stop_flag, http));
    let decoder_task =
        task::spawn(audio_decoder_task(download_rx, sender, decoder, init_data, audio_ready, track_crypto));

    let (dl_res, dec_res) = join!(download_task, decoder_task);
    log_task_result("audio download_task", dl_res);
    log_task_result("audio decoder_task", dec_res);
    Ok(())
}

// ---------------------------------------------------------------------------
// Player — constructs default (platform-native) sinks
// ---------------------------------------------------------------------------

impl Player<VideoRenderer, AudioRenderer> {
    pub fn new(window: Arc<Window>) -> Self {
        let start_time = Arc::new(Instant::now());

        let video_ready = Arc::new(Notify::new());
        let audio_ready = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());

        let video_renderer = Arc::new(VideoRenderer::new(window).block_on());
        let audio_renderer = Arc::new(AudioRenderer::new());

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

            video_renderer,
            audio_renderer,
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
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        ffmpeg_next::init()?;

        let manifest = match &self.manifest {
            Some(m) => m,
            None => return Err("Manifest not loaded!".into()),
        };
        let base_url = match &self.base_url {
            Some(u) => u.to_string(),
            None => return Err("BaseUrl not loaded!".into()),
        };
        let tracks = match Tracks::new(base_url, &manifest.mpd, &self.http).await {
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

    pub fn change_video_track(&self, representation: &VideoRepresenation) {
        *self.video_representation.lock().unwrap() = Some(representation.clone());
        let size = PhysicalSize::new(representation.width, representation.height);
        self.change_frame_size(size);
        self.seek(self.position());
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

        #[cfg(any(target_os = "windows", target_os = "linux"))]
        let video_decoder: Box<dyn HwVideoDecoder> =
            Box::new(decoders::ffmpeg_hw::FfmpegHwDecoder::new());
        #[cfg(target_os = "android")]
        let video_decoder: Box<dyn HwVideoDecoder> =
            Box::new(decoders::mediacodec::MediaCodecDecoder::new());

        #[cfg(any(target_os = "windows", target_os = "linux"))]
        let audio_decoder: Box<dyn AudioDecoder> =
            Box::new(decoders::ffmpeg_audio::FfmpegAudioDecoder::new());
        #[cfg(target_os = "android")]
        let audio_decoder: Box<dyn AudioDecoder> =
            Box::new(decoders::mediacodec_audio::MediaCodecAudioDecoder::new());

        let play = tokio::spawn(async move {
            let seek_offset = {
                let mut target = seek_target.write().await;
                stop_flag.store(false, Ordering::Relaxed);
                target.take().unwrap_or(Duration::ZERO)
            };

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

            let (frame_sender, frame_receiver) = mpsc::channel::<DecodedVideoFrame>(64);
            let (sample_sender, sample_receiver) = mpsc::channel::<DecodedAudioFrame>(256);

            let video = tokio::spawn(video_play(
                video_representation,
                video_start_index,
                video_ready.clone(),
                frame_sender,
                stop.clone(),
                stop_flag.clone(),
                decryptor_snapshot.clone(),
                video_decoder,
                http_video,
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
            ));

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
            )
            .await;

            let (play_res, audio_res) = join!(video, audio);
            log_task_result("video_play", play_res);
            log_task_result("audio_play", audio_res);
        });
        Ok(play)
    }

    pub fn seek(&self, target: Duration) {
        let seek_target = self.seek_target.clone();
        let stop = self.stop.clone();
        let stop_flag = self.stop_flag.clone();
        let audio_sink = self.audio_renderer.clone();
        tokio::spawn(async move {
            {
                let mut slot = seek_target.write().await;
                *slot = Some(target);
                stop_flag.store(true, Ordering::Relaxed);
            }
            audio_sink.flush();
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

    pub async fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        self.stop.notify_waiters();
        self.audio_renderer.stop().await;
    }

    pub fn volume(&self, volume_diff: f32) {
        let audio_sink = self.audio_renderer.clone();
        tokio::spawn(async move {
            audio_sink.volume(volume_diff).await;
        });
    }

    pub fn resize(&self, size: PhysicalSize<u32>) {
        let video_sink = self.video_renderer.clone();
        tokio::spawn(async move {
            video_sink.resize(size).await;
        });
    }

    fn change_frame_size(&self, size: PhysicalSize<u32>) {
        let video_sink = self.video_renderer.clone();
        tokio::spawn(async move {
            video_sink.change_frame_size(size).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn download_task(
    segments: Vec<Segment>,
    start_index: usize,
    segment_sender: Sender<DataSegment>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    http: Arc<HttpClient>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let segment_slice = &segments[..];
    for i in start_index..segment_slice.len() {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let sender = segment_sender.clone();
        let seg = &segment_slice[i];
        let mut should_break = false;
        tokio::select! {
            res = download_and_queue(i, seg, sender, &http) => {
                match res {
                    Ok(()) => println!("Producing segment {}", i),
                    Err(e) => {
                        eprintln!("download_task: segment {} failed: {}", i, e);
                        should_break = true;
                    }
                }
            }
            _ = stop.notified() => {
                should_break = true;
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
    size: usize,
    data: Vec<u8>,
}

fn log_task_result<T, E: std::fmt::Display>(
    name: &str,
    result: Result<Result<T, E>, tokio::task::JoinError>,
) {
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => eprintln!("{}: {}", name, e),
        Err(e) => eprintln!("{}: join error: {}", name, e),
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

    let senc_entries = parse_senc(data_vec, tc.iv_size)
        .ok_or("Encrypted track but segment has no parseable senc box")?;

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
        tc.decryptor
            .decrypt_sample(&tc.kid, &entry.iv, &mut data_vec[*offset..end], &entry.subsamples)?;
    }
    Ok(())
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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let dl = segment
        .download(http, RequestKind::Segment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> {
            format!("segment download: {}", e).into()
        })?;
    let data_segment = DataSegment {
        id: index,
        size: dl.data.len(),
        data: dl.data,
    };
    if let Err(e) = sender.send(data_segment).await {
        return Err(format!("downstream receiver dropped: {:?}", e).into());
    }
    Ok(())
}
