// Browser: undo the browser's conversion of a PQ VideoFrame so the engine's
// own PQ → SDR tonemap can run on the original code values — zero-copy,
// the frame never leaves the GPU.
//
// `copyExternalImageToTexture(VideoFrame)` hands us R'G'B'A the browser
// converted for an sRGB destination. Measured in Chrome (153, WebGPU on
// D3D, SDR display) with a synthetic 10-bit PQ grey ramp and colour
// patches, then confirmed pixel-for-pixel on real HDR10 content: Chrome
// does NOT linearize PQ at all. It expands the limited range, applies the
// BT.2020 → BT.709 primaries matrix to the still PQ-ENCODED values as if
// they were linear light, clips negatives, and sRGB-encodes the result
// (rms 0.0017 against the decoded stream, within its 8-bit rounding).
// That is why HDR looked washed out and lifted: 1 nit landed at 0.42 and
// 100 nits at 0.74 of the SDR range.
//
// Both steps are invertible: sRGB-decode gives the matrix output back
// (an rgba16float destination keeps the >1.0 values of saturated
// colours), the inverse matrix gives the PQ-encoded BT.2020 R'G'B' — the
// exact input the native shader derives from the P010 planes. The
// renderer verifies this model at start-up with the same synthetic frame
// (see VideoRenderer::calibrate_web_hdr) and falls back to the browser's
// picture when a browser behaves differently; `u_tm.web_inverse` selects
// the verified variant.

fn srgb_decode(v: f32) -> f32 {
    let x = max(v, 0.0);
    return select(pow((x + 0.055) / 1.055, 2.4), x / 12.92, x <= 0.04045);
}

// Inverse of bt2020_to_bt709's matrix (BT.709 → BT.2020, 6 decimals so the
// round trip is exact to float precision).
fn bt709_to_bt2020(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        0.627409 * c.r + 0.329260 * c.g + 0.043272 * c.b,
        0.069125 * c.r + 0.919549 * c.g + 0.011321 * c.b,
        0.016423 * c.r + 0.088048 * c.g + 0.895617 * c.b,
    );
}

// The browser expands the limited range exactly ((code − 64) / 876,
// (code − 512) / 896). The native shader — like the FFmpeg filter the SDR
// ladder was transcoded with — applies the normalised 8-bit form
// ((code/1023 · 255 − 16) / 219, … − 128) / 224) to the 10-bit codes
// (shader_pq_math.wgsl yuv_limited_to_pq_rgb), a 0.3 % scale and a small
// per-channel offset apart. Both are linear in the codes, so one affine
// map turns the exact R'G'B' into the filter-form R'G'B' the PC path
// computes: the web then reaches the same numbers, not just the same look.
fn exact_to_filter_form(pq: vec3<f32>) -> vec3<f32> {
    return 0.99706745 * pq + vec3<f32>(-0.00268530, 0.00101895, -0.00336699);
}

// Browser-converted R'G'B' → PQ-encoded BT.2020 R'G'B' (filter form).
//   web_inverse 0: sRGB decode, then the inverse primaries matrix (Chrome).
//   web_inverse 1: sRGB decode only (a browser that skips the matrix).
fn chrome_to_pq(c: vec3<f32>) -> vec3<f32> {
    let m = vec3<f32>(srgb_decode(c.r), srgb_decode(c.g), srgb_decode(c.b));
    if (u_tm.web_inverse == 1u) {
        return exact_to_filter_form(m);
    }
    return exact_to_filter_form(bt709_to_bt2020(m));
}
