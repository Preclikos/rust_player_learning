mod manifest;
mod networking;
mod parsers;
mod tracks;
mod utils;

use ffmpeg_next::Rational;
use ffmpeg_next::{codec::Context, Frame, Packet};
use re_mp4::{Mp4, StsdBoxContent};
use std::error::Error;
use tokio::sync::Notify;
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
use networking::HttpClient;

const MAX_SEGMENTS: usize = 2;
pub struct Player {
    http_client: HttpClient,
    base_url: Option<String>,
    manifest: Option<Manifest>,
    tracks: Option<Tracks>,

    video_adaptation: Option<VideoAdaptation>,
    video_representation: Option<VideoRepresenation>,
}

impl Player {
    pub fn new() -> Self {
        //Here i want pass texture and other device -> wgpu and cpal

        let client = HttpClient::new();
        Player {
            http_client: client,
            base_url: None,
            manifest: None,
            tracks: None,
            video_adaptation: None,
            video_representation: None,
        }
    }

    fn parse_base_url(full_url: &str) -> Result<String, Box<dyn Error>> {
        let mut url = Url::parse(full_url)?;

        url.path_segments_mut()
            .expect("Cannot modify path segments")
            .pop();

        return Ok(url.to_string() + "/");
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

    fn parse_h265_nals(sample_data: &[u8]) {
        let mut pos = 0;
        while pos + 4 <= sample_data.len() {
            // Read 4 bytes for the NAL unit length.
            let nal_length = u32::from_be_bytes([
                sample_data[pos],
                sample_data[pos + 1],
                sample_data[pos + 2],
                sample_data[pos + 3],
            ]) as usize;
            pos += 4;

            if pos + nal_length > sample_data.len() {
                eprintln!(
                    "Incomplete NAL unit: expected {} bytes, but only {} available.",
                    nal_length,
                    sample_data.len() - pos
                );
                break;
            }

            let nal_unit = &sample_data[pos..pos + nal_length];
            println!("Found H.265 NAL unit (size: {} bytes)", nal_unit.len());

            // (Optional) Parse the NAL header.
            if nal_unit.len() >= 2 {
                // For H.265, the NAL header is typically 2 bytes; bits 9..14 (from a big-endian 16-bit value)
                let nal_header = u16::from_be_bytes([nal_unit[0], nal_unit[1]]);
                let nal_type = (nal_header >> 9) & 0x3F;
                println!("  NAL type: {}", nal_type);
            }

            pos += nal_length;
        }
    }

    pub async fn play(&mut self) -> Result<(), Box<dyn Error>> {
        let (download_tx, mut download_rx) = mpsc::channel::<DataSegment>(MAX_SEGMENTS);
        let stop = Arc::new(Notify::new());

        let video_representation = match &self.video_representation {
            Some(success) => success.clone(),
            None => return Err("Video Track not set".into()),
        };

        let init_data = video_representation.segment_init.download().await?;

        let mp4_init = Mp4::read_bytes(&init_data[..]);
        let unwrap = mp4_init.unwrap();

        let codec_id = ffmpeg_next::codec::Id::HEVC;
        let codec = ffmpeg_next::decoder::find(codec_id).unwrap();
        let mut decoder = Context::new_with_codec(codec).decoder().video().unwrap();
        let nalu_header: Vec<u8> = vec![0x00, 0x00, 0x00, 0x01];

        match unwrap
            .moov
            .traks
            .first()
            .unwrap()
            .mdia
            .minf
            .stbl
            .stsd
            .contents
            .clone()
        {
            StsdBoxContent::Hvc1(hvc) => {
                println!("Hvc1");
                for nalus_unit in hvc.hvcc.arrays.clone() {
                    for nalu in nalus_unit.nalus {
                        let mut full_nalu = nalu_header.clone();
                        let mut data = nalu.data.clone();
                        full_nalu.append(&mut data);

                        let mut packet = Packet::new(full_nalu.len());
                        packet.data_mut().unwrap().clone_from_slice(&full_nalu[..]);

                        decoder.send_packet(&packet).unwrap();
                    }
                }
            }
            StsdBoxContent::Hev1(hev) => {
                println!("Hev1");
                for nalus_unit in hev.hvcc.arrays.clone() {
                    for nalu in nalus_unit.nalus {
                        let mut full_nalu = nalu_header.clone();
                        let mut data = nalu.data.clone();
                        full_nalu.append(&mut data);

                        let mut packet = Packet::new(full_nalu.len());
                        packet.data_mut().unwrap().clone_from_slice(&full_nalu[..]);

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

        let mut conter = 0;

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

                for (_track_id, track) in mp4.tracks() {
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

                        let mut index = 0;

                        while index < sample_data.len() {
                            let byte_array: [u8; 4] = sample_data[index..index + 4]
                                .try_into()
                                .expect("Failed to convert");
                            let length_u32 = u32::from_be_bytes(byte_array);
                            let length = usize::try_from(length_u32).unwrap();

                            index += 4;

                            if index + length > sample_data.len() {
                                panic!("Invalid length: Not enough bytes in the vector");
                            }

                            // Read the next `length` bytes into a separate vector
                            let chunk: Vec<u8> = sample_data[index..index + length].to_vec();
                            let mut chunk_mut = chunk.clone();
                            index += length;

                            let mut full_nalu = nalu_header.clone();
                            full_nalu.append(&mut chunk_mut);

                            let mut packet = Packet::new(full_nalu.len());

                            packet.set_pts(Some(sample.composition_timestamp));
                            packet.set_time_base(Rational(1, sample.timescale as i32));

                            packet.data_mut().unwrap().clone_from_slice(&full_nalu[..]);

                            if let Err(e) = decoder.send_packet(&packet) {
                                println!("Error sending packet: {:?}", e);
                            }
                        }

                        let mut frame = unsafe { Frame::empty() };
                        while let Ok(()) = decoder.receive_frame(&mut frame) {
                            conter += 1;
                            let frame_pts = frame.pts().unwrap();
                            println!(
                                "Decoded frame: {:?} key: {} pts: {}",
                                conter,
                                frame.is_key(),
                                frame_pts
                            );
                        }
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
    let downloaded_data = segment.download().await.unwrap();

    let data_segment = DataSegment {
        id: index,
        size: downloaded_data.len(),
        data: downloaded_data,
    };

    if let Err(e) = sender.send(data_segment).await {
        eprintln!("Error: {:?}", e);
    }

    Ok(())
}
