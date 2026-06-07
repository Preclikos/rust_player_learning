#![cfg(any(target_os = "windows", target_os = "linux"))]

use std::sync::Arc;

use ffmpeg_next::Packet;
use ffmpeg_sys_next::{
    av_buffer_ref, av_buffer_unref, av_hwdevice_ctx_create, AVBufferRef, AVCodecContext,
    AVHWDeviceType, AVPixelFormat,
};
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

        // Bump FFmpeg log level to VERBOSE and wire up the Rust log forwarder
        // (installs av_log_set_callback the first time; idempotent after that).
        // VERBOSE is needed to surface D3D11VA HRESULTs like 0x80070057 from
        // CreateTexture2D — without it the error is wrapped as AVERROR_UNKNOWN
        // with no driver detail.
        crate::ffmpeg_log::set_log_level(crate::ffmpeg_log::LogLevel::Verbose);

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
        // configure() now hands the codec context its OWN refcounted reference
        // (via av_buffer_ref) so the decoder's drop unrefs that one without
        // invalidating ours. Release ours here.
        if !self.hw_device_ctx.is_null() {
            unsafe { av_buffer_unref(&mut self.hw_device_ctx) };
        }
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
            (*ctx.as_mut_ptr()).hw_device_ctx = av_buffer_ref(self.hw_device_ctx);
            (*ctx.as_mut_ptr()).get_format = Some(select_hw_format);
        }

        // No pre-allocated hw_frames_ctx: let hevc_d3d11va2 auto-create it
        // inside its hwaccel->init() after get_format returns AV_PIX_FMT_D3D11.
        // A manually pre-set context with format=AV_PIX_FMT_D3D11 caused the
        // hwaccel to log "Invalid pixfmt for hwaccel!" and abort because the
        // frames context format was evaluated before avctx->pix_fmt was set.
        // FFmpeg auto-derives sw_format from the SPS (NV12 for Main 8-bit,
        // P010 for Main 10), so the auto-allocated context is always correct.

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

#[cfg(test)]
mod tests {
    //! Platform smoke tests for the FFmpeg HW decoder path.
    //!
    //! These run on Windows + Linux only (mirroring the module's own
    //! cfg gate) and verify the assumptions the rest of the player
    //! makes about the local FFmpeg build:
    //!
    //!   - HEVC + H.264 decoders are linked in (we'd fail later at
    //!     `decoder::find` with a less obvious message).
    //!   - The HW pixel-format constant we look for matches the
    //!     platform's hwaccel framework.
    //!   - The HW device context can be created on this host.
    //!     Skipped (not failed) on CI / headless boxes where no GPU
    //!     is available — distinguishable from a code-level failure
    //!     because the test runner reports "ok" with a log warning.
    //!
    //! Anything past this — `decoder.open(codec)`, `send_packet`,
    //! `receive_frame` — needs real HEVC NALUs (SPS/PPS/VPS plus
    //! sample data) and isn't worth carrying as test fixtures here.

    use super::*;

    #[test]
    fn ffmpeg_finds_hevc_decoder() {
        // This player only configures HEVC and H.264; if HEVC isn't
        // in the local FFmpeg, the rest of the test suite is moot.
        assert!(
            ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC).is_some(),
            "FFmpeg build is missing the HEVC decoder",
        );
    }

    #[test]
    fn ffmpeg_finds_h264_decoder() {
        assert!(
            ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::H264).is_some(),
            "FFmpeg build is missing the H.264 decoder",
        );
    }

    #[test]
    fn wanted_hw_pixel_format_matches_platform() {
        // Catches accidental cfg flips during a refactor — the format
        // we ask the decoder to negotiate must match the device-type
        // we register, otherwise get_format returns AV_PIX_FMT_NONE
        // and decoder open aborts.
        #[cfg(target_os = "windows")]
        assert_eq!(WANTED_HW, AVPixelFormat::AV_PIX_FMT_D3D11);
        #[cfg(target_os = "linux")]
        assert_eq!(WANTED_HW, AVPixelFormat::AV_PIX_FMT_VAAPI);
    }

    #[test]
    fn hw_device_create_against_real_driver() {
        // Try to allocate the real HW device context. On a workstation
        // with a working GPU this passes; on a headless CI runner
        // av_hwdevice_ctx_create returns < 0 (no D3D11 device / no
        // /dev/dri/renderD128). Skip-with-warning rather than fail so
        // we don't break unrelated PR builds.
        let mut decoder = FfmpegHwDecoder::new();
        match decoder.create_hw_device() {
            Ok(()) => {
                assert!(
                    !decoder.hw_device_ctx.is_null(),
                    "create_hw_device returned Ok but left ctx null",
                );
                // Cleanup happens via Drop.
            }
            Err(e) => {
                eprintln!(
                    "hw_device_create: skipping live-driver check — {} \
                     (headless / no GPU is OK here)",
                    e
                );
            }
        }
    }
}
