mod crypto;
mod decoders;
mod manifest;
mod networking;
mod parsers;
mod renderers;
mod tracks;
mod utils;
mod video;

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
use winit::dpi::PhysicalSize;
use winit::window::Window;

use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::{Notify, RwLock};
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

#[derive(Clone)]
pub struct Player {
    base_url: Option<String>,
    manifest: Option<Manifest>,
    tracks: Arc<StdMutex<Option<Tracks>>>,

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

    decryptor: Arc<StdMutex<Option<Arc<dyn Decryptor>>>>,

    video_renderer: Arc<VideoRenderer>,
    audio_renderer: Arc<AudioRenderer>,
}

// ---------------------------------------------------------------------------
// Unified sync producers (same logic on all platforms)
// ---------------------------------------------------------------------------

async fn video_sync_producer(
    start_time: Arc<Instant>,
    mut input_rx: mpsc::Receiver<DecodedVideoFrame>,
    renderer: Arc<VideoRenderer>,
    position_ms: Arc<AtomicU64>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
) {
    let mut last_pts_ms = 0u64;
    let mut frame_idx: u64 = 0;
    let mut last_render_elapsed: u64 = 0;
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
        let pts_ms = (frame.pts_us / 1000) as u64;
        let elapsed = start_time.elapsed().as_millis() as u64;

        if pts_ms < last_pts_ms {
            log::warn!("[vsync] BACKWARD #{} pts={}ms last={}ms Δ=-{}ms elapsed={}ms",
                frame_idx, pts_ms, last_pts_ms, last_pts_ms - pts_ms, elapsed);
        }

        if pts_ms > elapsed {
            let sleep_dur = Duration::from_millis(pts_ms - elapsed);
            tokio::select! {
                _ = tokio::time::sleep(sleep_dur) => {}
                _ = stop.notified() => break,
            }
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
        } else {
            let late_ms = elapsed - pts_ms;
            if late_ms > 80 {
                log::warn!("[vsync] LATE #{} pts={}ms elapsed={}ms late={}ms",
                    frame_idx, pts_ms, elapsed, late_ms);
            }
        }

        let render_start = start_time.elapsed().as_millis() as u64;
        let interval_ms = if last_render_elapsed > 0 { render_start - last_render_elapsed } else { 0 };
        let delta_pts = if pts_ms >= last_pts_ms { pts_ms - last_pts_ms } else { 0 };

        last_pts_ms = pts_ms;
        last_render_elapsed = render_start;
        frame_idx += 1;

        position_ms.store(pts_ms, Ordering::Relaxed);
        renderer.render_frame(frame).await;

        let render_done = start_time.elapsed().as_millis() as u64;
        let render_ms = render_done - render_start;

        // Log every frame: frame#, pts, wall when render started, how long render took, display interval
        log::info!("[vsync] #{} pts={}ms wall={}ms render={}ms interval={}ms Δpts={}ms",
            frame_idx - 1, pts_ms, render_start, render_ms, interval_ms, delta_pts);
    }
}

async fn audio_sync_producer(
    mut input_rx: mpsc::Receiver<DecodedAudioFrame>,
    audio_renderer: Arc<AudioRenderer>,
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
                _ = audio_renderer.put_samples_raw(&frame.samples) => {}
                _ = stop.notified() => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unified decoder tasks (same logic on all platforms)
// ---------------------------------------------------------------------------

async fn video_decoder_task(
    mut receiver: Receiver<DataSegment>,
    sender: Sender<DecodedVideoFrame>,
    mut decoder: Box<dyn HwVideoDecoder>,
    init_data: Vec<u8>,
    video_ready: Arc<Notify>,
    track_crypto: Option<TrackCrypto>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Fire video_ready only after the first frame is actually decoded, not on
    // first submit. Hardware decoders (MediaCodec HEVC) have a pipeline warmup
    // of ~8 frames during which try_recv returns nothing. If we signal ready on
    // submit, start_time is set ~333ms before any frame is available, and all
    // warmup-latency frames render instantly at max speed — visible as a jump.
    let mut first_frame_signaled = false;
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
                        if !first_frame_signaled {
                            video_ready.notify_one();
                            first_frame_signaled = true;
                        }
                        if sender.send(frame).await.is_err() {
                            return Ok(());
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
// Unified play pipeline builders
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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(MAX_SEGMENTS);

    let init_data = video_representation
        .segment_init
        .download()
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("init download: {}", e).into() })?;

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
                "Track is CENC-encrypted but no decryptor configured (call Player::set_clearkey)",
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
        task::spawn(Player::download_task(segments, start_index, download_tx, stop, stop_flag));
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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(MAX_SEGMENTS);

    let init_data = audio_representation
        .segment_init
        .download()
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("audio init download: {}", e).into() })?;

    let track_crypto = match parse_tenc(&init_data) {
        Some(tenc) => {
            println!(
                "audio: CENC encrypted, KID={} iv_size={}",
                hex::encode(tenc.default_kid),
                tenc.default_iv_size
            );
            let dec = decryptor.ok_or(
                "Audio track is CENC-encrypted but no decryptor configured (call Player::set_clearkey)",
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
        task::spawn(Player::download_task(segments, start_index, download_tx, stop, stop_flag));
    let decoder_task =
        task::spawn(audio_decoder_task(download_rx, sender, decoder, init_data, audio_ready, track_crypto));

    let (dl_res, dec_res) = join!(download_task, decoder_task);
    log_task_result("audio download_task", dl_res);
    log_task_result("audio decoder_task", dec_res);
    Ok(())
}

async fn lifetime_handler(
    seek_offset: Duration,
    video_ready: Arc<Notify>,
    video_rx: mpsc::Receiver<DecodedVideoFrame>,
    video_tx: Arc<VideoRenderer>,
    position_ms: Arc<AtomicU64>,
    audio_ready: Arc<Notify>,
    audio_rx: mpsc::Receiver<DecodedAudioFrame>,
    audio_tx: Arc<AudioRenderer>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
) {
    // Wait for both decoders to produce their first output before setting
    // start_time. This ensures A/V sync is established from a common wall-clock
    // origin. The audio channel is sized large enough (256 frames) that
    // audio_decoder_task won't block waiting for lifetime_handler even if
    // video download is slow.
    tokio::select! {
        _ = async { tokio::join!(video_ready.notified(), audio_ready.notified()); } => {}
        _ = stop.notified() => {
            stop.notify_waiters();
            return;
        }
    }
    let now = Instant::now();
    let start_time = Arc::new(now.checked_sub(seek_offset).unwrap_or(now));
    // Both sync producers must finish — if audio errors out early, video keeps playing.
    let (_, _) = tokio::join!(
        tokio::spawn(video_sync_producer(
            start_time.clone(),
            video_rx,
            video_tx,
            position_ms,
            stop.clone(),
            stop_flag.clone(),
        )),
        tokio::spawn(audio_sync_producer(
            audio_rx,
            audio_tx,
            stop.clone(),
            stop_flag.clone(),
        )),
    );
    stop.notify_waiters();
}

impl Player {
    pub fn new(window: Arc<Window>) -> Self {
        let start_time = Arc::new(Instant::now());

        let video_ready = Arc::new(Notify::new());
        let audio_ready = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());

        let video_renderer = Arc::new(VideoRenderer::new(window).block_on());
        let audio_renderer = Arc::new(AudioRenderer::new());

        Player {
            base_url: None,
            manifest: None,
            tracks: Arc::new(StdMutex::new(None)),
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

    pub fn set_clearkey(&self, keys: HashMap<String, String>) -> Result<(), Box<dyn Error>> {
        let decryptor = ClearKeyDecryptor::from_hex(keys)?;
        *self.decryptor.lock().unwrap() = Some(Arc::new(decryptor));
        Ok(())
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
        let manifest = Manifest::new(url).await?;
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
        let tracks = Tracks::new(base_url, &manifest.mpd).await?;
        *self.tracks.lock().unwrap() = Some(tracks);
        Ok(())
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

    async fn download_task(
        segments: Vec<Segment>,
        start_index: usize,
        segment_sender: Sender<DataSegment>,
        stop: Arc<Notify>,
        stop_flag: Arc<AtomicBool>,
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
                res = download_and_queue(i, seg, sender) => {
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

    /// Single play() implementation for all platforms. Creates platform-specific
    /// decoder instances and feeds them into the same generic pipeline.
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
        let video_renderer = self.video_renderer.clone();
        let audio_renderer = self.audio_renderer.clone();
        let seek_target = self.seek_target.clone();
        let position_ms = self.position_ms.clone();
        let decryptor_snapshot = self.decryptor.lock().unwrap().clone();

        // Create platform-specific decoder instances. The rest of the pipeline
        // (download tasks, sync producers, lifetime handler) is identical.
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
            ));

            let sample_rate = audio_renderer.sample_rate();
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
            ));

            lifetime_handler(
                snapped_seek_offset,
                video_ready.clone(),
                frame_receiver,
                video_renderer,
                position_ms,
                audio_ready.clone(),
                sample_receiver,
                audio_renderer,
                stop.clone(),
                stop_flag.clone(),
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
        let audio_renderer = self.audio_renderer.clone();
        tokio::spawn(async move {
            {
                let mut slot = seek_target.write().await;
                *slot = Some(target);
                stop_flag.store(true, Ordering::Relaxed);
            }
            audio_renderer.flush();
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
        let audio_renderer = self.audio_renderer.clone();
        tokio::spawn(async move {
            audio_renderer.volume(volume_diff).await;
        });
    }

    pub fn resize(&self, size: PhysicalSize<u32>) {
        let video_renderer: Arc<VideoRenderer> = self.video_renderer.clone();
        tokio::spawn(async move {
            video_renderer.resize(size).await;
        });
    }

    fn change_frame_size(&self, size: PhysicalSize<u32>) {
        let video_renderer: Arc<VideoRenderer> = self.video_renderer.clone();
        tokio::spawn(async move {
            video_renderer.change_frame_size(size).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers (unchanged from before)
// ---------------------------------------------------------------------------

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
) -> Result<(), Box<dyn Error>> {
    let downloaded_data = segment.download().await?;
    let data_segment = DataSegment {
        id: index,
        size: downloaded_data.len(),
        data: downloaded_data,
    };
    if let Err(e) = sender.send(data_segment).await {
        return Err(format!("downstream receiver dropped: {:?}", e).into());
    }
    Ok(())
}
