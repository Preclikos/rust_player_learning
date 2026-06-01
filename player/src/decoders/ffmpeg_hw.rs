#![cfg(any(target_os = "windows", target_os = "linux"))]

use std::sync::Arc;

use ffmpeg_next::Packet;
use ffmpeg_sys_next::{av_hwdevice_ctx_create, AVBufferRef, AVCodecContext, AVHWDeviceType, AVPixelFormat};

use crate::parsers::mp4::{apped_hevc_header, parse_hevc_nalu};

use super::{DecodedVideoFrame, DecoderError, HwVideoDecoder, PlatformFrame, VideoCodec, VideoDecoderParams};

pub struct FfmpegHwDecoder {
    decoder: Option<ffmpeg_next::decoder::Video>,
    hw_device_ctx: *mut AVBufferRef,
}

unsafe impl Send for FfmpegHwDecoder {}

impl FfmpegHwDecoder {
    pub fn new() -> Self {
        Self {
            decoder: None,
            hw_device_ctx: std::ptr::null_mut(),
        }
    }

    fn create_hw_device(&mut self) -> Result<(), DecoderError> {
        #[cfg(target_os = "windows")]
        let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;
        #[cfg(target_os = "linux")]
        let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI;

        let mut ctx: *mut AVBufferRef = std::ptr::null_mut();
        let ret = unsafe {
            av_hwdevice_ctx_create(&mut ctx, device_type, std::ptr::null(), std::ptr::null_mut(), 0)
        };
        if ret < 0 {
            return Err(format!("av_hwdevice_ctx_create failed: {}", ret).into());
        }
        self.hw_device_ctx = ctx;
        Ok(())
    }
}

impl Drop for FfmpegHwDecoder {
    fn drop(&mut self) {
        // ffmpeg-next's decoder Drop releases the codec context, which unref's
        // hw_device_ctx. We don't free it here to avoid a double-free.
    }
}

// libavcodec calls `get_format` during stream-header parsing with a
// null-terminated array of pixel formats the decoder can produce for
// the negotiated stream. The default impl picks the first software
// format, which leaves `frame.hw_frames_ctx == NULL` and breaks
// renderers/video/video_frame.rs:101's hw_frames_ctx expectation.
//
// We pick the platform's HW format if it's on offer and otherwise
// return AV_PIX_FMT_NONE so libavcodec aborts decoder open — louder
// than silently sliding into software and panicking downstream.
#[cfg(target_os = "windows")]
const WANTED_HW: AVPixelFormat = AVPixelFormat::AV_PIX_FMT_D3D11;
#[cfg(target_os = "linux")]
const WANTED_HW: AVPixelFormat = AVPixelFormat::AV_PIX_FMT_VAAPI;

unsafe extern "C" fn select_hw_format(
    _ctx: *mut AVCodecContext,
    fmts: *const AVPixelFormat,
) -> AVPixelFormat {
    // Snapshot the offered list so we can log it once. Critical for
    // diagnosing "decoder fails on this driver but works on another"
    // reports — different FFmpeg builds / GPU drivers offer different
    // sets, and a mismatch with WANTED_HW silently kills decoding.
    let mut offered = Vec::with_capacity(8);
    let mut p = fmts;
    while unsafe { *p } != AVPixelFormat::AV_PIX_FMT_NONE {
        offered.push(unsafe { *p });
        p = unsafe { p.add(1) };
    }
    log::info!(
        "[ffmpeg_hw] get_format offered: {:?}; want {:?}",
        offered,
        WANTED_HW
    );
    if offered.contains(&WANTED_HW) {
        WANTED_HW
    } else {
        AVPixelFormat::AV_PIX_FMT_NONE
    }
}

impl HwVideoDecoder for FfmpegHwDecoder {
    fn name(&self) -> &'static str {
        #[cfg(target_os = "windows")]
        {
            "D3D11VA (FFmpeg)"
        }
        #[cfg(target_os = "linux")]
        {
            "VAAPI (FFmpeg)"
        }
    }

    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError> {
        let codec_id = match params.codec {
            VideoCodec::Hevc => ffmpeg_next::codec::Id::HEVC,
            VideoCodec::H264 => ffmpeg_next::codec::Id::H264,
        };
        let codec = ffmpeg_next::decoder::find(codec_id)
            .ok_or_else(|| -> DecoderError { "cannot find FFmpeg decoder for codec".into() })?;

        let mut ctx = ffmpeg_next::codec::Context::new_with_codec(codec);

        if self.hw_device_ctx.is_null() {
            self.create_hw_device()?;
        }
        unsafe {
            (*ctx.as_mut_ptr()).hw_device_ctx = self.hw_device_ctx;
            // Leave `extra_hw_frames` at libavcodec's default (0). With 0,
            // D3D11VA's frame allocator stays on a *dynamic* pool that hands
            // out individual ID3D11Texture2D resources on demand; setting it
            // >0 flips the same allocator to a *static* Texture2DArray sized
            // to base_dpb + extra_hw_frames. Intel Arc D3D11VA returns ENOMEM
            // from the very first packet on that static-array path (observed
            // on A750, even at "safe" pool sizes), so the dynamic pool is the
            // universally-compatible default — matches FFmpeg's hw_decode.c.
            (*ctx.as_mut_ptr()).get_format = Some(select_hw_format);
        }

        let mut decoder = ctx
            .decoder()
            .video()
            .map_err(|e| -> DecoderError { format!("decoder open: {}", e).into() })?;

        // Feed the hvcC NALUs (VPS/SPS/PPS) so the decoder can parse subsequent slices.
        // Each element of `hvcc_nalus` is a raw NALU body (no prefix); apped_hevc_header
        // prepends the 00 00 00 01 start code.
        for nalu_data in &params.hvcc_nalus {
            let nalu = apped_hevc_header(nalu_data.clone());
            let mut packet = Packet::new(nalu.len());
            packet.data_mut().unwrap().clone_from_slice(&nalu);
            decoder
                .send_packet(&packet)
                .map_err(|e| -> DecoderError { format!("send hvcC NALU: {}", e).into() })?;
        }

        self.decoder = Some(decoder);
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| -> DecoderError { "submit before configure".into() })?;

        let nalus = parse_hevc_nalu(sample)
            .map_err(|e| -> DecoderError { format!("sample NALU parse: {}", e).into() })?;

        for nalu in nalus {
            let mut packet = Packet::new(nalu.len());
            // Store pts in milliseconds — FFmpeg's time base for this decoder is 1ms.
            packet.set_pts(Some(pts_us / 1000));
            packet.data_mut().unwrap().clone_from_slice(&nalu);
            decoder.send_packet(&packet).map_err(|e| -> DecoderError {
                // Include errno + Debug repr so opaque strerror strings like
                // "Not enough space" (Windows-localised ENOSPC?
                // AVERROR_BUFFER_TOO_SMALL?) can be cross-referenced against
                // FFmpeg's error codes during triage.
                let errno = match &e {
                    ffmpeg_next::Error::Other { errno } => Some(*errno),
                    _ => None,
                };
                format!(
                    "send_packet: {} (errno={:?}, nalu_len={}, pts_ms={})",
                    e,
                    errno,
                    nalu.len(),
                    pts_us / 1000
                )
                .into()
            })?;
        }
        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;

        let mut frame = ffmpeg_next::util::frame::Video::empty();
        match decoder.receive_frame(&mut frame) {
            Ok(()) => {
                // pts was stored in milliseconds; convert back to microseconds for the trait.
                let pts_us = frame.pts().unwrap_or(0) * 1000;
                let width = frame.width();
                let height = frame.height();

                // macOS: keep the AVFrame as AV_PIX_FMT_VIDEOTOOLBOX — the CVPixelBufferRef
                // lives in frame.data[3] and the video renderer wraps it as two MTLTextures
                // (Y + UV planes) via CVMetalTextureCache. Zero-copy GPU import.

                Ok(Some(DecodedVideoFrame {
                    pts_us,
                    width,
                    height,
                    native: PlatformFrame::FfmpegVideo(Arc::new(frame)),
                    desired_present_ns: 0,
                }))
            }
            Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_sys_next::EAGAIN => {
                Ok(None)
            }
            Err(e) => Err(format!("receive_frame: {}", e).into()),
        }
    }
}
