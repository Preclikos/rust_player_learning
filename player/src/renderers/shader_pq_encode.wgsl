// Encoding display-referred colour INTO a PQ / BT.2020 signal — the
// inverse direction of shader_pq_math.wgsl. Shared by the HDR display
// output shader (shader_hdr_output.wgsl: HLG and SDR frames re-encoded
// for an HDR surface) and the subtitle overlay's PQ variant
// (shader_subtitle.wgsl). Composed into each module by player::shader_src.

// BT.2408 reference white for SDR content placed in a PQ signal.
const SDR_WHITE_NITS: f32 = 203.0;

// SMPTE ST 2084 inverse EOTF, input = fraction of 10 000 nits.
fn oetf_st2084(x: f32) -> f32 {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = pow(clamp(x, 0.0, 1.0), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

fn nits_to_pq(nits: vec3<f32>) -> vec3<f32> {
    let n = nits / 10000.0;
    return vec3<f32>(oetf_st2084(n.r), oetf_st2084(n.g), oetf_st2084(n.b));
}

// BT.709 → BT.2020 primaries (linear light), ITU-R BT.2087.
fn bt709_to_bt2020(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        0.6274 * c.r + 0.3293 * c.g + 0.0433 * c.b,
        0.0691 * c.r + 0.9195 * c.g + 0.0114 * c.b,
        0.0164 * c.r + 0.0880 * c.g + 0.8956 * c.b,
    );
}

// BT.709 display-gamma R'G'B' (what the SDR shader and the subtitle
// rasterizer emit) → PQ BT.2020 with white at SDR_WHITE_NITS. BT.1886
// linearisation (pure 2.4 power), the inverse of the tonemap's output.
fn sdr_display_rgb_to_pq(rgb: vec3<f32>) -> vec3<f32> {
    let lin = pow(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(2.4));
    return nits_to_pq(bt709_to_bt2020(lin) * SDR_WHITE_NITS);
}
