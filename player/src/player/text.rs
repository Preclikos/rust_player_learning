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
pub(super) async fn text_play<V: VideoSink>(
    text_representation: crate::tracks::text::TextRepresenation,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    http: Arc<HttpClient>,
    video_sink: Arc<V>,
    active: Arc<StdMutex<Option<crate::tracks::text::TextRepresenation>>>,
    target_id: u32,
) -> Result<(), Box<dyn Error + Send + Sync>> {
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
        let dl_fut = http.get(url.clone(), RequestKind::InitSegment);
        let bytes = tokio::select! {
            r = dl_fut => match r {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("[subs] single-file download failed: {}", e);
                    return Ok(());
                }
            },
            _ = stop.notified() => return Ok(()),
        };
        if !still_selected(&active) {
            return Ok(());
        }
        let cues = crate::parsers::vtt::parse_segment(&bytes, 0);
        log::info!(
            "[subs] parsed {} cues from {} bytes (single VTT file)",
            cues.len(),
            bytes.len()
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

    for (i, seg) in text_representation.segments.iter().enumerate() {
        if stop_flag.load(Ordering::Relaxed) || !still_selected(&active) {
            break;
        }
        let dl_fut = seg.download(&http, RequestKind::Segment);
        let dl = tokio::select! {
            r = dl_fut => r,
            _ = stop.notified() => break,
        };
        match dl {
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


