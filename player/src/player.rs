mod manifest;
mod networking;
mod parsers;
mod tracks;
mod utils;

use re_mp4::{Mp4, Mp4Box, Mp4Sample};
use std::error::Error;
use std::io::{Cursor, Read, Seek, SeekFrom};
use tokio::join;
use tokio::sync::Notify;
use tracks::{
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
        let (frame_tx, mut frame_rx) = mpsc::channel::<DataSegment>(MAX_SEGMENTS);
        let stop = Arc::new(Notify::new());

        let video_representation = match &self.video_representation {
            Some(success) => success.clone(),
            None => return Err("Video Track not set".into()),
        };

        let init_data = video_representation.segment_init.download().await?;
        /*
                let init_bytes = &init_data[..];
                let mp4 = Mp4::read_bytes(init_bytes)?;

                let tracks = mp4.tracks().len();
                println!("Current tracks in MP4 {}", tracks);
        */

        let video = video_representation;
        let segments = video.segments.clone();

        let download_task = task::spawn(async move {
            let segment_slice = &segments[..];
            for i in 0..segment_slice.len() {
                let seg = &segment_slice[i];
                tokio::select! { data = seg.download() => {
                       let downloaded_data = data.unwrap();

                        let data_segment = DataSegment {
                            id: i,
                            size: downloaded_data.len(),
                            data: downloaded_data,
                        };

                        match download_tx.try_send(data_segment) {
                            Ok(()) => println!("Produced segment {}", i),
                            Err(e) => {
                                eprintln!("Failed to send segment {}: {}", i, e);
                                break;
                            },
                        }
                    }

                    _ = stop.notified() => {
                        break;
                    }
                }
            }
        });

        let decoder_task = task::spawn(async move {
            while let Some(segment) = download_rx.recv().await {
                let mut data_vec = init_data.clone();
                data_vec.extend_from_slice(&segment.data);

                let data = &data_vec[..];

                let mp4 = match Mp4::read_bytes(data) {
                    Ok(success) => success,
                    Err(e) => {
                        println!("Parsing error {}", e);
                        break;
                    }
                };

                let segment_data = &segment.data[..];

                for (track_id, track) in mp4.tracks() {
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
                        println!(
                            "Processing sample: offset {} size {} bytes",
                            sample_offset, sample_size
                        );
                    }
                }

                //frame_tx.send(segment).await.unwrap();
                // Simulate processing*/
            }
        });

        let read_frame_task = task::spawn(async move {
            while let Some(segment) = frame_rx.recv().await {
                println!(
                    "Consuming frame {} (size: {} bytes)",
                    segment.id, segment.size
                );
                // Simulate processing
            }
        });

        //stoping.clone().store(true, Ordering::Relaxed);
        _ = join!(download_task);
        _ = join!(decoder_task);
        //read_frame_task.await?;

        Ok(())
    }
}

#[derive(Debug)]
struct DataSegment {
    id: usize,
    size: usize,   // Size in bytes
    data: Vec<u8>, // Simulated data
}
