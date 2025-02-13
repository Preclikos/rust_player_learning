use quick_xml::de::from_str;
use reqwest::{Client, Error};
use serde::Deserialize;

pub struct Manifest {
    url: String,
    content: String,
    pub mpd: MPD,
}

impl Manifest {
    pub async fn new(url: String) -> Result<Self, Box<dyn std::error::Error>> {
        let content = Self::download(&url).await?;
        let mpd = Self::parse(&content)?;

        Ok(Manifest {
            url: url,
            content: content,
            mpd: mpd,
        })
    }

    async fn download(url: &str) -> Result<String, Error> {
        let client = Client::new();

        let response = client.get(url).send().await;

        let manifest_content = match response {
            Ok(success) => success.text().await?,
            Err(e) => {
                println!("Manifest download failed: {}", e);
                return Err(e);
            }
        };

        Ok(manifest_content)
    }

    fn parse(content: &str) -> Result<MPD, Box<dyn std::error::Error>> {
        let mpd = match from_str::<MPD>(content) {
            Ok(mpd) => mpd,
            Err(e) => {
                eprintln!("Failed to parse MPD: {}", e);
                return Err("Failed to parse MPD:".into());
            }
        };
        Ok(mpd)
    }
}

#[derive(Deserialize, Clone)]
pub struct MPD {
    #[serde(rename = "@mediaPresentationDuration")]
    pub media_presentation_duration: String,

    #[serde(rename = "Period")]
    pub periods: Vec<Period>,
}

#[derive(Deserialize, Clone)]
pub struct Period {
    #[serde(rename = "AdaptationSet")]
    pub adaptation_sets: Vec<AdaptationSet>,
}

#[derive(Deserialize, Clone)]
pub struct AdaptationSet {
    #[serde(rename = "@id")]
    pub id: u32,
    #[serde(rename = "@contentType")]
    pub content_type: String,
    #[serde(rename = "@subsegmentAlignment")]
    pub subsegment_alignment: Option<bool>,

    #[serde(rename = "@maxWidth")]
    pub max_width: Option<u32>,
    #[serde(rename = "@maxHeight")]
    pub max_height: Option<u32>,
    #[serde(rename = "@frameRate")]
    pub frame_rate: Option<String>,
    #[serde(rename = "@par")]
    pub par: Option<String>,

    #[serde(rename = "@lang")]
    pub lang: Option<String>,

    #[serde(rename = "Representation")]
    pub representations: Vec<Representation>,
}

#[derive(Deserialize, Clone)]
pub struct Representation {
    #[serde(rename = "@id")]
    pub id: u32,
    #[serde(rename = "@bandwidth")]
    pub bandwidth: u64,
    #[serde(rename = "@mimeType")]
    pub mime_type: String,

    #[serde(rename = "@codecs")]
    pub codecs: Option<String>,

    #[serde(rename = "@width")]
    pub width: Option<u32>,
    #[serde(rename = "@height")]
    pub height: Option<u32>,
    #[serde(rename = "@frameRate")]
    pub frame_rate: Option<u32>,
    #[serde(rename = "@sar")]
    pub sar: Option<String>,

    #[serde(rename = "@audioSamplingRate")]
    pub audio_sampling_rate: Option<u32>,

    #[serde(rename = "BaseURL")]
    pub base_url: BaseURL,
    #[serde(rename = "SegmentBase")]
    pub segment_base: Option<SegmentBase>,
}

#[derive(Deserialize, Clone)]
pub struct BaseURL {
    #[serde(rename = "$text")]
    pub value: String,
}

#[derive(Deserialize, Clone)]
pub struct SegmentBase {
    #[serde(rename = "@indexRange")]
    pub index_range: String,

    #[serde(rename = "Initialization")]
    pub initialization: Initialization,
}

#[derive(Deserialize, Clone)]
pub struct Initialization {
    #[serde(rename = "@range")]
    pub range: String,
}
