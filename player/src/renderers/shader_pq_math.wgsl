// PQ / BT.2020 source math shared by the HDR tonemap shader, the frame
// peak/average detection shader and their browser variants. Composed into
// each module by `player::shader_src` — one copy, so the detected signal
// is always computed from exactly the values the tonemap will see.
//
// Every function here is a port of FFmpeg's colorspace_common.cl /
// tonemap.cl (see shader_hdr_common.wgsl for the full pipeline notes).

const REFERENCE_WHITE: f32 = 100.0;
// Average light level for SDR signals (filter's compiled-in sdr_avg).
const SDR_AVG: f32 = 0.25;

// SMPTE ST 2084 (PQ) EOTF — colorspace_common.cl eotf_st2084: non-linear
// signal → linear light where 1.0 = REFERENCE_WHITE (100 nits), so a
// 10 000-nit peak decodes to 100.0. No [0,1] clamp on the input — code
// values above nominal range extrapolate exactly like powr does.
fn eotf_st2084(x: f32) -> f32 {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = pow(max(x, 0.0), 1.0 / m2);
    let num = max(p - c1, 0.0);
    let den = max(c2 - c3 * p, 1e-6);
    let c = pow(num / den, 1.0 / m1);
    return select(0.0, c * 10000.0 / REFERENCE_WHITE, x > 0.0);
}

// BT.2020 → BT.709 primaries conversion (linear light). Same values the
// filter computes from the primaries via XYZ (ff_fill_rgb2xyz_table) and
// bakes into its kernel at %.4f — identical to ITU-R BT.2087.
fn bt2020_to_bt709(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        1.6605 * c.r - 0.5876 * c.g - 0.0728 * c.b,
       -0.1246 * c.r + 1.1329 * c.g - 0.0083 * c.b,
       -0.0182 * c.r - 0.1006 * c.g + 1.1187 * c.b,
    );
}

// Limited (TV) range 10-bit BT.2020-NCL Y'CbCr codes (normalised unorm
// samples) → PQ-encoded R'G'B'. colorspace_common.cl yuv2rgb applies the
// normalised 8-bit form to the unorm sample regardless of bit depth; no
// clamp, super-range codes pass through like the filter. Kr = 0.2627,
// Kb = 0.0593 — the filter's rgb_matrix (inverse of ff_fill_rgb2yuv_table).
fn yuv_limited_to_pq_rgb(y_code: f32, cb_code: f32, cr_code: f32) -> vec3<f32> {
    let y_ = (y_code  * 255.0 -  16.0) / 219.0;
    let cb = (cb_code * 255.0 - 128.0) / 224.0;
    let cr = (cr_code * 255.0 - 128.0) / 224.0;
    return vec3<f32>(
        y_ + 1.4746 * cr,
        y_ - 0.16455 * cb - 0.57136 * cr,
        y_ + 1.8814 * cb,
    );
}

// PQ-encoded BT.2020 R'G'B' → destination-space (BT.709) linear RGB in
// REFERENCE_WHITE units — the filter's map_to_dst_space_from_yuv tail.
// Gamut first, tonemap second: the filter converts to destination
// primaries here and tonemaps there.
fn pq_rgb_to_dst_linear(pq: vec3<f32>) -> vec3<f32> {
    let c = vec3<f32>(eotf_st2084(pq.r), eotf_st2084(pq.g), eotf_st2084(pq.b));
    return bt2020_to_bt709(c);
}

// Max signal component of one pixel after conversion to destination-space
// linear RGB — the tonemap kernel's per-pixel `sig`, what the detection
// pass accumulates.
fn pq_rgb_sig(pq: vec3<f32>) -> f32 {
    let c = pq_rgb_to_dst_linear(pq);
    return max(c.r, max(c.g, c.b));
}
