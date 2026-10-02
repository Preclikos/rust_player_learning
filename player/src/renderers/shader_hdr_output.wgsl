// HDR display output — no tonemap. Composed after shader_pq_math.wgsl and
// shader_pq_encode.wgsl (see player::shader_src::hdr_output).
//
// Used while the surface is in an HDR output session (Apple: CAMetalLayer
// in rgba16float + kCGColorSpaceITUR_2100_PQ + EDR). The render target
// takes PQ-encoded BT.2020 R'G'B' and the OS maps it onto whatever
// headroom the display has, so every entry point here ends in PQ:
//
//   fs_pq  — HDR10 / DV-base-layer P010 planes: the limited-range
//            BT.2020-NCL Y'CbCr → R'G'B' matrix only. The PQ signal is
//            handed through untouched.
//   fs_hlg — HLG planes: BT.2100 HLG inverse OETF + OOTF for a
//            1000-nit reference display, re-encoded as PQ.
//   fs_sdr — SDR (BT.709) NV12 planes mid-session (ABR dropped to an
//            SDR rung): BT.2408 up-convert (shader_pq_encode.wgsl), the
//            same math as the Android FS_SDR_TO_PQ program.
//
// Bindings mirror shader.wgsl / shader_hdr_p010.wgsl (group 0, the shared
// plane layout); binding 3 (the tonemap uniform) is unused here.

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

// BT.2100 HLG reference display peak (the OOTF's Lw).
const HLG_PEAK_NITS: f32 = 1000.0;

// BT.2100 HLG inverse OETF: signal → normalised scene light [0, 1].
fn hlg_inverse_oetf(e: f32) -> f32 {
    let a = 0.17883277;
    let b = 0.28466892;
    let c = 0.55991073;
    let x = clamp(e, 0.0, 1.0);
    if (x <= 0.5) {
        return x * x / 3.0;
    }
    return (exp((x - c) / a) + b) / 12.0;
}

fn sample_planes(uv_in: vec2<f32>) -> vec3<f32> {
    let y_code = textureSample(t_texture_y, s_sampler, uv_in).r;
    let uv     = textureSample(t_texture_uv, s_sampler, uv_in).rg;
    return vec3<f32>(y_code, uv.r, uv.g);
}

@fragment
fn fs_pq(in: VertexOutput) -> @location(0) vec4<f32> {
    let s = sample_planes(in.tex_coords);
    let pq = yuv_limited_to_pq_rgb(s.x, s.y, s.z);
    return vec4<f32>(clamp(pq, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

@fragment
fn fs_hlg(in: VertexOutput) -> @location(0) vec4<f32> {
    let s = sample_planes(in.tex_coords);
    // HLG uses the same BT.2020-NCL matrix as PQ.
    let e = clamp(yuv_limited_to_pq_rgb(s.x, s.y, s.z), vec3<f32>(0.0), vec3<f32>(1.0));
    let scene = vec3<f32>(hlg_inverse_oetf(e.r), hlg_inverse_oetf(e.g), hlg_inverse_oetf(e.b));
    // OOTF: Fd = Lw · Ys^(γ−1) · E, γ = 1.2 at Lw = 1000 nits.
    let ys = dot(vec3<f32>(0.2627, 0.6780, 0.0593), scene);
    let nits = HLG_PEAK_NITS * pow(max(ys, 1e-6), 0.2) * scene;
    return vec4<f32>(nits_to_pq(nits), 1.0);
}

@fragment
fn fs_sdr(in: VertexOutput) -> @location(0) vec4<f32> {
    let s = sample_planes(in.tex_coords);
    // Limited-range BT.709 decode — identical to shader.wgsl.
    let y_ = (s.x * 255.0 -  16.0) / 219.0;
    let cb = (s.y * 255.0 - 128.0) / 224.0;
    let cr = (s.z * 255.0 - 128.0) / 224.0;
    let rgb = vec3<f32>(
        y_ + 1.5748 * cr,
        y_ - 0.18733 * cb - 0.46813 * cr,
        y_ + 1.8556 * cb,
    );
    return vec4<f32>(sdr_display_rgb_to_pq(rgb), 1.0);
}
