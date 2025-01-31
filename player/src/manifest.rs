use quick_xml::de::from_str;
use reqwest::{blocking::get, Error};
use serde::Deserialize;

pub struct Manifest {
    url: String,
    body: Option<String>,
    pub manifest: Option<MPD>,
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
                return Err("Failed to parse empty Manifest".into());
            }
        };

        let mpd = match from_str::<MPD>(manifest) {
            Ok(mpd) => mpd,
            Err(e) => {
                eprintln!("Failed to parse MPD: {}", e);
                return Err("Failed to parse MPD:".into());
            }
        };

        //println!("Parsed MPD: {:#?}", mpd);
        self.manifest = Some(mpd);

        Ok(())
    }
}

#[derive(Deserialize, Clone)]
pub struct MPD {
    #[serde(rename = "Period")]
    pub periods: Vec<Period>,
}

#[derive(Deserialize, Clone)]
pub struct Period {
    #[serde(rename = "@duration")]
    pub duration: String,

    #[serde(rename = "AdaptationSet")]
    pub adaptation_sets: Vec<AdaptationSet>,
}

#[derive(Deserialize, Clone)]
struct AdaptationSet {
    #[serde(rename = "@maxWidth")]
    pub max_width: Option<i32>,
    #[serde(rename = "@maxHeight")]
    pub max_height: Option<i32>,

    #[serde(rename = "Representation")]
    pub representations: Vec<Representation>,
}

#[derive(Deserialize, Clone)]
struct Representation {
    #[serde(rename = "@id")]
    pub id: String,
    #[serde(rename = "@bandwidth")]
    pub bandwidth: u64,
    #[serde(rename = "@mimeType")]
    pub mime_type: String,

    #[serde(rename = "@codecs")]
    pub codecs: Option<String>,

    #[serde(rename = "@width")]
    pub width: Option<i32>,
    #[serde(rename = "@height")]
    pub height: Option<i32>,
    #[serde(rename = "@frameRate")]
    pub frame_rate: Option<i32>,

    #[serde(rename = "BaseURL")]
    pub base_url: BaseURL,
    #[serde(rename = "SegmentBase")]
    pub segment_base: SegmentBase,
}

#[derive(Deserialize, Clone)]
struct BaseURL {
    #[serde(rename = "$text")]
    pub value: String,
}

#[derive(Deserialize, Clone)]
struct SegmentBase {
    #[serde(rename = "@indexRangeExact")]
    pub index_range_exact: bool,
    #[serde(rename = "@indexRange")]
    pub index_range: String,

    #[serde(rename = "Initialization")]
    pub initialization: Initialization,
}

#[derive(Deserialize, Clone)]
struct Initialization {
    #[serde(rename = "@range")]
    pub range: String,
}
