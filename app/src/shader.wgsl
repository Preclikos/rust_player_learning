@group(0) @binding(0) var my_texture: texture_2d<f32>;
@group(0) @binding(1) var my_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0)
    );

    var tex_coords = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0), // Invert Y coordinate
        vec2<f32>(1.0, 1.0), // Invert Y coordinate
        vec2<f32>(0.0, 0.0), // Invert Y coordinate
        vec2<f32>(0.0, 0.0), // Invert Y coordinate
        vec2<f32>(1.0, 1.0), // Invert Y coordinate
        vec2<f32>(1.0, 0.0)  // Invert Y coordinate
    );

    var output: VertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    output.tex_coords = tex_coords[vertex_index];
    return output;
}

@fragment
fn fs_main(@location(0) in_tex_coords: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(my_texture, my_sampler, in_tex_coords);
}