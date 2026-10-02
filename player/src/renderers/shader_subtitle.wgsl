// Subtitle overlay: one premultiplied-alpha RGBA cue bitmap drawn as a
// positioned quad over the video. `fs_main` targets the normal (SDR)
// surface; `fs_main_pq` the rgba16float PQ surface of an HDR output
// session. Composed after shader_pq_encode.wgsl (player::shader_src).

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
};

struct Quad {
    /// xy = NDC center, zw = NDC half-extent.
    transform: vec4<f32>,
};

@group(0) @binding(0) var t_tex: texture_2d<f32>;
@group(0) @binding(1) var s_tex: sampler;
@group(0) @binding(2) var<uniform> quad: Quad;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VertexOut {
    // Unit quad in [-1, 1] × [-1, 1], two triangles.
    var pos = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var uv = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );
    let p = pos[vi];
    var out: VertexOut;
    out.position = vec4<f32>(
        quad.transform.x + p.x * quad.transform.z,
        quad.transform.y + p.y * quad.transform.w,
        0.0, 1.0,
    );
    out.tex_coords = uv[vi];
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(t_tex, s_tex, in.tex_coords);
}

// HDR output session (PQ BT.2020 target): the same premultiplied cue,
// re-encoded so its sRGB-ish white lands on BT.2408 SDR white (203 nits)
// instead of PQ 1.0 = 10 000 nits. sdr_display_rgb_to_pq comes from
// shader_pq_encode.wgsl.
@fragment
fn fs_main_pq(in: VertexOut) -> @location(0) vec4<f32> {
    let c = textureSample(t_tex, s_tex, in.tex_coords);
    if (c.a <= 0.0) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(sdr_display_rgb_to_pq(c.rgb / c.a) * c.a, c.a);
}
