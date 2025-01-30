mod manifest;
use crate::manifest::Manifest;

pub struct Player {
    manifest: Option<Manifest>,
}

impl Player {
    pub fn new() -> Self {
        //Here i want pass texture and other device -> wgpu and cpal
        Player { manifest: None }
    }

    pub fn open_url(&mut self, url: &str) {
        let url = url.to_string();
        let mut manifest = Manifest::new(url);

        let download = manifest.download();
        match download {
            Ok(_) => {
                manifest.parse();
            }
            Err(_) => {
                eprintln!("Manifest download failed");
            }
        }

        self.manifest = Some(manifest);
    }

    
}
