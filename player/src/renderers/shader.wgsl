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
 

fn nv12_to_rgba(y: f32, u: f32, v: f32) -> vec4f {
    let r = y + 1.402 * (v - 0.5);
    let g = y - 0.344136 * (u - 0.5) - 0.714136 * (v - 0.5);
    let b = y + 1.772 * (u - 0.5);
    return vec4f(r, g, b, 1.0);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Sample Y, U, and V components from the textures
    let y = textureSample(t_texture_y, s_sampler, in.tex_coords).r;
    let uv = textureSample(t_texture_uv, s_sampler, in.tex_coords);
    
    // Adjust Y, U, V ranges
    let yyy = y * 255.0 - 32.0; // Y is from 16 to 235 (scaled to 0 to 219)
    let u = uv.r * 255.0 - 128.0; // U is from 16 to 240 (scaled to -112 to 112)
    let v = uv.g * 255.0 - 128.0; // V is from 16 to 240 (scaled to -112 to 112)

    // YUV to RGB conversion (standard coefficients for video YUV to RGB)
    let rr = yyy + 1.402 * v;
    let gg = yyy - 0.344136 * u - 0.714136 * v;
    let bb = yyy + 1.772 * u;

    // Return the final color with alpha = 1 for full opacity
    return vec4<f32>(rr / 255.0, gg / 255.0, bb / 255.0, 1.0); // BGRA format
}
