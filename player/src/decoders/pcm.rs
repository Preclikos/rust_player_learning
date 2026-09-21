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

/// Planar → interleaved: `planes[c][i]` → `out[i * channels + c]`.
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
