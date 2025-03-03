mod manifest;
mod networking;
mod parsers;
mod renderers;
mod tracks;
mod utils;
mod video;

use ffmpeg_next::format::sample::Type;
use ffmpeg_next::frame::{Audio, Video};
use ffmpeg_next::software::resampling::Context;
use ffmpeg_next::{Packet, Rational};
use ffmpeg_sys_next::{
    av_hwdevice_ctx_create, av_hwframe_transfer_data, AVBufferRef, AVCodecContext, AVHWDeviceType,
};
use parsers::mp4::{aac_sampling_frequency_index_to_u32, apped_hevc_header, parse_hevc_nalu};
use pollster::FutureExt;
use re_mp4::{Mp4, StsdBoxContent};
use renderers::audio::AudioRenderer;
use renderers::video::VideoRenderer;
use winit::window::Window;

use std::error::Error;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio::{join, sync::mpsc::Sender};
use tracks::audio::{AudioAdaptation, AudioRepresentation};
use tracks::{
    segment::Segment,
    video::{VideoAdaptation, VideoRepresenation},
    Tracks,
};
use url::Url;

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{self, Receiver};
use tokio::task::{self, JoinHandle};

use manifest::Manifest;

const MAX_SEGMENTS: usize = 2;

#[derive(Clone)]
pub struct Player {
    base_url: Option<String>,
    manifest: Option<Manifest>,
    tracks: Option<Tracks>,

    video_adaptation: Option<VideoAdaptation>,
    video_representation: Option<VideoRepresenation>,

    audio_adaptation: Option<AudioAdaptation>,
    audio_representation: Option<AudioRepresentation>,

    //frame_producer: Receiver<Video>,
    //frame_consumer: Sender<Video>,

    //play_handle: JoinHandle<()>,
    start_time: Arc<Instant>,
    video_ready: Arc<Notify>,
    audio_ready: Arc<Notify>,
    //text_ready: Arc<Notify>,
    stop: Arc<Notify>,

    video_renderer: Arc<VideoRenderer>,
    audio_renderer: Arc<AudioRenderer>,
}

async fn video_sync_producer(
    start_time: Arc<Instant>,
    mut input_rx: mpsc::Receiver<Arc<Video>>,
    output_tx: Arc<VideoRenderer>,
) {
    while let Some(frame) = input_rx.recv().await {
        let elapsed = start_time.elapsed().as_millis() as u64;
        let pts = frame.pts().unwrap() as u64;
        if pts > elapsed {
            tokio::time::sleep(Duration::from_millis(pts - elapsed)).await;
        }
        if pts + 20 < elapsed {
            println!("Video drift more then 20ms dropping frame");
            continue;
        }
        _ = output_tx.render(frame);
    }
}

async fn audio_sync_producer(
    start_time: Arc<Instant>,
    mut input_rx: mpsc::Receiver<Audio>,
    output_tx: Arc<AudioRenderer>,
) {
    while let Some(frame) = input_rx.recv().await {
        let elapsed = start_time.elapsed().as_millis() as u64;
        let pts = frame.pts().unwrap() as u64;
        if pts > elapsed {
            tokio::time::sleep(Duration::from_millis(pts - elapsed)).await;
        }
        if pts + 20 < elapsed {
            println!("Audio drift more then 20ms dropping frame");
            continue;
        }
        output_tx.put_sample(frame).await;
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
            tracks: None,
            video_adaptation: None,
            video_representation: None,
            audio_adaptation: None,
            audio_representation: None,

            video_ready,
            audio_ready,

            stop,

            start_time,

            video_renderer,
            audio_renderer,
        }
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
        self.tracks = Some(tracks);

        Ok(())
    }

    pub fn get_tracks(&mut self) -> Result<Tracks, Box<dyn Error>> {
        match &self.tracks {
            Some(success) => Ok(success.clone()),
            None => Err("No parsed tracks - player not prepared".into()),
        }
    }

    pub fn set_video_track(
        &mut self,
        adaptation: &VideoAdaptation,
        representation: &VideoRepresenation,
    ) {
        self.video_adaptation = Some(adaptation.clone());
        self.video_representation = Some(representation.clone());
    }

    pub fn set_audio_track(
        &mut self,
        adaptation: &AudioAdaptation,
        representation: &AudioRepresentation,
    ) {
        self.audio_adaptation = Some(adaptation.clone());
        self.audio_representation = Some(representation.clone());
    }

    async fn download_task(
        segments: Vec<Segment>,
        segment_sender: Sender<DataSegment>,
        stop: Arc<Notify>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let segment_slice = &segments[..];
        for i in 0..segment_slice.len() {
            let sender = segment_sender.clone();
            let seg = &segment_slice[i];
            tokio::select! {
                    _ = download_and_queue(i, seg, sender) => {
                        println!("Producing segment {}", i);
                    }
                _ = stop.notified() => {
                    break;
                }
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
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        while let Some(segment) = receiver.recv().await {
            println!("Consuming video segment: {}", segment.id);

            let mut data_vec = init_data.clone();
            data_vec.extend_from_slice(&segment.data[..]);

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
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        while let Some(segment) = receiver.recv().await {
            println!("Consuming audio segment: {}", segment.id);

            let mut data_vec = init_data.clone();
            data_vec.extend_from_slice(&segment.data[..]);

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

                let pts = sample.composition_timestamp * 1000 / (resampler.output().rate as i64);
                packet.set_pts(Some(pts));
                packet.set_time_base(Rational(1, sample.timescale as i32));
                packet.data_mut().unwrap().clone_from_slice(sample_data);

                if let Err(e) = decoder.send_packet(&packet) {
                    println!("Error sending packet to decoder: {:?}", e);
                    return Err(format!("Error sending packet to decoder: {:?}", e).into());
                }

                let mut frame = ffmpeg_next::util::frame::Audio::empty();
                let mut dst_frame = ffmpeg_next::util::frame::Audio::empty();

                audio_ready.notify_waiters();
                while let Ok(()) = decoder.receive_frame(&mut frame) {
                    _ = resampler.run(&frame, &mut dst_frame)?;
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
        video_ready: Arc<Notify>,
        sender: Sender<Arc<Video>>,
        stop: Arc<Notify>,
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

        unsafe {
            let mut hw_device_ctx: *mut AVBufferRef = std::ptr::null_mut();
            //let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2;
            let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;
            // Create the DXVA2 hardware device
            let ret = av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                device_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if ret < 0 {
                panic!("Failed to create DXVA2 hardware device: {}", ret);
            }

            // Assign the device context to the codec context
            let codec_ctx_ptr = decoder.as_mut_ptr();
            (*codec_ctx_ptr).hw_device_ctx = hw_device_ctx;

            println!("DXVA2 hardware device context created successfully.");
        }

        match track.trak(&mp4_info).mdia.minf.stbl.stsd.contents.clone() {
            StsdBoxContent::Hvc1(hvc) => {
                for nalus_unit in hvc.hvcc.arrays.clone() {
                    for nalu in nalus_unit.nalus {
                        let nalu_data = nalu.data;
                        let nalu = apped_hevc_header(nalu_data);

                        let mut packet = Packet::new(nalu.len());
                        packet.data_mut().unwrap().clone_from_slice(&nalu[..]);

                        decoder.send_packet(&packet).unwrap();
                    }
                }
            }
            StsdBoxContent::Hev1(hev) => {
                for nalus_unit in hev.hvcc.arrays.clone() {
                    for nalu in nalus_unit.nalus {
                        let nalu_data = nalu.data;
                        let nalu = apped_hevc_header(nalu_data);

                        let mut packet = Packet::new(nalu.len());
                        packet.data_mut().unwrap().clone_from_slice(&nalu[..]);
                        decoder.send_packet(&packet).unwrap();
                    }
                }
            }
            _ => {
                return Err("Codec not supported!".into());
            }
        };

        let video = video_representation;
        let segments = video.segments.clone();

        let download_task = task::spawn(Self::download_task(segments, download_tx, stop));

        let decoder_task = task::spawn(Self::video_decoder_task(
            download_rx,
            sender,
            decoder,
            init_data,
            video_ready,
        ));

        _ = join!(download_task, decoder_task);

        Ok(())
    }

    async fn audio_play(
        audio_representation: AudioRepresentation,
        audio_ready: Arc<Notify>,
        sender: Sender<Audio>,
        sample_rate: u32,
        stop: Arc<Notify>,
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

        let codec_id = ffmpeg_next::codec::Id::AAC;
        let codec = match ffmpeg_next::decoder::find(codec_id) {
            Some(codec) => codec,
            None => return Err("Cannot find codec for track".into()),
        };
        let mut decoder = match ffmpeg_next::codec::Context::new_with_codec(codec)
            .decoder()
            .audio()
        {
            Ok(context) => context,
            Err(e) => return Err(format!("Cannot find decoder for codec {}", e).into()),
        };

        let sample_rate_src = match track.trak(&mp4_info).mdia.minf.stbl.stsd.contents.clone() {
            StsdBoxContent::Mp4a(mp4a) => {
                let esds = mp4a.esds.unwrap();

                let frame_length = 1024;

                //let channel_config = esds.es_desc.dec_config.buffer_size_db;

                let profile = esds.es_desc.dec_config.dec_specific.profile;
                let sampling_frequency_index = esds.es_desc.dec_config.dec_specific.freq_index;
                let channel_config = esds.es_desc.dec_config.dec_specific.chan_conf;

                let mut header = [0u8; 8];

                //
                // Start constructing the ADTS header
                header[0] = 0xFF; // Fixed first byte
                header[1] = 0xF1; // MPEG-4 AAC, Layer 0
                                  // Byte 2: profile, sampling frequency index, and part of channel config
                                  // Adjust to ensure the result is 0x50
                header[2] = ((profile - 1) << 6) // Profile value (adjusted for correct byte)
                    | (sampling_frequency_index << 2) // Sampling frequency index
                    | ((channel_config & 0x4) >> 2); // Part of the channel config
                                                     // Byte 3: the remaining channel config and frame length (upper bits)
                                                     // Adjusted to match 0x80 in byte 3
                header[3] = ((channel_config & 0x3) << 6) | ((frame_length >> 11) as u8 & 0x03);
                // Byte 4: frame length (next 8 bits)
                header[4] = ((frame_length >> 3) & 0xFF) as u8;
                // Byte 5: frame length (remaining bits)
                header[5] = (((frame_length & 0x07) as u8) << 5) | 0x1F;
                // Byte 6: Buffer fullness (set to 0xFF for variable bitrate)
                header[6] = 0xFC; // Adjusted for the required value
                                  // Byte 8: Explicit END marker (distinct terminator)
                header[7] = 0xFF; // Explicit END marker

                let mut packet = Packet::new(header.len());
                packet.data_mut().unwrap().clone_from_slice(&header[..]);

                match decoder.send_packet(&packet) {
                    Ok(()) => {}
                    Err(e) => {
                        println!("Acc header error {}", e);
                    }
                };

                aac_sampling_frequency_index_to_u32(esds.es_desc.dec_config.dec_specific.freq_index)
            }
            _ => {
                return Err("Codec not supported!".into());
            }
        };

        let audio = audio_representation;
        let segments = audio.segments.clone();

        let download_task = task::spawn(Self::download_task(segments, download_tx, stop));

        let resampler = ffmpeg_next::software::resampling::Context::get(
            decoder.format(),
            decoder.channel_layout(),
            sample_rate_src,
            ffmpeg_next::util::format::sample::Sample::F32(Type::Packed),
            ffmpeg_next::util::channel_layout::ChannelLayout::STEREO,
            sample_rate,
        )?;

        let decoder_task = task::spawn(Self::audio_decoder_task(
            download_rx,
            sender,
            decoder,
            init_data,
            audio_ready,
            resampler,
        ));

        _ = join!(download_task, decoder_task);
        Ok(())
    }

    async fn lifetime_handler(
        mut start_time: Arc<Instant>,
        video_ready: Arc<Notify>,
        video_rx: mpsc::Receiver<Arc<Video>>,
        video_tx: Arc<VideoRenderer>,
        audio_ready: Arc<Notify>,
        audio_rx: mpsc::Receiver<Audio>,
        audio_tx: Arc<AudioRenderer>,
        stop: Arc<Notify>,
    ) {
        //loop {
        tokio::join!(video_ready.notified(), audio_ready.notified());
        start_time = Arc::new(Instant::now());
        tokio::select! {
            _ = tokio::spawn(video_sync_producer(start_time.clone(), video_rx, video_tx)) => {

            }
            _ = tokio::spawn(audio_sync_producer(start_time.clone(), audio_rx, audio_tx)) => { }
            /*_ = text_play => {
            }*/
        }
        stop.notify_waiters();
        //}
    }

    pub fn play(&mut self) -> Result<JoinHandle<()>, Box<dyn Error>> {
        let video_representation = match &self.video_representation {
            Some(success) => success.clone(),
            None => return Err("Video Track not set".into()),
        };

        let audio_representation = match &self.audio_representation {
            Some(success) => success.clone(),
            None => return Err("Audio Track not set".into()),
        };

        let start_time = self.start_time.clone();
        let video_ready = self.video_ready.clone();
        let audio_ready = self.audio_ready.clone();
        //let frame_consumer = self.frame_consumer.clone();
        //let sample_consumer = self.sample_consumer.clone();
        let stop = self.stop.clone();

        let video_renderer = self.video_renderer.clone();
        let audio_renderer = self.audio_renderer.clone();

        let play = tokio::spawn(async move {
            let (frame_sender, frame_receiver) = mpsc::channel::<Arc<Video>>(4);
            let (sample_sender, sample_receiver) = mpsc::channel::<Audio>(32);
            let play = tokio::spawn(Self::video_play(
                video_representation,
                video_ready.clone(),
                frame_sender,
                stop.clone(),
            ));

            let sample_rate = audio_renderer.sample_rate();

            let audio = tokio::spawn(Self::audio_play(
                audio_representation,
                audio_ready.clone(),
                sample_sender,
                sample_rate,
                stop.clone(),
            ));

            Self::lifetime_handler(
                start_time.clone(),
                video_ready.clone(),
                frame_receiver,
                video_renderer,
                audio_ready.clone(),
                sample_receiver,
                audio_renderer,
                stop.clone(),
            )
            .await;

            _ = join!(play, audio);
        });
        Ok(play)
    }

    pub async fn stop(&mut self) {
        self.audio_renderer.stop().await;
    }

    pub fn volume(&self, volume_diff: f32) {
        let audio_renderer = self.audio_renderer.clone();
        tokio::spawn(async move {
            audio_renderer.volume(volume_diff).await;
        });
    }
}

#[derive(Debug, Clone)]
struct DataSegment {
    id: usize,
    size: usize,   // Size in bytes
    data: Vec<u8>, // Simulated data
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
        eprintln!("Error: {:?}", e);
        return Err(format!("Error: {:?}", e).into());
    }

    Ok(())
}
