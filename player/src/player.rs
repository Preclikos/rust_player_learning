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
    parse_aac_config, parse_hvcc_nalus, parse_senc, parse_tenc, AacConfig, ClearKeyDecryptor,
    Decryptor, TrackCrypto,
};
use ffmpeg_next::format::sample::Type;
use ffmpeg_next::frame::{Audio, Video};
use ffmpeg_next::software::resampling::Context;
use ffmpeg_next::{Packet, Rational};
use ffmpeg_sys_next::{av_hwdevice_ctx_create, AVBufferRef, AVHWDeviceContext, AVHWDeviceType};
use parsers::mp4::{aac_sampling_frequency_index_to_u32, apped_hevc_header, parse_hevc_nalu};
use pollster::FutureExt;
use re_mp4::{Mp4, StsdBoxContent};
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

    //frame_producer: Receiver<Video>,
    //frame_consumer: Sender<Video>,

    //play_handle: JoinHandle<()>,
    start_time: Arc<Instant>,
    video_ready: Arc<Notify>,
    audio_ready: Arc<Notify>,
    //text_ready: Arc<Notify>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,

    seek_target: Arc<RwLock<Option<Duration>>>,
    position_ms: Arc<AtomicU64>,

    decryptor: Arc<StdMutex<Option<Arc<dyn Decryptor>>>>,

    video_renderer: Arc<VideoRenderer>,
    audio_renderer: Arc<AudioRenderer>,
}

async fn video_sync_producer(
    start_time: Arc<Instant>,
    mut input_rx: mpsc::Receiver<Arc<Video>>,
    output_tx: Arc<VideoRenderer>,
    position_ms: Arc<AtomicU64>,
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
        let elapsed = start_time.elapsed().as_millis() as u64;
        let pts = frame.pts().unwrap() as u64;
        if pts > elapsed {
            let sleep_dur = Duration::from_millis(pts - elapsed);
            tokio::select! {
                _ = tokio::time::sleep(sleep_dur) => {}
                _ = stop.notified() => break,
            }
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
        }
        if pts + 20 < elapsed {
            println!("Video drift more then 20ms dropping frame");
            continue;
        }

        position_ms.store(pts, Ordering::Relaxed);
        _ = output_tx.render(frame).await;
    }
}

async fn audio_sync_producer(
    start_time: Arc<Instant>,
    mut input_rx: mpsc::Receiver<Audio>,
    output_tx: Arc<AudioRenderer>,
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
        let elapsed = start_time.elapsed().as_millis() as u64;
        let pts = frame.pts().unwrap() as u64;
        if pts + 20 < elapsed {
            println!("Audio drift more then 20ms dropping frame");
            continue;
        }
        tokio::select! {
            _ = output_tx.put_sample(frame) => {}
            _ = stop.notified() => break,
        }
    }
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

    /// Provide ClearKey decryption keys directly. `keys` maps KID (hex) to key (hex),
    /// each 16 bytes / 32 hex chars. Replaces any previously configured decryptor.
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
        ffmpeg_next::init()?;
        let manifest = match &self.manifest {
            Some(success) => success,
            None => {
                eprintln!("Manifest not loaded!");
                return Err("Manifest not loaded!".into());
            }
        };

        let base_url = match &self.base_url {
            Some(success) => success.to_string(),
            None => {
                eprintln!("BaseUrl not loaded!");
                return Err("BaseUrl not loaded!".into());
            }
        };

        let tracks = Tracks::new(base_url, &manifest.mpd).await?;
        *self.tracks.lock().unwrap() = Some(tracks);

        Ok(())
    }

    pub fn get_tracks(&self) -> Result<Tracks, Box<dyn Error>> {
        match self.tracks.lock().unwrap().as_ref() {
            Some(success) => Ok(success.clone()),
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

    async fn video_decoder_task(
        mut receiver: Receiver<DataSegment>,
        sender: Sender<Arc<ffmpeg_next::util::frame::Video>>,
        mut decoder: ffmpeg_next::decoder::Video,
        init_data: Vec<u8>,
        video_ready: Arc<Notify>,
        track_crypto: Option<TrackCrypto>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        while let Some(segment) = receiver.recv().await {
            println!("Consuming video segment: {}", segment.id);

            let mut data_vec = init_data.clone();
            data_vec.extend_from_slice(&segment.data[..]);

            decrypt_segment_in_place(&mut data_vec, track_crypto.as_ref())?;

            let data = &data_vec[..];

            let mp4 = match Mp4::read_bytes(data) {
                Ok(success) => success,
                Err(e) => {
                    return Err(format!("Parsing error {}", e).into());
                }
            };

            let (_track_id, track) = mp4.tracks().first_key_value().unwrap();
            let samples = &track.samples;

            for sample in samples {
                let sample_offset = sample.offset as usize;
                let sample_size = sample.size as usize;

                if sample_offset + sample_size > data.len() {
                    eprintln!(
                        "Sample at offset {} (size {}) exceeds mdat bounds (size {})",
                        sample_offset,
                        sample_size,
                        data.len()
                    );
                    continue;
                }

                let sample_data = &data[sample_offset..sample_offset + sample_size];
                let nalus = parse_hevc_nalu(sample_data).unwrap();

                for nalu in nalus {
                    let mut packet = Packet::new(nalu.len());
                    let pts = sample.composition_timestamp * 1000 / (sample.timescale as i64);
                    packet.set_pts(Some(pts));

                    packet.data_mut().unwrap().clone_from_slice(&nalu[..]);

                    if let Err(e) = decoder.send_packet(&packet) {
                        return Err(format!("Error sending packet to decoder: {:?}", e).into());
                    }
                }

                let mut frame = ffmpeg_next::util::frame::Video::empty();
                //let mut cpu_frame = ffmpeg_next::util::frame::Video::empty();
                video_ready.notify_waiters();

                while let Ok(()) = decoder.receive_frame(&mut frame) {
                    let frame_arc = Arc::new(frame);
                    /*
                        // Transfer the GPU frame to system memory
                        let ret =
                            av_hwframe_transfer_data(cpu_frame.as_mut_ptr(), frame.as_mut_ptr(), 0);
                        if ret < 0 {
                            panic!("Failed to transfer data from GPU to CPU: {}", ret);
                        }
                    }

                    cpu_frame.set_pts(frame.pts());*/

                    match sender.send(frame_arc).await {
                        Ok(_success) => {}
                        Err(e) => return Err(format!("Cannot send frame to channel {}", e).into()),
                    };

                    frame = ffmpeg_next::util::frame::Video::empty(); // Create a new empty frame
                }
            }
        }
        Ok(())
    }

    async fn audio_decoder_task(
        mut receiver: Receiver<DataSegment>,
        sender: Sender<Audio>,
        mut decoder: ffmpeg_next::decoder::Audio,
        init_data: Vec<u8>,
        audio_ready: Arc<Notify>,
        mut resampler: Context,
        track_crypto: Option<TrackCrypto>,
        aac_config: AacConfig,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let target_sample_rate = resampler.output().rate;
        while let Some(segment) = receiver.recv().await {
            println!("Consuming audio segment: {}", segment.id);

            let mut data_vec = init_data.clone();
            data_vec.extend_from_slice(&segment.data[..]);

            decrypt_segment_in_place(&mut data_vec, track_crypto.as_ref())?;

            let data = &data_vec[..];

            let mp4 = match Mp4::read_bytes(data) {
                Ok(success) => success,
                Err(e) => {
                    return Err(format!("Parsing error {}", e).into());
                }
            };

            let (_track_id, track) = mp4.tracks().first_key_value().unwrap();
            let samples = &track.samples;

            for sample in samples {
                let sample_offset = sample.offset as usize;
                let sample_size = sample.size as usize;

                if sample_offset + sample_size > data.len() {
                    eprintln!(
                        "Sample at offset {} (size {}) exceeds mdat bounds (size {})",
                        sample_offset,
                        sample_size,
                        data.len()
                    );
                    continue;
                }

                let sample_data = &data[sample_offset..sample_offset + sample_size];

                let mut packet = Packet::new(sample_data.len());
                let pts = sample.composition_timestamp * 1000 / (target_sample_rate as i64);
                packet.set_pts(Some(pts));
                packet.set_time_base(Rational(1, target_sample_rate as i32));
                packet.data_mut().unwrap().clone_from_slice(sample_data);

                if let Err(e) = decoder.send_packet(&packet) {
                    println!("Error sending packet to decoder: {:?}", e);
                    return Err(format!("Error sending packet to decoder: {:?}", e).into());
                }

                let mut frame = ffmpeg_next::util::frame::Audio::empty();
                let mut dst_frame = ffmpeg_next::util::frame::Audio::empty();

                audio_ready.notify_waiters();
                while let Ok(()) = decoder.receive_frame(&mut frame) {
                    resampler.run(&frame, &mut dst_frame)?;
                    dst_frame.set_pts(frame.pts());

                    match sender.send(dst_frame.clone()).await {
                        Ok(_success) => {}
                        Err(e) => return Err(format!("Cannot send frame to channel {}", e).into()),
                    };
                }
            }
        }

        Ok(())
    }

    async fn video_play(
        video_representation: VideoRepresenation,
        start_index: usize,
        video_ready: Arc<Notify>,
        sender: Sender<Arc<Video>>,
        stop: Arc<Notify>,
        stop_flag: Arc<AtomicBool>,
        decryptor: Option<Arc<dyn Decryptor>>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let (download_tx, download_rx) = mpsc::channel::<DataSegment>(MAX_SEGMENTS);
        let init_data = match video_representation.segment_init.download().await {
            Ok(data) => data,
            Err(e) => return Err(format!("Error download init segment: {}", e).into()),
        };

        let mp4_info = match Mp4::read_bytes(&init_data[..]) {
            Ok(success) => success,
            Err(e) => return Err(format!("Error parsing mp4 Init {}", e).into()),
        };

        let (_track_id, track) = match mp4_info.tracks().first_key_value() {
            Some(track_info) => track_info,
            None => return Err("Cannot find any track".into()),
        };

        let codec_id = ffmpeg_next::codec::Id::HEVC;
        let codec = match ffmpeg_next::decoder::find(codec_id) {
            Some(codec) => codec,
            None => return Err("Cannot find codec for track".into()),
        };
        let mut decoder = match ffmpeg_next::codec::Context::new_with_codec(codec)
            .decoder()
            .video()
        {
            Ok(context) => context,
            Err(e) => return Err(format!("Cannot find decoder for codec {}", e).into()),
        };

        #[cfg(target_os = "windows")]
        unsafe {
            let mut hw_device_ctx: *mut AVBufferRef = std::ptr::null_mut();
            let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;
            let ret = av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                device_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if ret < 0 {
                panic!("Failed to create D3D11VA hardware device: {}", ret);
            }

            // Assign the device context to the codec context
            let codec_ctx_ptr = decoder.as_mut_ptr();
            (*codec_ctx_ptr).hw_device_ctx = hw_device_ctx;

            println!("D3D11VA hardware device context created successfully.");
        }

        #[cfg(target_os = "linux")]
        unsafe {
            let mut hw_device_ctx: *mut AVBufferRef = std::ptr::null_mut();
            let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI;
            let ret = av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                device_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if ret < 0 {
                panic!("Failed to create VAAPI hardware device: {}", ret);
            }

            // Assign the device context to the codec context
            let codec_ctx_ptr = decoder.as_mut_ptr();
            (*codec_ctx_ptr).hw_device_ctx = hw_device_ctx;

            println!("VAAPI hardware device context created successfully.");
        }

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

        let feed_hvcc_nalus = |decoder: &mut ffmpeg_next::decoder::Video,
                               nalus: Vec<Vec<u8>>|
         -> Result<(), Box<dyn Error + Send + Sync>> {
            for nalu_data in nalus {
                let nalu = apped_hevc_header(nalu_data);
                let mut packet = Packet::new(nalu.len());
                packet.data_mut().unwrap().clone_from_slice(&nalu[..]);
                if let Err(e) = decoder.send_packet(&packet) {
                    return Err(format!("Error sending hvcC NALU to decoder: {:?}", e).into());
                }
            }
            Ok(())
        };

        match track.trak(&mp4_info).mdia.minf.stbl.stsd.contents.clone() {
            StsdBoxContent::Hvc1(hvc) => {
                let nalus: Vec<Vec<u8>> = hvc
                    .hvcc
                    .arrays
                    .clone()
                    .into_iter()
                    .flat_map(|a| a.nalus.into_iter().map(|n| n.data))
                    .collect();
                feed_hvcc_nalus(&mut decoder, nalus)?;
            }
            StsdBoxContent::Hev1(hev) => {
                let nalus: Vec<Vec<u8>> = hev
                    .hvcc
                    .arrays
                    .clone()
                    .into_iter()
                    .flat_map(|a| a.nalus.into_iter().map(|n| n.data))
                    .collect();
                feed_hvcc_nalus(&mut decoder, nalus)?;
            }
            _ => {
                // Encrypted (encv/encf) or otherwise unrecognized — try the hvcC box directly.
                let nalus = parse_hvcc_nalus(&init_data)
                    .ok_or("Codec not supported (no hvcC in init segment)")?;
                feed_hvcc_nalus(&mut decoder, nalus)?;
            }
        };

        let video = video_representation;
        let segments = video.segments.clone();

        let download_task = task::spawn(Self::download_task(
            segments,
            start_index,
            download_tx,
            stop,
            stop_flag,
        ));

        let decoder_task = task::spawn(Self::video_decoder_task(
            download_rx,
            sender,
            decoder,
            init_data,
            video_ready,
            track_crypto,
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
        sender: Sender<Audio>,
        sample_rate: u32,
        stop: Arc<Notify>,
        stop_flag: Arc<AtomicBool>,
        decryptor: Option<Arc<dyn Decryptor>>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let (download_tx, download_rx) = mpsc::channel::<DataSegment>(MAX_SEGMENTS);
        let init_data = match audio_representation.segment_init.download().await {
            Ok(data) => data,
            Err(e) => return Err(format!("Error download init segment: {}", e).into()),
        };

        let mp4_info = match Mp4::read_bytes(&init_data[..]) {
            Ok(success) => success,
            Err(e) => return Err(format!("Error parsing mp4 Init {}", e).into()),
        };

        let (_track_id, track) = match mp4_info.tracks().first_key_value() {
            Some(track_info) => track_info,
            None => return Err("Cannot find any track".into()),
        };

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
                println!("audio: clear (no tenc box)");
                None
            }
        };

        // Prefer our own ASC parser over re_mp4's — independent of re_mp4's quirks.
        let aac_config = parse_aac_config(&init_data)
            .or_else(|| match track.trak(&mp4_info).mdia.minf.stbl.stsd.contents.clone() {
                StsdBoxContent::Mp4a(mp4a) => {
                    let esds = mp4a.esds?;
                    Some(AacConfig {
                        profile: esds.es_desc.dec_config.dec_specific.profile,
                        freq_index: esds.es_desc.dec_config.dec_specific.freq_index,
                        chan_conf: esds.es_desc.dec_config.dec_specific.chan_conf,
                    })
                }
                _ => None,
            })
            .ok_or("Audio codec not supported (no AAC config in init segment)")?;

        println!(
            "audio: AAC profile={} freq_index={} (={} Hz) chan_conf={}",
            aac_config.profile,
            aac_config.freq_index,
            aac_sampling_frequency_index_to_u32(aac_config.freq_index),
            aac_config.chan_conf
        );

        let codec_id = ffmpeg_next::codec::Id::AAC;
        let codec = match ffmpeg_next::decoder::find(codec_id) {
            Some(codec) => codec,
            None => return Err("Cannot find codec for track".into()),
        };

        let mut ctx = ffmpeg_next::codec::Context::new_with_codec(codec);

        // Build the 2-byte AudioSpecificConfig from the parsed config and install
        // it as the decoder's extradata. With this set, FFmpeg's AAC decoder reads
        // codec parameters at open time and accepts raw mdat frames without ADTS.
        let dsi: [u8; 2] = [
            (aac_config.profile << 3) | (aac_config.freq_index >> 1),
            ((aac_config.freq_index & 0x01) << 7) | (aac_config.chan_conf << 3),
        ];
        unsafe {
            let ctx_ptr = ctx.as_mut_ptr();
            let padding = ffmpeg_sys_next::AV_INPUT_BUFFER_PADDING_SIZE as usize;
            let extradata = ffmpeg_sys_next::av_mallocz(dsi.len() + padding);
            if extradata.is_null() {
                return Err("av_mallocz failed for AAC extradata".into());
            }
            std::ptr::copy_nonoverlapping(dsi.as_ptr(), extradata as *mut u8, dsi.len());
            (*ctx_ptr).extradata = extradata as *mut u8;
            (*ctx_ptr).extradata_size = dsi.len() as i32;
        }

        let mut decoder = match ctx.decoder().audio() {
            Ok(context) => context,
            Err(e) => return Err(format!("Cannot find decoder for codec {}", e).into()),
        };

        let sample_rate_src = aac_sampling_frequency_index_to_u32(aac_config.freq_index);
        let in_layout = match aac_config.chan_conf {
            1 => ffmpeg_next::util::channel_layout::ChannelLayout::MONO,
            2 => ffmpeg_next::util::channel_layout::ChannelLayout::STEREO,
            n => ffmpeg_next::util::channel_layout::ChannelLayout::default(n as i32),
        };

        let resampler = Context::get(
            ffmpeg_next::util::format::sample::Sample::F32(Type::Planar),
            in_layout,
            sample_rate_src,
            ffmpeg_next::util::format::sample::Sample::F32(Type::Packed),
            ffmpeg_next::util::channel_layout::ChannelLayout::STEREO,
            sample_rate,
        )?;

        let audio = audio_representation;
        let segments = audio.segments.clone();

        let download_task = task::spawn(Self::download_task(
            segments,
            start_index,
            download_tx,
            stop,
            stop_flag,
        ));

        let decoder_task = task::spawn(Self::audio_decoder_task(
            download_rx,
            sender,
            decoder,
            init_data,
            audio_ready,
            resampler,
            track_crypto,
            aac_config,
        ));

        let (dl_res, dec_res) = join!(download_task, decoder_task);
        log_task_result("audio download_task", dl_res);
        log_task_result("audio decoder_task", dec_res);
        Ok(())
    }

    async fn lifetime_handler(
        seek_offset: Duration,
        video_ready: Arc<Notify>,
        video_rx: mpsc::Receiver<Arc<Video>>,
        video_tx: Arc<VideoRenderer>,
        position_ms: Arc<AtomicU64>,
        audio_ready: Arc<Notify>,
        audio_rx: mpsc::Receiver<Audio>,
        audio_tx: Arc<AudioRenderer>,
        stop: Arc<Notify>,
        stop_flag: Arc<AtomicBool>,
    ) {
        //loop {
        tokio::select! {
            _ = async {
                tokio::join!(video_ready.notified(), audio_ready.notified());
            } => {}
            _ = stop.notified() => {
                stop.notify_waiters();
                return;
            }
        }
        let now = Instant::now();
        let start_time = Arc::new(now.checked_sub(seek_offset).unwrap_or(now));
        tokio::select! {
            _ = tokio::spawn(video_sync_producer(
                start_time.clone(),
                video_rx,
                video_tx,
                position_ms,
                stop.clone(),
                stop_flag.clone(),
            )) => { }
            _ = tokio::spawn(audio_sync_producer(
                start_time.clone(),
                audio_rx,
                audio_tx,
                stop.clone(),
                stop_flag.clone(),
            )) => { }
            /*_ = text_play => {
            }*/
        }
        stop.notify_waiters();
        //}
    }

    pub fn play(&self) -> Result<JoinHandle<()>, Box<dyn Error>> {
        let video_representation = match self.video_representation.lock().unwrap().as_ref() {
            Some(success) => success.clone(),
            None => return Err("Video Track not set".into()),
        };

        let audio_representation = match self.audio_representation.lock().unwrap().as_ref() {
            Some(success) => success.clone(),
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

            let (frame_sender, frame_receiver) = mpsc::channel::<Arc<Video>>(4);
            let (sample_sender, sample_receiver) = mpsc::channel::<Audio>(32);
            let play = tokio::spawn(Self::video_play(
                video_representation,
                video_start_index,
                video_ready.clone(),
                frame_sender,
                stop.clone(),
                stop_flag.clone(),
                decryptor_snapshot.clone(),
            ));

            let sample_rate = audio_renderer.sample_rate();

            let audio = tokio::spawn(Self::audio_play(
                audio_representation,
                audio_start_index,
                audio_ready.clone(),
                sample_sender,
                sample_rate,
                stop.clone(),
                stop_flag.clone(),
                decryptor_snapshot,
            ));

            Self::lifetime_handler(
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

            let (play_res, audio_res) = join!(play, audio);
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

#[derive(Debug, Clone)]
struct DataSegment {
    id: usize,
    size: usize,   // Size in bytes
    data: Vec<u8>, // Simulated data
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

/// Decrypt the samples in `data_vec` in place, if a `TrackCrypto` is configured.
/// Parses the segment's `senc` box for per-sample IVs and subsample maps, then runs
/// AES-CTR (or whatever the [`Decryptor`] implementation provides) on the encrypted bytes.
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

    // Collect (offset, size) per sample from re_mp4, drop the parser to release the borrow,
    // then decrypt the matching ranges of `data_vec` in place.
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
