//! Subtitle / closed-caption track metadata + per-representation
//! download artefacts.
//!
//! Phase 1 of PLAYER_INTEGRATION.md §6.3: enumerate text adaptations so
//! the consumer can list them, and (when activated via
//! `Player::set_subtitle_track`) drive a download / parse / render
//! pipeline that mirrors the audio one. Only WebVTT in ISO BMFF (and
//! raw WebVTT) is decoded today; TTML is enumerated but won't render.

use super::segment::Segment;

#[derive(Clone)]
pub struct TextAdaptation {
    pub id: u32,
    /// BCP-47 language tag, e.g. `"en"`, `"cs"`. Empty when the manifest
    /// omits `@lang`.
    pub lang: String,
    /// DASH `<Role value="..."/>`s (e.g. `"subtitle"`, `"caption"`,
    /// `"forced-subtitle"`). Most adaptation sets carry zero or one.
    pub roles: Vec<String>,
    pub representations: Vec<TextRepresenation>,
}

impl TextAdaptation {
    pub fn language(&self) -> Option<&str> {
        if self.lang.is_empty() {
            None
        } else {
            Some(self.lang.as_str())
        }
    }

    pub fn role(&self) -> Option<&str> {
        self.roles.first().map(|s| s.as_str())
    }

    /// True when this adaptation set is marked as forced subtitles via
    /// `<Role schemeIdUri="urn:mpeg:dash:role:2011" value="forced-subtitle"/>`.
    /// Consumers typically auto-enable forced tracks alongside the
    /// matching-language audio (signs, untranslated dialogue, etc.).
    pub fn is_forced(&self) -> bool {
        self.roles
            .iter()
            .any(|r| r == "forced-subtitle" || r == "forced_subtitle")
    }

    /// True when role is "caption" / "captions" — closed captions
    /// intended for hard-of-hearing viewers (often include "[door
    /// closes]" type cues).
    pub fn is_caption(&self) -> bool {
        self.roles.iter().any(|r| r == "caption" || r == "captions")
    }
}

#[derive(Clone)]
pub struct TextRepresenation {
    pub id: u32,
    /// Raw codecs string from MPD `@codecs`, e.g. `"wvtt"`, `"stpp"`,
    /// `"ttml"`. May be empty for sidecar TTML.
    pub codecs: String,
    /// MPD `@mimeType`, e.g. `"application/mp4"`, `"text/vtt"`,
    /// `"application/ttml+xml"`.
    pub mime_type: String,
    pub bandwidth: u64,

    pub base_url: String,
    pub file_url: String,

    /// CMAF / sidx-indexed delivery — populated when the representation
    /// has a `SegmentBase` with an `indexRange`. text_play streams these
    /// like audio/video segments.
    pub segment_init: Option<Segment>,
    pub segment_range: Option<Segment>,
    pub segments: Vec<Segment>,

    /// Single-file delivery — populated when the representation is just
    /// a `BaseURL` pointing at one VTT/TTML file (no SegmentBase, no
    /// indexRange). Most "rip with external subs" workflows ship this
    /// way: one .vtt per language, unencrypted, downloaded once at
    /// activation time. text_play fetches the full URL and parses the
    /// payload as raw WebVTT.
    pub single_file_url: Option<String>,
}

impl TextRepresenation {
    pub fn codec_short(&self) -> &str {
        let c = self.codecs.as_str();
        if c.starts_with("wvtt") || self.mime_type == "text/vtt" {
            "WebVTT"
        } else if c.starts_with("stpp") || c.starts_with("ttml") || self.mime_type.contains("ttml") {
            "TTML"
        } else if c.is_empty() {
            self.mime_type.as_str()
        } else {
            c
        }
    }

    /// True when the representation looks decodable today (i.e. WebVTT
    /// in ISO BMFF or raw). TTML and sidecar formats return false; the
    /// subtitle pipeline silently no-ops on those.
    pub fn is_webvtt(&self) -> bool {
        let c = self.codecs.as_str();
        c.starts_with("wvtt") || self.mime_type == "text/vtt" || self.mime_type == "application/x-mpegurl"
    }

    /// Single-line summary for a track picker, e.g. `"WebVTT · 12 kbps"`.
    pub fn label(&self) -> String {
        let kbps = (self.bandwidth as f64 / 1000.0).round() as u64;
        if kbps == 0 {
            self.codec_short().to_string()
        } else {
            format!("{} · {} kbps", self.codec_short(), kbps)
        }
    }
}
