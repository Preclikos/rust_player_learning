// HDR10 (Rec.2020 + PQ) → SDR (Rec.709) display path.
//
// Pipeline: limited-range 10-bit YCbCr → BT.2020 NCL R'G'B' (still PQ-encoded)
// → PQ EOTF to linear nits → ACES filmic tonemap → BT.2020→BT.709 matrix
// → sRGB OETF → 8-bit display.
//
// HLG transfer is not handled — pq_eotf is hardcoded. Most HDR DASH content
// is PQ; HLG support would need a uniform to switch transfer at draw time.

@group(0) @binding(0) var t_texture_y: texture_2d<f32>;
@group(0) @binding(1) var t_texture_uv: texture_2d<f32>;
@group(0) @binding(2) var s_sampler: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) tex_coords: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
}

@vertex
fn vs_main(model: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.tex_coords = model.tex_coords;
    out.clip_position = vec4<f32>(model.position, 1.0);
    return out;
}

// SMPTE ST 2084 (PQ) EOTF: non-linear signal in [0,1] → linear cd/m² up to 10000.
fn pq_eotf(v: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let vp = pow(max(v, vec3<f32>(0.0)), vec3<f32>(1.0 / m2));
    let num = max(vp - c1, vec3<f32>(0.0));
    let den = c2 - c3 * vp;
    return 10000.0 * pow(num / den, vec3<f32>(1.0 / m1));
}

// ACES filmic tonemap (Narkowicz fit). Input scaled so 1.0 ≈ SDR diffuse white.
fn aces_tonemap(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}

// BT.2020 → BT.709 primaries conversion (linear-light RGB, ITU-R BT.2087).
fn bt2020_to_bt709(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        1.6605 * c.r - 0.5876 * c.g - 0.0728 * c.b,
       -0.1246 * c.r + 1.1329 * c.g - 0.0083 * c.b,
       -0.0182 * c.r - 0.1006 * c.g + 1.1187 * c.b,
    );
}

// sRGB OETF. Surface formats here are non-sRGB Unorm, so we encode gamma ourselves.
fn srgb_oetf(c: vec3<f32>) -> vec3<f32> {
    let lin = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = lin * 12.92;
    let hi = 1.055 * pow(lin, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, lin <= vec3<f32>(0.0031308));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // P010 plane views: Y = R16Unorm, UV = Rg16Unorm. P010 stores the 10-bit
    // code in the high 10 bits of a 16-bit container; sampling as Unorm gives
    // a float in [0, 65472/65535] ≈ [0, 0.9998] which we treat as the normalised
    // 10-bit code (1/1023 step). The 0.1% scale error vs an exact (code/1023)
    // recovery is negligible against the limited-range expansion below.
    let y_code = textureSample(t_texture_y, s_sampler, in.tex_coords).r;
    let uv     = textureSample(t_texture_uv, s_sampler, in.tex_coords).rg;

    // Limited (TV) range expand for 10-bit:
    //   Y'  code 64..940  → Y'  in [0, 1]
    //   Cb' code 64..960  → Cb' in [-0.5, 0.5] (midpoint 512, full span 896)
    let y_pq = clamp((y_code - 64.0 / 1023.0) / (876.0 / 1023.0), 0.0, 1.0);
    let cb   = (uv.r - 512.0 / 1023.0) / (896.0 / 1023.0);
    let cr   = (uv.g - 512.0 / 1023.0) / (896.0 / 1023.0);

    // BT.2020 NCL Y'CbCr → R'G'B' (Kr = 0.2627, Kb = 0.0593, Kg = 0.6780).
    // Result is still PQ-encoded, not linear.
    let r_pq = y_pq + 1.4746 * cr;
    let g_pq = y_pq - 0.16455 * cb - 0.57135 * cr;
    let b_pq = y_pq + 1.8814 * cb;

    // Decode PQ to linear cd/m².
    let nits = pq_eotf(vec3<f32>(r_pq, g_pq, b_pq));

    // Scale 100 nits → 1.0 (SDR diffuse-white reference) so ACES sees the
    // input domain it was fit for. HDR highlights above 100 nits get
    // compressed by the tonemap's shoulder.
    let lin_bt2020 = nits / 100.0;

    // Tonemap in BT.2020 linear, then map primaries to BT.709 for the SDR
    // display. (Doing it in this order keeps highlight roll-off smooth; doing
    // BT.2020→BT.709 first can produce out-of-gamut negatives that get
    // clipped before the tonemap can resolve them.)
    let tm = aces_tonemap(lin_bt2020);
    let lin_bt709 = bt2020_to_bt709(tm);

    return vec4<f32>(srgb_oetf(lin_bt709), 1.0);
}
