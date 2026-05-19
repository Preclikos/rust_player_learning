// FFmpeg-based HwVideoDecoder for desktop (Windows D3D11VA + Linux VAAPI).
//
// This is the trait wrapper for the inline FFmpeg pipeline currently in
// `player::player::video_play` / `video_decoder_task`. Migration steps:
//
//   1. (this file) construct the decoder + hw_device_ctx, feed initial NALUs.
//   2. `submit(sample, pts)` parses NALUs out of the mdat sample and pushes
//      packets into the decoder.
//   3. `try_recv()` calls `receive_frame`, extracts the platform-native
//      handle (`D3D11Texture2D*` on Windows, `VASurfaceID` on Linux) and
//      hands it to the renderer as `PlatformFrame`.
//
// What's NOT done yet:
//   - The native handle extraction in step 3 lives in
//     `renderers/video/video_frame.rs::VideoFrame::new`. We need to lift
//     that into try_recv (or expose a small accessor) so the renderer can
//     consume `PlatformFrame` directly instead of `Arc<ffmpeg::Video>`.
//   - `video_play` still creates and runs its FFmpeg decoder inline. Once
//     that switches to `Box<dyn HwVideoDecoder>` calling this impl, the
//     same call sites will work for `MediaCodecDecoder` on Android.

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

// hw_device_ctx is an FFmpeg-managed refcounted resource; only this struct
// touches it (we don't share across threads after construction except via
// FFmpeg's own internal refcount).
unsafe impl Send for FfmpegHwDecoder {}

impl FfmpegHwDecoder {
    pub fn new() -> Result<Self, DecoderError> {
        Ok(Self {
            decoder: None,
            hw_device_ctx: std::ptr::null_mut(),
        })
    }

    fn create_hw_device(&mut self) -> Result<(), DecoderError> {
        #[cfg(target_os = "windows")]
        let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;
        #[cfg(target_os = "linux")]
        let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI;

        let mut ctx: *mut AVBufferRef = std::ptr::null_mut();
        let ret = unsafe {
            av_hwdevice_ctx_create(
                &mut ctx,
                device_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
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
        // ffmpeg-next's Drop on decoder will release the codec context, which
        // releases its reference to hw_device_ctx. We don't free the buffer
        // ourselves to avoid double-free.
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
            let ctx_ptr = ctx.as_mut_ptr();
            (*ctx_ptr).hw_device_ctx = self.hw_device_ctx;
        }

        let mut decoder = ctx
            .decoder()
            .video()
            .map_err(|e| -> DecoderError { format!("decoder open: {}", e).into() })?;

        // Feed codec-private NALUs (VPS/SPS/PPS) so the decoder can parse
        // subsequent slices. For HEVC this is the parsed hvcC arrays.
        // The video_play caller is expected to extract them via
        // `crypto::parse_hvcc_nalus` (or re_mp4's hvcC) and pass the body
        // here via `codec_specific_data` framed as length-prefixed NALUs.
        //
        // TODO: decide on the exact framing of `codec_specific_data`. For
        // now the caller can pass an empty Vec and feed NALUs by calling
        // `submit` directly, which works because parse_hevc_nalu/apped_hevc_header
        // already prepend start codes.
        if !params.codec_specific_data.is_empty() {
            for nalu in parse_hevc_nalu(&params.codec_specific_data)
                .map_err(|e| -> DecoderError { format!("hvcc NALU parse: {}", e).into() })?
            {
                let mut packet = Packet::new(nalu.len());
                packet.data_mut().unwrap().clone_from_slice(&nalu);
                decoder
                    .send_packet(&packet)
                    .map_err(|e| -> DecoderError { format!("send hvcC: {}", e).into() })?;
            }
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
            // pts is in microseconds at the trait boundary; FFmpeg pts is in
            // whatever time_base the codec uses. The existing pipeline stores
            // pts in milliseconds; keep parity by dividing by 1000 here.
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
                let pts_us = frame.pts().unwrap_or(0) * 1000;
                let width = frame.width();
                let height = frame.height();

                // TODO: extract the platform-native handle from `frame`.
                // The extraction logic currently lives in
                // `renderers/video/video_frame.rs::VideoFrame::new`. Move it
                // here (or expose a helper) and build the appropriate
                // PlatformFrame variant.
                //
                // For Windows: read (*frame.as_ptr()).data[0] as
                // ID3D11Texture2D*, data[1] as the array index.
                //
                // For Linux: read (*frame.as_ptr()).data[3] as VASurfaceID,
                // hw_device_ctx->hwctx->display as the VADisplay.
                let _ = (pts_us, width, height, frame);
                Err("FfmpegHwDecoder::try_recv: native handle extraction is TODO".into())
            }
            Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_sys_next::EAGAIN => {
                Ok(None)
            }
            Err(e) => Err(format!("receive_frame: {}", e).into()),
        }
    }
}

// Quiet "unused" warnings on the imports until try_recv is finished.
#[allow(dead_code)]
fn _unused_imports() -> Option<()> {
    let _: PlatformFrame;
    let _: Arc<()>;
    let _ = apped_hevc_header;
    None
}
