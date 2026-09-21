//! Interleaved-PCM helpers for decoders that hand us samples in the source
//! layout and leave the conversion to us — MediaCodec (Android) and
//! WebCodecs (browser). FFmpeg's decoder goes through swresample instead.
//!
//! The pipeline contract downstream of every audio decoder is "packed
//! stereo f32 at the output device rate" (see `DecodedAudioFrame`).

/// Downmix interleaved multichannel f32 PCM to stereo. Channel order follows
/// the AOSP / WebCodecs convention: mono / L,R / L,R,C / L,R,C,LFE,BL,BR for
/// 1/2/3/6 channels respectively. ITU-R BS.775 coefficients for 5.1 and a
/// simple average for unusual counts so the result is always audible rather
/// than mis-routed silence.
#[cfg_attr(not(any(target_os = "android", target_arch = "wasm32")), allow(dead_code))]
pub fn downmix_to_stereo(input: &[f32], channels: usize) -> Vec<f32> {
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

/// Linear-interpolation resample of interleaved PCM. Good enough for the
/// 44.1 ↔ 48 kHz device-rate mismatch it exists for; a proper windowed-sinc
/// resampler would be the upgrade if aliasing ever becomes audible.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn resample_linear(input: &[f32], channels: usize, from_rate: u32, to_rate: u32) -> Vec<f32> {
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

/// Linear resampler that keeps its phase across chunks, so a stream of
/// short chunks (10–30 ms AUs) comes out with exactly `in / ratio` frames
/// overall. [`resample_linear`] rounds each chunk UP on its own: at
/// 96 → 44.1 kHz a 1024-frame AU gives 471 frames instead of 470.4, a
/// +0.13 % speed error that made the A/V aligner trim 10 ms every ~8 s.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub struct LinearResampler {
    channels: usize,
    /// Input frames per output frame.
    ratio: f64,
    /// Position of the next output frame in input frames, relative to frame 0
    /// of the chunk about to be processed. Negative (≥ −1) means it falls
    /// between the previous chunk's last frame (`tail`) and this chunk's
    /// first.
    pos: f64,
    /// Last input frame of the previous chunk.
    tail: Vec<f32>,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
impl LinearResampler {
    pub fn new(channels: usize, from_rate: u32, to_rate: u32) -> Self {
        Self {
            channels: channels.max(1),
            ratio: from_rate.max(1) as f64 / to_rate.max(1) as f64,
            pos: 0.0,
            tail: Vec::new(),
        }
    }

    pub fn is_identity(&self) -> bool {
        (self.ratio - 1.0).abs() < 1e-9
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.is_identity() || input.is_empty() {
            return input.to_vec();
        }
        let ch = self.channels;
        let in_frames = input.len() / ch;
        if in_frames == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity((in_frames as f64 / self.ratio) as usize * ch + ch);
        let mut pos = self.pos;
        let last = (in_frames - 1) as f64;
        while pos <= last {
            let idx0 = pos.floor() as i64;
            let frac = (pos - idx0 as f64) as f32;
            let i1 = ((idx0 + 1).max(0) as usize).min(in_frames - 1);
            for c in 0..ch {
                let s0 = if idx0 < 0 {
                    self.tail.get(c).copied().unwrap_or(input[c])
                } else {
                    input[idx0 as usize * ch + c]
                };
                let s1 = input[i1 * ch + c];
                out.push(s0 + (s1 - s0) * frac);
            }
            pos += self.ratio;
        }
        // Carry the position into the next chunk's frame of reference.
        self.pos = pos - in_frames as f64;
        self.tail.clear();
        self.tail.extend_from_slice(&input[(in_frames - 1) * ch..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stateful_resampler_output_count_is_exact_over_many_chunks() {
        // 96 kHz → 44.1 kHz, 1024-frame AUs: 470.4 frames each on average.
        let mut rs = LinearResampler::new(2, 96_000, 44_100);
        let chunk = vec![0.5f32; 1024 * 2];
        let mut total = 0usize;
        for _ in 0..1000 {
            total += rs.process(&chunk).len() / 2;
        }
        let expected = 1000.0 * 1024.0 * 44_100.0 / 96_000.0;
        assert!((total as f64 - expected).abs() <= 1.0, "total {total} vs {expected}");
    }

    #[test]
    fn per_chunk_resampler_rounds_each_chunk_up() {
        // Documents the drift the stateful version exists to remove.
        let chunk = vec![0.5f32; 1024 * 2];
        assert_eq!(resample_linear(&chunk, 2, 96_000, 44_100).len() / 2, 471);
    }

    #[test]
    fn stateful_resampler_identity_and_interpolation() {
        let mut id = LinearResampler::new(1, 48_000, 48_000);
        assert_eq!(id.process(&[1.0, 2.0, 3.0]), vec![1.0, 2.0, 3.0]);
        // 2:1 decimation of a ramp lands on every other input sample.
        let mut half = LinearResampler::new(1, 48_000, 24_000);
        assert_eq!(half.process(&[0.0, 1.0, 2.0, 3.0]), vec![0.0, 2.0]);
        // The carried position continues the ramp across the chunk border.
        assert_eq!(half.process(&[4.0, 5.0, 6.0, 7.0]), vec![4.0, 6.0]);
    }
}

/// Planar → interleaved: `planes[c][i]` → `out[i * channels + c]`.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn interleave(planes: &[Vec<f32>]) -> Vec<f32> {
    let channels = planes.len();
    let frames = planes.first().map(|p| p.len()).unwrap_or(0);
    let mut out = Vec::with_capacity(frames * channels);
    for i in 0..frames {
        for p in planes {
            out.push(p.get(i).copied().unwrap_or(0.0));
        }
    }
    out
}
