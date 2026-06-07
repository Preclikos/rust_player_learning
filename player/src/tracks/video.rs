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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracks::segment::Segment;

    fn empty_segment() -> Segment {
        Segment::new(&String::new(), &String::new(), 0, 0, None, None, None)
            .expect("stub segment")
    }

    fn rep(codecs: &str, w: u32, h: u32, hdr10: bool, dolby_vision: bool) -> VideoRepresenation {
        VideoRepresenation {
            id: 1,
            base_url: String::new(),
            file_url: String::new(),
            segment_init: empty_segment(),
            segment_range: empty_segment(),
            segments: Vec::new(),
            bandwidth: 5_000_000,
            codecs: codecs.to_string(),
            mime_type: "video/mp4".to_string(),
            width: w,
            height: h,
            sar: "1:1".to_string(),
            hdr10,
            dolby_vision,
        }
    }

    // ---------------- codec_short ----------------

    #[test]
    fn codec_short_recognises_hevc_variants() {
        assert_eq!(rep("hev1.1.6.L120.90", 0, 0, false, false).codec_short(), "HEVC");
        assert_eq!(rep("hvc1.1.6.L120.90", 0, 0, false, false).codec_short(), "HEVC");
        assert_eq!(rep("hvc1.2.4.L150.90", 0, 0, false, false).codec_short(), "HEVC");
    }

    #[test]
    fn codec_short_recognises_h264_variants() {
        assert_eq!(rep("avc1.64001f", 0, 0, false, false).codec_short(), "H.264");
        assert_eq!(rep("avc3.64001f", 0, 0, false, false).codec_short(), "H.264");
    }

    #[test]
    fn codec_short_recognises_av1_vp9_dolby_vision() {
        assert_eq!(rep("av01.0.05M.08", 0, 0, false, false).codec_short(), "AV1");
        assert_eq!(rep("vp09.00.10.08", 0, 0, false, false).codec_short(), "VP9");
        assert_eq!(rep("dvh1.05.06", 0, 0, false, true).codec_short(), "Dolby Vision");
        assert_eq!(rep("dvhe.05.06", 0, 0, false, true).codec_short(), "Dolby Vision");
        assert_eq!(rep("dvav.05.06", 0, 0, false, true).codec_short(), "Dolby Vision");
    }

    #[test]
    fn codec_short_falls_back_to_raw_string_for_unknown() {
        // An unrecognised codec is returned verbatim so the user at least
        // sees *something* rather than "Unknown".
        assert_eq!(
            rep("future-codec.42", 0, 0, false, false).codec_short(),
            "future-codec.42",
        );
    }

    // ---------------- is_10bit ----------------

    #[test]
    fn is_10bit_true_for_hevc_main10_profile() {
        // hvc1.2 / hev1.2 == Main10 profile = 10-bit. The 4K HDR rep from
        // the user's manifest exercises this.
        assert!(rep("hvc1.2.4.L150.90", 0, 0, false, false).is_10bit());
        assert!(rep("hev1.2.4.L150.90", 0, 0, false, false).is_10bit());
    }

    #[test]
    fn is_10bit_false_for_hevc_main_profile() {
        // hvc1.1 / hev1.1 == Main profile = 8-bit SDR.
        assert!(!rep("hvc1.1.6.L120.90", 0, 0, false, false).is_10bit());
        assert!(!rep("hev1.1.6.L120.90", 0, 0, false, false).is_10bit());
    }

    #[test]
    fn is_10bit_true_for_dolby_vision_regardless_of_codec_string() {
        // DV reps are 10-bit even when codec string doesn't say so (the
        // dolby_vision flag covers them).
        assert!(rep("dvh1.05.06", 0, 0, false, true).is_10bit());
    }

    #[test]
    fn is_10bit_false_for_h264() {
        assert!(!rep("avc1.64001f", 0, 0, false, false).is_10bit());
    }

    // ---------------- is_hdr10 / is_dolby_vision ----------------

    #[test]
    fn hdr10_and_dolby_vision_flags_pass_through() {
        assert!(rep("hvc1.2.4.L150.90", 0, 0, true, false).is_hdr10());
        assert!(!rep("hvc1.2.4.L150.90", 0, 0, false, false).is_hdr10());
        assert!(rep("dvh1.05.06", 0, 0, false, true).is_dolby_vision());
        assert!(!rep("hvc1.1.6.L120.90", 0, 0, false, false).is_dolby_vision());
    }

    // ---------------- label ----------------

    #[test]
    fn label_4k_hdr10_main10() {
        let r = rep("hvc1.2.4.L150.90", 3840, 2160, true, false);
        // 14 Mbps via override.
        let mut r = r;
        r.bandwidth = 14_000_000;
        assert_eq!(r.label(), "4K HEVC 10-bit · 14.0 Mbps · HDR10");
    }

    #[test]
    fn label_1080p_sdr_hevc() {
        let r = rep("hvc1.1.6.L120.90", 1920, 1080, false, false);
        let mut r = r;
        r.bandwidth = 6_000_000;
        assert_eq!(r.label(), "1080p HEVC · 6.0 Mbps");
    }

    #[test]
    fn label_720p_dolby_vision_drops_explicit_bit_suffix() {
        // DV reps print " · DV" instead of " 10-bit · ... · HDR10" — the
        // is_10bit() branch is skipped when dolby_vision is set so the
        // label doesn't end up like "720p Dolby Vision 10-bit · DV".
        let r = rep("dvh1.05.06", 1280, 720, false, true);
        let mut r = r;
        r.bandwidth = 4_500_000;
        assert_eq!(r.label(), "720p Dolby Vision · 4.5 Mbps · DV");
    }

    #[test]
    fn label_under_480_uses_numeric_p_then_literal_size() {
        // Heights between 1 and 479 render as "Np".
        let mut r = rep("avc1.64001f", 320, 240, false, false);
        r.bandwidth = 500_000;
        assert_eq!(r.label(), "240p H.264 · 0.5 Mbps");

        // Height == 0 (manifest didn't specify) falls back to literal w×h.
        let mut r0 = rep("avc1.64001f", 1440, 0, false, false);
        r0.bandwidth = 2_000_000;
        assert_eq!(r0.label(), "1440x0 H.264 · 2.0 Mbps");
    }

    #[test]
    fn label_emits_2K_4K_buckets_by_height() {
        let mut r4k = rep("hvc1.2.4.L150.90", 3840, 2160, false, false);
        r4k.bandwidth = 14_000_000;
        assert!(r4k.label().starts_with("4K "));

        let mut r1080 = rep("hvc1.1.6.L120.90", 1920, 1080, false, false);
        r1080.bandwidth = 6_000_000;
        assert!(r1080.label().starts_with("1080p "));

        let mut r720 = rep("hvc1.1.6.L93.90", 1280, 720, false, false);
        r720.bandwidth = 3_000_000;
        assert!(r720.label().starts_with("720p "));

        let mut r480 = rep("hvc1.1.6.L90.90", 854, 480, false, false);
        r480.bandwidth = 1_500_000;
        assert!(r480.label().starts_with("480p "));
    }
}
