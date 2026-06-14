//! Player event surface — broadcast stream consumed via `Player::events()`.
//! Shape defined by PLAYER_INTEGRATION.md §4.

use std::time::Duration;

/// Subscribed by consumers via `Player::events()`. Variants are emitted
/// for every state transition; lifecycle events (`Idle`, `ManifestLoaded`,
/// `Prepared`, `Buffering`, `Playing`, `Paused`, `EndOfStream`, `Error`)
/// are guaranteed to fire — the player uses a `broadcast(64)` channel
/// so subscribers can fall behind by up to 64 events without missing one.
/// `Position` and `Stats` are rate-limited polls and may be skipped if
/// the subscriber lags.
#[derive(Clone, Debug)]
pub enum PlayerEvent {
    /// Initial — before `open_url`.
    Idle,
    /// `open_url` succeeded.
    ManifestLoaded {
        duration: Duration,
        video_tracks: usize,
        audio_tracks: usize,
        subtitle_tracks: usize,
    },
    /// `prepare()` done — init segments fetched, decoders ready.
    Prepared,
    /// Waiting for media data (initial fill, segment-boundary stall,
    /// post-seek, track switch).
    Buffering { reason: BufferingReason },
    /// Playback active.
    Playing,
    /// Paused by the consumer via `pause()`.
    Paused,
    /// Periodic — emitted at ≤ 4 Hz during playback.
    Position {
        position: Duration,
        duration: Duration,
        /// Seconds of decoded video ahead of `position` — specifically,
        /// (end PTS of the latest segment whose download AND decode both
        /// completed) minus the current playback PTS. The amount of media
        /// that is safe to play through if the network drops right now.
        buffered_ahead_secs: f32,
        /// EWMA bytes/s over the last ~8 segment downloads.
        bandwidth_bps: u64,
    },
    /// Track selection changed (initial, user switch, or future ABR).
    TrackChanged { kind: TrackKind, info: TrackInfo },
    /// Decoder hiccup the player recovered from. UI hint, not fatal.
    GlitchRecovered { detail: String },
    /// Cumulative stats — emitted at ≤ 1 Hz.
    Stats {
        video_frames_decoded: u64,
        video_frames_dropped: u64,
        audio_underruns: u64,
        /// Wall-clock ms the decoder was blocked waiting on network in
        /// the last second (0 = healthy).
        net_stall_ms: u64,
        /// Human-readable decoder backend name, e.g. `"D3D11VA HEVC"`,
        /// `"MediaCodec H.264"`. Known to the player internally.
        decoder_name: String,
        /// What is actually being rendered post-ABR (matches the current
        /// representation). `None` before the first frame.
        current_resolution: Option<(u32, u32)>,
        /// Last-frame L/R peak in dB (range typically -60..=0).
        /// `None` until at least one audio frame has been mixed.
        audio_peak_db: Option<[f32; 2]>,
        /// Measured A/V clock drift since pipeline start, in ms: video
        /// wall clock minus the audio device clock (negative = audio
        /// ahead). `None` while unmeasured (first second, or sinks that
        /// can't report playback position). Expect a slow linear trend
        /// from crystal mismatch; jumps indicate sync bugs.
        av_drift_ms: Option<i64>,
    },
    /// End of media reached.
    EndOfStream,
    /// Fatal — playback cannot continue. `kind` lets the consumer
    /// branch (retry, re-auth, abandon) without parsing `detail`.
    Error {
        kind: PlayerErrorKind,
        detail: String,
    },
}

/// Categorised player error so consumers can branch programmatically
/// (refresh token on 401, abandon on Decoder, etc.) without parsing
/// human-readable detail strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerErrorKind {
    /// TCP / TLS / DNS — transient connectivity problem.
    Network,
    /// HTTP non-2xx that wasn't recovered by retry.
    Http { status: u16 },
    /// `RequestInterceptor::intercept` returned `Err` (or timed out).
    Interceptor,
    /// `LicenseResolver::resolve` returned `Err` (or timed out).
    LicenseResolver,
    /// MPD parse failed or has unsupported structure (e.g. multi-period).
    ManifestParse,
    /// Decoder pipeline failed unrecoverably.
    Decoder,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Clone, Copy, Debug)]
pub enum BufferingReason {
    Initial,
    Stall,
    Seek,
    TrackSwitch,
}

/// Exact frame rate. DASH carries fractional rates like NTSC drop-frame
/// (`30000/1001` → 29.97) and cinema NTSC (`24000/1001` → 23.976).
/// Storing num/den preserves precision; `f32` doesn't.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fps {
    pub num: u32,
    /// Typically 1, 1000, or 1001.
    pub den: u32,
}

impl Fps {
    pub fn as_f32(&self) -> f32 {
        if self.den == 0 {
            0.0
        } else {
            self.num as f32 / self.den as f32
        }
    }

    /// Parse a DASH `@frameRate` string. Accepts integer ("24"), fraction
    /// ("24000/1001"), and decimal ("23.976" → 23976/1000) forms.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some((n, d)) = s.split_once('/') {
            let num = n.trim().parse::<u32>().ok()?;
            let den = d.trim().parse::<u32>().ok()?;
            if den == 0 {
                return None;
            }
            return Some(Self { num, den });
        }
        if let Some(dot) = s.find('.') {
            let int_part = &s[..dot];
            let frac_part = &s[dot + 1..];
            let int_v: u32 = int_part.parse().ok()?;
            let frac_v: u32 = frac_part.parse().ok()?;
            let den = 10u32.checked_pow(frac_part.len() as u32)?;
            let num = int_v.checked_mul(den)?.checked_add(frac_v)?;
            return Some(Self { num, den });
        }
        let num = s.parse::<u32>().ok()?;
        Some(Self { num, den: 1 })
    }
}

impl std::fmt::Display for Fps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.den == 1 {
            write!(f, "{}", self.num)
        } else {
            write!(f, "{:.3}", self.as_f32())
        }
    }
}

/// Per-track metadata surfaced in `TrackChanged` events. Mirrors the
/// human-readable fields the TUI needs without exposing raw MPD types.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub representation_id: u32,
    /// Simplified codec family: "HEVC", "H.264", "AAC", "DDP".
    pub codec: String,
    pub bitrate_bps: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<Fps>,
    pub channels: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub language: Option<String>,
    /// Pre-formatted summary, e.g. "1080p HEVC · 8.5 Mbps".
    pub label: String,
    pub hdr10: bool,
    pub dolby_vision: bool,
}
