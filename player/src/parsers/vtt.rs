//! WebVTT cue extraction from DASH text segments.
//!
//! DASH ships WebVTT either as ISO/IEC 14496-30 ISO BMFF (samples wrap
//! `vttc` / `vtte` boxes with `payl` payloads) or — less commonly — as
//! raw WebVTT text inside `mdat`. The entry point [`parse_segment`] sniffs
//! both forms.
//!
//! Text scope: plain-text cues. We strip any inline tags (`<b>`, `<i>`,
//! `<c.classname>` …) so the renderer just gets readable UTF-8. Cue
//! settings (`line:`, `position:`, `align:`, `size:`) are parsed into
//! [`CueLayout`] and drive where the overlay puts the cue — the same
//! fields, defaults and derivations as ExoPlayer's `WebvttCueParser`
//! (`Cue.line`/`lineType`/`lineAnchor`, `position`/`positionAnchor`,
//! `size`, `textAlignment`). `region:` and `vertical:` are accepted and
//! ignored (vertical text renders horizontally).

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
    /// `settings` parsed — what the overlay actually positions by.
    pub layout: CueLayout,
}

/// Where a cue goes, from its WebVTT cue settings. Field for field what
/// ExoPlayer's `WebvttCueParser` fills on `Cue`, with the same defaults
/// when a setting is absent or malformed (that setting is skipped, the
/// rest still apply).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CueLayout {
    /// `line:` — `Auto` when absent (renderer default placement).
    pub line: CueLine,
    /// `line:…,start|center|end` — which edge of the cue box `line` pins.
    pub line_anchor: Anchor,
    /// `position:` as a fraction `0..=1` of the parent width; `None` =
    /// derived from `align` (left → 0, right → 1, else 0.5).
    pub position: Option<f32>,
    /// `position:…,line-left|center|line-right`; `None` = derived from
    /// `align` (left/start → Start, right/end → End, else Middle).
    pub position_anchor: Option<Anchor>,
    /// `align:` — text alignment inside the cue box. Default center.
    pub align: TextAlign,
    /// `size:` as a fraction `0..=1` of the available width. Default 1.
    pub size: f32,
    /// A `vertical:` setting was present (rendered horizontally anyway).
    pub vertical: bool,
}

/// `line:` value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CueLine {
    /// No `line:` — the renderer's default (bottom padding) placement.
    Auto,
    /// `line:N%` — fraction `0..=1` of the parent height (`Cue.LINE_TYPE_FRACTION`).
    Fraction(f32),
    /// `line:N` — line number; `0` = first line at the top, `-1` = last
    /// line at the bottom (`Cue.LINE_TYPE_NUMBER`).
    Number(i32),
}

/// `Cue.ANCHOR_TYPE_*`: which edge of the cue box an anchor coordinate pins.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    Start,
    Middle,
    End,
}

/// `align:` values. `Start`/`End` follow the text direction; the overlay
/// has no bidi layout and treats them as left/right.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextAlign {
    Start,
    Center,
    End,
    Left,
    Right,
}

impl CueLayout {
    pub const DEFAULT: Self = Self {
        line: CueLine::Auto,
        line_anchor: Anchor::Start,
        position: None,
        position_anchor: None,
        align: TextAlign::Center,
        size: 1.0,
        vertical: false,
    };

    /// Parse a cue settings string (`line:90% position:50%,line-left
    /// align:center size:80%`). Unknown names and malformed values are
    /// skipped individually, like ExoPlayer's per-setting `try/catch`.
    pub fn parse(settings: &str) -> Self {
        let mut l = Self::DEFAULT;
        for setting in settings.split_ascii_whitespace() {
            let Some((name, value)) = setting.split_once(':') else { continue };
            match name {
                "line" => {
                    let (value, anchor) = split_anchor(value);
                    if let Some(a) = anchor.and_then(parse_line_anchor) {
                        l.line_anchor = a;
                    }
                    if let Some(pct) = value.strip_suffix('%') {
                        if let Some(f) = parse_fraction(pct) {
                            l.line = CueLine::Fraction(f);
                        }
                    } else if let Ok(n) = value.parse::<i32>() {
                        l.line = CueLine::Number(n);
                    }
                }
                "position" => {
                    let (value, anchor) = split_anchor(value);
                    if let Some(a) = anchor.and_then(parse_position_anchor) {
                        l.position_anchor = Some(a);
                    }
                    if let Some(f) = value.strip_suffix('%').and_then(parse_fraction) {
                        l.position = Some(f);
                    }
                }
                "size" => {
                    if let Some(f) = value.strip_suffix('%').and_then(parse_fraction) {
                        l.size = f;
                    }
                }
                "align" => {
                    l.align = match value {
                        "start" => TextAlign::Start,
                        "center" | "middle" => TextAlign::Center,
                        "end" => TextAlign::End,
                        "left" => TextAlign::Left,
                        "right" => TextAlign::Right,
                        _ => l.align,
                    }
                }
                "vertical" => l.vertical = matches!(value, "rl" | "lr"),
                _ => {}
            }
        }
        l
    }

    /// Effective `position` (fraction) — explicit, else derived from
    /// `align` exactly as `WebvttCueParser.derivePosition`.
    pub fn position_or_derived(&self) -> f32 {
        self.position.unwrap_or(match self.align {
            TextAlign::Left => 0.0,
            TextAlign::Right => 1.0,
            _ => 0.5,
        })
    }

    /// Effective position anchor — explicit, else derived from `align`
    /// exactly as `WebvttCueParser.derivePositionAnchor`.
    pub fn position_anchor_or_derived(&self) -> Anchor {
        self.position_anchor.unwrap_or(match self.align {
            TextAlign::Left | TextAlign::Start => Anchor::Start,
            TextAlign::Right | TextAlign::End => Anchor::End,
            TextAlign::Center => Anchor::Middle,
        })
    }
}

impl Default for CueLayout {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `value[,anchor]` → (`value`, `Some(anchor)`).
fn split_anchor(value: &str) -> (&str, Option<&str>) {
    match value.split_once(',') {
        Some((v, a)) => (v, Some(a)),
        None => (value, None),
    }
}

/// `"12.5"` (the part before `%`) → `0.125`; out-of-range or non-numeric → None.
fn parse_fraction(pct: &str) -> Option<f32> {
    pct.parse::<f32>()
        .ok()
        .filter(|f| f.is_finite() && (0.0..=100.0).contains(f))
        .map(|f| f / 100.0)
}

fn parse_line_anchor(s: &str) -> Option<Anchor> {
    match s {
        "start" => Some(Anchor::Start),
        "center" => Some(Anchor::Middle),
        "end" => Some(Anchor::End),
        _ => None,
    }
}

fn parse_position_anchor(s: &str) -> Option<Anchor> {
    match s {
        "line-left" => Some(Anchor::Start),
        "center" => Some(Anchor::Middle),
        "line-right" => Some(Anchor::End),
        _ => None,
    }
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

pub(crate) fn parse_raw_webvtt(data: &[u8]) -> Vec<VttCue> {
    // UTF-8 lossy — keep going even if the source has a stray invalid
    // byte (some web-scraped subs do). The cue text characters that
    // matter for rendering are virtually always valid UTF-8.
    //
    // Host-supplied sidecar files take the `parsers::sidecar` route
    // instead, which sniffs the charset first; segments off the wire are
    // UTF-8 per spec, so lossy decoding is the right call here.
    parse_cue_blocks(&String::from_utf8_lossy(data))
}

/// Split an already-decoded WebVTT / SubRip body into cues.
///
/// Both formats are "blocks separated by a blank line, each block an
/// optional identifier line, a `-->` timing line, then the text", so one
/// parser covers them: `parse_timestamp` takes SubRip's comma as readily
/// as WebVTT's decimal point, and SubRip's sequence number lands on the
/// identifier line the WebVTT grammar already allows.
pub(crate) fn parse_cue_blocks(body: &str) -> Vec<VttCue> {
    let mut text = body.to_string();

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
pub(crate) fn parse_cue_block(block: &str) -> Option<VttCue> {
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
        text.push_str(&clean_cue_line(line));
    }

    if text.is_empty() {
        return None;
    }
    // fontdue has no shaping engine — collapse decomposed sequences
    // (NFD) to precomposed code points (NFC) so diacritics render as a
    // single glyph the font actually carries.
    let text: String = text.nfc().collect();
    let layout = CueLayout::parse(&settings);
    Some(VttCue {
        start_ms,
        end_ms,
        text,
        settings,
        layout,
    })
}

/// Parse `HH:MM:SS.mmm` or `MM:SS.mmm` to milliseconds.
///
/// A comma is accepted in place of the decimal point: SubRip writes
/// `00:00:01,000`, and sidecar `.srt` files otherwise go through exactly
/// the same block parser as WebVTT.
fn parse_timestamp(s: &str) -> Option<i64> {
    let (time_part, ms_part) = s
        .split_once(['.', ','])
        .unwrap_or((s, "0"));
    let parts: Vec<&str> = time_part.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        [m, s] => (0, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        _ => return None,
    };
    let ms: i64 = ms_part.parse().ok()?;
    Some(((h * 3600 + m * 60 + sec) * 1000) + ms)
}

/// Turn one WebVTT cue payload line into renderable text: strip inline tags,
/// decode character references, drop invisible bidi/zero-width controls.
fn clean_cue_line(line: &str) -> String {
    strip_bidi_controls(&decode_entities(&strip_inline_tags(line)))
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

/// Decode the character references WebVTT cue text may carry (the spec's
/// named set — `&amp; &lt; &gt; &lrm; &rlm; &nbsp;` — plus the HTML-common
/// `&quot; &apos;` and numeric `&#NNN;` / `&#xHHH;`). Browser/ExoPlayer
/// renderers go through an HTML-ish text pipeline that decodes these; our
/// rasterizer draws the string verbatim, so without this a `&lrm;` (very
/// common in Arabic/Hebrew-aware subtitle exports, also sprinkled into
/// Latin-script files by some tools) showed up as the literal six characters.
/// Unknown references are left as-is.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        // A reference is `&` + up to ~10 chars + `;` with no whitespace.
        let decoded = after[1..]
            .find(';')
            .filter(|&semi| semi > 0 && semi <= 10)
            .and_then(|semi| {
                let name = &after[1..1 + semi];
                let ch = match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some('\u{00A0}'),
                    "lrm" => Some('\u{200E}'),
                    "rlm" => Some('\u{200F}'),
                    _ if name.starts_with("#x") || name.starts_with("#X") => {
                        u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32)
                    }
                    _ if name.starts_with('#') => {
                        name[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => None,
                };
                ch.map(|c| (c, 2 + semi))
            });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &after[len..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Remove Unicode bidi / zero-width control characters. They carry no glyph;
/// fontdue has no bidi engine to act on them and would draw a `.notdef` box
/// (or nothing, font-dependent) — either way they must not reach the
/// rasterizer. Covers LRM/RLM/ALM, the LRE…PDF embeddings, the LRI…PDI
/// isolates, ZWSP/ZWNJ/ZWJ and the BOM.
fn strip_bidi_controls(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !matches!(
                *c,
                '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{061C}' | '\u{FEFF}'
            )
        })
        .collect()
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
                        payload.push_str(&clean_cue_line(line));
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
    let layout = CueLayout::parse(&settings);
    Some(VttCue {
        start_ms,
        end_ms,
        text: payload,
        settings,
        layout,
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
        assert_eq!(cues[1].layout.align, TextAlign::Center);
        assert_eq!(cues[0].layout, CueLayout::DEFAULT);
    }

    #[test]
    fn cue_settings_map_like_exoplayers_webvtt_cue_parser() {
        // Absent → the same defaults WebvttCueParser starts from.
        assert_eq!(CueLayout::parse(""), CueLayout::DEFAULT);

        let l = CueLayout::parse("line:90% position:20%,line-left align:left size:60%");
        assert_eq!(l.line, CueLine::Fraction(0.9));
        assert_eq!(l.line_anchor, Anchor::Start);
        assert_eq!(l.position, Some(0.2));
        assert_eq!(l.position_anchor, Some(Anchor::Start));
        assert_eq!(l.align, TextAlign::Left);
        assert!((l.size - 0.6).abs() < 1e-6);

        // Integer line numbers, negative counts from the bottom; the
        // anchor suffix applies to percentages and numbers alike.
        assert_eq!(CueLayout::parse("line:-2").line, CueLine::Number(-2));
        let l = CueLayout::parse("line:0,end");
        assert_eq!(l.line, CueLine::Number(0));
        assert_eq!(l.line_anchor, Anchor::End);
        assert_eq!(CueLayout::parse("line:50%,center").line_anchor, Anchor::Middle);

        // Derived position/anchor follow the alignment when unset.
        let right = CueLayout::parse("align:right");
        assert_eq!(right.position_or_derived(), 1.0);
        assert_eq!(right.position_anchor_or_derived(), Anchor::End);
        let start = CueLayout::parse("align:start");
        assert_eq!(start.position_or_derived(), 0.5);
        assert_eq!(start.position_anchor_or_derived(), Anchor::Start);
        assert_eq!(CueLayout::DEFAULT.position_or_derived(), 0.5);
        assert_eq!(CueLayout::DEFAULT.position_anchor_or_derived(), Anchor::Middle);

        // Malformed values skip only that setting; the rest still apply.
        let l = CueLayout::parse("line:abc position:150% size:x% align:right vertical:rl region:r1");
        assert_eq!(l.line, CueLine::Auto);
        assert_eq!(l.position, None);
        assert_eq!(l.size, 1.0);
        assert_eq!(l.align, TextAlign::Right);
        assert!(l.vertical);
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

    #[test]
    fn decodes_character_references() {
        assert_eq!(
            decode_entities("Tom &amp; Jerry &lt;3 &gt; &quot;hi&quot;"),
            "Tom & Jerry <3 > \"hi\""
        );
        assert_eq!(decode_entities("a&nbsp;b"), "a\u{00A0}b");
        assert_eq!(decode_entities("&#65;&#x42;&#X43;"), "ABC");
        // Unknown / malformed references are left alone.
        assert_eq!(decode_entities("&bogus; & &amp"), "&bogus; & &amp");
        assert_eq!(decode_entities("no refs"), "no refs");
    }

    #[test]
    fn lrm_marks_do_not_reach_the_renderer() {
        // The field report: exports carrying `&lrm;` rendered the literal
        // six characters. Both the reference form and a raw U+200E vanish.
        assert_eq!(clean_cue_line("&lrm;- Ahoj.&lrm;"), "- Ahoj.");
        assert_eq!(clean_cue_line("\u{200E}<i>Ne\u{200F}</i>"), "Ne");
        assert_eq!(
            clean_cue_line("\u{202B}x\u{202C} \u{2066}y\u{2069} \u{200B}z"),
            "x y z"
        );
    }

    #[test]
    fn cue_text_is_cleaned_end_to_end() {
        let data = b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n&lrm;<b>Tom &amp; Jerry</b>\n";
        let cues = parse_raw_webvtt(data);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Tom & Jerry");
    }
}
