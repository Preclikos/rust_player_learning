// Browser GPU path: the frame arrives as an already-converted R'G'B'A texture.
//
// WebCodecs decodes on the GPU and `copyExternalImageToTexture(VideoFrame)`
// lets the browser do the Y'CbCr → R'G'B' conversion on the GPU too, into an
// RGBA8 texture — no CPU touches the pixels. What is left for us is a
// textured quad. Values are display-referred sRGB-encoded, written to the
// (non-sRGB) swapchain format as-is, exactly like shader.wgsl's output.
//
// Same bind-group layout as shader.wgsl so the renderer keeps ONE layout:
// binding 0 is the RGBA texture, binding 1 is bound to the same view and
// unused here, 2 the sampler, 3 the (unused) tonemap uniform.

@group(0) @binding(0) var t_texture_rgba: texture_2d<f32>;
@group(0) @binding(1) var t_texture_unused: texture_2d<f32>;
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
    let rgb = textureSample(t_texture_rgba, s_sampler, in.tex_coords).rgb;
    return vec4<f32>(rgb, 1.0);
}
