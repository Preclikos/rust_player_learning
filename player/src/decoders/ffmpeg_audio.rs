#![cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios"
))]

use ffmpeg_next::format::sample::Type;
use ffmpeg_next::software::resampling::Context as ResampleCtx;
use ffmpeg_next::util::channel_layout::ChannelLayout;
use ffmpeg_next::Packet;

use super::{AudioCodec, AudioDecoder, AudioDecoderParams, DecodedAudioFrame, DecoderError};

pub struct FfmpegAudioDecoder {
    decoder: Option<ffmpeg_next::decoder::Audio>,
    /// Built lazily from the FIRST decoded frame's actual format. Manifest
    /// hints (channel count, sample rate) lie often enough for AC-3 / EAC-3
    /// — `AudioChannelConfiguration` may be missing or wrong, and the
    /// bitstream layout is authoritative. Rebuilt if the format changes
    /// mid-stream (e.g. dependent EAC-3 substream comes online).
    resampler: Option<ResampleCtx>,
    /// Cached resampler input definition so we can detect when a new frame
    /// no longer matches and trigger a rebuild.
    resampler_in_rate: u32,
    resampler_in_layout_bits: u64,
    /// Target output rate (cpal device rate) passed in at configure time.
    output_sample_rate: u32,
}

unsafe impl Send for FfmpegAudioDecoder {}

impl FfmpegAudioDecoder {
    pub fn new() -> Self {
        Self {
            decoder: None,
            resampler: None,
            resampler_in_rate: 0,
            resampler_in_layout_bits: 0,
            output_sample_rate: 0,
        }
    }

    /// (Re)build the resampler so its input matches the decoded frame's
    /// actual `rate` + `layout`, and its output is fixed stereo at the
    /// device rate. Called lazily on first frame and whenever the input
    /// format drifts.
    fn build_resampler(&mut self, in_rate: u32, in_layout: ChannelLayout) -> Result<(), DecoderError> {
        let resampler = ResampleCtx::get(
            ffmpeg_next::util::format::sample::Sample::F32(Type::Planar),
            in_layout,
            in_rate,
            ffmpeg_next::util::format::sample::Sample::F32(Type::Packed),
            ChannelLayout::STEREO,
            self.output_sample_rate,
        )
        .map_err(|e| -> DecoderError {
            format!("resampler init ({}Hz {}ch -> {}Hz stereo): {}",
                in_rate, in_layout.channels(), self.output_sample_rate, e).into()
        })?;
        self.resampler_in_rate = in_rate;
        self.resampler_in_layout_bits = in_layout.bits();
        self.resampler = Some(resampler);
        log::info!(
            "FfmpegAudioDecoder: resampler {}Hz {}ch -> {}Hz stereo",
            in_rate, in_layout.channels(), self.output_sample_rate
        );
        Ok(())
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

        let in_layout = match params.input_channels {
            1 => ffmpeg_next::util::channel_layout::ChannelLayout::MONO,
            2 => ffmpeg_next::util::channel_layout::ChannelLayout::STEREO,
            n => ffmpeg_next::util::channel_layout::ChannelLayout::default(n as i32),
        };

        unsafe {
            let ctx_ptr = ctx.as_mut_ptr();

            // Install codec-specific extradata for codecs that need it
            // (AAC's 2-byte AudioSpecificConfig). AC-3 / EAC-3 don't —
            // their decoders read params from each frame's syncinfo.
            if !params.codec_specific_data.is_empty() {
                let padding = ffmpeg_sys_next::AV_INPUT_BUFFER_PADDING_SIZE as usize;
                let extra = ffmpeg_sys_next::av_mallocz(params.codec_specific_data.len() + padding);
                if extra.is_null() {
                    return Err("av_mallocz failed for audio extradata".into());
                }
                std::ptr::copy_nonoverlapping(
                    params.codec_specific_data.as_ptr(),
                    extra as *mut u8,
                    params.codec_specific_data.len(),
                );
                (*ctx_ptr).extradata = extra as *mut u8;
                (*ctx_ptr).extradata_size = params.codec_specific_data.len() as i32;
            }

            // Seed sample_rate / channel layout from the manifest so the
            // decoder opens with sane defaults. AC-3 / EAC-3 carry their
            // own params in syncinfo and will overwrite these on the first
            // decoded packet — but several FFmpeg audio decoders refuse to
            // open with sample_rate=0, which is what we'd get without this.
            (*ctx_ptr).sample_rate = params.input_sample_rate as i32;
            (*ctx_ptr).ch_layout = in_layout.into();
        }

        let mut decoder = ctx
            .decoder()
            .audio()
            .map_err(|e| -> DecoderError { format!("audio decoder open: {}", e).into() })?;

        // Most FFmpeg audio decoders output planar float natively, but
        // ask explicitly so we don't have to branch on sample_fmt later.
        decoder.request_format(ffmpeg_next::util::format::sample::Sample::F32(Type::Planar));

        log::info!(
            "FfmpegAudioDecoder: opened {:?} (manifest hint: {}Hz {}ch -> stereo {}Hz). \
             Resampler will be built lazily from the first decoded frame.",
            params.codec,
            params.input_sample_rate,
            params.input_channels,
            params.output_sample_rate,
        );
        self.decoder = Some(decoder);
        self.output_sample_rate = params.output_sample_rate;
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

        // Log + swallow per-packet errors so one bad EAC-3 frame (e.g. a
        // segment-boundary sync glitch) doesn't kill the audio task for
        // the rest of playback. The decoder typically resyncs on the next
        // packet that lands on a syncword.
        if let Err(e) = decoder.send_packet(&packet) {
            log::warn!(
                "audio send_packet failed (pts={}ms, {} bytes): {}",
                pts_us / 1000, sample.len(), e
            );
        }
        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;

        let mut frame = ffmpeg_next::util::frame::Audio::empty();
        match decoder.receive_frame(&mut frame) {
            Ok(()) => {
                let pts_ms = frame.pts().unwrap_or(0);

                // Authoritative input format comes from the frame itself.
                // For EAC-3 in particular, the manifest hint is unreliable —
                // syncinfo in the bitstream is the ground truth.
                let in_rate = frame.rate();
                let mut in_layout = frame.channel_layout();
                if in_layout.bits() == 0 && frame.channels() > 0 {
                    // Some decoders leave channel_layout = 0 and only
                    // populate channels. Fall back to the default layout.
                    in_layout = ChannelLayout::default(frame.channels() as i32);
                }

                let needs_rebuild = self.resampler.is_none()
                    || self.resampler_in_rate != in_rate
                    || self.resampler_in_layout_bits != in_layout.bits();
                if needs_rebuild {
                    self.build_resampler(in_rate, in_layout)?;
                }
                let resampler = self.resampler.as_mut().unwrap();

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

#[cfg(test)]
mod tests {
    //! Platform smoke tests for the FFmpeg audio decoder build.
    //!
    //! Catches the easy regressions:
    //!   - FFmpeg was rebuilt without AAC / AC-3 / E-AC-3 support
    //!     (we'd hit the lookup failure at runtime when the user
    //!     opens an EC-3 audio adaptation — better to catch at CI).
    //!   - ChannelLayout construction for the channel counts the
    //!     player asks for (1/2/6/8) compiles and yields a layout
    //!     with the right channel count.
    //!   - The resampling output format we hand to ResampleCtx::get
    //!     (F32 packed stereo) is still supported by this FFmpeg
    //!     build.
    //!
    //! Real-decode tests would need PCM-back samples and are skipped
    //! here — the integration smoke-app exercises that path.

    use super::*;
    use ffmpeg_next::util::channel_layout::ChannelLayout;
    use ffmpeg_next::util::format::sample::{Sample, Type};

    #[test]
    fn ffmpeg_finds_aac_decoder() {
        assert!(
            ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::AAC).is_some(),
            "FFmpeg build is missing the AAC decoder",
        );
    }

    #[test]
    fn ffmpeg_finds_ac3_and_eac3_decoders() {
        // Both AC-3 and E-AC-3 (a.k.a. EC-3) are in the user's manifest,
        // exposed as `ac-3` and `ec-3` mime codec strings respectively.
        assert!(
            ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::AC3).is_some(),
            "FFmpeg build is missing the AC-3 decoder",
        );
        assert!(
            ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::EAC3).is_some(),
            "FFmpeg build is missing the E-AC-3 decoder",
        );
    }

    #[test]
    fn channel_layout_constants_have_expected_channels() {
        assert_eq!(ChannelLayout::MONO.channels(), 1);
        assert_eq!(ChannelLayout::STEREO.channels(), 2);
    }

    #[test]
    fn channel_layout_default_handles_5_1_and_7_1() {
        // The player uses `ChannelLayout::default(N)` for unusual N
        // (matching the EC-3 6-channel and Atmos 8-channel cases).
        // ffmpeg-next's `default(i32) -> ChannelLayout` returns the
        // FFmpeg-canonical layout for that channel count — verify it
        // round-trips the count.
        assert_eq!(ChannelLayout::default(6).channels(), 6);
        assert_eq!(ChannelLayout::default(8).channels(), 8);
    }

    #[test]
    fn resampler_can_be_built_for_typical_stream_params() {
        // The "real" resampler config the player builds in `build_resampler`:
        // F32 planar input, F32 packed stereo output, 48 kHz. Verifies
        // ffmpeg-next + libswresample actually accept this combo on this
        // platform — a build with libswresample missing would fail here
        // at `ResampleCtx::get` long before any real audio frame.
        let r = ffmpeg_next::software::resampling::Context::get(
            Sample::F32(Type::Planar),
            ChannelLayout::default(6),
            48_000,
            Sample::F32(Type::Packed),
            ChannelLayout::STEREO,
            48_000,
        );
        assert!(r.is_ok(), "resampler unavailable: {:?}", r.err());
    }
}
