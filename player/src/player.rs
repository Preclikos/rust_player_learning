mod manifest;
mod track_manager;
mod tracks;
use std::error::Error;
use url::Url;

use crate::manifest::Manifest;
use crate::track_manager::TrackManager;

pub struct Player<'a> {
    base_url: Option<String>,
    manifest: Option<Manifest>,
    track_manager: Option<TrackManager<'a>>,
}

impl<'a> Player<'a> {
    pub fn new() -> Self {
        //Here i want pass texture and other device -> wgpu and cpal
        Player {
            base_url: None,
            manifest: None,
            track_manager: None,
        }
    }

    fn parse_base_url(full_url: &str) -> String {
        let mut url = Url::parse(full_url).expect("Invalid URL");

        url.path_segments_mut()
            .expect("Cannot modify path segments")
            .pop();

        return url.to_string() + "/";
    }

    pub fn open_url(&mut self, url: &str) -> Result<(), Box<dyn Error>> {
        let base_url = Self::parse_base_url(url);
        self.base_url = Some(base_url);

        let url = url.to_string();
        let manifest = Manifest::new(url)?;
        self.manifest = Some(manifest);

        Ok(())
    }

    pub fn prepare(&'a mut self) -> Result<(), Box<dyn Error>> {
        let manifest = match &self.manifest {
            Some(success) => success,
            None => {
                eprintln!("Manifest not loaded!");
                return Err("Manifest not loaded!".into());
            }
        };

        let track_manager = TrackManager::new(&manifest.mpd)?;
        self.track_manager = Some(track_manager);

        Ok(())
    }
}
