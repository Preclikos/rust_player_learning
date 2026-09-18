//! Host-supplied sidecar subtitles: bytes in, cues out.
//!
//! Backs [`crate::Player::add_external_subtitle_track`]. The desktop flow
//! is "user picks a .srt/.vtt next to the movie" — the host owns the file
//! picker, we only ever see the bytes — so everything about the payload is
//! unknown up front and has to be sniffed:
//!
//!   * **Charset.** WebVTT mandates UTF-8, but `.srt` files predate that
//!     habit: Czech and Polish subs off the usual sites are routinely
//!     CP1250 or ISO-8859-2, with no declaration anywhere in the file.
//!     We try UTF-8 first (valid UTF-8 is essentially never accidentally
//!     valid legacy text) and fall back to `chardetng`, which is
//!     Firefox's detector — so we land on the same encoding the browser
//!     a user compares us against would.
//!   * **Format.** WebVTT announces itself with a `WEBVTT` magic; SubRip
//!     has no header at all, so it's identified by shape (a `-->` timing
//!     line near the top).
//!
//! Parsing itself is shared with the streaming path: both formats reduce
//! to `vtt::parse_cue_blocks`.

use crate::parsers::vtt::{self, VttCue};

/// Source format of a host-supplied subtitle file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubtitleFormat {
    /// Sniff it from the payload. What hosts should normally pass — the
    /// bytes are a stronger signal than a file extension.
    #[default]
    Auto,
    /// WebVTT (`.vtt`), with or without the `WEBVTT` magic.
    WebVtt,
    /// SubRip (`.srt`).
    SubRip,
}

/// Why a sidecar file could not be turned into a subtitle track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarError {
    /// Zero bytes handed in.
    Empty,
    /// The bytes decoded fine but contained no `-->` timing line, so this
    /// is not a subtitle file we understand — an .ass/.sub/.idx, or the
    /// wrong file entirely.
    NotSubtitleText,
    /// Recognisably a subtitle file, but every cue block failed to parse.
    /// Almost always malformed or truncated timings.
    NoCues,
    /// `encoding` named a charset `encoding_rs` doesn't know.
    UnknownEncoding(String),
}

impl std::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SidecarError::Empty => write!(f, "subtitle file is empty"),
            SidecarError::NotSubtitleText => {
                write!(f, "not a WebVTT or SubRip file (no '-->' timing line found)")
            }
            SidecarError::NoCues => write!(f, "no cues could be parsed from the file"),
            SidecarError::UnknownEncoding(label) => {
                write!(f, "unknown character encoding '{}'", label)
            }
        }
    }
}

impl std::error::Error for SidecarError {}

/// A successfully parsed sidecar file.
#[derive(Debug)]
pub struct ParsedSidecar {
    pub cues: Vec<VttCue>,
    /// What the payload turned out to be. Worth surfacing: it tells a
    /// host whether the `.txt` the user picked really was SubRip.
    pub format: SubtitleFormat,
    /// Charset the bytes were decoded as, as an `encoding_rs` label
    /// ("UTF-8", "windows-1250", …). Hosts show this so a user looking at
    /// mangled diacritics knows which encoding to force.
    pub encoding: &'static str,
}

/// Decode, sniff and parse a sidecar subtitle file.
///
/// `encoding` forces a charset by `encoding_rs` label (e.g.
/// `"windows-1250"`) instead of detecting one — the escape hatch for the
/// short files detection gets wrong. `time_offset_ms` shifts every cue,
/// which is how a host exposes the usual "subtitles run two seconds
/// early" nudge; cues that would land before zero are clamped, not
/// dropped.
pub fn parse(
    bytes: &[u8],
    format: SubtitleFormat,
    encoding: Option<&str>,
    time_offset_ms: i64,
) -> Result<ParsedSidecar, SidecarError> {
    if bytes.is_empty() {
        return Err(SidecarError::Empty);
    }

    let (body, encoding_used) = decode(bytes, encoding)?;
    let detected = match format {
        SubtitleFormat::Auto => sniff(&body).ok_or(SidecarError::NotSubtitleText)?,
        explicit => explicit,
    };

    let mut cues = vtt::parse_cue_blocks(&body);
    if cues.is_empty() {
        // Tell "wrong kind of file" apart from "right kind, broken
        // timings" — the host shows a different message for each.
        return Err(if body.contains("-->") {
            SidecarError::NoCues
        } else {
            SidecarError::NotSubtitleText
        });
    }

    if time_offset_ms != 0 {
        for cue in &mut cues {
            cue.start_ms = (cue.start_ms + time_offset_ms).max(0);
            cue.end_ms = (cue.end_ms + time_offset_ms).max(0);
        }
    }
    // The overlay's cue lookup requires start-ordered cues. Sidecar files
    // usually are already, but nothing guarantees it, and a negative
    // offset clamping at zero can reorder the first few.
    cues.sort_by_key(|c| c.start_ms);

    Ok(ParsedSidecar {
        cues,
        format: detected,
        encoding: encoding_used,
    })
}

/// Bytes to text. UTF-8 when the bytes are valid UTF-8 (or a BOM says
/// otherwise), else whatever `chardetng` votes for.
fn decode(bytes: &[u8], forced: Option<&str>) -> Result<(String, &'static str), SidecarError> {
    if let Some(label) = forced {
        let enc = encoding_rs::Encoding::for_label(label.as_bytes())
            .ok_or_else(|| SidecarError::UnknownEncoding(label.to_string()))?;
        let (text, _, _) = enc.decode(bytes);
        return Ok((text.into_owned(), enc.name()));
    }

    // A BOM is authoritative — honour UTF-8 / UTF-16 ones before guessing.
    if let Some((enc, _)) = encoding_rs::Encoding::for_bom(bytes) {
        let (text, _, _) = enc.decode(bytes);
        return Ok((text.into_owned(), enc.name()));
    }

    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok((text.to_string(), encoding_rs::UTF_8.name()));
    }

    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    // `false`: we have no top-level domain to bias the guess with.
    let enc = detector.guess(None, false);
    let (text, _, had_errors) = enc.decode(bytes);
    if had_errors {
        log::warn!(
            "[subs] sidecar decoded as {} with replacement characters — \
             the host may need to force an encoding",
            enc.name()
        );
    }
    Ok((text.into_owned(), enc.name()))
}

/// Identify the format from the decoded text. `None` when it looks like
/// neither.
fn sniff(body: &str) -> Option<SubtitleFormat> {
    // The magic may sit behind a BOM, and some tools prepend blank lines.
    let head: String = body.chars().take(64).collect();
    if head
        .trim_start_matches('\u{FEFF}')
        .trim_start()
        .starts_with("WEBVTT")
    {
        return Some(SubtitleFormat::WebVtt);
    }
    // No header in SubRip, so look for a timing line. Bounded scan, so a
    // large non-subtitle file is rejected without walking all of it.
    let timing = body.lines().take(64).find(|l| l.contains("-->"))?;
    // WebVTT writes fractions with a dot, SubRip with a comma. Both parse
    // identically either way, so this only decides what we report back.
    if timing.contains(',') {
        Some(SubtitleFormat::SubRip)
    } else {
        Some(SubtitleFormat::WebVtt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRT: &str = "1\r\n00:00:01,000 --> 00:00:04,000\r\nPrvní řádek\r\n\r\n2\r\n00:00:05,500 --> 00:00:08,000\r\nDruhý řádek\r\nna dvou řádcích\r\n";
    const VTT: &str = "WEBVTT\n\n00:00:01.000 --> 00:00:04.000\nFirst line\n";

    #[test]
    fn parses_subrip_with_crlf_and_comma_timings() {
        let p = parse(SRT.as_bytes(), SubtitleFormat::Auto, None, 0).unwrap();
        assert_eq!(p.format, SubtitleFormat::SubRip);
        assert_eq!(p.encoding, "UTF-8");
        assert_eq!(p.cues.len(), 2);
        assert_eq!(p.cues[0].start_ms, 1000);
        assert_eq!(p.cues[0].end_ms, 4000);
        assert_eq!(p.cues[0].text, "První řádek");
        assert_eq!(p.cues[1].start_ms, 5500);
        assert_eq!(p.cues[1].text, "Druhý řádek\nna dvou řádcích");
    }

    #[test]
    fn parses_webvtt() {
        let p = parse(VTT.as_bytes(), SubtitleFormat::Auto, None, 0).unwrap();
        assert_eq!(p.format, SubtitleFormat::WebVtt);
        assert_eq!(p.cues.len(), 1);
        assert_eq!(p.cues[0].text, "First line");
    }

    #[test]
    fn decodes_legacy_windows_1250() {
        // The exact case this exists for: a Czech .srt saved by a Windows
        // tool. Plain UTF-8 decoding would produce replacement chars.
        let (bytes, _, _) = encoding_rs::WINDOWS_1250.encode(SRT);
        assert!(std::str::from_utf8(&bytes).is_err());
        let p = parse(&bytes, SubtitleFormat::Auto, None, 0).unwrap();
        assert_eq!(p.cues[0].text, "První řádek");
        assert!(!p.cues[0].text.contains('\u{FFFD}'));
    }

    #[test]
    fn forced_encoding_overrides_detection() {
        let (bytes, _, _) = encoding_rs::WINDOWS_1250.encode(SRT);
        let p = parse(&bytes, SubtitleFormat::Auto, Some("windows-1250"), 0).unwrap();
        assert_eq!(p.encoding, "windows-1250");
        assert_eq!(p.cues[0].text, "První řádek");
    }

    #[test]
    fn unknown_forced_encoding_is_an_error() {
        let err = parse(SRT.as_bytes(), SubtitleFormat::Auto, Some("klingon"), 0).unwrap_err();
        assert_eq!(err, SidecarError::UnknownEncoding("klingon".to_string()));
    }

    #[test]
    fn time_offset_shifts_and_clamps_at_zero() {
        let p = parse(SRT.as_bytes(), SubtitleFormat::Auto, None, -2000).unwrap();
        // 1000 - 2000 would be negative; clamped.
        assert_eq!(p.cues[0].start_ms, 0);
        assert_eq!(p.cues[0].end_ms, 2000);
        assert_eq!(p.cues[1].start_ms, 3500);

        let p = parse(SRT.as_bytes(), SubtitleFormat::Auto, None, 1500).unwrap();
        assert_eq!(p.cues[0].start_ms, 2500);
    }

    #[test]
    fn cues_come_back_start_ordered() {
        let shuffled = "2\n00:00:09,000 --> 00:00:10,000\nlater\n\n1\n00:00:01,000 --> 00:00:02,000\nearlier\n";
        let p = parse(shuffled.as_bytes(), SubtitleFormat::Auto, None, 0).unwrap();
        assert_eq!(p.cues[0].text, "earlier");
        assert_eq!(p.cues[1].text, "later");
    }

    #[test]
    fn rejects_non_subtitle_payloads() {
        assert_eq!(
            parse(b"", SubtitleFormat::Auto, None, 0).unwrap_err(),
            SidecarError::Empty
        );
        assert_eq!(
            parse(b"just some notes about the movie\n", SubtitleFormat::Auto, None, 0).unwrap_err(),
            SidecarError::NotSubtitleText
        );
    }

    #[test]
    fn reports_broken_timings_separately_from_wrong_file() {
        // Looks like SubRip, but no timing parses.
        let broken = "1\nnot:a:timestamp --> also,bad\nsome text\n";
        assert_eq!(
            parse(broken.as_bytes(), SubtitleFormat::Auto, None, 0).unwrap_err(),
            SidecarError::NoCues
        );
    }
}
