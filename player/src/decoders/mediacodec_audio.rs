// MediaCodec-based AAC / AC-3 / EAC-3 audio decoder for Android.
//
// Decodes raw access units (from DASH mdat) to interleaved stereo f32 PCM
// at the stream's native sample rate. Multichannel input (typical for AC-3
// and EAC-3 5.1) is downmixed to stereo here — the cpal AudioRenderer is
// always stereo, so the decoder owns the downmix instead of relying on the
// renderer to interpret arbitrary channel counts.
//
// The actual output channel count comes from MediaCodec's OutputFormatChanged
// event, not the input hint — for EAC-3 streams the MPD often advertises
// "6" channels via AudioChannelConfiguration but the codec may decide
// differently, and trusting the input hint led to silent / scrambled audio.

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
    /// Actual channel count of the decoder's PCM output. Initially set from
    /// the manifest hint; corrected by the OutputFormatChanged event before
    /// the first frame is drained.
    channels: usize,
}

unsafe impl Send for MediaCodecAudioDecoder {}

impl MediaCodecAudioDecoder {
    pub fn new() -> Self {
        Self { codec: None, input_rate: 44100, output_rate: 44100, channels: 2 }
    }
}

/// Downmix interleaved multichannel f32 PCM to stereo. Channel order for
/// MediaCodec PCM follows the AOSP convention: mono / L,R / L,R,C / L,R,C,LFE,BL,BR
/// for 1/2/3/6 channels respectively. We use ITU-R BS.775 coefficients for
/// 5.1 and a simple average for unusual counts so the result is always
/// audible rather than mis-routed silence.
fn downmix_to_stereo(input: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        let mut out = Vec::with_capacity(input.len() * 2);
        for &s in input {
            out.push(s);
            out.push(s);
        }
        return out;
    }
    if channels == 2 {
        return input.to_vec();
    }
    let frames = input.len() / channels;
    let mut out = Vec::with_capacity(frames * 2);
    const ATT: f32 = 0.707; // -3 dB
    for f in 0..frames {
        let base = f * channels;
        let (l, r) = match channels {
            // L,R,C — fold C equally into L/R.
            3 => (input[base] + ATT * input[base + 2], input[base + 1] + ATT * input[base + 2]),
            // L,R,BL,BR (quad).
            4 => (
                input[base] + ATT * input[base + 2],
                input[base + 1] + ATT * input[base + 3],
            ),
            // L,R,C,BL,BR (5.0) — same as 5.1 minus LFE.
            5 => (
                input[base] + ATT * input[base + 2] + ATT * input[base + 3],
                input[base + 1] + ATT * input[base + 2] + ATT * input[base + 4],
            ),
            // L,R,C,LFE,BL,BR (5.1) — LFE dropped, standard ITU downmix.
            6 => (
                input[base] + ATT * input[base + 2] + ATT * input[base + 4],
                input[base + 1] + ATT * input[base + 2] + ATT * input[base + 5],
            ),
            // L,R,C,LFE,BL,BR,SL,SR (7.1).
            8 => (
                input[base]
                    + ATT * input[base + 2]
                    + ATT * input[base + 4]
                    + ATT * input[base + 6],
                input[base + 1]
                    + ATT * input[base + 2]
                    + ATT * input[base + 5]
                    + ATT * input[base + 7],
            ),
            // Unknown layout — average even-indexed → L, odd → R as a
            // best-effort fallback rather than producing silence.
            _ => {
                let mut sum_l = 0.0_f32;
                let mut sum_r = 0.0_f32;
                let mut cnt_l = 0_u32;
                let mut cnt_r = 0_u32;
                for c in 0..channels {
                    if c % 2 == 0 {
                        sum_l += input[base + c];
                        cnt_l += 1;
                    } else {
                        sum_r += input[base + c];
                        cnt_r += 1;
                    }
                }
                (sum_l / cnt_l.max(1) as f32, sum_r / cnt_r.max(1) as f32)
            }
        };
        // Clip — the additive downmix can exceed ±1.0 on hot 5.1 sources.
        out.push(l.clamp(-1.0, 1.0));
        out.push(r.clamp(-1.0, 1.0));
    }
    out
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

                // Decode pipeline:
                //   1. Read i16 PCM from the codec buffer
                //   2. Convert to f32 normalised to ±1.0
                //   3. Downmix to stereo (no-op when input is already stereo)
                //   4. Linear resample to the output device rate
                let samples = {
                    let buf: &[u8] = out.buffer();
                    let pcm = &buf[offset..offset + size];
                    let pcm_i16: &[i16] = bytemuck::cast_slice(pcm);
                    let raw_f32: Vec<f32> =
                        pcm_i16.iter().map(|&s| s as f32 / 32768.0_f32).collect();
                    let stereo = downmix_to_stereo(&raw_f32, self.channels);
                    // After downmix we ALWAYS have 2 channels.
                    resample_linear(&stereo, 2, self.input_rate, self.output_rate)
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
                let rate = fmt.i32("sample-rate").unwrap_or(0);
                let ch = fmt.i32("channel-count").unwrap_or(0);
                if rate > 0 {
                    self.input_rate = rate as u32;
                }
                if ch > 0 {
                    // The codec's actual output may not match the manifest
                    // hint (typical for AC-3 / EAC-3 — input_channels=6 was
                    // a guess; the bitstream might decode to 6, 2 or 8).
                    // Trust what the codec just told us.
                    self.channels = ch as usize;
                }
                log::info!(
                    "audio output format: {}Hz {}ch (decoder-reported)",
                    self.input_rate, self.channels
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
