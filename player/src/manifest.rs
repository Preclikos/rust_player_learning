use quick_xml::de::from_str;
use serde::Deserialize;

use crate::net::{HttpClient, RequestKind};

#[derive(Clone)]
pub struct Manifest {
    pub content: String,
    pub mpd: MPD,
}

impl Manifest {
    pub async fn new(
        url: String,
        http: &HttpClient,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let content = http
            .get_text(url, RequestKind::Manifest)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> {
                format!("manifest download: {}", e).into()
            })?;
        let mpd = Self::parse(&content)?;
        Ok(Manifest { content, mpd })
    }

    fn parse(content: &str) -> Result<MPD, Box<dyn std::error::Error>> {
        from_str::<MPD>(content).map_err(|e| -> Box<dyn std::error::Error> {
            log::error!("Failed to parse MPD: {}", e);
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

    // NOTE: SupplementalProperty / EssentialProperty are deliberately NOT
    // parsed here. quick-xml's serde adapter requires repeating elements
    // (Vec<T>) to be CONTIGUOUS in the XML; real DASH manifests interleave
    // SupplementalProperty with ContentProtection / Role / Representation
    // and the parser blows up with "duplicate field SupplementalProperty".
    // HDR/DV detection therefore relies on a post-hoc raw-XML scan
    // (`manifest::extract_property_values`) at track-build time, not on
    // serde fields. See VideoRepresenation::is_hdr10 / is_dolby_vision.

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

    // SupplementalProperty / EssentialProperty / AudioChannelConfiguration
    // at Representation level have the same interleaving problem as on
    // AdaptationSet. We pull them out of the raw XML in
    // `manifest::extract_property_values`.
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

// ---------------------------------------------------------------------------
// Raw-XML helpers — work around quick-xml's "duplicate field" error on
// interleaved repeating elements. We slice out the substring for a given
// AdaptationSet / Representation block from `Manifest::content` and walk
// it for the specific descriptors we care about. Light string search, no
// re-parse — we already validated structure via serde.
// ---------------------------------------------------------------------------

/// Slice the substring `<AdaptationSet id="adaptation_id" ...>...</AdaptationSet>`
/// out of the raw MPD content. Returns `None` if no AdaptationSet with
/// that id is found (e.g. content_type mismatch in caller's mental model).
pub fn slice_adaptation_set<'a>(content: &'a str, adaptation_id: u32) -> Option<&'a str> {
    let needle = format!("<AdaptationSet id=\"{}\"", adaptation_id);
    let start = content.find(&needle)?;
    let rest = &content[start..];
    let end = rest.find("</AdaptationSet>")?;
    Some(&rest[..end])
}

/// Slice the substring `<Representation id="rep_id" ...>...</Representation>`
/// out of the given block (typically the result of `slice_adaptation_set`).
pub fn slice_representation<'a>(block: &'a str, rep_id: u32) -> Option<&'a str> {
    let needle = format!("<Representation id=\"{}\"", rep_id);
    let start = block.find(&needle)?;
    let rest = &block[start..];
    let end = rest.find("</Representation>")?;
    Some(&rest[..end])
}

/// Find all `<SupplementalProperty|EssentialProperty>` elements in
/// `block` whose `schemeIdUri` contains `scheme_substring`. Returns the
/// `value` attribute of each match.
pub fn find_descriptor_values(block: &str, scheme_substring: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = block;
    while let Some(open) = cursor.find('<') {
        let after = &cursor[open + 1..];
        let tag_end = match after.find('>') {
            Some(i) => i,
            None => break,
        };
        let tag = &after[..tag_end];
        // We only care about *Property elements (Supplemental / Essential).
        let is_prop = tag.starts_with("SupplementalProperty")
            || tag.starts_with("EssentialProperty");
        if is_prop && tag.contains(scheme_substring) {
            // Pull out the value="..." attribute if present.
            if let Some(val_start) = tag.find("value=\"") {
                let rest = &tag[val_start + 7..];
                if let Some(val_end) = rest.find('"') {
                    out.push(rest[..val_end].to_string());
                }
            }
        }
        cursor = &after[tag_end + 1..];
    }
    out
}

/// Parse the comma-separated `@value` of any
/// `<SupplementalProperty schemeIdUri="urn:mpeg:dash:adaptation-set-switching:2016" value="..."/>`
/// inside `block` and return the referenced AdaptationSet IDs.
///
/// Per ISO/IEC 23009-1 §5.8.5.6, this property declares the set of *other*
/// AdaptationSet ids that the containing set can be seamlessly switched
/// to/from. The relationship is meant to be symmetric: if A lists B, B
/// should list A. Callers should still treat the resulting graph as
/// undirected (be defensive against asymmetric manifests).
///
/// Multiple SupplementalProperty elements with this scheme are allowed and
/// are concatenated. Non-integer values are skipped silently.
pub fn find_switchable_ids(block: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for raw_value in find_descriptor_values(block, "adaptation-set-switching:2016") {
        for piece in raw_value.split(',') {
            if let Ok(id) = piece.trim().parse::<u32>() {
                out.push(id);
            }
        }
    }
    out
}

/// Return the integer value of the FIRST `<AudioChannelConfiguration value="N"/>`
/// in `block`, e.g. for parsing audio channel counts from a Representation.
pub fn find_audio_channel_count(block: &str) -> Option<u32> {
    let mut cursor = block;
    while let Some(open) = cursor.find("<AudioChannelConfiguration") {
        let after = &cursor[open..];
        let tag_end = after.find('>')?;
        let tag = &after[..tag_end];
        if let Some(val_start) = tag.find("value=\"") {
            let rest = &tag[val_start + 7..];
            if let Some(val_end) = rest.find('"') {
                if let Ok(n) = rest[..val_end].parse::<u32>() {
                    return Some(n);
                }
            }
        }
        cursor = &after[tag_end + 1..];
    }
    None
}
