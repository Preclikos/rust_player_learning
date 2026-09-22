//! Browser HDR calibration: does the browser's VideoFrame → RGBA conversion
//! of PQ content match the model `shader_chrome_inverse.wgsl` undoes?
//!
//! At renderer start-up a synthetic 10-bit PQ frame (grey ramp + colour
//! patches, built on the CPU — it is not video) goes through the SAME
//! `copyExternalImageToTexture` the real frames take, and the 32 KiB
//! result is read back once and compared against the model. The verdict
//! selects the shader's inverse variant, or disables the engine tonemap
//! on the web when the browser does something this build cannot undo
//! (then PQ frames show the browser's own picture, as before).
//!
//! The math helpers are target-independent so the evaluation is unit
//! tested natively; only `calibrate` touches the browser.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// Synthetic frame: `RAMP_W` columns, 4 rows. Rows 0-1 carry the grey
/// ramp (Y' code = column, Cb = Cr = 512), rows 2-3 four colour patches
/// of `RAMP_W / 4` columns each. 4:2:0 chroma: rows 0-1 share chroma row
/// 0, rows 2-3 chroma row 1.
pub(crate) const RAMP_W: u32 = 1024;
pub(crate) const FRAME_H: u32 = 4;
/// rgba16float bytes per texel.
pub(crate) const TEXEL_BYTES: u32 = 8;
/// One readback row in bytes (a multiple of 256, wgpu's copy alignment).
pub(crate) const BYTES_PER_ROW: u32 = RAMP_W * TEXEL_BYTES;

/// Colour patches as PQ-encoded BT.2020 R'G'B' — inside the BT.709 gamut
/// after the primaries matrix so no channel clips, and far enough from
/// grey that "matrix applied" and "matrix skipped" differ by > 0.2.
pub(crate) const PATCHES: [[f32; 3]; 4] = [
    [0.60, 0.20, 0.20],
    [0.30, 0.60, 0.30],
    [0.20, 0.20, 0.60],
    [0.70, 0.70, 0.30],
];

/// Grey-ramp tolerance: the browser feeds the CPU frame through an 8-bit
/// stage (4 codes ≈ 0.0046 PQ) plus float16 storage; a real tone-map
/// would miss by > 0.2.
const GREY_TOLERANCE: f32 = 0.02;
/// Colour-patch tolerance per channel. The browser's 8-bit stage quantises
/// the chroma codes (4 codes → up to 0.008 in B', amplified by the matrix)
/// and its own matrix rounds differently from ours: Chrome 153 measured
/// 0.036 against the matrix model and 0.25 against identity — the two
/// hypotheses differ by > 0.2, so 0.06 keeps them apart with margin.
const PATCH_TOLERANCE: f32 = 0.06;

/// sRGB electro-optical transfer (decode).
pub(crate) fn srgb_decode(v: f32) -> f32 {
    let x = v.max(0.0);
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

/// The renderer's BT.2020 → BT.709 primaries matrix (shader_pq_math.wgsl).
pub(crate) fn bt2020_to_bt709(c: [f32; 3]) -> [f32; 3] {
    [
        1.6605 * c[0] - 0.5876 * c[1] - 0.0728 * c[2],
        -0.1246 * c[0] + 1.1329 * c[1] - 0.0083 * c[2],
        -0.0182 * c[0] - 0.1006 * c[1] + 1.1187 * c[2],
    ]
}

/// PQ-encoded BT.2020 R'G'B' → limited-range 10-bit BT.2020-NCL Y'CbCr
/// codes (the inverse of the shader's `yuv_limited_to_pq_rgb`).
pub(crate) fn pq_rgb_to_yuv10(rgb: [f32; 3]) -> (u16, u16, u16) {
    let [r, g, b] = rgb;
    let y = 0.2627 * r + 0.6780 * g + 0.0593 * b;
    let cb = (b - y) / 1.8814;
    let cr = (r - y) / 1.4746;
    let code = |v: f32| v.round().clamp(0.0, 1023.0) as u16;
    (code(64.0 + 876.0 * y), code(512.0 + 896.0 * cb), code(512.0 + 896.0 * cr))
}

/// PQ code of a grey-ramp column (what `yuv_limited_to_pq_rgb` yields for
/// Y' = `code`, neutral chroma).
pub(crate) fn ramp_pq(code: u32) -> f32 {
    (code as f32 - 64.0) / 876.0
}

/// I420P10 planes of the synthetic frame, little-endian u16, Y then Cb
/// then Cr — the layout `new VideoFrame(buffer, {format: "I420P10"})`
/// expects without an explicit plane layout.
pub(crate) fn synthetic_planes() -> Vec<u8> {
    let w = RAMP_W as usize;
    let h = FRAME_H as usize;
    let (cw, ch) = (w / 2, h / 2);
    let mut y = vec![0u16; w * h];
    let mut cb = vec![512u16; cw * ch];
    let mut cr = vec![512u16; cw * ch];
    for x in 0..w {
        y[x] = x as u16;
        y[w + x] = x as u16;
    }
    let patch_w = w / PATCHES.len();
    for (i, p) in PATCHES.iter().enumerate() {
        let (yc, cbc, crc) = pq_rgb_to_yuv10(*p);
        for x in i * patch_w..(i + 1) * patch_w {
            y[2 * w + x] = yc;
            y[3 * w + x] = yc;
        }
        for x in i * patch_w / 2..(i + 1) * patch_w / 2 {
            cb[cw + x] = cbc;
            cr[cw + x] = crc;
        }
    }
    let mut out = Vec::with_capacity((y.len() + cb.len() + cr.len()) * 2);
    for plane in [&y, &cb, &cr] {
        for v in plane {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// IEEE half → f32 (rgba16float readback).
pub(crate) fn f16_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = ((bits >> 10) & 0x1f) as i32;
    let frac = (bits & 0x3ff) as f32;
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        0x1f => {
            if frac == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// What the browser was measured to do, expressed as the shader's
/// `web_inverse` selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Inverse {
    /// sRGB-encode(BT.2020→709 matrix · PQ codes) — Chrome.
    Matrix = 0,
    /// sRGB-encode(PQ codes) — matrix skipped.
    Identity = 1,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Verdict {
    pub inverse: Inverse,
    pub grey_rms: f32,
    pub grey_max: f32,
    pub patch_max: f32,
}

fn texel(rows: &[u8], bytes_per_row: usize, x: usize, y: usize) -> [f32; 3] {
    let o = y * bytes_per_row + x * TEXEL_BYTES as usize;
    let ch = |i: usize| f16_to_f32(u16::from_le_bytes([rows[o + 2 * i], rows[o + 2 * i + 1]]));
    [ch(0), ch(1), ch(2)]
}

/// Compare the rgba16float readback of the synthetic frame with the model.
pub(crate) fn evaluate(rows: &[u8], bytes_per_row: usize) -> Result<Verdict, String> {
    if rows.len() < bytes_per_row * FRAME_H as usize {
        return Err(format!("readback too short: {} bytes", rows.len()));
    }
    // Grey ramp: sRGB-decoded output must equal the PQ code (both
    // hypotheses agree on grey — the matrix keeps grey grey).
    let mut sq = 0.0f32;
    let mut n = 0usize;
    let mut grey_max = 0.0f32;
    let mut code = 128u32;
    while code <= 896 {
        let px = texel(rows, bytes_per_row, code as usize, 0);
        let got = srgb_decode(px[1]);
        let err = (got - ramp_pq(code)).abs();
        sq += err * err;
        n += 1;
        grey_max = grey_max.max(err);
        code += 8;
    }
    let grey_rms = (sq / n as f32).sqrt();
    if grey_max > GREY_TOLERANCE {
        return Err(format!(
            "grey ramp does not decode to the PQ codes (rms {grey_rms:.4}, max {grey_max:.4}): the browser tone-maps or linearizes"
        ));
    }
    // Colour patches decide whether the primaries matrix was applied.
    let patch_w = RAMP_W as usize / PATCHES.len();
    let (mut err_matrix, mut err_identity) = (0.0f32, 0.0f32);
    for (i, p) in PATCHES.iter().enumerate() {
        let px = texel(rows, bytes_per_row, i * patch_w + patch_w / 2, 2);
        let lin = [srgb_decode(px[0]), srgb_decode(px[1]), srgb_decode(px[2])];
        let matrix = bt2020_to_bt709(*p).map(|v| v.max(0.0));
        for c in 0..3 {
            err_matrix = err_matrix.max((lin[c] - matrix[c]).abs());
            err_identity = err_identity.max((lin[c] - p[c]).abs());
        }
    }
    let (inverse, patch_max) = if err_matrix <= PATCH_TOLERANCE {
        (Inverse::Matrix, err_matrix)
    } else if err_identity <= PATCH_TOLERANCE {
        (Inverse::Identity, err_identity)
    } else {
        return Err(format!(
            "colour patches match neither model (matrix {err_matrix:.4}, identity {err_identity:.4})"
        ));
    };
    Ok(Verdict { inverse, grey_rms, grey_max, patch_max })
}

/// Run the calibration on the live device: synthetic VideoFrame →
/// `copyExternalImageToTexture` → rgba16float → one 32 KiB readback.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn calibrate(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Verdict, String> {
    use web_sys::{VideoColorPrimaries, VideoMatrixCoefficients, VideoTransferCharacteristics};

    let planes = synthetic_planes();
    let data = js_sys::Uint8Array::from(planes.as_slice());
    let init = web_sys::VideoFrameBufferInit::new_with_f64(
        FRAME_H,
        RAMP_W,
        web_sys::VideoPixelFormat::I420p10,
        0.0,
    );
    let cs = web_sys::VideoColorSpaceInit::new();
    cs.set_primaries(Some(VideoColorPrimaries::Bt2020));
    cs.set_transfer(Some(VideoTransferCharacteristics::Pq));
    cs.set_matrix(Some(VideoMatrixCoefficients::Bt2020Ncl));
    cs.set_full_range(Some(false));
    init.set_color_space(&cs);
    let frame = web_sys::VideoFrame::new_with_u8_array_and_video_frame_buffer_init(&data, &init)
        .map_err(|e| format!("VideoFrame(I420P10) construction failed: {e:?}"))?;

    let extent = wgpu::Extent3d {
        width: RAMP_W,
        height: FRAME_H,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("web HDR calibration"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    queue.copy_external_image_to_texture(
        &wgpu::CopyExternalImageSourceInfo {
            source: wgpu::ExternalImageSource::VideoFrame(Clone::clone(&frame)),
            origin: wgpu::Origin2d::ZERO,
            flip_y: false,
        },
        wgpu::CopyExternalImageDestInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
            color_space: wgpu::PredefinedColorSpace::Srgb,
            premultiplied_alpha: false,
        },
        extent,
    );
    frame.close();

    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("web HDR calibration readback"),
        size: (BYTES_PER_ROW * FRAME_H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(BYTES_PER_ROW),
                rows_per_image: None,
            },
        },
        extent,
    );
    queue.submit([encoder.finish()]);

    let (tx, rx) = tokio::sync::oneshot::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    // The browser resolves mapAsync from its event loop; polling is a no-op
    // there but keeps the call correct on every backend.
    let _ = device.poll(wgpu::PollType::Poll);
    rx.await
        .map_err(|_| "map_async callback dropped".to_string())?
        .map_err(|e| format!("map_async failed: {e:?}"))?;
    let bytes = readback.slice(..).get_mapped_range().to_vec();
    readback.unmap();
    evaluate(&bytes, BYTES_PER_ROW as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_to_f16(v: f32) -> u16 {
        // Enough for the test values (normal range, positive).
        let bits = v.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        let frac = (bits >> 13) & 0x3ff;
        if v == 0.0 {
            return sign;
        }
        sign | ((exp as u16) << 10) | frac as u16
    }

    fn srgb_encode(l: f32) -> f32 {
        if l <= 0.0031308 {
            12.92 * l
        } else {
            1.055 * l.powf(1.0 / 2.4) - 0.055
        }
    }

    /// Build a readback as a browser following `model` would produce it.
    fn simulate(model: Inverse) -> Vec<u8> {
        let bpr = BYTES_PER_ROW as usize;
        let mut rows = vec![0u8; bpr * FRAME_H as usize];
        let patch_w = RAMP_W as usize / PATCHES.len();
        for y in 0..FRAME_H as usize {
            for x in 0..RAMP_W as usize {
                let pq = if y < 2 {
                    let v = ramp_pq(x as u32).clamp(0.0, 1.0);
                    [v, v, v]
                } else {
                    PATCHES[x / patch_w]
                };
                let lin = match model {
                    Inverse::Matrix => bt2020_to_bt709(pq).map(|v| v.max(0.0)),
                    Inverse::Identity => pq,
                };
                let o = y * bpr + x * TEXEL_BYTES as usize;
                for c in 0..3 {
                    let h = f32_to_f16(srgb_encode(lin[c]));
                    rows[o + 2 * c..o + 2 * c + 2].copy_from_slice(&h.to_le_bytes());
                }
            }
        }
        rows
    }

    #[test]
    fn half_float_roundtrip() {
        for v in [0.0f32, 0.25, 0.5, 0.7417, 1.0, 1.248] {
            assert!((f16_to_f32(f32_to_f16(v)) - v).abs() < 1e-3, "{v}");
        }
    }

    #[test]
    fn yuv_codes_roundtrip_through_shader_formula() {
        for p in PATCHES {
            let (y, cb, cr) = pq_rgb_to_yuv10(p);
            // shader_pq_math.wgsl yuv_limited_to_pq_rgb on unorm codes.
            let y_ = (y as f32 / 1023.0 * 255.0 - 16.0) / 219.0;
            let cb_ = (cb as f32 / 1023.0 * 255.0 - 128.0) / 224.0;
            let cr_ = (cr as f32 / 1023.0 * 255.0 - 128.0) / 224.0;
            let rgb = [
                y_ + 1.4746 * cr_,
                y_ - 0.16455 * cb_ - 0.57136 * cr_,
                y_ + 1.8814 * cb_,
            ];
            // 10-bit quantisation (≤ 0.002) plus the shader's normalised
            // 8-bit range form, 0.3 % off the exact 876/896 divisors (see
            // shader_chrome_inverse.wgsl exact_to_filter_form).
            for c in 0..3 {
                assert!((rgb[c] - p[c]).abs() < 0.006, "{p:?} -> {rgb:?}");
            }
        }
    }

    #[test]
    fn synthetic_frame_layout() {
        let planes = synthetic_planes();
        let w = RAMP_W as usize;
        assert_eq!(planes.len(), (w * 4 + 2 * (w / 2) * 2) * 2);
        // Y(0, 300) == 300, chroma row 0 neutral.
        let y = |i: usize| u16::from_le_bytes([planes[2 * i], planes[2 * i + 1]]);
        assert_eq!(y(300), 300);
        let cb0 = w * 4;
        assert_eq!(y(cb0 + 5), 512);
    }

    #[test]
    fn evaluate_recognises_chrome_model() {
        let v = evaluate(&simulate(Inverse::Matrix), BYTES_PER_ROW as usize).unwrap();
        assert_eq!(v.inverse, Inverse::Matrix);
        assert!(v.grey_max < 0.01 && v.patch_max < 0.01, "{v:?}");
    }

    #[test]
    fn evaluate_recognises_identity_model() {
        let v = evaluate(&simulate(Inverse::Identity), BYTES_PER_ROW as usize).unwrap();
        assert_eq!(v.inverse, Inverse::Identity);
    }

    #[test]
    fn evaluate_rejects_a_tone_mapped_ramp() {
        // A browser that linearizes PQ properly: 100 nits → ~0.5, blacks stay black.
        let mut rows = simulate(Inverse::Matrix);
        let bpr = BYTES_PER_ROW as usize;
        for x in 0..RAMP_W as usize {
            let v = f32_to_f16((ramp_pq(x as u32).clamp(0.0, 1.0)).powf(4.0));
            for c in 0..3 {
                rows[x * 8 + 2 * c..x * 8 + 2 * c + 2].copy_from_slice(&v.to_le_bytes());
                rows[bpr + x * 8 + 2 * c..bpr + x * 8 + 2 * c + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        assert!(evaluate(&rows, bpr).is_err());
    }
}
