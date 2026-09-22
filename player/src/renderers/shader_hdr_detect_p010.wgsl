// Detection — native input variant: P010 plane views (textureLoad only).
// Composed after shader_pq_math.wgsl + shader_hdr_detect_common.wgsl.

@group(0) @binding(0) var t_texture_y: texture_2d<f32>;
@group(0) @binding(1) var t_texture_uv: texture_2d<f32>;

// Loads are clamped to the visible frame like the filter's CLAMP_TO_EDGE
// sampler (its aligned global work size over-reads edges the same way).
fn pixel_sig(px: vec2<u32>, uv_texel: vec2<f32>) -> f32 {
    let xy = min(px, vec2<u32>(u_tm.frame_w - 1u, u_tm.frame_h - 1u));
    let y_code = textureLoad(t_texture_y, vec2<i32>(xy), 0).r;
    return pq_rgb_sig(yuv_limited_to_pq_rgb(y_code, uv_texel.x, uv_texel.y));
}

// Max signal of the 2×2 Y quad that shares UV texel `g`.
fn block_sig(g: vec2<u32>) -> f32 {
    let uv_w = (u_tm.frame_w + 1u) / 2u;
    let uv_h = (u_tm.frame_h + 1u) / 2u;
    let uv_xy = min(g, vec2<u32>(uv_w - 1u, uv_h - 1u));
    let uv_texel = textureLoad(t_texture_uv, vec2<i32>(uv_xy), 0).rg;

    let x = 2u * g.x;
    let y = 2u * g.y;
    let sig0 = pixel_sig(vec2<u32>(x,      y),      uv_texel);
    let sig1 = pixel_sig(vec2<u32>(x + 1u, y),      uv_texel);
    let sig2 = pixel_sig(vec2<u32>(x,      y + 1u), uv_texel);
    let sig3 = pixel_sig(vec2<u32>(x + 1u, y + 1u), uv_texel);
    return max(sig0, max(sig1, max(sig2, sig3)));
}
