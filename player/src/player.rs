mod manifest;
mod tracks;
use std::error::Error;
use tracks::Tracks;
use url::Url;

use manifest::Manifest;

pub struct Player {
    base_url: Option<String>,
    manifest: Option<Manifest>,
    tracks: Option<Tracks>,
}

impl Player {
    pub fn new() -> Self {
        //Here i want pass texture and other device -> wgpu and cpal
        Player {
            base_url: None,
            manifest: None,
            tracks: None,
        }
    }

    fn parse_base_url(full_url: &str) -> Result<String, Box<dyn Error>> {
        let mut url = Url::parse(full_url)?;

        url.path_segments_mut()
            .expect("Cannot modify path segments")
            .pop();

        return Ok(url.to_string() + "/");
    }

    pub fn open_url(&mut self, url: &str) -> Result<(), Box<dyn Error>> {
        let base_url = Self::parse_base_url(url)?;
        self.base_url = Some(base_url);

        let url = url.to_string();
        let manifest = Manifest::new(url)?;
        self.manifest = Some(manifest);

        Ok(())
    }

    pub fn prepare(mut self) -> Result<(), Box<dyn Error>> {
        let manifest = match &self.manifest {
            Some(success) => success,
            None => {
                eprintln!("Manifest not loaded!");
                return Err("Manifest not loaded!".into());
            }
        };

        let track_manager = Tracks::new(&manifest.mpd)?;
        self.tracks = Some(track_manager);

        Ok(())
    }
}
