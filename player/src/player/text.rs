//! Subtitle track plumbing: host-supplied sidecar tracks and the task that
//! feeds cues to the renderer.

use super::*;

// ---------------------------------------------------------------------------
// Subtitle (WebVTT) pipeline
// ---------------------------------------------------------------------------

/// Turn a parsed sidecar file into the track pair the rest of the player
/// speaks: one representation carrying the cues, wrapped in its own
/// adaptation set (a sidecar file has its own language and role, which is
/// exactly what an adaptation set is for).
pub(super) fn external_track(
    id: u32,
    parsed: crate::parsers::sidecar::ParsedSidecar,
    options: &ExternalSubtitleOptions,
) -> (crate::tracks::text::TextRepresenation, crate::tracks::text::TextAdaptation) {
    let mime_type = match parsed.format {
        crate::parsers::sidecar::SubtitleFormat::SubRip => "application/x-subrip",
        _ => "text/vtt",
    }
    .to_string();

    let representation = crate::tracks::text::TextRepresenation {
        id,
        // `wvtt` regardless of the source format: by this point the cues
        // are parsed and everything downstream only sees WebVTT cues.
        codecs: "wvtt".to_string(),
        mime_type,
        bandwidth: 0,
        base_url: String::new(),
        file_url: String::new(),
        segment_init: None,
        segment_range: None,
        segments: Vec::new(),
        single_file_url: None,
        external_cues: Some(Arc::new(parsed.cues)),
    };

    let mut roles = vec!["subtitle".to_string()];
    if options.forced {
        roles.push("forced-subtitle".to_string());
    }
    let adaptation = crate::tracks::text::TextAdaptation {
        id,
        lang: options.language.clone().unwrap_or_default(),
        roles,
        representations: vec![representation.clone()],
    };
    (representation, adaptation)
}

/// Fetch + parse the selected subtitle representation, push cues into
/// the video sink so the wgpu overlay can render them.
///
/// Two delivery patterns are supported:
///   1. Single-file VTT (the common "external .vtt per language"
///      pattern) — `single_file_url` set, segments empty. Download
///      whole file once, parse as raw WebVTT, hand off, done.
///   2. ISO BMFF VTT in CMAF — `segment_init` + `segments` populated.
///      Stream segments through the normal download path, parse each
///      as `vttc` boxes, push cues incrementally.
///
/// Both paths skip silently if the representation isn't decodable
/// (TTML for now).
/// `active` is the live cell holding the currently-selected subtitle
/// representation. If the consumer flips it (via clear_subtitle_track
/// or set_subtitle_track to a different track) the task notices between
/// operations and exits so stale downloads stop wasting bandwidth.
///
/// Only a real `Player::stop()` (a new `stop_epoch`) ends the task. It used
/// to share the player's per-pipeline stop signal, which every seek and
/// audio switch fires: a seek during the single-file download, or any seek
/// during CMAF streaming, ended the subtitles for good.
pub(super) async fn text_play<V: VideoSink>(
    text_representation: crate::tracks::text::TextRepresenation,
    stop_epoch: Arc<AtomicU64>,
    http: Arc<HttpClient>,
    video_sink: Arc<V>,
    active: Arc<StdMutex<Option<crate::tracks::text::TextRepresenation>>>,
    target_id: u32,
    position_ms: Arc<AtomicU64>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let epoch_at_start = stop_epoch.load(Ordering::SeqCst);
    let stopped = || stop_epoch.load(Ordering::SeqCst) != epoch_at_start;
    // Helper: did the consumer change subtitle selection out from under us?
    let still_selected = |active: &Arc<StdMutex<Option<crate::tracks::text::TextRepresenation>>>| -> bool {
        active
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.id == target_id)
            .unwrap_or(false)
    };

    // ---- host-supplied cues: already parsed, nothing to fetch ----
    if let Some(cues) = &text_representation.external_cues {
        log::info!(
            "[subs] external track {} selected: {} cues",
            text_representation.id,
            cues.len()
        );
        video_sink.queue_subtitle_cues(cues.as_ref().clone());
        return Ok(());
    }

    if !text_representation.is_webvtt() {
        log::info!(
            "[subs] representation {} is not WebVTT ({}/{}) — skipping",
            text_representation.id,
            text_representation.codecs,
            text_representation.mime_type,
        );
        return Ok(());
    }

    // ---- single-file delivery ----
    if let Some(url) = &text_representation.single_file_url {
        log::info!("[subs] downloading single-file VTT: {}", url);
        let bytes = match http.get(url.clone(), RequestKind::InitSegment).await {
            Ok(b) => b,
            Err(e) => {
                log::warn!("[subs] single-file download failed: {}", e);
                return Ok(());
            }
        };
        if stopped() || !still_selected(&active) {
            return Ok(());
        }
        let cues = crate::parsers::vtt::parse_segment(&bytes, 0);
        log::info!(
            "[subs] parsed {} cues from {} bytes (single VTT file); {}",
            cues.len(),
            bytes.len(),
            settings_summary(&cues)
        );
        // Diagnostic for "Czech (or any non-ASCII) renders wrong" reports:
        // if the source isn't UTF-8 (some Windows-1250 / ISO-8859-2 VTT
        // files exist in the wild despite the spec) we'd see U+FFFD
        // replacement chars in the text. If the source IS UTF-8 but the
        // consumer's font lacks Latin Extended-A glyphs, the text here
        // looks fine and the problem is the font. The byte preview lets
        // us tell which.
        if let Some(first) = cues.first() {
            let head = first.text.chars().take(80).collect::<String>();
            let raw_head: String = bytes
                .iter()
                .take(160)
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(" ");
            log::debug!(
                "[subs] first cue text {:?} (chars={}); raw bytes head: {}",
                head,
                first.text.chars().count(),
                raw_head
            );
        }
        if cues.is_empty() {
            // Dump a hex+ASCII preview of the first 256 bytes so future
            // "0 cues" reports show what we actually got — line endings,
            // BOM, or some upstream format we don't recognise yet.
            let preview_len = bytes.len().min(256);
            let mut hex_dump = String::new();
            let mut ascii_dump = String::new();
            for &b in &bytes[..preview_len] {
                hex_dump.push_str(&format!("{:02x} ", b));
                ascii_dump.push(if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else if b == b'\n' {
                    '↵'
                } else if b == b'\r' {
                    '⏎'
                } else {
                    '.'
                });
            }
            log::warn!(
                "[subs] no cues parsed — preview ({} bytes):\n  hex: {}\n  txt: {}",
                preview_len, hex_dump, ascii_dump
            );
        }
        video_sink.queue_subtitle_cues(cues);
        return Ok(());
    }

    // ---- ISO BMFF CMAF delivery ----
    let init = match &text_representation.segment_init {
        Some(s) => s,
        None => {
            log::info!(
                "[subs] representation {} has no segments and no single-file URL",
                text_representation.id
            );
            return Ok(());
        }
    };
    let _ = init.download(&http, RequestKind::InitSegment).await;

    // Segments are fetched in a window around the playhead: the one under it
    // first, then up to CMAF_LOOKAHEAD ahead. The whole title used to be
    // fetched back to back from the first segment at start (thousands of
    // requests on a film, competing with the video download). A seek, either
    // way, just moves the window; what was fetched stays queued.
    let segments = &text_representation.segments;
    let origin = segments.first().map(|s| s.start_time()).unwrap_or_default();
    let mut fetched = vec![false; segments.len()];
    let mut remaining = segments.len();
    while remaining > 0 {
        if stopped() || !still_selected(&active) {
            break;
        }
        let pos = Duration::from_millis(position_ms.load(Ordering::Relaxed));
        let Some(i) = next_text_segment(segments, &fetched, origin, pos, CMAF_LOOKAHEAD) else {
            crate::rt::sleep(Duration::from_millis(500)).await;
            continue;
        };
        fetched[i] = true;
        remaining -= 1;
        let seg = &segments[i];
        match seg.download(&http, RequestKind::Segment).await {
            Ok(d) => {
                let pts_ms = seg.start_time().as_millis() as i64;
                let cues = crate::parsers::vtt::parse_segment(&d.data, pts_ms);
                if !cues.is_empty() {
                    log::debug!("[subs] segment {} produced {} cues", i, cues.len());
                    video_sink.queue_subtitle_cues(cues);
                }
            }
            Err(e) => {
                log::warn!("[subs] segment {} download failed: {}", i, e);
            }
        }
    }
    Ok(())
}



/// How far past the playhead CMAF subtitle segments are fetched.
const CMAF_LOOKAHEAD: Duration = Duration::from_secs(60);

/// The text segment to fetch next: the unfetched one under the playhead,
/// else the first unfetched one starting within `lookahead` of it. Times are
/// relative to the first segment (the playhead is 0-based). `None` = nothing
/// due yet.
fn next_text_segment(
    segments: &[crate::tracks::segment::Segment],
    fetched: &[bool],
    origin: Duration,
    pos: Duration,
    lookahead: Duration,
) -> Option<usize> {
    let rel = |t: Duration| t.saturating_sub(origin);
    let horizon = pos + lookahead;
    (0..segments.len()).find(|&i| {
        !fetched[i] && rel(segments[i].end_time()) > pos && rel(segments[i].start_time()) <= horizon
    })
}

/// One line on the cue settings a track carries — the placement input the
/// renderer honours and the first thing to check when "our subtitles sit
/// higher/lower than player X" comes in: `line:`/`position:` in the file
/// override every default, in every player.
fn settings_summary(cues: &[crate::parsers::vtt::VttCue]) -> String {
    let with = cues.iter().filter(|c| !c.settings.is_empty()).count();
    if with == 0 {
        return "no cue settings (default placement)".to_string();
    }
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for c in cues {
        if !c.settings.is_empty() {
            *counts.entry(c.settings.as_str()).or_insert(0) += 1;
        }
    }
    let mut top: Vec<(&str, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let shown: Vec<String> = top.iter().take(3).map(|(s, n)| format!("{:?}×{}", s, n)).collect();
    format!("{} with settings, most common {}", with, shown.join(", "))
}

#[cfg(test)]
mod tests {
    use super::next_text_segment;
    use crate::tracks::segment::Segment;
    use std::time::Duration;

    fn segs(n: u64, secs: u64, origin_s: u64) -> Vec<Segment> {
        (0..n)
            .map(|i| {
                let (a, b) = ((origin_s + i * secs) * 1000, (origin_s + (i + 1) * secs) * 1000);
                Segment::new(&"http://x/".to_string(), &format!("{i}.m4s"), 0, 0, Some(a), Some(b), Some(1000)).unwrap()
            })
            .collect()
    }

    #[test]
    fn text_segments_follow_the_playhead_window() {
        // 100 segments of 6 s, presentation times starting at 3600 s.
        let s = segs(100, 6, 3600);
        let origin = s[0].start_time();
        let ahead = Duration::from_secs(60);
        let mut fetched = vec![false; s.len()];
        let next = |f: &[bool], pos_s: u64| next_text_segment(&s, f, origin, Duration::from_secs(pos_s), ahead);
        // Start at 0: segment 0 first, then only up to 60 s ahead (0..=10).
        for want in 0..=10 {
            assert_eq!(next(&fetched, 0), Some(want));
            fetched[want] = true;
        }
        assert_eq!(next(&fetched, 0), None);
        // Seek to 300 s: the segment under the playhead comes first.
        assert_eq!(next(&fetched, 300), Some(50));
        fetched[50] = true;
        assert_eq!(next(&fetched, 300), Some(51));
        // Seek back to 30 s: already fetched there, so only the window's tail.
        assert_eq!(next(&fetched, 30), Some(11));
    }
}
