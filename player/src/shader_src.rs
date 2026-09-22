//! WGSL sources of the HDR shaders, composed from shared parts.
//!
//! The tonemap math (`shader_pq_math.wgsl`, `shader_hdr_common.wgsl`,
//! `shader_hdr_detect_common.wgsl`) exists once; each source variant adds
//! only its bindings and the code that turns its input into PQ-encoded
//! BT.2020 R'G'B'. WGSL resolves module-scope declarations in any order,
//! so concatenation is all the composition needs.
//!
//! Public (doc-hidden) so `tests/shader_validation.rs` validates exactly
//! the strings the renderer compiles.

const PQ_MATH: &str = include_str!("renderers/shader_pq_math.wgsl");
const HDR_COMMON: &str = include_str!("renderers/shader_hdr_common.wgsl");
const HDR_P010: &str = include_str!("renderers/shader_hdr_p010.wgsl");
const HDR_WEB: &str = include_str!("renderers/shader_hdr_web.wgsl");
const CHROME_INVERSE: &str = include_str!("renderers/shader_chrome_inverse.wgsl");
const DETECT_COMMON: &str = include_str!("renderers/shader_hdr_detect_common.wgsl");
const DETECT_P010: &str = include_str!("renderers/shader_hdr_detect_p010.wgsl");
const DETECT_WEB: &str = include_str!("renderers/shader_hdr_detect_web.wgsl");

fn compose(parts: &[&str]) -> String {
    parts.join("\n")
}

/// Native HDR tonemap render shader (P010 planes).
pub fn hdr() -> String {
    compose(&[PQ_MATH, HDR_COMMON, HDR_P010])
}

/// Native detection compute shader (P010 planes).
pub fn hdr_detect() -> String {
    compose(&[PQ_MATH, DETECT_COMMON, DETECT_P010])
}

/// Browser HDR tonemap render shader (browser-converted RGBA, inverted).
pub fn web_hdr() -> String {
    compose(&[PQ_MATH, HDR_COMMON, CHROME_INVERSE, HDR_WEB])
}

/// Browser detection compute shader (browser-converted RGBA, inverted).
pub fn web_hdr_detect() -> String {
    compose(&[PQ_MATH, DETECT_COMMON, CHROME_INVERSE, DETECT_WEB])
}
