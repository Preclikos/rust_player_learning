// Hardware video decoder abstraction.
//
// Each platform implements HwVideoDecoder and AudioDecoder. The unified
// play loop in player.rs uses Box<dyn HwVideoDecoder> / Box<dyn AudioDecoder>
// so the same timing and sync logic works everywhere; only the concrete
// types differ per platform.

use std::error::Error;

// macOS AND iOS use ffmpeg for AUDIO (AudioToolbox's AAC decoder mis-behaves
// when fed access units packet-by-packet — first packet decodes, then it
// returns 0 frames forever) but the native VideoToolbox path for VIDEO
// (= zero-copy CVPixelBuffer → wgpu Metal). Windows / Linux keep both FFmpeg;
// Android uses MediaCodec for both.
#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios"
))]
pub mod ffmpeg_audio;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod ffmpeg_hw;
#[cfg(target_os = "android")]
pub mod mediacodec;
#[cfg(target_os = "android")]
pub mod mediacodec_audio;
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub mod videotoolbox;

// ---------------------------------------------------------------------------
// Video decoder types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum VideoCodec {
    Hevc,
    // H.264 reserved for the planned ffmpeg/MediaCodec branch; currently only
    // HEVC streams are exercised so the variant is never constructed.
    #[allow(dead_code)]
    H264,
}

pub struct VideoDecoderParams {
    pub codec: VideoCodec,
    // Width/height are consumed by MediaCodec on Android; the FFmpeg desktop
    // path reads them from the bitstream/init segment so they're unused there.
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
    /// Raw NALU bytes (no length prefix, no start code) — VPS/SPS/PPS for HEVC,
    /// extracted from the hvcC box in the init segment.
    pub hvcc_nalus: Vec<Vec<u8>>,
    /// Colour information for the representation, parsed from the SPS VUI
    /// (authoritative — the MPD often mis-signals BT.709 on PQ content).
    /// Drives 10-bit surface allocation and the HDR tonemap path selection;
    /// decoders stamp it onto every [`DecodedVideoFrame`] so the renderer
    /// keeps making the right per-frame choice across ABR SDR↔HDR swaps.
    pub color: VideoColorInfo,
    /// Android direct mode: `ANativeWindow*` of the dedicated video plane
    /// (as usize). When non-zero MediaCodec renders straight into it — the
    /// HW video plane carries HDR/HDR10+/DV to the display — and frames
    /// come back as [`PlatformFrame::MediaCodecDirect`] release handles
    /// instead of AHardwareBuffers. 0 = classic ImageReader/GL path.
    /// Ignored by the desktop / Apple decoders.
    pub direct_window: usize,
    /// Dolby Vision profile (5/7/8) when the representation is DV. In
    /// direct mode the Android decoder then prefers the platform
    /// `video/dolby-vision` codec with the RPU NALs KEPT — full DV
    /// reconstruction in the OS pipeline — and falls back to the HEVC
    /// base layer for profiles 7/8 when no DV decoder exists.
    pub dovi_profile: Option<u8>,
}

/// Transfer function of the video signal, from the SPS VUI
/// `transfer_characteristics` (H.273: 16 = PQ, 18 = HLG).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TransferFunction {
    /// SDR (BT.709 / BT.1886 family) — also the fallback when unspecified.
    #[default]
    Sdr,
    /// SMPTE ST 2084 perceptual quantizer (HDR10, Dolby Vision 8.1 BL).
    Pq,
    /// Hybrid log-gamma (ARIB STD-B67).
    Hlg,
}

/// Per-representation colour description, renderer-facing.
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoColorInfo {
    /// Luma bit depth (8 = SDR ladder, 10 = Main10 / HDR).
    pub bit_depth: u8,
    pub transfer: TransferFunction,
    /// Primaries/matrix are BT.2020 (wide gamut).
    pub bt2020: bool,
    pub full_range: bool,
}

impl VideoColorInfo {
    /// True when the HDR tonemap path must run (PQ or HLG signal).
    pub fn is_hdr(&self) -> bool {
        !matches!(self.transfer, TransferFunction::Sdr)
    }

    /// Build from a parsed SPS, with the hvcC bit depth as fallback.
    pub fn from_sps(
        sps: Option<crate::parsers::hevc::SpsColorInfo>,
        hvcc_bit_depth: Option<u8>,
    ) -> Self {
        match sps {
            Some(s) => Self {
                bit_depth: s.bit_depth_luma,
                transfer: match s.transfer_characteristics {
                    16 => TransferFunction::Pq,
                    18 => TransferFunction::Hlg,
                    _ => TransferFunction::Sdr,
                },
                bt2020: s.colour_primaries == 9 || s.matrix_coeffs == 9,
                full_range: s.full_range,
            },
            None => Self {
                bit_depth: hvcc_bit_depth.unwrap_or(8),
                ..Default::default()
            },
        }
    }
}

pub struct DecodedVideoFrame {
    pub pts_us: i64,
    pub width: u32,
    pub height: u32,
    pub native: PlatformFrame,
    /// CLOCK_MONOTONIC nanoseconds when this frame should be displayed.
    /// Set by the A/V sync loop; 0 = no constraint (display ASAP).
    pub desired_present_ns: i64,
    /// Colour description stamped by the decoder from its configure params.
    /// Per-frame (not per-renderer state) so ABR SDR↔HDR swaps stay correct
    /// for frames of the OLD representation still queued in the channel.
    pub color: VideoColorInfo,
    /// Dynamic/static HDR metadata for this frame, parsed from the
    /// bitstream SEI (HDR10+ per access unit; mastering display / MaxCLL
    /// as the static fallback). None = no metadata seen — the renderer
    /// uses its default peak.
    pub hdr_meta: Option<HdrFrameMeta>,
}

/// Tonemap-relevant HDR metadata resolved per frame, in nits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrFrameMeta {
    /// Scene/frame peak (HDR10+ maxscl, DV L1 max, or MaxCLL/mastering).
    pub peak_nits: f32,
    /// Scene average of max(R,G,B) when the metadata carries one
    /// (HDR10+ average_maxrgb, DV L1 avg).
    pub avg_nits: Option<f32>,
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
    /// Direct mode: a not-yet-released MediaCodec output buffer. The
    /// renderer "renders" it by releasing it to the video Surface with a
    /// presentation timestamp; dropping the frame unrendered (LATE drain,
    /// teardown) releases it back to the codec without rendering.
    #[cfg(target_os = "android")]
    MediaCodecDirect(mediacodec::DirectVideoFrame),
    /// macOS / iOS / tvOS: native VTDecompressionSession output. The
    /// retained CVPixelBufferRef points to GPU memory (IOSurface-backed)
    /// the video renderer imports via CVMetalTextureCache zero-copy.
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    CvPixelBuffer(CvPixelBufferOwned),
}

/// Reference-counted wrapper around a CVPixelBufferRef. CFRetain on
/// construction, CFRelease on Drop — so an owned `CvPixelBufferOwned`
/// keeps the underlying IOSurface alive across thread boundaries until
/// the renderer is done with it.
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub struct CvPixelBufferOwned {
    raw: *mut std::ffi::c_void,
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
impl CvPixelBufferOwned {
    /// SAFETY: `raw` must be a valid CVPixelBufferRef. Retains the buffer
    /// so the caller can drop their reference after this call.
    pub unsafe fn from_retained(raw: *mut std::ffi::c_void) -> Self {
        extern "C" {
            fn CFRetain(cf: *const std::ffi::c_void) -> *const std::ffi::c_void;
        }
        unsafe { CFRetain(raw as *const _) };
        Self { raw }
    }

    pub fn as_ptr(&self) -> *const std::ffi::c_void {
        self.raw as *const _
    }
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
impl Drop for CvPixelBufferOwned {
    fn drop(&mut self) {
        extern "C" {
            fn CFRelease(cf: *const std::ffi::c_void);
        }
        if !self.raw.is_null() {
            unsafe { CFRelease(self.raw as *const _) };
        }
    }
}

// CVPixelBuffer is documented as thread-safe for retain/release; the
// IOSurface backing is also safe to share. The frame travels from the
// decode worker to the renderer task across thread boundaries.
#[cfg(any(target_os = "ios", target_os = "macos"))]
unsafe impl Send for CvPixelBufferOwned {}

#[cfg(target_os = "android")]
pub struct SendableAhb {
    buffer: ndk::hardware_buffer::HardwareBufferRef,
    // The acquired AImage is what keeps this buffer OUT of the ImageReader's
    // free pool. AHardwareBuffer_acquire alone only pins the allocation —
    // once the AImage is deleted, MediaCodec may dequeue the slot and decode
    // a FUTURE frame into the same memory while this frame is still queued
    // for render (channel + reorder buffer ≈ half a second). On screen that
    // is invisible mid-scene but flashes back and forth at scene cuts.
    // Deleting the image (and thus releasing the slot) only when the last
    // Arc clone drops — including the renderer's keepalive ring — is what
    // actually implements the "released back to the pool when all clones
    // are dropped" contract documented on AndroidHardwareBufferFrame.
    _image: Option<ndk::media::image_reader::Image>,
    // Keeps the ImageReader alive until every in-flight image from it has
    // been deleted, so a decoder teardown (ABR swap, loop restart) can't
    // pull the reader out from under queued frames.
    _reader: Option<std::sync::Arc<ndk::media::image_reader::ImageReader>>,
}

// HardwareBufferRef wraps NonNull<AHardwareBuffer>. AHardwareBuffer itself is
// reference-counted (AHardwareBuffer_acquire/release) and thread-safe per
// Android docs, so it's safe to share refs across threads. AImage/AImageReader
// carry no documented thread affinity; AImage_delete from the dropping thread
// is the same pattern MediaCodecDecoder itself relies on (unsafe impl Send).
#[cfg(target_os = "android")]
unsafe impl Send for SendableAhb {}
#[cfg(target_os = "android")]
unsafe impl Sync for SendableAhb {}

#[cfg(target_os = "android")]
impl SendableAhb {
    pub fn new(
        buffer: ndk::hardware_buffer::HardwareBufferRef,
        image: ndk::media::image_reader::Image,
        reader: std::sync::Arc<ndk::media::image_reader::ImageReader>,
    ) -> Self {
        Self { buffer, _image: Some(image), _reader: Some(reader) }
    }

    pub fn as_ptr(&self) -> *mut ndk_sys::AHardwareBuffer {
        self.buffer.as_ptr()
    }
}

#[cfg(target_os = "android")]
pub struct AndroidHardwareBufferFrame {
    /// Arc so the renderer can clone a reference into its keepalive queue;
    /// the underlying AHB is only released back to MediaCodec's ImageReader
    /// pool when both this frame and all keepalive clones are dropped.
    pub buffer: std::sync::Arc<SendableAhb>,
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
    Ac3,
    Eac3,
}

pub struct AudioDecoderParams {
    pub codec: AudioCodec,
    /// Native sample rate from the source stream (e.g. 44100 or 48000).
    pub input_sample_rate: u32,
    /// Channel count from the source stream.
    pub input_channels: u16,
    /// Target sample rate for the output device (from AudioRenderer::sample_rate()).
    pub output_sample_rate: u32,
    /// Codec-specific extradata. For AAC this is the 2-byte AudioSpecificConfig
    /// from `esds`. For AC-3 / EAC-3 it is empty — those formats are
    /// self-describing (each frame carries its own syncinfo header), so neither
    /// FFmpeg nor MediaCodec needs csd-0 to initialise.
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
    /// Human-readable backend identifier surfaced via `PlayerEvent::Stats`.
    /// Example: `"D3D11VA HEVC"`, `"MediaCodec H.264"`, `"VideoToolbox"`.
    /// Concrete impls override; default keeps tests / shells compiling.
    fn name(&self) -> &'static str {
        "unknown"
    }

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

    /// Reset internal decoder state. The play pipeline currently tears
    /// decoders down on seek/track-switch instead of calling this — kept
    /// as part of the trait API for future zero-restart paths.
    #[allow(dead_code)]
    fn flush(&mut self) -> Result<(), DecoderError> {
        Ok(())
    }

    /// True when frames are direct-render handles (MediaCodec output
    /// buffers awaiting a timed release) rather than copies/refs the
    /// pipeline can hold freely. The decode pipeline then keeps far fewer
    /// frames in flight — every queued frame pins one of the codec's
    /// scarce output buffers and over-holding stalls the decoder.
    fn is_direct(&self) -> bool {
        false
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

    /// See `HwVideoDecoder::flush`.
    #[allow(dead_code)]
    fn flush(&mut self) -> Result<(), DecoderError> {
        Ok(())
    }
}
