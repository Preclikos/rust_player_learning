use super::segment::Segment;
use crate::events::Fps;

#[derive(Clone)]
pub struct VideoAdaptation {
    pub id: u32,

    pub frame_rate: String,
    pub max_width: u32,
    pub max_height: u32,

    //pub par: String,
    pub subsegment_alignment: bool,

    /// Role values lifted from `<Role schemeIdUri="urn:mpeg:dash:role:2011"
    /// value="..."/>` (e.g. "main", "alternate").
    pub roles: Vec<String>,

    pub representations: Vec<VideoRepresenation>,
}

impl VideoAdaptation {
    /// Frame rate parsed into [`Fps`] num/den form. Returns `None` if the
    /// MPD value couldn't be parsed.
    pub fn fps(&self) -> Option<Fps> {
        Fps::parse(&self.frame_rate)
    }
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

    /// Pre-computed flags from a raw-XML walk at track-build time.
    /// `supplemental_properties` and `essential_properties` can't be
    /// parsed via serde (quick-xml chokes on interleaving), so the
    /// detection runs once in `Tracks::parse_*` against the raw
    /// `Manifest::content` and the bool is cached here.
    pub hdr10: bool,
    pub dolby_vision: bool,
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
        } else if c.starts_with("dvh1") || c.starts_with("dvhe") || c.starts_with("dvav") {
            "Dolby Vision"
        } else {
            self.codecs.as_str()
        }
    }

    pub fn is_hdr10(&self) -> bool {
        self.hdr10
    }
    pub fn is_dolby_vision(&self) -> bool {
        self.dolby_vision
    }

    /// Returns `true` if the codec string indicates a 10-bit profile
    /// (HEVC Main10 etc.). Useful for the label suffix.
    pub fn is_10bit(&self) -> bool {
        let c = self.codecs.as_str();
        c.starts_with("hvc1.2") || c.starts_with("hev1.2") || self.dolby_vision
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
        let bit = if self.is_10bit() && !self.dolby_vision {
            " 10-bit"
        } else {
            ""
        };
        let mbps = self.bandwidth as f64 / 1_000_000.0;
        let hdr = if self.dolby_vision {
            " · DV"
        } else if self.hdr10 {
            " · HDR10"
        } else {
            ""
        };
        format!("{} {}{} · {:.1} Mbps{}", resolution, codec, bit, mbps, hdr)
    }
}
