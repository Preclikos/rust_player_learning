use quick_xml::de::from_str;
use reqwest::{blocking::get, Error};
use serde::Deserialize;

pub struct Manifest {
    url: String,
    body: Option<String>,
    manifest: Option<MPD>,
}

impl Manifest {
    pub fn new(url: String) -> Self {
        Manifest {
            url: url,
            body: None,
            manifest: None,
        }
    }

    pub fn download(&mut self) -> Result<(), Error> {
        let response = get(&self.url);

        match response {
            Ok(success) => {
                let body: String = success.text()?;
                self.body = Some(body);
                Ok(())
            }
            Err(e) => {
                println!("Manifest download failed: {}", e);
                return Err(e);
            }
        }
    }

    pub fn parse(&mut self) {
        match &self.body {
            Some(body) => match from_str::<MPD>(body.as_str()) {
                Ok(mpd) => {
                    println!("Parsed MPD: {:#?}", mpd);
                    self.manifest = Some(mpd)
                }
                Err(e) => {
                    eprintln!("Failed to parse MPD: {}", e);
                }
            },
            None => {
                eprintln!("Failed to parse empty Manifest");
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MPD {
    #[serde(rename = "Period")]
    periods: Vec<Period>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Period {
    #[serde(rename = "AdaptationSet")]
    adaptation_sets: Vec<AdaptationSet>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdaptationSet {
    #[serde(rename = "Representation")]
    representations: Vec<Representation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Representation {
    #[serde(rename = "@id")]
    id: String,
    #[serde(rename = "@bandwidth")]
    bandwidth: u64,
    #[serde(rename = "@mimeType")]
    mime_type: String,
}
