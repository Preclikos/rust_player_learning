// SDR (BT.709) NV12 display path.
//
// Limited-range 8-bit BT.709 Y'CbCr → R'G'B', emitted to the (non-sRGB)
// swapchain as-is — video R'G'B' values are already display-referred, so
// no transfer conversion happens here. This is the exact inverse of the
// rgb2yuv leg of FFmpeg's tonemap_opencl (yuv_matrix = BT.709,
// r=tv), which produced this project's SDR ladder — keeping it inverse-
// exact makes an SDR representation land on the same displayed values as
// the player's own HDR tonemap of the HDR sibling (see shader_hdr.wgsl).
//
// The previous version of this shader expanded Y to [0, 219/255] (white
// rendered ~14 % dark), dropped the /224 chroma normalisation and used
// BT.601-ish coefficients (1.403/1.779) — SDR content played visibly
// darker and duller than the same frame's HDR rendering.

@group(0) @binding(0) var t_texture_y: texture_2d<f32>;
@group(0) @binding(1) var t_texture_uv: texture_2d<f32>;
@group(0) @binding(2) var s_sampler: sampler;

// Vertex shader

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) tex_coords: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
}

@vertex
fn vs_main(
    model: VertexInput,
) -> VertexOutput {
    var out: VertexOutput;
    out.tex_coords = model.tex_coords;
    out.clip_position = vec4<f32>(model.position, 1.0);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let y_code = textureSample(t_texture_y, s_sampler, in.tex_coords).r;
    let uv     = textureSample(t_texture_uv, s_sampler, in.tex_coords).rg;

    // Limited (TV) range expansion: Y' 16..235 → [0, 1], Cb/Cr 16..240
    // → [-0.5, 0.5] (the same normalised form colorspace_common.cl uses).
    let y_ = (y_code * 255.0 -  16.0) / 219.0;
    let cb = (uv.r   * 255.0 - 128.0) / 224.0;
    let cr = (uv.g   * 255.0 - 128.0) / 224.0;

    // BT.709 Y'CbCr → R'G'B' (Kr = 0.2126, Kb = 0.0722).
    let r = y_ + 1.5748 * cr;
    let g = y_ - 0.18733 * cb - 0.46813 * cr;
    let b = y_ + 1.8556 * cb;

    return vec4<f32>(r, g, b, 1.0);
}
