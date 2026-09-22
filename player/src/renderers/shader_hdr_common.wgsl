// HDR10 (Rec.2020 + PQ) → SDR (Rec.709) display path — the tonemap stage
// shared by every source variant (composed after shader_pq_math.wgsl and
// before an input file that declares bindings 0/1 and `fs_main`).
//
// Faithful WGSL port of FFmpeg's tonemap_opencl filter
// (libavfilter/opencl/tonemap.cl + colorspace_common.cl), configured like
// the reference SDR transcode of this project's ladder:
//
//   tonemap_opencl=tonemap=mobius:param=0.01:desat=0:r=tv:p=bt709:t=bt709:m=bt709
//
// Pipeline (matching the filter's map_to_dst_space_from_yuv + map_one_pixel_rgb):
//   source → R'G'B' (PQ-encoded)           — the input variant's job
//   → eotf_st2084 to linear (1.0 = REFERENCE_WHITE = 100 nits)
//   → BT.2020 → BT.709 primaries (linear)
//   → mobius tonemap of the max component, scaled by the frame
//     peak/average detection result (computed by the detection shader
//     into the group-1 storage buffer, exactly like the filter's
//     detect_peak_avg)
//   → inverse_eotf_bt1886 (pure 1/2.4 power — the filter's bt709
//     "delinearize") → display.
//
// The filter then packs the result into BT.709 TV-range NV12; the player's
// SDR shader decodes that straight back to the same R'G'B', so emitting the
// delinearized R'G'B' here directly makes an HDR representation land on the
// SAME displayed values as playing its offline-transcoded SDR sibling
// (modulo the transcode's 4:2:0 chroma subsampling).
//
// Input variants:
//   shader_hdr_p010.wgsl — native: P010 plane views (Y = R16Unorm,
//                          UV = Rg16Unorm) → yuv_limited_to_pq_rgb.
//   shader_hdr_web.wgsl  — browser: the RGBA texture Chrome filled from the
//                          WebCodecs VideoFrame, undone back to the PQ codes
//                          by shader_chrome_inverse.wgsl. Same math from
//                          there on — the browser shows the PC picture.
//
// HLG transfer is not handled — eotf_st2084 is hardcoded. Most HDR DASH
// content is PQ; HLG support would need the filter's inverse_oetf_hlg +
// ootf_hlg variants switched by a uniform.
//
// The tunables (group 0, binding 3) mirror the filter's options 1:1 —
// tone_param / desat / peak / scene_threshold; pushed from the host via
// Player::set_hdr_tonemap. See player/HDR_TONEMAP.md.

struct TonemapUniforms {
    // Mobius knee `j` (filter `param`). Reference transcode: 0.01.
    tone_param: f32,
    // Desaturation strength (filter `desat`). Reference transcode: 0 (off).
    desat: f32,
    // Source signal peak in REFERENCE_WHITE units, pre-resolved on the CPU
    // (params.peak, or 100.0 for an untagged PQ source — the
    // ff_determine_signal_peak fallback). First-frame seed only; the frame
    // detection takes over from the second frame.
    peak: f32,
    // Scene-change reset threshold (filter `threshold`). Used by the
    // detection pass, carried here so both stages share one uniform.
    scene_threshold: f32,
    // Workgroup count of the detection accumulate pass (detection only).
    num_wg: u32,
    // Content dimensions in pixels (detection only — the imported decoder
    // texture may be padded past the visible frame).
    frame_w: u32,
    frame_h: u32,
    // Browser variant only: which inverse of the browser's conversion to
    // apply (see shader_chrome_inverse.wgsl). 0 on native.
    web_inverse: u32,
}

@group(0) @binding(2) var s_sampler: sampler;
@group(0) @binding(3) var<uniform> u_tm: TonemapUniforms;

// Result slots of the frame peak/average detection (see
// shader_hdr_detect_common.wgsl for the buffer's full layout — the ring
// buffers and totals at the front are only touched by the compute passes).
struct DetectionResult {
    _ring_and_totals: array<u32, 132>,
    out_peak: f32,
    out_average: f32,
}
@group(1) @binding(0) var<storage, read> r_detect: DetectionResult;

// luma_dst — BT.709 luma coefficients (output colorspace), used by the
// desat path's get_luma_dst.
const LUMA_DST: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);

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

// tonemap.cl mobius(): linear below the knee j, Möbius compression above,
// normalised so peak → 1.0.
fn mobius(s: f32, peak: f32) -> f32 {
    let j = u_tm.tone_param;
    if (s <= j) {
        return s;
    }
    let a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
    let b = (j * j - 2.0 * j * peak + peak) / max(peak - 1.0, 1e-6);
    return (b * b + 2.0 * b * j + j * j) / (b - a) * (s + a) / (s + b);
}

// tonemap.cl map_one_pixel_rgb() with target_peak = 1.0 (SDR output, so
// the rescale branch is compiled out): tonemap the max component, scale
// the pixel by the ratio. `peak`/`average` come from the frame detection.
fn map_one_pixel_rgb(rgb_in: vec3<f32>, peak_in: f32, average: f32) -> vec3<f32> {
    var rgb = rgb_in;
    var sig = max(max(rgb.r, max(rgb.g, rgb.b)), 1e-6);
    let sig_old = sig;

    // Scale the signal to compensate for differences in the average
    // brightness (the slope is 1.0 — inactive — until a scene averages
    // brighter than SDR_AVG, i.e. 25 nits).
    let slope = min(1.0, SDR_AVG / average);
    sig = sig * slope;
    let peak = peak_in * slope;

    // Desaturate toward luma with a signal-dependent coefficient. Skipped
    // entirely at desat == 0 (the reference transcode), like the filter's
    // compile-time `desat_param > 0` check.
    if (u_tm.desat > 0.0) {
        let luma = dot(LUMA_DST, rgb);
        var coeff = max(sig - 0.18, 1e-6) / max(sig, 1e-6);
        coeff = pow(coeff, 10.0 / u_tm.desat);
        rgb = mix(rgb, vec3<f32>(luma), vec3<f32>(coeff));
        sig = mix(sig, luma * slope, coeff);
    }

    sig = min(mobius(sig, peak), 1.0);
    return rgb * (sig / sig_old);
}

// colorspace_common.cl inverse_eotf_bt1886 — the filter's "delinearize"
// for a bt709-transfer target: pure 1/2.4 power, NOT the BT.709 OETF
// (and not sRGB). Out-of-gamut negatives from the primaries matrix
// clamp to 0 exactly like the filter's `c < 0.0f ? 0.0f : powr(...)`.
fn inverse_eotf_bt1886(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4));
}

// PQ-encoded BT.2020 R'G'B' → displayed R'G'B': the whole tonemap stage.
// Every input variant ends its fs_main here.
fn tonemap_pq_rgb(pq: vec3<f32>) -> vec4<f32> {
    // linearize (1.0 = 100 nits; ootf is identity for a PQ source) and
    // convert to destination primaries.
    var c = pq_rgb_to_dst_linear(pq);

    c = map_one_pixel_rgb(c, r_detect.out_peak, r_detect.out_average);

    // inverse_ootf is identity for a bt709 target; delinearize and emit
    // R'G'B' — exactly what the SDR path shows after decoding the
    // filter's NV12 output.
    return vec4<f32>(inverse_eotf_bt1886(c), 1.0);
}
