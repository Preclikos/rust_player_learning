// Hardware video decoder abstraction.
//
// Each platform implements HwVideoDecoder and AudioDecoder. The unified
// play loop in player.rs uses Box<dyn HwVideoDecoder> / Box<dyn AudioDecoder>
// so the same timing and sync logic works everywhere; only the concrete
// types differ per platform.

use std::error::Error;

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod ffmpeg_audio;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod ffmpeg_hw;
#[cfg(target_os = "android")]
pub mod mediacodec;
#[cfg(target_os = "android")]
pub mod mediacodec_audio;
#[cfg(target_os = "ios")]
pub mod videotoolbox;

// ---------------------------------------------------------------------------
// Video decoder types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum VideoCodec {
    Hevc,
    H264,
}

pub struct VideoDecoderParams {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Raw NALU bytes (no length prefix, no start code) — VPS/SPS/PPS for HEVC,
    /// extracted from the hvcC box in the init segment.
    pub hvcc_nalus: Vec<Vec<u8>>,
}

pub struct DecodedVideoFrame {
    pub pts_us: i64,
    pub width: u32,
    pub height: u32,
    pub native: PlatformFrame,
}

/// Platform-native handle wrapping a decoded frame's GPU surface.
#[non_exhaustive]
pub enum PlatformFrame {
    /// Desktop (Windows + Linux): the raw decoded frame from FFmpeg, which
    /// carries its own D3D11/VAAPI hw_frames_ctx pointer. The video renderer
    /// imports the native surface via VideoFrame::new.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    FfmpegVideo(std::sync::Arc<ffmpeg_next::frame::Video>),
    /// Owned AHardwareBuffer produced by MediaCodec's output Surface.
    #[cfg(target_os = "android")]
    HardwareBuffer(AndroidHardwareBufferFrame),
    #[cfg(target_os = "ios")]
    CvPixelBuffer { buffer: *mut std::ffi::c_void },
}

#[cfg(target_os = "android")]
pub struct AndroidHardwareBufferFrame {
    pub buffer: ndk::hardware_buffer::HardwareBufferRef,
    pub width: u32,
    pub height: u32,
}

#[cfg(target_os = "android")]
unsafe impl Send for AndroidHardwareBufferFrame {}

unsafe impl Send for PlatformFrame {}

// ---------------------------------------------------------------------------
// Audio decoder types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum AudioCodec {
    Aac,
}

pub struct AudioDecoderParams {
    pub codec: AudioCodec,
    /// Native sample rate from the source stream (e.g. 44100 or 48000).
    pub input_sample_rate: u32,
    /// Channel count from the source stream.
    pub input_channels: u16,
    /// Target sample rate for the output device (from AudioRenderer::sample_rate()).
    pub output_sample_rate: u32,
    /// AudioSpecificConfig bytes (2 bytes for AAC LC).
    pub codec_specific_data: Vec<u8>,
}

/// A decoded audio buffer: interleaved stereo f32 PCM samples at
/// `output_sample_rate`, timestamped in milliseconds.
pub struct DecodedAudioFrame {
    pub pts_ms: i64,
    pub samples: Vec<f32>,
}

pub type DecoderError = Box<dyn Error + Send + Sync>;

pub trait HwVideoDecoder: Send {
    /// Install codec parameters. Called once before the first `submit`.
    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError>;

    /// Queue a compressed sample for decoding. `sample` is the raw mdat bytes
    /// (length-prefixed NALU format, CENC-decrypted). Each decoder impl handles
    /// the format conversion (Annex-B etc.) internally.
    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError>;

    /// Pull the next decoded frame, if one is ready. Non-blocking: returns
    /// `Ok(None)` when the decoder hasn't produced a frame yet. `pts_us` in
    /// the returned `DecodedVideoFrame` is in microseconds.
    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError>;

    fn flush(&mut self) -> Result<(), DecoderError> {
        Ok(())
    }
}

pub trait AudioDecoder: Send {
    /// Install codec parameters. Called once before the first `submit`.
    fn configure(&mut self, params: AudioDecoderParams) -> Result<(), DecoderError>;

    /// Queue a compressed audio sample. `sample` is the raw mdat bytes for one
    /// access unit (AAC frame, already CENC-decrypted). `pts_us` in microseconds.
    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError>;

    /// Pull the next decoded audio frame, if ready. Non-blocking.
    /// Returned `DecodedAudioFrame.pts_ms` is in milliseconds;
    /// `samples` is interleaved stereo f32 PCM at `output_sample_rate`.
    fn try_recv(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError>;

    fn flush(&mut self) -> Result<(), DecoderError> {
        Ok(())
    }
}
