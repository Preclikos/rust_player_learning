//! Parse + validate every WGSL shader with the same naga revision the
//! wgpu fork uses at runtime. Shader errors otherwise only surface as a
//! `create_shader_module` panic on the first rendered frame — long after
//! `cargo check` went green.
//!
//! The HDR shaders are composed from shared parts (`player::shader_src`);
//! the exact strings the renderer compiles are validated, not the parts.

fn validate(name: &str, src: &str) {
    let module = naga::front::wgsl::parse_str(src)
        .unwrap_or_else(|e| panic!("{name}: WGSL parse error:\n{}", e.emit_to_string(src)));
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap_or_else(|e| panic!("{name}: WGSL validation error:\n{e:?}"));
}

#[test]
fn sdr_shader_validates() {
    validate("shader.wgsl", include_str!("../src/renderers/shader.wgsl"));
}

#[test]
fn hdr_shader_validates() {
    validate("shader_hdr (P010)", &player::shader_src::hdr());
}

#[test]
fn hdr_detect_shader_validates() {
    validate("shader_hdr_detect (P010)", &player::shader_src::hdr_detect());
}

// Browser GPU-path quad over the browser-converted `VideoFrame` copy.
#[test]
fn web_rgba_shader_validates() {
    validate("shader_rgba.wgsl", include_str!("../src/renderers/shader_rgba.wgsl"));
}

// Browser HDR: the browser's conversion undone, then the same tonemap.
#[test]
fn web_hdr_shader_validates() {
    validate("shader_hdr (web)", &player::shader_src::web_hdr());
}

#[test]
fn web_hdr_detect_shader_validates() {
    validate("shader_hdr_detect (web)", &player::shader_src::web_hdr_detect());
}

// The browser inverse must undo the renderer's primaries matrix exactly:
// both matrices live in WGSL, so check them numerically here.
#[test]
fn web_inverse_matrix_is_the_inverse() {
    let m = [
        [1.6605f64, -0.5876, -0.0728],
        [-0.1246, 1.1329, -0.0083],
        [-0.0182, -0.1006, 1.1187],
    ];
    let inv = [
        [0.627409f64, 0.329260, 0.043272],
        [0.069125, 0.919549, 0.011321],
        [0.016423, 0.088048, 0.895617],
    ];
    for i in 0..3 {
        for j in 0..3 {
            let mut acc = 0.0;
            for k in 0..3 {
                acc += inv[i][k] * m[k][j];
            }
            let expect = if i == j { 1.0 } else { 0.0 };
            assert!((acc - expect).abs() < 2e-5, "({i},{j}) = {acc}");
        }
    }
    let src = player::shader_src::web_hdr();
    for row in inv {
        for v in row {
            assert!(src.contains(&format!("{v:.6}")), "matrix entry {v} not in shader_chrome_inverse.wgsl");
        }
    }
}
