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
    // Sample Y, U, and V components from the textures
    let y = textureSample(t_texture_y, s_sampler, in.tex_coords).r;
    let uv = textureSample(t_texture_uv, s_sampler, in.tex_coords);
    
    let y_full = (y * 255.0 - 54.0);// * (219.0 / 219.0); // Scale Y from [16, 235] -> [0, 219] (not fully 0-255!)
    let u_full = (uv.r * 255.0 - 128.0); // Center U around 0 (was [-112,112])
    let v_full = (uv.g * 255.0 - 128.0); // Center V around 0
    
    // Increase saturation by scaling U and V components
    let saturation_factor = 1.05; // Adjust this to control saturation strength
    let u_sat = u_full * saturation_factor;
    let v_sat = v_full * saturation_factor;

    // YUV to RGB conversion using BT.709 coefficients (standard for video)
    var rr = y_full + 1.403 * v_sat;
    var gg = y_full - 0.344 * u_sat - 0.7169 * v_sat;
    var bb = y_full + 1.779 * u_sat;

    // Return the final color with alpha = 1 for full opacity
    return vec4<f32>(rr / 255.0, gg / 255.0, bb / 255.0, 1.0); // BGRA format
}
