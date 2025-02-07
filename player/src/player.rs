mod manifest;
mod networking;
mod parsers;
mod tracks;
mod utils;

use re_mp4::Mp4;
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
                println!("Consuming segment and push it frame");

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

                for track in mp4.tracks() {
                    println!("--- Track id: {} ---", track.0);
                    // For each sample in the track, print its offset and size.
                    // (The sample data is stored in the mdat box.)
                    let samples = &track.1.samples;

                    for sample in samples {
                        let mut cursor = Cursor::new(&data_vec);

                        println!("Sample: offset {} size {}", sample.offset, sample.size);

                        // Read the sample's raw data from our in-memory vector.
                        let mut sample_data = vec![0u8; sample.size as usize];
                        // Seek to the beginning of the sample in the vector.
                        _ = cursor.seek(SeekFrom::Start(sample.offset));
                        // Read the sample data.
                        _ = cursor.read_exact(&mut sample_data);

                        let hex_string: String = sample_data[1..8]
                            .iter()
                            .map(|b| format!("{:02X}", b))
                            .collect::<Vec<_>>()
                            .join(" ");
                        println!("{}", hex_string);

                        // Now `sample_data` holds the raw bytes from the sample (typically stored in the mdat box).
                        // You can now pass these bytes to your decoder or reassembly logic.
                        println!("Read {} bytes of sample data.", sample_data.len());
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
