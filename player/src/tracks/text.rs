//! Subtitle / closed-caption track metadata.
//!
//! Phase 1 of PLAYER_INTEGRATION.md §6.3: enumerate text adaptations and
//! representations so the consumer can list them in a UI. Phase 2 (decoded
//! cues via a callback channel) lives behind a future API once the WebVTT /
//! TTML pipeline is in place.

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
