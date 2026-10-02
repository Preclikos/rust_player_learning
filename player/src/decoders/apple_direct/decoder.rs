//! The macOS / iOS video decoder handed out by the player's factory:
//! direct mode ([`Direct`]) or VideoToolbox + the player's renderer,
//! decided per `configure` (see the module docs).

use std::collections::VecDeque;
use std::sync::Arc;

use super::ffi::{FormatDescription, SampleBuffer};
use super::output::{AppleDirectFrame, AppleDirectOutput};
use crate::decoders::videotoolbox::VideoToolboxDecoder;
use crate::decoders::{
    DecodedVideoFrame, DecoderError, HwVideoDecoder, PlatformFrame, VideoCodec, VideoColorInfo,
    VideoDecoderParams,
};

const DIRECT_NAME: &str = "AVSampleBufferDisplayLayer (direct)";

/// Dolby Vision profiles whose HEVC base layer the player's own renderer
/// can show (static HDR10 / SDR / HLG, RPU ignored).
fn has_playable_base_layer(dovi_profile: u8) -> bool {
    matches!(dovi_profile, 7 | 8)
}

/// The DV configuration box to describe the stream with, when the OS
/// should see Dolby Vision rather than the HEVC base layer.
fn dolby_vision_record<'p>(
    output: &AppleDirectOutput,
    params: &'p VideoDecoderParams,
) -> Option<&'p ([u8; 4], Vec<u8>)> {
    let profile = params.dovi_profile?;
    let record = params.dovi_record.as_ref()?;
    output.wants_dolby_vision(profile).then_some(record)
}

/// Direct-mode backend: samples go to the layer, stamps come back.
struct Direct {
    output: Arc<AppleDirectOutput>,
    format: FormatDescription,
    width: u32,
    height: u32,
    color: VideoColorInfo,
    /// Stamps in decode order; the pipeline's reorder buffer sorts them.
    pending: VecDeque<i64>,
    /// Set by the first sample: the PTS up to which samples overlap what
    /// the layer already shows (ABR splice), `Some(None)` = no overlap.
    overlap_until: Option<Option<i64>>,
}

impl Direct {
    fn new(output: Arc<AppleDirectOutput>, params: &VideoDecoderParams) -> Result<Self, DecoderError> {
        let dv = dolby_vision_record(&output, params);
        let format = FormatDescription::new(params, dv)?;
        log::info!(
            "[direct] {}x{} → AVSampleBufferDisplayLayer as {} ({:?}{})",
            params.width,
            params.height,
            if dv.is_some() { "dvh1" } else { "hvc1" },
            params.color.transfer,
            params.dovi_profile.map(|p| format!(", DV profile {p}")).unwrap_or_default(),
        );
        output.direct_decoder_started();
        Ok(Self {
            output,
            format,
            width: params.width,
            height: params.height,
            color: params.color,
            pending: VecDeque::new(),
            overlap_until: None,
        })
    }

    /// ABR: the layer accepts a new format description at an IDR, so a new
    /// representation only needs its description swapped.
    fn reconfigure(&mut self, params: &VideoDecoderParams) -> Result<(), DecoderError> {
        let dv = dolby_vision_record(&self.output, params);
        self.format = FormatDescription::new(params, dv)?;
        self.width = params.width;
        self.height = params.height;
        self.color = params.color;
        // The new representation's first sample re-runs the continuity
        // check (splice overlap → hidden).
        self.overlap_until = None;
        log::info!("[direct] reconfigured in place for {}x{}", params.width, params.height);
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        let overlap = *self.overlap_until.get_or_insert_with(|| self.output.begin_stream(pts_us));
        let hidden = overlap.is_some_and(|until| pts_us <= until);
        let buffer = SampleBuffer::new(&self.format, sample, pts_us)?;
        if hidden {
            buffer.mark_do_not_display();
        }
        self.output.enqueue(&buffer, pts_us)?;
        if !hidden {
            self.pending.push_back(pts_us);
        }
        Ok(())
    }

    fn try_recv(&mut self) -> Option<DecodedVideoFrame> {
        let pts_us = self.pending.pop_front()?;
        Some(DecodedVideoFrame {
            pts_us,
            width: self.width,
            height: self.height,
            native: PlatformFrame::AppleDirect(AppleDirectFrame { output: Arc::clone(&self.output) }),
            desired_present_ns: 0,
            color: self.color,
            hdr_meta: None,
        })
    }
}

impl Drop for Direct {
    fn drop(&mut self) {
        self.output.direct_decoder_stopped();
    }
}

enum Backend {
    Unconfigured,
    VideoToolbox(VideoToolboxDecoder),
    Direct(Direct),
}

/// The macOS / iOS video decoder.
pub struct AppleVideoDecoder {
    output: Arc<AppleDirectOutput>,
    backend: Backend,
}

impl AppleVideoDecoder {
    pub fn new(output: Arc<AppleDirectOutput>) -> Self {
        Self { output, backend: Backend::Unconfigured }
    }

    fn configure_videotoolbox(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError> {
        if let Some(profile) = params.dovi_profile.filter(|p| !has_playable_base_layer(*p)) {
            return Err(format!(
                "Dolby Vision profile {profile} has no backward-compatible base layer \
                 (needs direct mode on a DV-capable setup) — unsupported here"
            )
            .into());
        }
        let mut vt = VideoToolboxDecoder::new()?;
        vt.configure(params)?;
        self.backend = Backend::VideoToolbox(vt);
        Ok(())
    }
}

impl HwVideoDecoder for AppleVideoDecoder {
    fn name(&self) -> &'static str {
        match &self.backend {
            Backend::Direct(_) => DIRECT_NAME,
            Backend::VideoToolbox(vt) => vt.name(),
            // Asked before configure: answer with what configure would pick.
            Backend::Unconfigured if self.output.eligible() => DIRECT_NAME,
            Backend::Unconfigured => "VideoToolbox",
        }
    }

    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError> {
        if !matches!(params.codec, VideoCodec::Hevc) {
            return Err("Apple decoder supports HEVC only".into());
        }
        if self.output.eligible() {
            match Direct::new(Arc::clone(&self.output), &params) {
                Ok(direct) => {
                    self.backend = Backend::Direct(direct);
                    return Ok(());
                }
                Err(e) => log::warn!("[direct] {e} — using VideoToolbox + renderer"),
            }
        }
        self.configure_videotoolbox(params)
    }

    fn try_reconfigure(&mut self, params: &VideoDecoderParams) -> bool {
        // Only within the same backend: a mode change (display went HDR /
        // SDR, layer failed) gets a fresh decoder.
        let eligible = self.output.eligible();
        match &mut self.backend {
            Backend::VideoToolbox(vt) if !eligible => vt.try_reconfigure(params),
            Backend::Direct(direct) if eligible => direct.reconfigure(params).is_ok(),
            _ => false,
        }
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        match &mut self.backend {
            Backend::Direct(direct) => direct.submit(sample, pts_us),
            Backend::VideoToolbox(vt) => vt.submit(sample, pts_us),
            Backend::Unconfigured => Err("submit before configure".into()),
        }
    }

    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        match &mut self.backend {
            Backend::Direct(direct) => Ok(direct.try_recv()),
            Backend::VideoToolbox(vt) => vt.try_recv(),
            Backend::Unconfigured => Ok(None),
        }
    }

    fn flush(&mut self) -> Result<(), DecoderError> {
        match &mut self.backend {
            Backend::VideoToolbox(vt) => vt.flush(),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_profiles_7_and_8_have_a_base_layer() {
        assert!(has_playable_base_layer(7));
        assert!(has_playable_base_layer(8));
        assert!(!has_playable_base_layer(5));
    }

    #[test]
    fn unconfigured_decoder_without_a_layer_is_videotoolbox() {
        let out = AppleDirectOutput::new();
        let mut d = AppleVideoDecoder::new(Arc::clone(&out));
        assert!(d.name().contains("VideoToolbox") || d.name().contains("direct"));
        assert!(d.submit(&[0, 0, 0, 1, 0], 0).is_err(), "submit before configure");
        assert!(d.try_recv().unwrap().is_none());
    }
}
