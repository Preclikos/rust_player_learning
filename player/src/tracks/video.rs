use super::segment::Segment;
use crate::events::Fps;
use crate::manifest::Property;

#[derive(Clone)]
pub struct VideoAdaptation {
    pub id: u32,

    pub frame_rate: String,
    pub max_width: u32,
    pub max_height: u32,

    //pub par: String,
    pub subsegment_alignment: bool,

    /// Roles ("main", "alternate", etc.) — pre-cloned from the
    /// AdaptationSet for offline inspection.
    pub roles: Vec<Property>,

    pub representations: Vec<VideoRepresenation>,
}

#[derive(Clone)]
pub struct VideoRepresenation {
    pub id: u32,

    pub base_url: String,
    pub file_url: String,

    pub segment_init: Segment,
    pub segment_range: Segment,
    pub segments: Vec<Segment>,

    pub bandwidth: u64,

    pub codecs: String,
    pub mime_type: String,

    pub width: u32,
    pub height: u32,
    pub sar: String,

    /// Pre-cloned MPD descriptor lists; queried by `is_hdr10` /
    /// `is_dolby_vision` so callers don't need to walk the original MPD.
    pub supplemental_properties: Vec<Property>,
    pub essential_properties: Vec<Property>,
}

impl VideoRepresenation {
    /// Human-readable codec family. See PLAYER_INTEGRATION.md §5 for the
    /// canonical mapping table.
    pub fn codec_short(&self) -> &str {
        let c = self.codecs.as_str();
        if c.starts_with("hev1") || c.starts_with("hvc1") {
            "HEVC"
        } else if c.starts_with("avc1") || c.starts_with("avc3") {
            "H.264"
        } else if c.starts_with("av01") {
            "AV1"
        } else if c.starts_with("vp09") {
            "VP9"
        } else {
            // Fall back to whatever the manifest provided so the UI at
            // least shows *something* recognisable.
            self.codecs.as_str()
        }
    }

    /// HDR10 detection — DASH carries it as `SupplementalProperty` with
    /// either `colour_primaries=9` (BT.2020) or
    /// `transfer_characteristics=16/18` (PQ / HLG).
    pub fn is_hdr10(&self) -> bool {
        for p in self
            .supplemental_properties
            .iter()
            .chain(self.essential_properties.iter())
        {
            let scheme = p.scheme_id_uri.as_str();
            let value = p.value.as_deref().unwrap_or("");
            if scheme.contains("colour_primaries") && value == "9" {
                return true;
            }
            if scheme.contains("transfer_characteristics")
                && (value == "16" || value == "18")
            {
                return true;
            }
        }
        false
    }

    /// Dolby Vision detection — `EssentialProperty` with
    /// `…dolby_vision_profile` in the scheme URI.
    pub fn is_dolby_vision(&self) -> bool {
        // DV strictly requires EssentialProperty (so non-DV-aware players
        // are forced to ignore the stream), but tolerate SupplementalProperty
        // too in case a manifest is non-conforming.
        for p in self
            .essential_properties
            .iter()
            .chain(self.supplemental_properties.iter())
        {
            if p.scheme_id_uri.contains("dolby_vision_profile")
                || p.scheme_id_uri.contains("dolby_vision")
            {
                return true;
            }
        }
        // Codec sniff for `dvh1.*` / `dvhe.*` strings as a fallback.
        let c = self.codecs.as_str();
        c.starts_with("dvh1") || c.starts_with("dvhe") || c.starts_with("dvav")
    }

    /// Returns `true` if the codec string indicates a 10-bit profile
    /// (HEVC Main10 etc.). Useful for the label suffix.
    pub fn is_10bit(&self) -> bool {
        let c = self.codecs.as_str();
        // HEVC Main10 = profile_idc 2 → "hvc1.2.*" / "hev1.2.*"
        c.starts_with("hvc1.2") || c.starts_with("hev1.2")
    }

    /// Pre-formatted single-line summary, e.g. "1080p HEVC 10-bit · 8.5 Mbps".
    pub fn label(&self) -> String {
        let resolution = match self.height {
            h if h >= 2160 => "4K".to_string(),
            h if h >= 1080 => "1080p".to_string(),
            h if h >= 720 => "720p".to_string(),
            h if h >= 480 => "480p".to_string(),
            h if h > 0 => format!("{}p", h),
            _ => format!("{}x{}", self.width, self.height),
        };
        let codec = self.codec_short();
        let bit = if self.is_10bit() { " 10-bit" } else { "" };
        let mbps = self.bandwidth as f64 / 1_000_000.0;
        let hdr = if self.is_dolby_vision() {
            " · DV"
        } else if self.is_hdr10() {
            " · HDR10"
        } else {
            ""
        };
        format!("{} {}{} · {:.1} Mbps{}", resolution, codec, bit, mbps, hdr)
    }
}

impl VideoAdaptation {
    /// Frame rate parsed into [`Fps`] num/den form. Returns `None` if the
    /// MPD value couldn't be parsed.
    pub fn fps(&self) -> Option<Fps> {
        Fps::parse(&self.frame_rate)
    }
}
