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

        let manifest_content = match response {
            Ok(success) => success.text()?,
            Err(e) => {
                println!("Manifest download failed: {}", e);
                return Err(e);
            }
        };

        self.body = Some(manifest_content);
        Ok(())
    }

    pub fn parse(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let manifest = match &self.body {
            Some(body) => body.as_str(),
            None => {
                eprintln!("Failed to parse empty Manifest");
                return Err("".into());
            }
        };

        let mpd = match from_str::<MPD>(manifest) {
            Ok(mpd) => mpd,
            Err(e) => {
                eprintln!("Failed to parse MPD: {}", e);
                return Err("Failed to parse MPD:".into());
            }
        };

        println!("Parsed MPD: {:#?}", mpd);
        self.manifest = Some(mpd);

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct MPD {
    #[serde(rename = "Period")]
    periods: Vec<Period>,
}

#[derive(Debug, Deserialize)]
struct Period {
    #[serde(rename = "@duration")]
    duration: String,

    #[serde(rename = "AdaptationSet")]
    adaptation_sets: Vec<AdaptationSet>,
}

#[derive(Debug, Deserialize)]
struct AdaptationSet {
    #[serde(rename = "@maxWidth")]
    max_width: Option<i32>,
    #[serde(rename = "@maxHeight")]
    max_height: Option<i32>,

    #[serde(rename = "Representation")]
    representations: Vec<Representation>,
}

#[derive(Debug, Deserialize)]
struct Representation {
    #[serde(rename = "@id")]
    id: String,
    #[serde(rename = "@bandwidth")]
    bandwidth: u64,
    #[serde(rename = "@mimeType")]
    mime_type: String,

    #[serde(rename = "@codecs")]
    codecs: Option<String>,

    #[serde(rename = "@width")]
    width: Option<i32>,
    #[serde(rename = "@height")]
    height: Option<i32>,
    #[serde(rename = "@frameRate")]
    frameRate: Option<i32>,

    #[serde(rename = "BaseURL")]
    base_url: BaseURL,
    #[serde(rename = "SegmentBase")]
    segment_base: SegmentBase,
}

#[derive(Debug, Deserialize)]
struct BaseURL {
    #[serde(rename = "$text")]
    value: String,
}

#[derive(Debug, Deserialize)]
struct SegmentBase {
    #[serde(rename = "@indexRangeExact")]
    indexRangeExact: bool,
    #[serde(rename = "@indexRange")]
    indexRange: String,

    #[serde(rename = "Initialization")]
    initialization: Initialization,
}

#[derive(Debug, Deserialize)]
struct Initialization {
    #[serde(rename = "@range")]
    range: String,
}
