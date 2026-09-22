// HDR tonemap — native input variant: P010 plane views. Composed after
// shader_pq_math.wgsl + shader_hdr_common.wgsl (see player::shader_src).

@group(0) @binding(0) var t_texture_y: texture_2d<f32>;
@group(0) @binding(1) var t_texture_uv: texture_2d<f32>;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // P010 plane views: Y = R16Unorm, UV = Rg16Unorm. P010 stores the
    // 10-bit code in the high bits of the 16-bit container, so the
    // sampled unorm value equals code/65535 with the low 6 bits zero —
    // the same normalisation OpenCL's CL_UNORM_INT16 read_imagef gives
    // the filter. Chroma is sampled bilinearly (the filter uses nearest
    // within its 2×2 quad) — a spatial-only difference, identical in
    // tone/colour.
    let y_code = textureSample(t_texture_y, s_sampler, in.tex_coords).r;
    let uv     = textureSample(t_texture_uv, s_sampler, in.tex_coords).rg;
    return tonemap_pq_rgb(yuv_limited_to_pq_rgb(y_code, uv.r, uv.g));
}
