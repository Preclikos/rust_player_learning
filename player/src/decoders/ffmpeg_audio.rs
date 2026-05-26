#![cfg(any(target_os = "windows", target_os = "linux"))]

use ffmpeg_next::format::sample::Type;
use ffmpeg_next::software::resampling::Context as ResampleCtx;
use ffmpeg_next::Packet;

use super::{AudioCodec, AudioDecoder, AudioDecoderParams, DecodedAudioFrame, DecoderError};

pub struct FfmpegAudioDecoder {
    decoder: Option<ffmpeg_next::decoder::Audio>,
    resampler: Option<ResampleCtx>,
}

unsafe impl Send for FfmpegAudioDecoder {}

impl FfmpegAudioDecoder {
    pub fn new() -> Self {
        Self { decoder: None, resampler: None }
    }
}

impl AudioDecoder for FfmpegAudioDecoder {
    fn configure(&mut self, params: AudioDecoderParams) -> Result<(), DecoderError> {
        let codec_id = match params.codec {
            AudioCodec::Aac => ffmpeg_next::codec::Id::AAC,
            AudioCodec::Ac3 => ffmpeg_next::codec::Id::AC3,
            AudioCodec::Eac3 => ffmpeg_next::codec::Id::EAC3,
        };
        let codec = ffmpeg_next::decoder::find(codec_id).ok_or_else(|| -> DecoderError {
            format!("cannot find FFmpeg decoder for {:?}", params.codec).into()
        })?;

        let mut ctx = ffmpeg_next::codec::Context::new_with_codec(codec);

        // Install the 2-byte AudioSpecificConfig as extradata so FFmpeg can
        // parse codec parameters at open time and accept raw mdat frames.
        if !params.codec_specific_data.is_empty() {
            unsafe {
                let ctx_ptr = ctx.as_mut_ptr();
                let padding = ffmpeg_sys_next::AV_INPUT_BUFFER_PADDING_SIZE as usize;
                let extra = ffmpeg_sys_next::av_mallocz(params.codec_specific_data.len() + padding);
                if extra.is_null() {
                    return Err("av_mallocz failed for AAC extradata".into());
                }
                std::ptr::copy_nonoverlapping(
                    params.codec_specific_data.as_ptr(),
                    extra as *mut u8,
                    params.codec_specific_data.len(),
                );
                (*ctx_ptr).extradata = extra as *mut u8;
                (*ctx_ptr).extradata_size = params.codec_specific_data.len() as i32;
            }
        }

        let decoder = ctx
            .decoder()
            .audio()
            .map_err(|e| -> DecoderError { format!("audio decoder open: {}", e).into() })?;

        let in_layout = match params.input_channels {
            1 => ffmpeg_next::util::channel_layout::ChannelLayout::MONO,
            2 => ffmpeg_next::util::channel_layout::ChannelLayout::STEREO,
            n => ffmpeg_next::util::channel_layout::ChannelLayout::default(n as i32),
        };

        let resampler = ResampleCtx::get(
            ffmpeg_next::util::format::sample::Sample::F32(Type::Planar),
            in_layout,
            params.input_sample_rate,
            ffmpeg_next::util::format::sample::Sample::F32(Type::Packed),
            ffmpeg_next::util::channel_layout::ChannelLayout::STEREO,
            params.output_sample_rate,
        )
        .map_err(|e| -> DecoderError { format!("resampler init: {}", e).into() })?;

        self.decoder = Some(decoder);
        self.resampler = Some(resampler);
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| -> DecoderError { "submit before configure".into() })?;

        let mut packet = Packet::new(sample.len());
        // Store pts in milliseconds (microseconds / 1000).
        packet.set_pts(Some(pts_us / 1000));
        packet.data_mut().unwrap().clone_from_slice(sample);

        decoder
            .send_packet(&packet)
            .map_err(|e| -> DecoderError { format!("audio send_packet: {}", e).into() })?;
        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;
        let resampler = self
            .resampler
            .as_mut()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;

        let mut frame = ffmpeg_next::util::frame::Audio::empty();
        match decoder.receive_frame(&mut frame) {
            Ok(()) => {
                let pts_ms = frame.pts().unwrap_or(0) as i64;

                let mut dst = ffmpeg_next::util::frame::Audio::empty();
                resampler
                    .run(&frame, &mut dst)
                    .map_err(|e| -> DecoderError { format!("resample: {}", e).into() })?;

                if dst.samples() == 0 {
                    // Resampler may buffer; flush to drain any pending output.
                    resampler
                        .flush(&mut dst)
                        .map_err(|e| -> DecoderError { format!("resample flush: {}", e).into() })?;
                    if dst.samples() == 0 {
                        return Ok(None);
                    }
                }
                let expected_bytes = dst.samples() * 2 * std::mem::size_of::<f32>();
                let samples: Vec<f32> = bytemuck::cast_slice(&dst.data(0)[..expected_bytes]).to_vec();
                Ok(Some(DecodedAudioFrame { pts_ms, samples }))
            }
            Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_sys_next::EAGAIN => {
                Ok(None)
            }
            Err(e) => Err(format!("audio receive_frame: {}", e).into()),
        }
    }
}
