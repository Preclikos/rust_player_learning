mod manifest;
mod networking;
mod parsers;
mod tracks;
mod utils;

use std::error::Error;
use tracks::Tracks;
use url::Url;

use manifest::Manifest;
use networking::HttpClient;

pub struct Player {
    http_client: HttpClient,
    base_url: Option<String>,
    manifest: Option<Manifest>,
    tracks: Option<Tracks>,
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

        let base_url = match &self.base_url {
            Some(success) => success.to_string(),
            None => {
                eprintln!("BaseUrl not loaded!");
                return Err("BaseUrl not loaded!".into());
            }
        };

        let tracks = Tracks::new(base_url, &manifest.mpd)?;
        self.tracks = Some(tracks);

        Ok(())
    }
}
