use quick_xml::de::from_str;
use serde::Deserialize;

use crate::net::{HttpClient, RequestKind};

#[derive(Clone)]
pub struct Manifest {
    pub url: String,
    pub content: String,
    pub mpd: MPD,
}

impl Manifest {
    pub async fn new(
        url: String,
        http: &HttpClient,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let content = http
            .get_text(url.clone(), RequestKind::Manifest)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> {
                format!("manifest download: {}", e).into()
            })?;
        let mpd = Self::parse(&content)?;
        Ok(Manifest { url, content, mpd })
    }

    fn parse(content: &str) -> Result<MPD, Box<dyn std::error::Error>> {
        from_str::<MPD>(content).map_err(|e| -> Box<dyn std::error::Error> {
            eprintln!("Failed to parse MPD: {}", e);
            "Failed to parse MPD".into()
        })
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

    /// DASH `Role` element — `urn:mpeg:dash:role:2011` `value` attribute
    /// (e.g. "main", "dub", "commentary"). Multiple roles per track are
    /// allowed by the spec; we keep the lot.
    #[serde(rename = "Role", default)]
    pub roles: Vec<Property>,

    /// Optional `SupplementalProperty` / `EssentialProperty` elements
    /// (HDR colour primaries, Dolby Vision profile, etc.). Used by
    /// `TrackInfo::hdr10` / `dolby_vision` heuristics.
    #[serde(rename = "SupplementalProperty", default)]
    pub supplemental_properties: Vec<Property>,
    #[serde(rename = "EssentialProperty", default)]
    pub essential_properties: Vec<Property>,

    #[serde(rename = "Representation")]
    pub representations: Vec<Representation>,
}

/// DASH `SupplementalProperty` / `EssentialProperty` / `Role` element.
/// Identified by `schemeIdUri`; optional `value` carries the payload.
#[derive(Deserialize, Clone, Debug)]
pub struct Property {
    #[serde(rename = "@schemeIdUri")]
    pub scheme_id_uri: String,
    #[serde(rename = "@value")]
    pub value: Option<String>,
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
    pub frame_rate: Option<String>,
    #[serde(rename = "@sar")]
    pub sar: Option<String>,

    #[serde(rename = "@audioSamplingRate")]
    pub audio_sampling_rate: Option<u32>,

    #[serde(rename = "@audioChannelConfiguration")]
    pub audio_channel_config: Option<String>,

    #[serde(rename = "BaseURL")]
    pub base_url: BaseURL,
    #[serde(rename = "SegmentBase")]
    pub segment_base: Option<SegmentBase>,

    #[serde(rename = "SupplementalProperty", default)]
    pub supplemental_properties: Vec<Property>,
    #[serde(rename = "EssentialProperty", default)]
    pub essential_properties: Vec<Property>,
    #[serde(rename = "AudioChannelConfiguration", default)]
    pub audio_channel_configurations: Vec<Property>,
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
