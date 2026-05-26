use super::segment::Segment;

#[derive(Clone)]
pub struct AudioAdaptation {
    pub id: u32,

    pub lang: String,

    pub subsegment_alignment: bool,

    pub roles: Vec<String>,

    pub representations: Vec<AudioRepresentation>,
}

impl AudioAdaptation {
    pub fn language(&self) -> Option<&str> {
        if self.lang.is_empty() {
            None
        } else {
            Some(self.lang.as_str())
        }
    }

    /// Returns the first DASH `Role` value (e.g. "main", "dub",
    /// "commentary"). Most adaptation sets carry exactly one role.
    pub fn role(&self) -> Option<&str> {
        self.roles.first().map(|s| s.as_str())
    }
}

#[derive(Clone)]
pub struct AudioRepresentation {
    pub id: u32,

    pub base_url: String,
    pub file_url: String,

    pub segment_init: Segment,
    pub segment_range: Segment,
    pub segments: Vec<Segment>,

    pub bandwidth: u64,

    pub codecs: String,
    pub mime_type: String,

    pub audio_sampling_rate: u32,

    /// Channel count pulled from `<AudioChannelConfiguration value="N"/>`.
    /// `None` when the manifest omits the descriptor.
    pub channels: Option<u32>,
}

impl AudioRepresentation {
    pub fn codec_short(&self) -> &str {
        let c = self.codecs.as_str();
        if c == "mp4a.40.2" {
            "AAC"
        } else if c == "mp4a.40.5" {
            "AAC-HE"
        } else if c.starts_with("mp4a") {
            "AAC"
        } else if c == "ec-3" {
            "DDP"
        } else if c == "ac-3" {
            "DD"
        } else if c.starts_with("opus") {
            "Opus"
        } else {
            self.codecs.as_str()
        }
    }

    pub fn channels(&self) -> Option<u32> {
        self.channels
    }

    /// Single-line summary used by the TUI track picker, e.g.
    /// "5.1 · DDP · 384 kbps". Language is on the adaptation, not the
    /// representation, so callers prefix it themselves if needed.
    pub fn label(&self) -> String {
        let layout = match self.channels {
            Some(1) => "Mono".to_string(),
            Some(2) => "2.0".to_string(),
            Some(6) => "5.1".to_string(),
            Some(8) => "7.1".to_string(),
            Some(n) => format!("{}ch", n),
            None => String::new(),
        };
        let kbps = (self.bandwidth as f64 / 1000.0).round() as u64;
        if layout.is_empty() {
            format!("{} · {} kbps", self.codec_short(), kbps)
        } else {
            format!("{} · {} · {} kbps", layout, self.codec_short(), kbps)
        }
    }
}
