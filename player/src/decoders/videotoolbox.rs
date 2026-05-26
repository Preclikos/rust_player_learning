// iOS VideoToolbox-based HwVideoDecoder.
//
// Status: skeleton. Will use `core-video` / `video-toolbox` crates or raw
// CoreFoundation bindings via `core-foundation-sys`. Produces
// `CVPixelBuffer` outputs that wgpu can import as Metal textures via
// IOSurface.

#![cfg(target_os = "ios")]

use super::{DecodedVideoFrame, DecoderError, HwVideoDecoder, VideoDecoderParams};

pub struct VideoToolboxDecoder {
    // TODO
}

impl VideoToolboxDecoder {
    pub fn new() -> Result<Self, DecoderError> {
        Ok(Self {})
    }
}

impl HwVideoDecoder for VideoToolboxDecoder {
    fn name(&self) -> &'static str {
        "VideoToolbox"
    }

    fn configure(&mut self, _params: VideoDecoderParams) -> Result<(), DecoderError> {
        Err("VideoToolboxDecoder::configure not yet implemented".into())
    }

    fn submit(&mut self, _sample: &[u8], _pts_us: i64) -> Result<(), DecoderError> {
        Err("VideoToolboxDecoder::submit not yet implemented".into())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        Err("VideoToolboxDecoder::try_recv not yet implemented".into())
    }
}
