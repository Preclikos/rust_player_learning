// MediaCodec-based AAC audio decoder for Android.
//
// Decodes raw AAC access units (from DASH mdat) to interleaved stereo f32 PCM.
// Output is signed 16-bit PCM converted to f32; no software resampling is
// applied — the stream's native rate is passed through. The cpal AudioRenderer
// queues samples at the system output rate; a slight pitch shift is possible
// if the device's preferred rate differs from the stream's.
//
// TODO: add software resampling (e.g. via rubato) if audible pitch shift is
// observed on devices whose preferred output rate differs from the stream rate.

#![cfg(target_os = "android")]

use std::time::Duration;

use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;

use super::{AudioCodec, AudioDecoder, AudioDecoderParams, DecodedAudioFrame, DecoderError};

pub struct MediaCodecAudioDecoder {
    codec: Option<MediaCodec>,
}

unsafe impl Send for MediaCodecAudioDecoder {}

impl MediaCodecAudioDecoder {
    pub fn new() -> Self {
        Self { codec: None }
    }
}

impl AudioDecoder for MediaCodecAudioDecoder {
    fn configure(&mut self, params: AudioDecoderParams) -> Result<(), DecoderError> {
        let mime = match params.codec {
            AudioCodec::Aac => "audio/mp4a-latm",
        };

        let codec =
            MediaCodec::from_decoder_type(mime).ok_or_else(|| -> DecoderError {
                format!("MediaCodec::from_decoder_type({}) returned None", mime).into()
            })?;

        let mut format = MediaFormat::new();
        format.set_str("mime", mime);
        format.set_i32("channel-count", params.input_channels as i32);
        format.set_i32("sample-rate", params.input_sample_rate as i32);
        if !params.codec_specific_data.is_empty() {
            format.set_buffer("csd-0", &params.codec_specific_data);
        }

        codec
            .configure(&format, None, MediaCodecDirection::Decoder)
            .map_err(|e| -> DecoderError { format!("audio configure: {:?}", e).into() })?;
        codec
            .start()
            .map_err(|e| -> DecoderError { format!("audio start: {:?}", e).into() })?;

        log::info!(
            "MediaCodecAudioDecoder: configured {}, {}Hz {}ch",
            mime,
            params.input_sample_rate,
            params.input_channels
        );
        self.codec = Some(codec);
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        let codec = self
            .codec
            .as_ref()
            .ok_or_else(|| -> DecoderError { "submit before configure".into() })?;

        let input = codec
            .dequeue_input_buffer(Duration::ZERO)
            .map_err(|e| -> DecoderError { format!("audio dequeue_input: {:?}", e).into() })?;

        let mut input_buf = match input {
            DequeuedInputBufferResult::Buffer(b) => b,
            DequeuedInputBufferResult::TryAgainLater => {
                return Err("no audio input buffer; retry".into());
            }
        };

        let dst = input_buf.buffer_mut();
        let copy_len = sample.len().min(dst.len());
        unsafe {
            std::ptr::copy_nonoverlapping(
                sample.as_ptr(),
                dst.as_mut_ptr() as *mut u8,
                copy_len,
            );
        }

        codec
            .queue_input_buffer(input_buf, 0, copy_len, pts_us as u64, 0)
            .map_err(|e| -> DecoderError { format!("audio queue_input: {:?}", e).into() })?;

        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let codec = self
            .codec
            .as_ref()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;

        let dequeued = codec
            .dequeue_output_buffer(Duration::ZERO)
            .map_err(|e| -> DecoderError { format!("audio dequeue_output: {:?}", e).into() })?;

        match dequeued {
            DequeuedOutputBufferInfoResult::Buffer(out) => {
                let pts_us = out.info().presentation_time_us();
                let offset = out.info().offset() as usize;
                let size = out.info().size() as usize;

                // Read signed 16-bit PCM from the output buffer and convert to f32.
                // buffer() returns &[u8]; valid until release_output_buffer is called.
                let samples = {
                    let buf: &[u8] = out.buffer();
                    let pcm = &buf[offset..offset + size];
                    // Cast &[u8] → &[i16] (little-endian, 2 bytes per sample).
                    let pcm_i16: &[i16] = bytemuck::cast_slice(pcm);
                    pcm_i16.iter().map(|&s| s as f32 / 32768.0_f32).collect::<Vec<f32>>()
                };

                codec
                    .release_output_buffer(out, false)
                    .map_err(|e| -> DecoderError { format!("audio release: {:?}", e).into() })?;

                Ok(Some(DecodedAudioFrame {
                    pts_ms: pts_us / 1000,
                    samples,
                }))
            }
            DequeuedOutputBufferInfoResult::TryAgainLater => Ok(None),
            DequeuedOutputBufferInfoResult::OutputFormatChanged => {
                let fmt = codec.output_format();
                log::info!(
                    "audio output format: {}Hz {}ch",
                    fmt.i32("sample-rate").unwrap_or(0),
                    fmt.i32("channel-count").unwrap_or(0),
                );
                Ok(None)
            }
            DequeuedOutputBufferInfoResult::OutputBuffersChanged => Ok(None),
        }
    }

    fn flush(&mut self) -> Result<(), DecoderError> {
        if let Some(codec) = self.codec.as_ref() {
            codec
                .flush()
                .map_err(|e| -> DecoderError { format!("audio flush: {:?}", e).into() })?;
        }
        Ok(())
    }
}
