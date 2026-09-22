// Detection — browser input variant: the rgba16float texture Chrome filled
// from the VideoFrame, undone to PQ codes per pixel (shader_chrome_inverse).
// Composed after shader_pq_math.wgsl + shader_hdr_detect_common.wgsl +
// shader_chrome_inverse.wgsl. Same 2×2 quads as the P010 variant so the
// statistics match the native detection.

@group(0) @binding(0) var t_texture_rgba: texture_2d<f32>;
@group(0) @binding(1) var t_texture_unused: texture_2d<f32>;

fn pixel_sig(px: vec2<u32>) -> f32 {
    let xy = min(px, vec2<u32>(u_tm.frame_w - 1u, u_tm.frame_h - 1u));
    let c = textureLoad(t_texture_rgba, vec2<i32>(xy), 0).rgb;
    return pq_rgb_sig(chrome_to_pq(c));
}

fn block_sig(g: vec2<u32>) -> f32 {
    let x = 2u * g.x;
    let y = 2u * g.y;
    let sig0 = pixel_sig(vec2<u32>(x,      y));
    let sig1 = pixel_sig(vec2<u32>(x + 1u, y));
    let sig2 = pixel_sig(vec2<u32>(x,      y + 1u));
    let sig3 = pixel_sig(vec2<u32>(x + 1u, y + 1u));
    return max(sig0, max(sig1, max(sig2, sig3)));
}
