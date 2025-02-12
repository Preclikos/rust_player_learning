mod manifest;
mod networking;
mod parsers;
mod tracks;
mod utils;

use ffmpeg_next::frame::Video;
use ffmpeg_next::Rational;
use ffmpeg_next::{codec::Context, Packet};
use parsers::mp4::{apped_hevc_header, parse_hevc_nalu};
use re_mp4::{Mp4, StsdBoxContent};
use std::error::Error;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::sleep;
use tokio::{join, sync::mpsc::Sender};
use tracks::{
    segment::Segment,
    video::{VideoAdaptation, VideoRepresenation},
    Tracks,
};
use url::Url;

use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task;

use manifest::Manifest;

const MAX_SEGMENTS: usize = 2;

pub struct Player {
    base_url: Option<String>,
    manifest: Option<Manifest>,
    tracks: Option<Tracks>,

    video_adaptation: Option<VideoAdaptation>,
    video_representation: Option<VideoRepresenation>,

    frame_sender: Option<Sender<Video>>,
}

impl Player {
    pub fn new(sender: Sender<Video>) -> Self {
        //Here i want pass texture and other device -> wgpu and cpal
        Player {
            base_url: None,
            manifest: None,
            tracks: None,
            video_adaptation: None,
            video_representation: None,
            frame_sender: Some(sender),
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

    pub async fn play(&mut self) -> Result<(), Box<dyn Error>> {
        let (download_tx, mut download_rx) = mpsc::channel::<DataSegment>(MAX_SEGMENTS);
        let stop = Arc::new(Notify::new());

        let video_representation = match &self.video_representation {
            Some(success) => success.clone(),
            None => return Err("Video Track not set".into()),
        };

        let init_data = video_representation.segment_init.download().await?;

        let mp4_info = match Mp4::read_bytes(&init_data[..]) {
            Ok(success) => success,
            Err(e) => return Err(format!("Error parsing mp4 Init {}", e).into()),
        };

        let (_track_id, track) = mp4_info.tracks().first_key_value().unwrap();

        let codec_id = ffmpeg_next::codec::Id::HEVC;
        let codec = ffmpeg_next::decoder::find(codec_id).unwrap();
        let mut decoder = Context::new_with_codec(codec).decoder().video().unwrap();

        match track.trak(&mp4_info).mdia.minf.stbl.stsd.contents.clone() {
            StsdBoxContent::Hvc1(hvc) => {
                println!("Hvc1");
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
                println!("Hev1");
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
                println!("WTF");
                return Err("Codec not supported!".into());
            }
        };

        let video = video_representation;
        let segments = video.segments.clone();

        let download_task = task::spawn(async move {
            let segment_slice = &segments[..];
            for i in 0..segment_slice.len() {
                let seg = &segment_slice[i];
                let sender = download_tx.clone();
                tokio::select! {
                        _ = download_and_queue(i, seg, sender) => {
                            println!("Producing segment {}", i);
                        }
                    _ = stop.notified() => {
                        break;
                    }
                }
            }
        });

        let sender = self.frame_sender.clone().unwrap();

        let decoder_task = task::spawn(async move {
            while let Some(segment) = download_rx.recv().await {
                println!("Consuming segment: {}", segment.id);
                let mut data_vec = init_data.clone();
                data_vec.extend_from_slice(&segment.data.clone());

                let data = &data_vec[..];

                let mp4 = match Mp4::read_bytes(data) {
                    Ok(success) => success,
                    Err(e) => {
                        println!("Parsing error {}", e);
                        break;
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

                        packet.set_pts(Some(sample.composition_timestamp));
                        packet.set_time_base(Rational(1, sample.timescale as i32));

                        packet.data_mut().unwrap().clone_from_slice(&nalu[..]);

                        if let Err(e) = decoder.send_packet(&packet) {
                            println!("Error sending packet: {:?}", e);
                        }
                    }

                    let mut frame = ffmpeg_next::util::frame::Video::empty();
                    while let Ok(()) = decoder.receive_frame(&mut frame) {
                        _ = sender.send(frame.clone()).await;
                    }
                }
            }
        });

        _ = join!(download_task);
        _ = join!(decoder_task);

        Ok(())
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
