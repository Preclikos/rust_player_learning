//! Decoder support probe: drop representations this platform cannot decode
//! BEFORE track selection and ABR see them, so the default pick, the auto
//! switch and the host's track list only ever name playable rungs.
//!
//! Browser: WebCodecs `VideoDecoder.isConfigSupported` /
//! `AudioDecoder.isConfigSupported`, asked per representation with the
//! manifest's `codecs` string (`hvc1.2.4.L150.90`, `mp4a.40.2`, `ec-3`, …).
//! This is the browser's own answer — no JS glue in the host, and no
//! maintained table of what each browser can do. Everything else keeps the
//! manifest as-is (native decoders report unsupported codecs at configure
//! time through the pipeline's error path, as before).

use crate::tracks::Tracks;

/// Remove unsupported representations (and adaptations left empty) from
/// `tracks`. Logs what it dropped. Never removes the last video or audio
/// representation on a probe FAILURE (API missing, promise rejected) — only
/// on a definite "not supported".
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn prune_unsupported(_tracks: &mut Tracks) {}

#[cfg(target_arch = "wasm32")]
pub(crate) async fn prune_unsupported(tracks: &mut Tracks) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    async fn video_supported(codecs: &str, width: u32, height: u32) -> Option<bool> {
        let cfg = web_sys::VideoDecoderConfig::new(codecs);
        if width > 0 && height > 0 {
            cfg.set_coded_width(width);
            cfg.set_coded_height(height);
        }
        let support: web_sys::VideoDecoderSupport =
            JsFuture::from(web_sys::VideoDecoder::is_config_supported(&cfg)).await.ok()?.unchecked_into();
        support.get_supported()
    }

    async fn audio_supported(codecs: &str, channels: u32, sample_rate: u32) -> Option<bool> {
        let cfg = web_sys::AudioDecoderConfig::new(codecs, channels.max(1), sample_rate.max(8_000));
        let support: web_sys::AudioDecoderSupport =
            JsFuture::from(web_sys::AudioDecoder::is_config_supported(&cfg)).await.ok()?.unchecked_into();
        support.get_supported()
    }

    let mut dropped: Vec<String> = Vec::new();

    for adapt in &mut tracks.video {
        let mut keep = Vec::with_capacity(adapt.representations.len());
        for r in adapt.representations.drain(..) {
            match video_supported(&r.codecs, r.width, r.height).await {
                Some(false) => dropped.push(format!("video {}x{} {}", r.width, r.height, r.codecs)),
                // Supported, or the probe itself failed: keep and let the
                // decoder's configure() have the final word.
                _ => keep.push(r),
            }
        }
        adapt.representations = keep;
    }
    tracks.video.retain(|a| !a.representations.is_empty());

    for adapt in &mut tracks.audio {
        let mut keep = Vec::with_capacity(adapt.representations.len());
        for r in adapt.representations.drain(..) {
            let ch = r.channels.unwrap_or(2);
            match audio_supported(&r.codecs, ch, r.audio_sampling_rate).await {
                Some(false) => dropped.push(format!("audio {} {}ch {}Hz", r.codecs, ch, r.audio_sampling_rate)),
                _ => keep.push(r),
            }
        }
        adapt.representations = keep;
    }
    tracks.audio.retain(|a| !a.representations.is_empty());

    if dropped.is_empty() {
        log::info!("[support] every representation is decodable here (WebCodecs isConfigSupported)");
    } else {
        log::info!(
            "[support] {} representation(s) this browser cannot decode were removed: {}",
            dropped.len(),
            dropped.join("; ")
        );
    }
}
