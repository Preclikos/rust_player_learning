#![cfg(any(target_os = "windows", target_os = "linux"))]

use std::sync::Arc;

use ffmpeg_next::Packet;
use ffmpeg_sys_next::{av_hwdevice_ctx_create, AVBufferRef, AVHWDeviceType};

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

impl HwVideoDecoder for FfmpegHwDecoder {
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
            // D3D11/VAAPI pools default to ~20 surfaces. The video channel
            // holds up to 64 decoded frames, so we need a pool large enough
            // to cover them all. extra_hw_frames enlarges the fixed pool by
            // this amount on top of FFmpeg's inferred minimum.
            (*ctx.as_mut_ptr()).extra_hw_frames = 64;
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
            decoder
                .send_packet(&packet)
                .map_err(|e| -> DecoderError { format!("send_packet: {}", e).into() })?;
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
                Ok(Some(DecodedVideoFrame {
                    pts_us,
                    width,
                    height,
                    native: PlatformFrame::FfmpegVideo(Arc::new(frame)),
                }))
            }
            Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_sys_next::EAGAIN => {
                Ok(None)
            }
            Err(e) => Err(format!("receive_frame: {}", e).into()),
        }
    }
}
