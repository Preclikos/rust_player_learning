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
 let y = textureSample(t_texture_y, s_sampler, in.tex_coords).r;
    let uv = textureSample(t_texture_uv, s_sampler, in.tex_coords);
    let u = uv.r - 0.5;
    let v = uv.g - 0.5;

    let r = y + 1.13983 * v;
    let g = y - 0.39465 * u - 0.58060 * v;
    let b = y + 2.03211 * u;

        // Debug output
    if (y == 0.0 && u == 0.0 && v == 0.0) {
        return vec4<f32>(1.0, 0.0, 0.0, 1.0); // Red for debugging
    } else if (y == 1.0 && u == 1.0 && v == 1.0) {
        return vec4<f32>(0.0, 1.0, 0.0, 1.0); // Green for debugging
    }

    return vec4<f32>(r, g, b, 0); // BGRA format
}
