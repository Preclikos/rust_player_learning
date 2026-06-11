// Android MediaCodec-based HwVideoDecoder. Zero-copy via:
//
//   AMediaCodec
//     ── decoded output ──▶ Surface (AImageReader::window())
//                              │
//                              ▼
//                          AImageReader::acquire_latest_image()
//                              │
//                              ▼
//                          AImage::hardware_buffer()  →  AHardwareBuffer
//
// The AHB is then imported into Vulkan as VkImage via
// VK_ANDROID_external_memory_android_hardware_buffer, and the resulting
// VkImage becomes a `wgpu::Texture` (same pattern as `video_vaapi.rs`'s
// DMA-BUF path and `video_directx.rs`'s D3D11 shared-handle path).
//
// The CPU never touches the pixels; this is what makes 4K HEVC playback
// viable on Android.

#![cfg(target_os = "android")]

use std::sync::Arc;
use std::time::Duration;

use ndk::media::image_reader::{AcquireResult, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;

use crate::parsers::mp4::parse_hevc_nalu;

use super::{
    AndroidHardwareBufferFrame, DecodedVideoFrame, DecoderError, HwVideoDecoder, PlatformFrame,
    SendableAhb, VideoCodec, VideoDecoderParams,
};

pub struct MediaCodecDecoder {
    // ImageReader owns the consumer surface. It's behind Arc so we can
    // hold a stable reference for the lifetime of the codec — MediaCodec
    // keeps the Surface internally too via its NativeWindow.
    reader: Option<Arc<ImageReader>>,
    codec: Option<MediaCodec>,
    width: u32,
    height: u32,
    last_decoded_pts_us: i64,
    decoded_frame_idx: u64,
}

// MediaCodec/ImageReader wrap NonNull pointers and aren't auto-Send.
// Single-threaded ownership inside the decoder task is fine.
unsafe impl Send for MediaCodecDecoder {}

impl MediaCodecDecoder {
    pub fn new() -> Self {
        Self {
            reader: None,
            codec: None,
            width: 0,
            height: 0,
            last_decoded_pts_us: -1,
            decoded_frame_idx: 0,
        }
    }
}

impl HwVideoDecoder for MediaCodecDecoder {
    fn name(&self) -> &'static str {
        "MediaCodec"
    }

    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError> {
        let mime = match params.codec {
            VideoCodec::Hevc => "video/hevc",
            VideoCodec::H264 => "video/avc",
        };

        // 32 physical NV12 surfaces ≈ 1.33 s of buffer at 24 fps.
        // Segment boundaries cause a ~1.5 s decoder stall (MediaCodec IDR
        // warmup + segment parse). With 16 images the 667 ms buffer drained
        // before the stall ended, triggering LATE + an aggressive drain that
        // skipped 2.7 s of content — visible freeze-then-jump every 6 s.
        // 32 images nearly covers the stall; combined with proportional
        // frame drain in player.rs the recovery is smooth.
        // 64 caused a device-level SIGABRT on the MT8696 (PowerVR driver
        // has a hard limit on AImageReader max_images for YUV_420_888).
        // Memory cost: 32 × ~1.4 MB (720p NV12) ≈ 45 MB.
        let max_images = 32;

        // YUV_420_888 gives us an AHardwareBuffer with a defined NV12-ish
        // layout (Y plane + interleaved CbCr plane) that Vulkan can
        // import via VK_FORMAT_G8_B8R8_2PLANE_420_UNORM without needing
        // VkSamplerYcbcrConversion. On devices that refuse it we'd fall
        // back to ImageFormat::PRIVATE + VkExternalFormatANDROID.
        let reader = ImageReader::new(
            params.width as i32,
            params.height as i32,
            ImageFormat::YUV_420_888,
            max_images,
        )
        .map_err(|e| -> DecoderError { format!("ImageReader::new: {:?}", e).into() })?;

        let window = reader
            .window()
            .map_err(|e| -> DecoderError { format!("ImageReader::window: {:?}", e).into() })?;

        let codec = MediaCodec::from_decoder_type(mime).ok_or_else(|| -> DecoderError {
            format!("MediaCodec::from_decoder_type({}) returned None", mime).into()
        })?;

        let mut format = MediaFormat::new();
        format.set_str("mime", mime);
        format.set_i32("width", params.width as i32);
        format.set_i32("height", params.height as i32);
        if !params.hvcc_nalus.is_empty() {
            // Build Annex-B csd-0: start-code prefix + raw NALU body for each NALU.
            let mut csd = Vec::new();
            for n in &params.hvcc_nalus {
                csd.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                csd.extend_from_slice(n);
            }
            format.set_buffer("csd-0", &csd);
        }

        codec
            .configure(&format, Some(&window), MediaCodecDirection::Decoder)
            .map_err(|e| -> DecoderError { format!("MediaCodec::configure: {:?}", e).into() })?;
        codec
            .start()
            .map_err(|e| -> DecoderError { format!("MediaCodec::start: {:?}", e).into() })?;

        self.reader = Some(Arc::new(reader));
        self.codec = Some(codec);
        self.width = params.width;
        self.height = params.height;
        log::info!(
            "MediaCodecDecoder: configured {}, {}x{}, surface output",
            mime,
            params.width,
            params.height
        );
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        let codec = self
            .codec
            .as_ref()
            .ok_or_else(|| -> DecoderError { "submit before configure".into() })?;

        // `sample` is length-prefixed NALU (raw mdat). Convert to Annex-B
        // (start-code prefixed) — MediaCodec expects this for HEVC/H.264.
        let nalus = parse_hevc_nalu(sample)
            .map_err(|e| -> DecoderError { format!("NALU parse: {}", e).into() })?;
        let mut annex_b = Vec::with_capacity(sample.len() + nalus.len() * 4);
        for n in nalus {
            annex_b.extend_from_slice(&n);
        }

        // Retry until an input slot is free. Each wait is 5 ms; the codec
        // typically frees a slot within one frame interval (~33ms at 24fps).
        // If we stall here for a long time it means all output surfaces are
        // occupied (AHBs in the video channel), starving MediaCodec of buffers.
        let mut input_buf = {
            let mut retries = 0u32;
            loop {
                match codec
                    .dequeue_input_buffer(Duration::from_millis(5))
                    .map_err(|e| -> DecoderError { format!("dequeue_input_buffer: {:?}", e).into() })?
                {
                    DequeuedInputBufferResult::Buffer(b) => break b,
                    DequeuedInputBufferResult::TryAgainLater => {
                        retries += 1;
                        if retries % 20 == 0 {
                            log::warn!("[mc] dequeue_input stall {}x5ms={} ms pts={}", retries, retries * 5, pts_us / 1000);
                        }
                        std::thread::yield_now();
                    }
                }
            }
        };

        let dst = input_buf.buffer_mut();
        let copy_len = annex_b.len().min(dst.len());
        unsafe {
            std::ptr::copy_nonoverlapping(
                annex_b.as_ptr(),
                dst.as_mut_ptr() as *mut u8,
                copy_len,
            );
        }

        codec
            .queue_input_buffer(input_buf, 0, copy_len, pts_us as u64, 0)
            .map_err(|e| -> DecoderError { format!("queue_input_buffer: {:?}", e).into() })?;

        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let codec = self
            .codec
            .as_ref()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;
        let reader = self
            .reader
            .as_ref()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;

        // Drain the codec: dequeue one output buffer and release it to the
        // surface (render = true). This pushes the decoded frame into the
        // ImageReader's queue, where acquire_latest_image() can pick it up.
        let dequeued = codec
            .dequeue_output_buffer(Duration::ZERO)
            .map_err(|e| -> DecoderError { format!("dequeue_output_buffer: {:?}", e).into() })?;

        let pts_us = match dequeued {
            DequeuedOutputBufferInfoResult::Buffer(out) => {
                let pts = out.info().presentation_time_us();
                let idx = self.decoded_frame_idx;
                let delta = if self.last_decoded_pts_us >= 0 { pts - self.last_decoded_pts_us } else { 0 };
                if self.last_decoded_pts_us >= 0 && pts < self.last_decoded_pts_us {
                    log::warn!("[mc] BACKWARD #{} pts={}ms last={}ms Δ={}ms",
                        idx, pts / 1000, self.last_decoded_pts_us / 1000, delta / 1000);
                } else {
                    log::trace!("[mc] decoded #{} pts={}ms Δ={}ms",
                        idx, pts / 1000, delta / 1000);
                }
                self.last_decoded_pts_us = pts;
                self.decoded_frame_idx += 1;
                codec
                    .release_output_buffer(out, /* render = */ true)
                    .map_err(|e| -> DecoderError {
                        format!("release_output_buffer: {:?}", e).into()
                    })?;
                pts
            }
            DequeuedOutputBufferInfoResult::TryAgainLater => return Ok(None),
            DequeuedOutputBufferInfoResult::OutputFormatChanged => {
                let fmt = codec.output_format();
                if let (Some(w), Some(h)) = (fmt.i32("width"), fmt.i32("height")) {
                    self.width = w as u32;
                    self.height = h as u32;
                    log::info!("MediaCodec output format: {}x{}", w, h);
                }
                return Ok(None);
            }
            DequeuedOutputBufferInfoResult::OutputBuffersChanged => return Ok(None),
        };

        // Pull the freshly-rendered image off the surface in FIFO order.
        // release_output_buffer(render=true) is nearly synchronous for
        // ImageReader but may need a few retries. We MUST NOT return None
        // here after having consumed a MediaCodec output slot — doing so
        // leaves a frame stranded in ImageReader, and the next try_recv
        // would pair the next MediaCodec PTS with the stale image, causing
        // visible content to be displayed at the wrong timestamp.
        let mut acquire_retries = 0u32;
        let image = loop {
            match reader
                .acquire_next_image()
                .map_err(|e| -> DecoderError { format!("acquire_next_image: {:?}", e).into() })?
            {
                AcquireResult::Image(img) => break img,
                AcquireResult::MaxImagesAcquired => {
                    // All surfaces are held by AHB refs in the video channel.
                    // The sync producer (on another thread) must render+drop frames
                    // to free slots. Yield and retry — do NOT return None, which
                    // would strand this rendered frame and cause PTS/content mismatch.
                    acquire_retries += 1;
                    if acquire_retries % 100 == 0 {
                        log::warn!("[mc] MAX_IMAGES_ACQUIRED spin {}x pts={}", acquire_retries, pts_us / 1000);
                    }
                    std::thread::yield_now();
                }
                AcquireResult::NoBufferAvailable => {
                    std::thread::yield_now();
                }
            }
        };

        // Get the unowned HardwareBuffer reference from the Image and acquire
        // a strong ref, but keep the Image itself inside the frame: deleting
        // the AImage returns the slot to the ImageReader's free pool and lets
        // MediaCodec overwrite the pixels with a future frame while this one
        // is still queued for render (the scene-cut flicker). The slot is
        // held until the renderer's keepalive ring drops the last Arc clone.
        let hb_unowned = image
            .hardware_buffer()
            .map_err(|e| -> DecoderError { format!("Image::hardware_buffer: {:?}", e).into() })?;
        let buffer = Arc::new(SendableAhb::new(hb_unowned.acquire(), image, Arc::clone(reader)));

        Ok(Some(DecodedVideoFrame {
            pts_us,
            width: self.width,
            height: self.height,
            native: PlatformFrame::HardwareBuffer(AndroidHardwareBufferFrame {
                buffer,
                width: self.width,
                height: self.height,
            }),
            desired_present_ns: 0,
        }))
    }

    fn flush(&mut self) -> Result<(), DecoderError> {
        if let Some(codec) = self.codec.as_ref() {
            codec
                .flush()
                .map_err(|e| -> DecoderError { format!("MediaCodec::flush: {:?}", e).into() })?;
        }
        self.last_decoded_pts_us = -1;
        self.decoded_frame_idx = 0;
        Ok(())
    }
}
