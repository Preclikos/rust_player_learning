// HDR tonemap — browser input variant: the rgba16float texture Chrome
// filled from the WebCodecs VideoFrame, undone to PQ codes by
// shader_chrome_inverse.wgsl. Composed after shader_pq_math.wgsl +
// shader_hdr_common.wgsl + shader_chrome_inverse.wgsl (player::shader_src).
//
// Same bind-group layout as the P010 variant: binding 1 is bound to the
// same view and unused here.

@group(0) @binding(0) var t_texture_rgba: texture_2d<f32>;
@group(0) @binding(1) var t_texture_unused: texture_2d<f32>;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let c = textureSample(t_texture_rgba, s_sampler, in.tex_coords).rgb;
    return tonemap_pq_rgb(chrome_to_pq(c));
}
