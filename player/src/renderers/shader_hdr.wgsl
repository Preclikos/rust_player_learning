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
    // Sample Y, U, and V components from the P010 textures
    let y = textureSample(t_texture_y, s_sampler, in.tex_coords).r;
    let uv = textureSample(t_texture_uv, s_sampler, in.tex_coords);

    // Convert sampled values to 10-bit integer range (assuming stored in MSBs of 16-bit values)
    let y_10bit = floor(y * 65535.0) / 64.0; // Extract upper 10 bits
    let u_10bit = floor(uv.r * 65535.0) / 64.0;
    let v_10bit = floor(uv.g * 65535.0) / 64.0;

    // Adjust Y, U, V to their video-range scales
    let yyy = (y - 64.0); // Scale Y from [64,940] to [0,255]
    let u = (uv.r - 512.0);
    let v = (uv.g - 512.0);

    // YUV to RGB conversion (standard coefficients for video YUV to RGB)
    let rr = yyy + 1.402 * v;
    let gg = yyy - 0.344136 * u - 0.714136 * v;
    let bb = yyy + 1.772 * u;

    // Return the final color with alpha = 1 for full opacity
    return vec4<f32>(rr / 1023.0, gg / 1023.0, bb / 1023.0, 1.0); // BGRA format
}
