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

    // Convert from 10-bit range [0, 1023] to [0, 255]
    let y_10bit = y * 1023.0;
    let u_10bit = uv.r * 1023.0;
    let v_10bit = uv.g * 1023.0;
    
    // Convert YUV values to full-range floating-point format
    let y_full = (y_10bit - 64.0) * (255.0 / 876.0); // Scale Y from [64, 940] -> [0, 255]
    let u_full = (u_10bit - 512.0) * (255.0 / 448.0); // Center U around 0 (was [-448,448])
    let v_full = (v_10bit - 512.0) * (255.0 / 448.0); // Center V around 0
    
    // Increase saturation by scaling U and V components (adjust the factor if needed)
    let saturation_factor = 1.05; // Try adjusting this if needed
    let u_sat = u_full * saturation_factor;
    let v_sat = v_full * saturation_factor;

    // YUV to RGB conversion using BT.709 coefficients (standard for HD video)
    var rr = y_full + 1.5748 * v_sat;
    var gg = y_full - 0.1873 * u_sat - 0.4681 * v_sat;
    var bb = y_full + 1.8556 * u_sat;

    // Return the final color with alpha = 1 for full opacity
    return vec4<f32>(rr / 255.0, gg / 255.0, bb / 255.0, 1.0);
}
