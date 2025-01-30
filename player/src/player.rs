mod manifest;
use std::error::Error;

use crate::manifest::Manifest;

pub struct Player {
    manifest: Option<Manifest>,
}

impl Player {
    pub fn new() -> Self {
        //Here i want pass texture and other device -> wgpu and cpal
        Player { manifest: None }
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
}
