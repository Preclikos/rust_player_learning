//! WebVTT cue extraction from DASH text segments.
//!
//! DASH ships WebVTT either as ISO/IEC 14496-30 ISO BMFF (samples wrap
//! `vttc` / `vtte` boxes with `payl` payloads) or — less commonly — as
//! raw WebVTT text inside `mdat`. The entry point [`parse_segment`] sniffs
//! both forms.
//!
//! Phase 1 scope: plain-text cues only. We strip any inline tags
//! (`<b>`, `<i>`, `<c.classname>` …) so the renderer just gets readable
//! UTF-8. Cue settings (`line:`, `position:`, `align:`) are parsed for
//! future use but currently ignored by the overlay.

use std::time::Duration;
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Debug)]
pub struct VttCue {
    /// Cue start time in milliseconds, relative to the same timeline as
    /// the player's `position_ms` (i.e. media-timeline ms, not segment-
    /// relative).
    pub start_ms: i64,
    pub end_ms: i64,
    /// UTF-8 payload, inline tags stripped, line breaks preserved as `\n`.
    pub text: String,
    /// Raw cue settings string ("line:90% position:50% align:center").
    /// Empty when the cue had none.
    pub settings: String,
}

impl VttCue {
    pub fn is_active(&self, pts_ms: i64) -> bool {
        pts_ms >= self.start_ms && pts_ms < self.end_ms
    }

    pub fn duration(&self) -> Duration {
        let ms = (self.end_ms - self.start_ms).max(0) as u64;
        Duration::from_millis(ms)
    }
}

/// Best-effort parse of one DASH text segment. `segment_pts_ms` is the
/// composition timestamp of the first sample in the segment; ISO BMFF
/// VTT cues carry segment-relative timing inside `vttc` boxes but the
/// payload itself uses media-timeline timestamps when present — we keep
/// the simpler behaviour and report cues with segment-relative timing
/// shifted by `segment_pts_ms`.
pub fn parse_segment(data: &[u8], segment_pts_ms: i64) -> Vec<VttCue> {
    // Raw WebVTT text: starts with the literal "WEBVTT" magic. Some
    // sources (HLS-flavoured DASH, sidecar tracks) ship the full WebVTT
    // file directly in `mdat` without ISO BMFF framing.
    if data.windows(6).take(64).any(|w| w == b"WEBVTT") {
        return parse_raw_webvtt(data);
    }
    // Otherwise assume ISO BMFF VTT in a CMAF fragment: walk the box
    // tree and pull samples out of `mdat` using `trun` offsets.
    parse_iso_bmff_vtt(data, segment_pts_ms)
}

// ---------------------------------------------------------------------------
// Raw WebVTT text parser
// ---------------------------------------------------------------------------

fn parse_raw_webvtt(data: &[u8]) -> Vec<VttCue> {
    // UTF-8 lossy — keep going even if the source has a stray invalid
    // byte (some web-scraped subs do). The cue text characters that
    // matter for rendering are virtually always valid UTF-8.
    let mut text = String::from_utf8_lossy(data).into_owned();

    // Strip the optional UTF-8 BOM. WebVTT files served from real CDNs
    // often have it.
    if text.starts_with('\u{FEFF}') {
        text.drain(..'\u{FEFF}'.len_utf8());
    }

    // Normalise line endings so block-splitting works regardless of
    // whether the producer used LF, CRLF, or (legacy Mac) CR. With
    // pure-CRLF files (common from Windows tooling) the original
    // `split("\n\n")` matched nothing — block separators are
    // `\r\n\r\n` and contain no consecutive `\n` chars.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");

    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let block = block.trim_matches(|c: char| c == '\n' || c == ' ' || c == '\t');
        if block.is_empty() {
            continue;
        }
        if block.starts_with("WEBVTT")
            || block.starts_with("STYLE")
            || block.starts_with("REGION")
            || block.starts_with("NOTE")
        {
            continue;
        }
        if let Some(cue) = parse_cue_block(block) {
            out.push(cue);
        }
    }
    out
}

/// Parse one cue block of the form:
/// ```text
/// [identifier]
/// 00:00:01.000 --> 00:00:04.000 position:50% align:center
/// First line of text
/// Second line
/// ```
fn parse_cue_block(block: &str) -> Option<VttCue> {
    let mut lines = block.lines();
    let mut first = lines.next()?.trim();
    // Optional identifier line — if it doesn't contain "-->" the next
    // line is the timing line.
    let timing = if first.contains("-->") {
        first
    } else {
        first = lines.next()?.trim();
        if !first.contains("-->") {
            return None;
        }
        first
    };

    let (timings, rest) = timing.split_once("-->")?;
    // `rest` typically starts with whitespace (`-->` and the end time
    // are space-separated). Trim FIRST, then split on the next
    // whitespace boundary to peel off the end time from any cue
    // settings. The previous version split before trimming, hit the
    // leading space, and ended up with `end_part = ""` — every cue
    // failed to parse and the whole file became zero cues.
    let rest = rest.trim_start();
    let (end_part, settings) = match rest.split_once(char::is_whitespace) {
        Some((e, s)) => (e, s.trim().to_string()),
        None => (rest, String::new()),
    };

    let start_ms = parse_timestamp(timings.trim())?;
    let end_ms = parse_timestamp(end_part)?;

    let mut text = String::new();
    for line in lines {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&strip_inline_tags(line));
    }

    if text.is_empty() {
        return None;
    }
    // fontdue has no shaping engine — collapse decomposed sequences
    // (NFD) to precomposed code points (NFC) so diacritics render as a
    // single glyph the font actually carries.
    let text: String = text.nfc().collect();
    Some(VttCue {
        start_ms,
        end_ms,
        text,
        settings,
    })
}

/// Parse `HH:MM:SS.mmm` or `MM:SS.mmm` to milliseconds.
fn parse_timestamp(s: &str) -> Option<i64> {
    let (time_part, ms_part) = s.split_once('.').unwrap_or((s, "0"));
    let parts: Vec<&str> = time_part.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        [m, s] => (0, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        _ => return None,
    };
    let ms: i64 = ms_part.parse().ok()?;
    Some(((h * 3600 + m * 60 + sec) * 1000) + ms)
}

/// Strip simple WebVTT inline tags. Phase 1 doesn't render styling, so
/// `<b>bold</b>` becomes `bold`, `<c.red>foo</c>` becomes `foo`, etc.
fn strip_inline_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_tag = false;
    for c in line.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// ISO BMFF WebVTT parser (ISO/IEC 14496-30)
// ---------------------------------------------------------------------------
//
// Wire format inside `mdat` is a sequence of WebVTT-sample boxes. Each
// sample (per `trun` size) is either:
//   - `vtte` — empty, no cue payload
//   - One or more `vttc` boxes, each containing children:
//       `payl` — UTF-8 cue payload (mandatory)
//       `sttg` — cue settings string (optional)
//       `iden` — cue identifier (optional)
//       `ctim` — current presentation time (rarely used)
//
// Timing comes from the MP4 sample table (composition_timestamp +
// sample_duration), not from the VTT box. We approximate by using each
// sample's PTS as the cue start and adding the sample's duration for
// the end. For multi-cue samples we share the same window.

fn parse_iso_bmff_vtt(data: &[u8], segment_pts_ms: i64) -> Vec<VttCue> {
    // Use re_mp4 to walk the moof/trun/mdat. Re-use the same approach as
    // the audio/video decoder tasks: extract (offset, size, pts_ms,
    // duration_ms) for each sample.
    let mp4 = match re_mp4::Mp4::read_bytes(data) {
        Ok(m) => m,
        Err(e) => {
            log::debug!("[vtt] mp4 parse failed: {} — segment dropped", e);
            return Vec::new();
        }
    };
    let track = match mp4.tracks().values().next() {
        Some(t) => t,
        None => return Vec::new(),
    };
    let timescale = track.samples.first().map(|s| s.timescale).unwrap_or(1000);

    let mut out = Vec::new();
    for sample in &track.samples {
        let off = sample.offset as usize;
        let size = sample.size as usize;
        if off + size > data.len() {
            continue;
        }
        let sample_data = &data[off..off + size];
        let pts_ms = if timescale > 0 {
            sample.composition_timestamp * 1000 / timescale as i64
        } else {
            segment_pts_ms
        };
        let dur_ms = if timescale > 0 {
            (sample.duration as i64) * 1000 / timescale as i64
        } else {
            2000
        };
        parse_vtt_sample(sample_data, pts_ms, pts_ms + dur_ms, &mut out);
    }
    out
}

fn parse_vtt_sample(sample: &[u8], start_ms: i64, end_ms: i64, out: &mut Vec<VttCue>) {
    let mut i = 0;
    while i + 8 <= sample.len() {
        let size = u32::from_be_bytes([
            sample[i], sample[i + 1], sample[i + 2], sample[i + 3],
        ]) as usize;
        let kind = &sample[i + 4..i + 8];
        if size < 8 || i + size > sample.len() {
            break;
        }
        let body = &sample[i + 8..i + size];
        match kind {
            b"vttc" => {
                if let Some(cue) = parse_vttc(body, start_ms, end_ms) {
                    out.push(cue);
                }
            }
            // `vtte` is the empty box (intentional cue gap). Skip.
            // `vtta` is a comment / additional text — skip.
            _ => {}
        }
        i += size;
    }
}

fn parse_vttc(body: &[u8], start_ms: i64, end_ms: i64) -> Option<VttCue> {
    let mut payload = String::new();
    let mut settings = String::new();
    let mut i = 0;
    while i + 8 <= body.len() {
        let size = u32::from_be_bytes([
            body[i], body[i + 1], body[i + 2], body[i + 3],
        ]) as usize;
        let kind = &body[i + 4..i + 8];
        if size < 8 || i + size > body.len() {
            break;
        }
        let child = &body[i + 8..i + size];
        match kind {
            b"payl" => {
                if let Ok(s) = std::str::from_utf8(child) {
                    for line in s.lines() {
                        if !payload.is_empty() {
                            payload.push('\n');
                        }
                        payload.push_str(&strip_inline_tags(line));
                    }
                }
            }
            b"sttg" => {
                if let Ok(s) = std::str::from_utf8(child) {
                    settings = s.trim().to_string();
                }
            }
            _ => {}
        }
        i += size;
    }
    if payload.is_empty() {
        return None;
    }
    // Same NFC fold-down as the raw-WebVTT path — see parse_cue_block.
    let payload: String = payload.nfc().collect();
    Some(VttCue {
        start_ms,
        end_ms,
        text: payload,
        settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_webvtt() {
        let data = b"WEBVTT\n\n00:00:01.500 --> 00:00:04.000\nHello world\n\n00:00:05.000 --> 00:00:06.250 align:center\nSecond cue";
        let cues = parse_raw_webvtt(data);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ms, 1500);
        assert_eq!(cues[0].end_ms, 4000);
        assert_eq!(cues[0].text, "Hello world");
        assert_eq!(cues[1].settings, "align:center");
    }

    #[test]
    fn parses_crlf_webvtt_with_bom() {
        // Real-world VTT served from Windows tooling: UTF-8 BOM + CRLF.
        let data = b"\xEF\xBB\xBFWEBVTT\r\n\r\n00:00:01.500 --> 00:00:04.000\r\nHello world\r\n\r\n00:00:05.000 --> 00:00:06.250\r\nSecond";
        let cues = parse_raw_webvtt(data);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "Hello world");
        assert_eq!(cues[1].start_ms, 5000);
    }

    #[test]
    fn parses_short_timestamp_form() {
        assert_eq!(parse_timestamp("01:02.345"), Some(62345));
        assert_eq!(parse_timestamp("00:01:02.345"), Some(62345));
    }

    #[test]
    fn strips_inline_tags() {
        assert_eq!(strip_inline_tags("<b>bold</b> <c.red>red</c>"), "bold red");
    }
}
