//! Parse + validate every WGSL shader with the same naga revision the
//! wgpu fork uses at runtime. Shader errors otherwise only surface as a
//! `create_shader_module` panic on the first rendered frame — long after
//! `cargo check` went green.

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
    validate("shader_hdr.wgsl", include_str!("../src/renderers/shader_hdr.wgsl"));
}

#[test]
fn hdr_detect_shader_validates() {
    validate(
        "shader_hdr_detect.wgsl",
        include_str!("../src/renderers/shader_hdr_detect.wgsl"),
    );
}
