mod manifest;
mod track_manager;
use std::error::Error;

use crate::manifest::Manifest;
use crate::track_manager::TrackManager;

pub struct Player<'a> {
    manifest: Option<Manifest>,
    track_manager: Option<TrackManager<'a>>,
}

impl<'a> Player<'a> {
    pub fn new() -> Self {
        //Here i want pass texture and other device -> wgpu and cpal
        Player {
            manifest: None,
            track_manager: None,
        }
    }

    pub fn open_url(&mut self, url: &str) -> Result<(), Box<dyn Error>> {
        let url = url.to_string();
        let mut manifest = Manifest::new(url);

        let download = manifest.download();
        let parse = match download {
            Ok(_) => manifest.parse(),
            Err(e) => {
                eprintln!("Manifest download failed: {}", e);
                return Err("Manifest download failed".into());
            }
        };

        match parse {
            Ok(_) => {
                self.manifest = Some(manifest);
            }
            Err(e) => {
                eprintln!("Manifest parsing failed");
                return Err(e);
            }
        };

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

        let mpd = match &manifest.manifest {
            Some(success) => success,
            None => {
                eprintln!("MPD not prepared!");
                return Err("MPD not loaded!".into());
            }
        };

        let track_manager = TrackManager::new(&mpd);
        self.track_manager = Some(track_manager);

        Ok(())
    }
}
