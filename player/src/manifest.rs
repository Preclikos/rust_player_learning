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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fragment of the user's real DASH manifest — kept tight enough to read
    /// at a glance but with all the elements the helpers below need to
    /// distinguish (two switching-equivalent video AdaptationSets, an audio
    /// adaptation with EC-3 channel config, multi-language text). When
    /// adding new helper tests, prefer extracting the slice from this
    /// fixture so they stay realistic.
    /// Compact-but-realistic DASH fragment. Carries enough of the
    /// elements the helpers below need to distinguish (two
    /// switching-equivalent video AdaptationSets, an audio adaptation
    /// with EC-3 channel config) so test failures point at real-world
    /// breakages, not synthetic edge cases.
    const REAL_MPD: &str = r#"<MPD mediaPresentationDuration="PT9382.375S">
<Period id="0">
<AdaptationSet id="223705" contentType="video" maxWidth="1920" maxHeight="1080">
<ContentProtection schemeIdUri="urn:uuid:1077efec-c0b2-4d02-ace3-3c1e52e2fb4b"/>
<Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
<Representation id="315074" bandwidth="6000000" codecs="hvc1.1.6.L120.90" mimeType="video/mp4" width="1920" height="1080"><BaseURL>seg-315074/</BaseURL><SegmentBase indexRange="1130-19929"><Initialization range="0-1129"/></SegmentBase></Representation>
<Representation id="315075" bandwidth="3000000" codecs="hvc1.1.6.L93.90" mimeType="video/mp4" width="1280" height="720"><BaseURL>seg-315075/</BaseURL><SegmentBase indexRange="1130-19929"><Initialization range="0-1129"/></SegmentBase></Representation>
<SupplementalProperty schemeIdUri="urn:mpeg:dash:adaptation-set-switching:2016" value="223714"/>
</AdaptationSet>
<AdaptationSet id="223707" contentType="audio" lang="en">
<Representation id="315079" bandwidth="768000" codecs="ec-3" mimeType="audio/mp4"><BaseURL>seg-315079/</BaseURL><AudioChannelConfiguration schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="6"/></Representation>
</AdaptationSet>
<AdaptationSet id="223714" contentType="video" maxWidth="3840" maxHeight="2160">
<ContentProtection schemeIdUri="urn:uuid:1077efec-c0b2-4d02-ace3-3c1e52e2fb4b"/>
<Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
<SupplementalProperty schemeIdUri="urn:mpeg:dash:colour_primaries" value="9"/>
<SupplementalProperty schemeIdUri="urn:mpeg:dash:TransferCharacteristics" value="16"/>
<Representation id="315086" bandwidth="14000000" codecs="hvc1.2.4.L150.90" mimeType="video/mp4" width="3840" height="2160"><BaseURL>seg-315086/</BaseURL><SegmentBase indexRange="1130-19929"><Initialization range="0-1129"/></SegmentBase></Representation>
<SupplementalProperty schemeIdUri="urn:mpeg:dash:adaptation-set-switching:2016" value="223705"/>
</AdaptationSet>
</Period>
</MPD>"#;

    // -------------------------------------------------------------------
    // slice_adaptation_set
    // -------------------------------------------------------------------

    #[test]
    fn slice_adaptation_set_returns_block_for_matching_id() {
        let block = slice_adaptation_set(REAL_MPD, 223705).expect("found");
        assert!(block.contains("contentType=\"video\""));
        assert!(block.contains("id=\"315074\""));
        assert!(block.contains("id=\"315075\""));
        // Must not bleed into the next AdaptationSet.
        assert!(!block.contains("id=\"315086\""));
        assert!(!block.contains("contentType=\"audio\""));
    }

    #[test]
    fn slice_adaptation_set_handles_second_video_set() {
        // 223714 sits AFTER an audio AdaptationSet — verifies the slicer
        // doesn't terminate at the first </AdaptationSet> after `start`.
        let block = slice_adaptation_set(REAL_MPD, 223714).expect("found");
        assert!(block.contains("maxWidth=\"3840\""));
        assert!(block.contains("id=\"315086\""));
        assert!(!block.contains("id=\"315074\""));
    }

    #[test]
    fn slice_adaptation_set_missing_id_returns_none() {
        assert!(slice_adaptation_set(REAL_MPD, 999_999).is_none());
    }

    #[test]
    fn slice_adaptation_set_prefix_id_doesnt_match() {
        // "22370" must NOT match adaptation id "223705" because the
        // needle ends with the closing `"` quote. Guards against future
        // refactors that drop the quote and accidentally let `22370`
        // match a longer id by prefix.
        assert!(slice_adaptation_set(REAL_MPD, 22370).is_none());
    }

    // -------------------------------------------------------------------
    // slice_representation
    // -------------------------------------------------------------------

    #[test]
    fn slice_representation_pulls_inner_rep_block() {
        let adapt = slice_adaptation_set(REAL_MPD, 223705).unwrap();
        let rep = slice_representation(adapt, 315074).expect("rep found");
        assert!(rep.contains("bandwidth=\"6000000\""));
        assert!(rep.contains("width=\"1920\""));
        assert!(!rep.contains("id=\"315075\""));
    }

    #[test]
    fn slice_representation_missing_id_returns_none() {
        let adapt = slice_adaptation_set(REAL_MPD, 223705).unwrap();
        assert!(slice_representation(adapt, 999).is_none());
    }

    // -------------------------------------------------------------------
    // find_descriptor_values
    // -------------------------------------------------------------------

    #[test]
    fn find_descriptor_values_finds_supplemental_property_values() {
        let adapt = slice_adaptation_set(REAL_MPD, 223714).unwrap();
        let primaries = find_descriptor_values(adapt, "colour_primaries");
        assert_eq!(primaries, vec!["9".to_string()]);
        let xfer = find_descriptor_values(adapt, "TransferCharacteristics");
        assert_eq!(xfer, vec!["16".to_string()]);
    }

    #[test]
    fn find_descriptor_values_returns_empty_when_scheme_absent() {
        let adapt = slice_adaptation_set(REAL_MPD, 223705).unwrap();
        // No Dolby Vision essentials in this fragment.
        let dv = find_descriptor_values(adapt, "dolby_vision");
        assert!(dv.is_empty());
    }

    #[test]
    fn find_descriptor_values_skips_non_property_elements_with_value_attr() {
        // ContentProtection has a value-ish-looking schemeIdUri but isn't
        // a *Property element, so it must NOT be returned.
        let adapt = slice_adaptation_set(REAL_MPD, 223705).unwrap();
        let cp = find_descriptor_values(adapt, "uuid:1077efec");
        assert!(cp.is_empty());
    }

    #[test]
    fn find_descriptor_values_can_find_multiple_matches() {
        // Sanity-check with a hand-rolled fragment containing two
        // SupplementalProperty elements with the same scheme.
        let frag = r#"<AdaptationSet>
            <SupplementalProperty schemeIdUri="urn:foo:bar" value="A"/>
            <SupplementalProperty schemeIdUri="urn:foo:bar" value="B"/>
            <SupplementalProperty schemeIdUri="urn:other" value="X"/>
        </AdaptationSet>"#;
        let vals = find_descriptor_values(frag, "foo:bar");
        assert_eq!(vals, vec!["A".to_string(), "B".to_string()]);
    }

    // -------------------------------------------------------------------
    // find_switchable_ids
    // -------------------------------------------------------------------

    #[test]
    fn find_switchable_ids_returns_single_id() {
        let adapt = slice_adaptation_set(REAL_MPD, 223705).unwrap();
        assert_eq!(find_switchable_ids(adapt), vec![223714]);
    }

    #[test]
    fn find_switchable_ids_handles_comma_separated_list() {
        let frag = r#"<AdaptationSet>
            <SupplementalProperty
                schemeIdUri="urn:mpeg:dash:adaptation-set-switching:2016"
                value="1, 2,3 , 4"/>
        </AdaptationSet>"#;
        // Whitespace around commas is tolerated.
        assert_eq!(find_switchable_ids(frag), vec![1, 2, 3, 4]);
    }

    #[test]
    fn find_switchable_ids_skips_non_integer_pieces() {
        let frag = r#"<AdaptationSet>
            <SupplementalProperty
                schemeIdUri="urn:mpeg:dash:adaptation-set-switching:2016"
                value="42,not-a-number,99"/>
        </AdaptationSet>"#;
        assert_eq!(find_switchable_ids(frag), vec![42, 99]);
    }

    #[test]
    fn find_switchable_ids_empty_when_property_absent() {
        let frag = r#"<AdaptationSet>
            <Role value="main"/>
        </AdaptationSet>"#;
        assert!(find_switchable_ids(frag).is_empty());
    }

    // -------------------------------------------------------------------
    // find_audio_channel_count
    // -------------------------------------------------------------------

    #[test]
    fn find_audio_channel_count_reads_value() {
        let audio_adapt = slice_adaptation_set(REAL_MPD, 223707).unwrap();
        assert_eq!(find_audio_channel_count(audio_adapt), Some(6));
    }

    #[test]
    fn find_audio_channel_count_none_when_absent() {
        let video_adapt = slice_adaptation_set(REAL_MPD, 223705).unwrap();
        // Video doesn't carry AudioChannelConfiguration.
        assert_eq!(find_audio_channel_count(video_adapt), None);
    }

    #[test]
    fn find_audio_channel_count_returns_first_match() {
        let frag = r#"<Representation>
            <AudioChannelConfiguration value="2"/>
            <AudioChannelConfiguration value="6"/>
        </Representation>"#;
        // Spec-wise we only expect one per Representation, but if there
        // are two the helper picks the first (consistent, predictable).
        assert_eq!(find_audio_channel_count(frag), Some(2));
    }

    // -------------------------------------------------------------------
    // Manifest::parse — round-trip a small MPD through serde
    // -------------------------------------------------------------------

    #[test]
    fn parse_accepts_realistic_two_video_adaptation_mpd() {
        // Use from_str directly so any serde/quick-xml error surfaces in
        // the test output instead of being swallowed by Manifest::parse's
        // generic "Failed to parse MPD" wrap.
        let mpd: MPD = quick_xml::de::from_str(REAL_MPD)
            .unwrap_or_else(|e| panic!("MPD parse failed: {}\n--- input ---\n{}", e, REAL_MPD));
        assert_eq!(mpd.periods.len(), 1);
        let p = &mpd.periods[0];
        // Three AdaptationSets: video, audio, video.
        assert_eq!(p.adaptation_sets.len(), 3);
        let video_ids: Vec<u32> = p
            .adaptation_sets
            .iter()
            .filter(|a| a.content_type == "video")
            .map(|a| a.id)
            .collect();
        assert_eq!(video_ids, vec![223705, 223714]);
    }

    #[test]
    fn parse_rejects_malformed_xml() {
        let bad = "<MPD><Period><AdaptationSet";
        assert!(Manifest::parse(bad).is_err());
    }
}
