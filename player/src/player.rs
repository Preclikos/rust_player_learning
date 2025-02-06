mod manifest;
mod networking;
mod parsers;
mod tracks;
mod utils;

use quick_xml::se;
use std::{
    error::Error,
    sync::atomic::{AtomicBool, Ordering},
};
use tracks::{
    audio::AudioAdaptation,
    video::{VideoAdaptation, VideoRepresenation},
    Tracks,
};
use url::Url;

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task;
use tokio::time::{sleep, Duration};

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
        let stoping = Arc::new(AtomicBool::new(false));

        let (download_tx, mut download_rx) = mpsc::channel::<DataSegment>(2);
        let (frame_tx, mut frame_rx) = mpsc::channel::<DataSegment>(2);

        let video_representation = match &self.video_representation {
            Some(success) => success,
            None => return Err("Video Track not set".into()),
        };

        let init_data = video_representation.segment_init.download().await?;

        let video = video_representation;
        let segments = video.segments.clone();

        let stopp = stoping.clone();
        let download_task = task::spawn(async move {
            let segment_slice = &segments[..];
            for i in 0..segment_slice.len() {
                if stopp.load(Ordering::Relaxed) {
                    print!("Stopping producer");
                    break;
                }

                let seg = &segment_slice[i];

                let data = seg.download().await.unwrap();

                let data_segment = DataSegment {
                    id: i,
                    size: data.len(),
                    data,
                };

                download_tx.send(data_segment).await.unwrap();
                println!("Produced segment {}", i);
            }
            drop(download_tx);
        });

        let decoder_task = task::spawn(async move {
            while let Some(segment) = download_rx.recv().await {
                println!("Consuming segment and push it frame");
                frame_tx.send(segment).await.unwrap();
                // Simulate processing
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
        sleep(Duration::from_millis(2000)).await;

        //stoping.clone().store(true, Ordering::Relaxed);

        download_task.await?;
        decoder_task.await?;
        read_frame_task.await?;

        Ok(())
    }
}

#[derive(Debug)]
struct DataSegment {
    id: usize,
    size: usize,   // Size in bytes
    data: Vec<u8>, // Simulated data
}
