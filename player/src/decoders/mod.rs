// Hardware video decoder abstraction.
//
// Today the pipeline in `player::player::video_play` hard-codes an FFmpeg
// `decoder::Video` wired to D3D11VA (Windows) or VAAPI (Linux). To support
// MediaCodec on Android and VideoToolbox on iOS without an explosion of cfg
// blocks, all four sit behind this trait. Each backend takes compressed
// samples in, produces decoded frames as platform-native handles out; the
// renderer converts those handles into `wgpu::Texture` per-platform.
//
// Status: definition only. The existing FFmpeg pipeline still calls FFmpeg
// directly. Migration plan:
//   1. Wrap FFmpeg + D3D11VA in an `FfmpegHwDecoder` (Windows) and
//      `FfmpegVaapiDecoder` (Linux) impl.
//   2. Replace the inline calls in `video_play` / `video_decoder_task` with
//      `Box<dyn HwVideoDecoder>`.
//   3. Add `MediaCodecDecoder` (Android) and `VideoToolboxDecoder` (iOS).

use std::error::Error;

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod ffmpeg_hw;
#[cfg(target_os = "android")]
pub mod mediacodec;
#[cfg(target_os = "ios")]
pub mod videotoolbox;

#[derive(Clone, Copy, Debug)]
pub enum VideoCodec {
    Hevc,
    H264,
}

pub struct VideoDecoderParams {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Codec-private data: `hvcC` body for HEVC, `avcC` body for H.264.
    pub codec_specific_data: Vec<u8>,
}

pub struct DecodedVideoFrame {
    pub pts_us: i64,
    pub width: u32,
    pub height: u32,
    pub native: PlatformFrame,
}

/// Platform-native handle wrapping a decoded frame's GPU surface.
///
/// Each variant is the minimum information the renderer needs to import the
/// frame into wgpu without an extra GPU copy:
///   * Windows: shared `ID3D11Texture2D` (subresource index for arrays).
///   * Linux:   VA-API surface that we export as a DMA-BUF.
///   * Android: `AHardwareBuffer` (importable via Vulkan external memory).
///   * iOS:     `CVPixelBufferRef` (importable via Metal IOSurface).
#[non_exhaustive]
pub enum PlatformFrame {
    #[cfg(target_os = "windows")]
    D3d11 { texture_ptr: *mut std::ffi::c_void, array_index: u32 },
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    Vaapi { surface_id: u32, display: *mut std::ffi::c_void },
    #[cfg(target_os = "android")]
    HardwareBuffer { ahb: *mut std::ffi::c_void },
    #[cfg(target_os = "ios")]
    CvPixelBuffer { buffer: *mut std::ffi::c_void },
}

// Raw native handles cross thread boundaries; the underlying objects are
// refcounted by their platform APIs.
unsafe impl Send for PlatformFrame {}

pub type DecoderError = Box<dyn Error + Send + Sync>;

pub trait HwVideoDecoder: Send {
    /// Install codec parameters. Called once before the first `submit`.
    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError>;

    /// Queue a compressed sample for decoding. `sample` is the raw mdat bytes
    /// from the MP4 track (already CENC-decrypted if applicable).
    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError>;

    /// Pull the next decoded frame, if one is ready. Non-blocking: returns
    /// `Ok(None)` when the decoder hasn't produced a frame yet.
    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError>;

    /// Drop in-flight state (used on seek). Default impl is a no-op for
    /// decoders that tolerate out-of-order reset via codec-specific data.
    fn flush(&mut self) -> Result<(), DecoderError> {
        Ok(())
    }
}
