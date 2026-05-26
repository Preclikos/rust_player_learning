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
    input_rate: u32,
    output_rate: u32,
    channels: usize,
}

unsafe impl Send for MediaCodecAudioDecoder {}

impl MediaCodecAudioDecoder {
    pub fn new() -> Self {
        Self { codec: None, input_rate: 44100, output_rate: 44100, channels: 2 }
    }
}

fn resample_linear(input: &[f32], channels: usize, from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let in_frames = input.len() / channels;
    let ratio = from_rate as f64 / to_rate as f64;
    let out_frames = (in_frames as f64 / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_frames * channels);
    for i in 0..out_frames {
        let pos = i as f64 * ratio;
        let idx0 = pos as usize;
        let idx1 = (idx0 + 1).min(in_frames - 1);
        let frac = (pos - idx0 as f64) as f32;
        for ch in 0..channels {
            let s0 = input[idx0 * channels + ch];
            let s1 = input[idx1 * channels + ch];
            out.push(s0 + (s1 - s0) * frac);
        }
    }
    out
}

impl AudioDecoder for MediaCodecAudioDecoder {
    fn configure(&mut self, params: AudioDecoderParams) -> Result<(), DecoderError> {
        let mime = match params.codec {
            AudioCodec::Aac => "audio/mp4a-latm",
            AudioCodec::Ac3 => "audio/ac3",
            AudioCodec::Eac3 => "audio/eac3",
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
        self.input_rate = params.input_sample_rate;
        self.output_rate = params.output_sample_rate;
        self.channels = params.input_channels as usize;
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        let codec = self
            .codec
            .as_ref()
            .ok_or_else(|| -> DecoderError { "submit before configure".into() })?;

        let mut input_buf = loop {
            match codec
                .dequeue_input_buffer(Duration::from_millis(5))
                .map_err(|e| -> DecoderError { format!("audio dequeue_input: {:?}", e).into() })?
            {
                DequeuedInputBufferResult::Buffer(b) => break b,
                DequeuedInputBufferResult::TryAgainLater => {
                    std::thread::yield_now();
                }
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
                    let samples_f32: Vec<f32> =
                        pcm_i16.iter().map(|&s| s as f32 / 32768.0_f32).collect();
                    resample_linear(&samples_f32, self.channels, self.input_rate, self.output_rate)
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
