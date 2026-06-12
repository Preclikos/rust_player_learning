//! Zero-copy video rendering via EGL_ANDROID_image_native_buffer + GL_TEXTURE_EXTERNAL_OES.
//!
//! Flow:
//!   AHardwareBuffer
//!     → eglGetNativeClientBufferANDROID()  → EGLClientBuffer
//!     → eglCreateImageKHR(EGL_NATIVE_BUFFER_ANDROID) → EGLImageKHR
//!     → glEGLImageTargetTexture2DOES(GL_TEXTURE_EXTERNAL_OES, …)
//!     → OES sampler (hardware YCbCr→RGB) → wgpu swapchain renderbuffer
//!
//! The OES sampler converts YCbCr→RGB automatically using the AHB's HAL format
//! metadata; no shader-side colour math is needed.
//!
//! References:
//!   EGL_KHR_image_base + EGL_ANDROID_image_native_buffer
//!   EGL_ANDROID_get_native_client_buffer (API 26+)

#![cfg(target_os = "android")]

use glow::HasContext;
use std::ffi::c_void;

// EGL extension constants
const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
// GL_OES_EGL_image_external
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

// eglGetNativeClientBufferANDROID: AHardwareBuffer* → EGLClientBuffer (= *mut c_void)
type FnEglGetNativeClientBufferANDROID =
    unsafe extern "system" fn(buffer: *const c_void) -> *mut c_void;

// eglCreateImageKHR(dpy, ctx, target, buffer, attrib_list) → EGLImageKHR
type FnEglCreateImageKHR = unsafe extern "system" fn(
    *mut c_void, // EGLDisplay
    *mut c_void, // EGLContext  (EGL_NO_CONTEXT = null)
    u32,         // target      (EGL_NATIVE_BUFFER_ANDROID)
    *mut c_void, // buffer      (EGLClientBuffer from eglGetNativeClientBufferANDROID)
    *const i32,  // attrib_list (null-terminated or null for no attributes)
) -> *mut c_void; // EGLImageKHR (null = EGL_NO_IMAGE_KHR on failure)

// eglDestroyImageKHR(dpy, image) → EGL_TRUE/EGL_FALSE
type FnEglDestroyImageKHR = unsafe extern "system" fn(*mut c_void, *mut c_void) -> u32;

// glEGLImageTargetTexture2DOES(target, image)
type FnGlEglImageTargetTexture2DOES = unsafe extern "system" fn(u32, *mut c_void);

// eglPresentationTimeANDROID(dpy, surface, time_ns) → EGL_TRUE/FALSE
// Sets the desired presentation time (CLOCK_MONOTONIC ns) for the next eglSwapBuffers.
// The compositor holds the frame until the VSync nearest to time_ns.
type FnEglPresentationTimeANDROID =
    unsafe extern "system" fn(*mut c_void, *mut c_void, i64) -> u32;

// eglSurfaceAttrib(dpy, surface, attribute, value) — used for the
// EGL_EXT_surface_SMPTE2086_metadata / EGL_EXT_surface_CTA861_3_metadata
// attributes that attach static HDR metadata to swapped buffers. Some
// HWCs gate the HDMI HDR mode switch on this metadata being present.
type FnEglSurfaceAttrib = unsafe extern "system" fn(*mut c_void, *mut c_void, i32, i32) -> u32;

// EGL_EXT_surface_SMPTE2086_metadata
const EGL_SMPTE2086_DISPLAY_PRIMARY_RX_EXT: i32 = 0x3341;
const EGL_SMPTE2086_DISPLAY_PRIMARY_RY_EXT: i32 = 0x3342;
const EGL_SMPTE2086_DISPLAY_PRIMARY_GX_EXT: i32 = 0x3343;
const EGL_SMPTE2086_DISPLAY_PRIMARY_GY_EXT: i32 = 0x3344;
const EGL_SMPTE2086_DISPLAY_PRIMARY_BX_EXT: i32 = 0x3345;
const EGL_SMPTE2086_DISPLAY_PRIMARY_BY_EXT: i32 = 0x3346;
const EGL_SMPTE2086_WHITE_POINT_X_EXT: i32 = 0x3347;
const EGL_SMPTE2086_WHITE_POINT_Y_EXT: i32 = 0x3348;
const EGL_SMPTE2086_MAX_LUMINANCE_EXT: i32 = 0x3349;
const EGL_SMPTE2086_MIN_LUMINANCE_EXT: i32 = 0x334A;
// EGL_EXT_surface_CTA861_3_metadata
const EGL_CTA861_3_MAX_CONTENT_LIGHT_LEVEL_EXT: i32 = 0x3360;
const EGL_CTA861_3_MAX_FRAME_AVERAGE_LEVEL_EXT: i32 = 0x3361;
// Chromaticity/luminance values are scaled by EGL_METADATA_SCALING_EXT.
const EGL_METADATA_SCALING: f32 = 50000.0;

// Vertex layout: (x, y, u, v)
//
// OES texture Y=0 is the TOP of the AHB frame (Android convention).
// GLES NDC Y=+1 is the TOP of the viewport.
// wgpu present() Y-flips from the internal renderbuffer to the EGL window surface,
// so NDC-top → screen-top.
// Net: natural mapping — no extra flip needed.
const VERTICES: [f32; 24] = [
    -1.0, -1.0, 0.0, 1.0, // bottom-left
     1.0, -1.0, 1.0, 1.0, // bottom-right
    -1.0,  1.0, 0.0, 0.0, // top-left
     1.0, -1.0, 1.0, 1.0, // bottom-right (repeat)
     1.0,  1.0, 1.0, 0.0, // top-right
    -1.0,  1.0, 0.0, 0.0, // top-left  (repeat)
];

const VS_SRC: &str = "#version 300 es
in vec2 a_pos;
in vec2 a_tex;
uniform float u_scale_x;
uniform float u_scale_y;
uniform float u_tex_x_max;
uniform float u_tex_y_max;
out vec2 v_tex;
void main() {
    gl_Position = vec4(a_pos.x * u_scale_x, a_pos.y * u_scale_y, 0.0, 1.0);
    v_tex = vec2(a_tex.x * u_tex_x_max, a_tex.y * u_tex_y_max);
}";

const FS_SRC: &str = "#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision mediump float;
uniform samplerExternalOES u_texture;
in vec2 v_tex;
out vec4 out_color;
void main() {
    out_color = texture(u_texture, v_tex);
}";

// HDR10 (PQ / BT.2020) → SDR fragment shader. GLSL ES port of
// shader_hdr.wgsl, which is itself an exact port of FFmpeg's
// tonemap_opencl (mobius). One difference from the wgpu path: the OES
// sampler has already applied the Y'CbCr→R'G'B' matrix for the buffer's
// dataspace (BT.2020 limited for MediaCodec HDR output), so sampling
// yields PQ-ENCODED BT.2020 R'G'B' directly — we start the pipeline at
// the eotf_st2084 step. And there is no compute-pass scene detection
// here (the hook draws with a bare ES 3.0 context): peak/average come
// from uniforms — static metadata, HDR10+/DV dynamic metadata, or the
// 1000-nit fallback — resolved per frame on the CPU.
//
// highp is required: PQ code values need the full 10-bit significand
// and mediump is only guaranteed ~10 bits of *relative* precision.
const FS_HDR_SRC: &str = "#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision highp float;
uniform samplerExternalOES u_texture;
uniform float u_tone_param; // mobius knee j (filter `param`)
uniform float u_desat;      // desaturation strength (filter `desat`)
uniform float u_peak;       // signal peak, 1.0 = 100 nits
uniform float u_average;    // scene average, 1.0 = 100 nits
in vec2 v_tex;
out vec4 out_color;

const float REFERENCE_WHITE = 100.0;
const float SDR_AVG = 0.25;
const vec3 LUMA_DST = vec3(0.2126, 0.7152, 0.0722);

// SMPTE ST 2084 (PQ) EOTF: non-linear signal -> linear light where
// 1.0 = 100 nits (so 10 000-nit peak = 100.0).
float eotf_st2084(float x) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(max(x, 0.0), 1.0 / m2);
    float num = max(p - c1, 0.0);
    float den = max(c2 - c3 * p, 1e-6);
    float c = pow(num / den, 1.0 / m1);
    return x > 0.0 ? c * 10000.0 / REFERENCE_WHITE : 0.0;
}

// BT.2020 -> BT.709 primaries (linear light), ITU-R BT.2087 matrix.
vec3 bt2020_to_bt709(vec3 c) {
    return vec3(
         1.6605 * c.r - 0.5876 * c.g - 0.0728 * c.b,
        -0.1246 * c.r + 1.1329 * c.g - 0.0083 * c.b,
        -0.0182 * c.r - 0.1006 * c.g + 1.1187 * c.b);
}

float mobius(float s, float peak) {
    float j = u_tone_param;
    if (s <= j) {
        return s;
    }
    float a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
    float b = (j * j - 2.0 * j * peak + peak) / max(peak - 1.0, 1e-6);
    return (b * b + 2.0 * b * j + j * j) / (b - a) * (s + a) / (s + b);
}

vec3 map_one_pixel_rgb(vec3 rgb) {
    float sig = max(max(rgb.r, max(rgb.g, rgb.b)), 1e-6);
    float sig_old = sig;
    float slope = min(1.0, SDR_AVG / u_average);
    sig = sig * slope;
    float peak = u_peak * slope;
    if (u_desat > 0.0) {
        float luma = dot(LUMA_DST, rgb);
        float coeff = max(sig - 0.18, 1e-6) / max(sig, 1e-6);
        coeff = pow(coeff, 10.0 / u_desat);
        rgb = mix(rgb, vec3(luma), vec3(coeff));
        sig = mix(sig, luma * slope, coeff);
    }
    sig = min(mobius(sig, peak), 1.0);
    return rgb * (sig / sig_old);
}

void main() {
    // PQ-encoded BT.2020 R'G'B' (the driver already applied the YCbCr
    // matrix for the buffer's dataspace).
    vec3 c_pq = texture(u_texture, v_tex).rgb;
    vec3 c = vec3(eotf_st2084(c_pq.r), eotf_st2084(c_pq.g), eotf_st2084(c_pq.b));
    c = bt2020_to_bt709(c);
    c = map_one_pixel_rgb(c);
    // inverse_eotf_bt1886 (pure 1/2.4) -> display-referred R'G'B'.
    out_color = vec4(pow(max(c, vec3(0.0)), vec3(1.0 / 2.4)), 1.0);
}";

// ---------------------------------------------------------------------------
// Scene peak/average detection (GLES port of shader_hdr_detect.wgsl)
// ---------------------------------------------------------------------------
// The ES 3.0 present-hook context has no compute, so the same statistics
// are produced with two fragment-shader reduction passes + an async PBO
// readback:
//   pass 1: OES frame → DETECT_W×DETECT_H grid; each fragment samples an
//           8×8 point grid inside its cell, converts to linear BT.709
//           maxRGB (eotf_st2084 + primaries matrix — identical math to the
//           tonemap shader) and writes the cell's max + mean, PQ-re-encoded
//           into RGBA8 (perceptually uniform quantisation, no float-FBO
//           extension needed).
//   pass 2: grid → 1×1: max of cell maxima, mean of cell means.
//   readback: glReadPixels into a double-buffered PBO; the PREVIOUS
//           frame's pixel is mapped (never the just-issued one) so the GPU
//           is never stalled. One frame of latency — the same semantics as
//           the desktop's publish-before-accumulate ordering.
// The rolling 63-frame window + scene-change reset then runs on the CPU
// with the same constants as shader_hdr_detect.wgsl, and the result
// overrides the seed peak/average uniforms of the tonemap draw.

const DETECT_W: i32 = 80;
const DETECT_H: i32 = 45;
/// Sliding window length (frames) — shader_hdr_detect.wgsl's DETECTION_FRAMES.
const DETECT_WINDOW: usize = 63;
const REFERENCE_WHITE: f32 = 100.0;

// Fullscreen-quad vertex shader for the reduction passes (plain clip-space
// quad, varying = the quad's 0..1 texcoord).
const VS_DETECT_SRC: &str = "#version 300 es
in vec2 a_pos;
in vec2 a_tex;
out vec2 v_tex;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    v_tex = a_tex;
}";

// Pass 1: per-cell max/avg of linear BT.709 maxRGB, PQ-encoded into RG of
// an RGBA8 target. u_tex_max crops codec padding exactly like the draw.
const FS_DETECT_MAP_SRC: &str = "#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision highp float;
uniform samplerExternalOES u_texture;
uniform vec2 u_tex_max;
in vec2 v_tex;
out vec4 out_color;

float eotf_st2084(float x) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(max(x, 0.0), 1.0 / m2);
    float c = pow(max(p - c1, 0.0) / max(c2 - c3 * p, 1e-6), 1.0 / m1);
    return x > 0.0 ? c * 100.0 : 0.0; // 1.0 = 100 nits
}

float oetf_st2084(float x) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(clamp(x / 100.0, 0.0, 1.0), m1); // 100.0 = 10000 nits
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

vec3 bt2020_to_bt709(vec3 c) {
    return vec3(
         1.6605 * c.r - 0.5876 * c.g - 0.0728 * c.b,
        -0.1246 * c.r + 1.1329 * c.g - 0.0083 * c.b,
        -0.0182 * c.r - 0.1006 * c.g + 1.1187 * c.b);
}

void main() {
    // 8x8 sample grid inside this fragment's cell of the source frame.
    // Only the cell MEAN is kept: the filter's frame peak is the max over
    // 16x16-WORKGROUP AVERAGES (tonemap_opencl's detect kernel atomic_max's
    // the workgroup mean, not the pixel max) — our cells play the role of
    // its workgroups, so the per-pixel max must NOT be propagated or the
    // detected peak lands way above the filter's and over-compresses.
    vec2 cell = vec2(1.0) / vec2(80.0, 45.0);
    float sig_sum = 0.0;
    for (int j = 0; j < 8; j++) {
        for (int i = 0; i < 8; i++) {
            vec2 off = (vec2(float(i), float(j)) + 0.5) / 8.0 - 0.5;
            vec2 uv = clamp(v_tex + off * cell, vec2(0.0), vec2(1.0)) * u_tex_max;
            vec3 pq = texture(u_texture, uv).rgb;
            vec3 lin = vec3(eotf_st2084(pq.r), eotf_st2084(pq.g), eotf_st2084(pq.b));
            lin = bt2020_to_bt709(lin);
            sig_sum += max(lin.r, max(lin.g, lin.b));
        }
    }
    float mean_pq = oetf_st2084(sig_sum / 64.0);
    out_color = vec4(mean_pq, mean_pq, 0.0, 1.0);
}";

// SDR (BT.709, display gamma) → PQ BT.2020 up-conversion, for SDR frames
// rendered while the surface is locked to the BT2020_PQ dataspace (ABR
// switched to an SDR rep mid-HDR-session). BT.2408: SDR reference white
// maps to 203 nits; linearisation is the BT.1886 2.4 power (the inverse of
// what the tonemap path emits).
const FS_SDR_TO_PQ_SRC: &str = "#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision highp float;
uniform samplerExternalOES u_texture;
in vec2 v_tex;
out vec4 out_color;

vec3 bt709_to_bt2020(vec3 c) {
    return vec3(
        0.6274 * c.r + 0.3293 * c.g + 0.0433 * c.b,
        0.0691 * c.r + 0.9195 * c.g + 0.0114 * c.b,
        0.0164 * c.r + 0.0880 * c.g + 0.8956 * c.b);
}

// PQ OETF, input = fraction of 10 000 nits.
float oetf_pq(float x) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(clamp(x, 0.0, 1.0), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

void main() {
    vec3 c = texture(u_texture, v_tex).rgb; // BT.709 display-gamma R'G'B'
    vec3 lin = pow(max(c, vec3(0.0)), vec3(2.4)) * (203.0 / 10000.0);
    lin = bt709_to_bt2020(lin);
    out_color = vec4(oetf_pq(lin.r), oetf_pq(lin.g), oetf_pq(lin.b), 1.0);
}";

/// CPU side of the scene detection: rolling window + PBO bookkeeping.
struct DetectState {
    /// (frame_peak, frame_avg) in REFERENCE_WHITE units, newest at the back.
    window: std::collections::VecDeque<(f32, f32)>,
    /// Which of the two PBOs the NEXT readback should be issued into.
    pbo_idx: usize,
    /// Whether each PBO currently holds an issued, not-yet-mapped readback.
    pbo_pending: [bool; 2],
    /// Published values (None until the first readback lands).
    out_peak_avg: Option<(f32, f32)>,
    /// Readbacks processed (diagnostic log rate limiting).
    frames: u64,
}

/// GL objects for the detection pass. The grid (14 KB) is read back whole
/// through the PBOs and reduced on the CPU — a second GPU reduction pass
/// proved unreliable on PowerVR (the 1×1 draw read bogus values) and a
/// 3 600-texel loop is nothing on the CPU anyway.
struct HdrDetectGl {
    map_program: glow::Program,
    map_tex_max_loc: Option<glow::UniformLocation>,
    /// Pass target (DETECT_W×DETECT_H RGBA8).
    grid_fbo: glow::Framebuffer,
    _grid_tex: glow::Texture,
    pbos: [glow::Buffer; 2],
    state: std::sync::Mutex<DetectState>,
}

const GRID_BYTES: i32 = DETECT_W * DETECT_H * 4;

impl HdrDetectGl {
    /// Build the reduction programs, FBOs and PBOs. Must be called with
    /// the EGL context current. Any failure is non-fatal for playback —
    /// the caller degrades to seed-only tonemapping.
    unsafe fn new(gl: &glow::Context) -> Result<Self, String> {
        let vs = compile_shader(gl, glow::VERTEX_SHADER, VS_DETECT_SRC)?;
        let fs_map = compile_shader(gl, glow::FRAGMENT_SHADER, FS_DETECT_MAP_SRC)?;
        let map_program = link_program(gl, vs, fs_map)?;
        gl.delete_shader(fs_map);
        gl.delete_shader(vs);

        gl.use_program(Some(map_program));
        if let Some(loc) = gl.get_uniform_location(map_program, "u_texture") {
            gl.uniform_1_i32(Some(&loc), 0);
        }
        let map_tex_max_loc = gl.get_uniform_location(map_program, "u_tex_max");
        gl.use_program(None);

        let make_target = |w: i32, h: i32, label: &str| -> Result<(glow::Framebuffer, glow::Texture), String> {
            let tex = gl.create_texture().map_err(|e| e.to_string())?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, w, h);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            let fbo = gl.create_framebuffer().map_err(|e| e.to_string())?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                return Err(format!("{} FBO incomplete: {:#x}", label, status));
            }
            Ok((fbo, tex))
        };
        let (grid_fbo, grid_tex) = make_target(DETECT_W, DETECT_H, "detect grid")?;

        let mut make_pbo = || -> Result<glow::Buffer, String> {
            let b = gl.create_buffer().map_err(|e| e.to_string())?;
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(b));
            gl.buffer_data_size(glow::PIXEL_PACK_BUFFER, GRID_BYTES, glow::STREAM_READ);
            Ok(b)
        };
        let pbos = [make_pbo()?, make_pbo()?];
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);

        Ok(Self {
            map_program,
            map_tex_max_loc,
            grid_fbo,
            _grid_tex: grid_tex,
            pbos,
            state: std::sync::Mutex::new(DetectState {
                window: std::collections::VecDeque::with_capacity(DETECT_WINDOW + 1),
                pbo_idx: 0,
                pbo_pending: [false; 2],
                out_peak_avg: None,
                frames: 0,
            }),
        })
    }

    /// CPU mirror of the shader's PQ OETF/EOTF pair (for the RGBA8 codes).
    fn pq_decode(code: u8) -> f32 {
        let x = code as f32 / 255.0;
        let m1 = 0.159_301_76_f32;
        let m2 = 78.84375_f32;
        let c1 = 0.8359375_f32;
        let c2 = 18.8515625_f32;
        let c3 = 18.6875_f32;
        let p = x.max(0.0).powf(1.0 / m2);
        let c = ((p - c1).max(0.0) / (c2 - c3 * p).max(1e-6)).powf(1.0 / m1);
        // Fraction of 10 000 nits → REFERENCE_WHITE units (1.0 = 100 nits).
        c * 10_000.0 / REFERENCE_WHITE
    }

    /// Run both reduction passes for the frame currently bound on the OES
    /// texture unit 0, map the PREVIOUS frame's readback into the rolling
    /// window, and return the published (peak, average) once available.
    /// Caller restores draw state afterwards (program, viewport, FBO 0 are
    /// all clobbered here).
    unsafe fn run(
        &self,
        gl: &glow::Context,
        vao: glow::VertexArray,
        tex_x_max: f32,
        tex_y_max: f32,
        scene_threshold: f32,
    ) -> Option<(f32, f32)> {
        let mut st = self.state.lock().unwrap();

        // 1. Map the oldest pending PBO (the readback issued LAST frame —
        //    the GPU has long finished it, so this never stalls) and reduce
        //    the grid of PQ-encoded cell means on the CPU.
        let map_idx = st.pbo_idx;
        if st.pbo_pending[map_idx] {
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(self.pbos[map_idx]));
            let ptr = gl.map_buffer_range(
                glow::PIXEL_PACK_BUFFER,
                0,
                GRID_BYTES,
                glow::MAP_READ_BIT,
            );
            if !ptr.is_null() {
                let grid = std::slice::from_raw_parts(ptr, GRID_BYTES as usize);
                // 256-entry LUT: PQ code byte → linear. pq_decode already
                // yields REFERENCE_WHITE units (1.0 = 100 nits).
                let lut: Vec<f32> = (0..256u32)
                    .map(|c| Self::pq_decode(c as u8))
                    .collect();
                let mut peak = 0.0f32;
                let mut sum = 0.0f32;
                let cells = (DETECT_W * DETECT_H) as usize;
                for cell in 0..cells {
                    let v = lut[grid[cell * 4] as usize];
                    peak = peak.max(v);
                    sum += v;
                }
                let avg = sum / cells as f32;
                gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);

                // Scene-change reset — same rule as shader_hdr_detect.wgsl:
                // |frame avg − window mean| > threshold (REFERENCE_WHITE
                // units) drops the whole window.
                if scene_threshold > 0.0 && !st.window.is_empty() {
                    let mean: f32 = st.window.iter().map(|(_, a)| a).sum::<f32>()
                        / st.window.len() as f32;
                    if (avg - mean).abs() > scene_threshold {
                        st.window.clear();
                    }
                }
                st.window.push_back((peak, avg));
                while st.window.len() > DETECT_WINDOW {
                    st.window.pop_front();
                }
                let n = st.window.len() as f32;
                let peak_out =
                    (st.window.iter().map(|(p, _)| p).sum::<f32>() / n).max(1.0);
                let avg_out =
                    (st.window.iter().map(|(_, a)| a).sum::<f32>() / n).max(1e-3);
                st.out_peak_avg = Some((peak_out, avg_out));
                st.frames += 1;
                if st.frames % 120 == 1 {
                    log::debug!(
                        "[gles_oes] detect: frame peak={:.3} avg={:.3} → window({}) peak={:.3} avg={:.3}",
                        peak, avg, st.window.len(), peak_out, avg_out
                    );
                }
            }
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            st.pbo_pending[map_idx] = false;
        }

        // 2. Pass 1: OES frame → grid. The OES texture is already bound on
        //    unit 0 by the caller.
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.grid_fbo));
        gl.viewport(0, 0, DETECT_W, DETECT_H);
        gl.use_program(Some(self.map_program));
        if let Some(ref loc) = self.map_tex_max_loc {
            gl.uniform_2_f32(Some(loc), tex_x_max, tex_y_max);
        }
        gl.bind_vertex_array(Some(vao));
        gl.draw_arrays(glow::TRIANGLES, 0, 6);

        gl.bind_vertex_array(None);

        // 3. Issue the async grid readback for THIS frame into the current
        //    PBO (mapped two frames from now).
        let issue_idx = st.pbo_idx;
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.grid_fbo));
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(self.pbos[issue_idx]));
        gl.read_pixels(
            0,
            0,
            DETECT_W,
            DETECT_H,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::BufferOffset(0),
        );
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
        st.pbo_pending[issue_idx] = true;
        st.pbo_idx = (issue_idx + 1) % 2;

        st.out_peak_avg
    }
}

// libEGL.so is always present on Android.
// eglGetProcAddress loads EGL/GL extension entry points at runtime.
// eglGetCurrentDisplay/eglGetCurrentSurface return the EGL display/surface for
// whichever context is current.
#[link(name = "EGL")]
extern "C" {
    fn eglGetProcAddress(procname: *const i8) -> Option<unsafe extern "system" fn()>;
    fn eglGetCurrentDisplay() -> *mut c_void;
    fn eglGetCurrentSurface(which: i32) -> *mut c_void;
    fn eglGetError() -> u32;
}
const EGL_DRAW: i32 = 0x3059;

/// Frame data passed from the render thread to the present hook.
/// Stored as primitive types so the struct is Send (raw pointers are stored as usize).
pub struct GlesOesPendingFrame {
    pub ahb_ptr: usize,  // *mut AHardwareBuffer cast to usize
    pub scale_x: f32,
    pub scale_y: f32,
    /// (content_width - 1) / buffer_width — crops the right-edge codec
    /// padding so we don't sample uninitialised memory (visible as a green
    /// rectangle on PowerVR Rogue / MT8696 where the buffer is 1920×1088 for
    /// 1280×720 content). Inset by 1 texel so bilinear filtering and the
    /// half-res NV12 chroma plane can't bleed padding into the edge pixels
    /// (see the padding-update block in video.rs). 1.0 = no padding.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub tex_x_max: f32,
    pub tex_y_max: f32,
    /// CLOCK_MONOTONIC nanoseconds when this frame should appear on screen.
    /// Passed to eglPresentationTimeANDROID; 0 = no constraint.
    pub desired_present_ns: i64,
    /// How this frame's signal maps onto the surface — resolved per frame
    /// in render_android_gles so ABR SDR↔HDR swaps switch programs on
    /// exactly the right frame.
    pub mode: OesRenderMode,
}

/// Per-frame program selection for the OES draw.
#[derive(Clone, Copy, Debug)]
pub enum OesRenderMode {
    /// SDR content on an SDR-dataspace surface — plain OES passthrough.
    Sdr,
    /// PQ content tonemapped to SDR in-shader (SDR display fallback).
    TonemapHdr(HdrFrameParams),
    /// PQ content handed through untouched — the surface dataspace is
    /// BT2020_PQ and the display presents HDR natively. Uses the same
    /// plain program as Sdr (the OES sampler already yields PQ BT.2020
    /// R'G'B'; there is nothing to convert).
    PassthroughPq,
    /// SDR (BT.709) content up-converted to PQ BT.2020 because the
    /// surface is locked to the BT2020_PQ dataspace (sticky passthrough
    /// session) — SDR white maps to 203 nits per BT.2408.
    SdrToPq,
}

/// Per-frame mobius tonemap inputs for the HDR fragment shader.
/// Peak/average are in REFERENCE_WHITE units (1.0 = 100 nits) — the same
/// scale the wgpu detection buffer publishes on desktop. `peak`/`average`
/// act as the SEED: once the GL scene detection has a window, its values
/// take over (exactly like the desktop detection supersedes the uniform
/// seed from the second frame on).
#[derive(Clone, Copy, Debug)]
pub struct HdrFrameParams {
    pub tone_param: f32,
    pub desat: f32,
    pub peak: f32,
    pub average: f32,
    /// Scene-change reset threshold in REFERENCE_WHITE units (filter
    /// `threshold`, default 0.2 = 20 nits average jump).
    pub scene_threshold: f32,
}

/// Renders AHardwareBuffer frames directly to FBO 0 (window surface) via GL_TEXTURE_EXTERNAL_OES.
/// Called from inside wgpu's present() via a present hook, after the window EGL surface is current.
/// Zero-copy: the AHB is imported as an EGL image, the OES sampler handles
/// YCbCr→RGB on the GPU; no CPU copy at any stage.
pub struct GlesOesRenderer {
    program: glow::Program,
    vao: glow::VertexArray,
    _vbo: glow::Buffer, // keep-alive (freed when the GL context is destroyed)
    oes_texture: glow::Texture,
    scale_x_loc: Option<glow::UniformLocation>,
    scale_y_loc: Option<glow::UniformLocation>,
    tex_x_max_loc: Option<glow::UniformLocation>,
    tex_y_max_loc: Option<glow::UniformLocation>,
    /// HDR (PQ→SDR mobius tonemap) program. None if its compilation failed
    /// at init — HDR frames then render through the SDR program (washed
    /// out, but alive) with a one-time warning.
    hdr: Option<HdrProgram>,
    /// Scene peak/average detection (GL reduction + PBO readback). None →
    /// the tonemap runs on the seed peak/average alone.
    hdr_detect: Option<HdrDetectGl>,
    /// SDR→PQ up-conversion program for SDR frames inside a sticky
    /// BT2020_PQ passthrough session. None → SDR frames render through
    /// the plain program (washed out on an HDR surface, but alive).
    sdr_to_pq: Option<BasicProgram>,
    // EGL extension function pointers stored as usize (makes the struct Send + Sync).
    fn_get_native_client_buffer: usize,  // eglGetNativeClientBufferANDROID
    fn_egl_create_image: usize,          // eglCreateImageKHR
    fn_egl_destroy_image: usize,         // eglDestroyImageKHR
    fn_gl_egl_image_target_texture: usize, // glEGLImageTargetTexture2DOES
    fn_egl_presentation_time: usize,     // eglPresentationTimeANDROID (0 = unavailable)
    fn_egl_surface_attrib: usize,        // eglSurfaceAttrib (core since EGL 1.1)
    // Raw EGLDisplay pointer captured at init time.
    egl_display: usize,
    /// Static HDR metadata has been attached to the draw surface (set once
    /// per surface on the first PassthroughPq frame).
    hdr_metadata_set: std::sync::atomic::AtomicBool,
}

unsafe impl Send for GlesOesRenderer {}
unsafe impl Sync for GlesOesRenderer {}

/// A program with just the geometry/crop uniforms (SDR→PQ up-convert).
struct BasicProgram {
    program: glow::Program,
    scale_x_loc: Option<glow::UniformLocation>,
    scale_y_loc: Option<glow::UniformLocation>,
    tex_x_max_loc: Option<glow::UniformLocation>,
    tex_y_max_loc: Option<glow::UniformLocation>,
}

/// The HDR program and its uniform locations.
struct HdrProgram {
    program: glow::Program,
    scale_x_loc: Option<glow::UniformLocation>,
    scale_y_loc: Option<glow::UniformLocation>,
    tex_x_max_loc: Option<glow::UniformLocation>,
    tex_y_max_loc: Option<glow::UniformLocation>,
    tone_param_loc: Option<glow::UniformLocation>,
    desat_loc: Option<glow::UniformLocation>,
    peak_loc: Option<glow::UniformLocation>,
    average_loc: Option<glow::UniformLocation>,
}

impl GlesOesRenderer {
    /// Initialise the GL program, VAO/VBO, and OES texture.
    /// Must be called while the EGL context is current (`AdapterContext::lock()` guard held).
    pub unsafe fn new(gl: &glow::Context) -> Result<Self, String> {
        // Load all required EGL/GL extension functions via eglGetProcAddress.
        macro_rules! load_fn {
            ($name:literal) => {{
                let sym = concat!($name, "\0");
                eglGetProcAddress(sym.as_ptr() as *const i8)
                    .ok_or_else(|| format!("{} not available", $name))?
                    as usize
            }};
        }

        let fn_get_client = load_fn!("eglGetNativeClientBufferANDROID");
        let fn_create = load_fn!("eglCreateImageKHR");
        let fn_destroy = load_fn!("eglDestroyImageKHR");
        let fn_target = load_fn!("glEGLImageTargetTexture2DOES");
        // EGL_ANDROID_presentation_time: available on API 21+. Not fatal if absent.
        let fn_presentation_time: usize = {
            let sym = concat!("eglPresentationTimeANDROID", "\0");
            match eglGetProcAddress(sym.as_ptr() as *const i8) {
                Some(f) => f as usize,
                None => {
                    log::warn!("[gles_oes] eglPresentationTimeANDROID unavailable — VSync jitter not corrected");
                    0
                }
            }
        };
        // eglSurfaceAttrib is core EGL; via GetProcAddress for consistency.
        let fn_surface_attrib: usize = {
            let sym = concat!("eglSurfaceAttrib", "\0");
            eglGetProcAddress(sym.as_ptr() as *const i8).map(|f| f as usize).unwrap_or(0)
        };

        // Capture the EGLDisplay while the context is current.
        let display = eglGetCurrentDisplay() as usize;
        if display == 0 {
            return Err("eglGetCurrentDisplay returned EGL_NO_DISPLAY".to_string());
        }

        log::info!(
            "[gles_oes] init: eglGetNativeClientBufferANDROID={:#x} \
             eglCreateImageKHR={:#x} eglPresentationTimeANDROID={:#x} display={:#x}",
            fn_get_client,
            fn_create,
            fn_presentation_time,
            display
        );

        // Compile and link the OES shader program.
        let vs = compile_shader(gl, glow::VERTEX_SHADER, VS_SRC)?;
        let fs = compile_shader(gl, glow::FRAGMENT_SHADER, FS_SRC)?;
        let program = link_program(gl, vs, fs)?;
        gl.delete_shader(vs);
        gl.delete_shader(fs);

        // Build VAO + VBO with the fullscreen quad.
        let vao = gl.create_vertex_array().map_err(|e| e.to_string())?;
        let vbo = gl.create_buffer().map_err(|e| e.to_string())?;

        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let bytes = bytemuck::cast_slice::<f32, u8>(&VERTICES);
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);

        // attribute 0 = a_pos (x, y), stride = 4 floats, offset 0
        // attribute 1 = a_tex (u, v), stride = 4 floats, offset 8 bytes
        let stride = (4 * std::mem::size_of::<f32>()) as i32;
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 2 * 4);

        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        gl.bind_vertex_array(None);

        // Allocate a persistent OES texture object.
        // glEGLImageTargetTexture2DOES will re-point it to each frame's EGL image.
        let oes_texture = gl.create_texture().map_err(|e| e.to_string())?;
        gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(oes_texture));
        gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, None);

        // Bind the sampler uniform to texture unit 0 (done once at init time).
        gl.use_program(Some(program));
        if let Some(loc) = gl.get_uniform_location(program, "u_texture") {
            gl.uniform_1_i32(Some(&loc), 0);
        }
        let scale_x_loc = gl.get_uniform_location(program, "u_scale_x");
        let scale_y_loc = gl.get_uniform_location(program, "u_scale_y");
        let tex_x_max_loc = gl.get_uniform_location(program, "u_tex_x_max");
        let tex_y_max_loc = gl.get_uniform_location(program, "u_tex_y_max");
        gl.use_program(None);

        log::info!(
            "[gles_oes] shader compiled, uniforms: scale_x={} scale_y={} tex_x_max={} tex_y_max={}",
            scale_x_loc.is_some(),
            scale_y_loc.is_some(),
            tex_x_max_loc.is_some(),
            tex_y_max_loc.is_some(),
        );

        // The HDR program is optional — a compile failure must not take the
        // SDR path down with it.
        let hdr = (|| -> Result<HdrProgram, String> {
            let vs = compile_shader(gl, glow::VERTEX_SHADER, VS_SRC)?;
            let fs = compile_shader(gl, glow::FRAGMENT_SHADER, FS_HDR_SRC)?;
            let program = link_program(gl, vs, fs)?;
            gl.delete_shader(vs);
            gl.delete_shader(fs);
            gl.use_program(Some(program));
            if let Some(loc) = gl.get_uniform_location(program, "u_texture") {
                gl.uniform_1_i32(Some(&loc), 0);
            }
            let p = HdrProgram {
                scale_x_loc: gl.get_uniform_location(program, "u_scale_x"),
                scale_y_loc: gl.get_uniform_location(program, "u_scale_y"),
                tex_x_max_loc: gl.get_uniform_location(program, "u_tex_x_max"),
                tex_y_max_loc: gl.get_uniform_location(program, "u_tex_y_max"),
                tone_param_loc: gl.get_uniform_location(program, "u_tone_param"),
                desat_loc: gl.get_uniform_location(program, "u_desat"),
                peak_loc: gl.get_uniform_location(program, "u_peak"),
                average_loc: gl.get_uniform_location(program, "u_average"),
                program,
            };
            gl.use_program(None);
            Ok(p)
        })();
        let hdr = match hdr {
            Ok(p) => {
                log::info!("[gles_oes] HDR tonemap shader compiled");
                Some(p)
            }
            Err(e) => {
                log::warn!("[gles_oes] HDR shader unavailable: {} — HDR frames will render through the SDR program", e);
                None
            }
        };

        // Scene detection — optional refinement; seed-only tonemap without it.
        let hdr_detect = if hdr.is_some() {
            match HdrDetectGl::new(gl) {
                Ok(d) => {
                    log::info!("[gles_oes] HDR scene detection ready ({}x{} grid)", DETECT_W, DETECT_H);
                    Some(d)
                }
                Err(e) => {
                    log::warn!("[gles_oes] HDR scene detection unavailable: {} — using static peak", e);
                    None
                }
            }
        } else {
            None
        };

        // SDR→PQ up-convert program (passthrough sessions only).
        let sdr_to_pq = (|| -> Result<BasicProgram, String> {
            let vs = compile_shader(gl, glow::VERTEX_SHADER, VS_SRC)?;
            let fs = compile_shader(gl, glow::FRAGMENT_SHADER, FS_SDR_TO_PQ_SRC)?;
            let program = link_program(gl, vs, fs)?;
            gl.delete_shader(vs);
            gl.delete_shader(fs);
            gl.use_program(Some(program));
            if let Some(loc) = gl.get_uniform_location(program, "u_texture") {
                gl.uniform_1_i32(Some(&loc), 0);
            }
            let p = BasicProgram {
                scale_x_loc: gl.get_uniform_location(program, "u_scale_x"),
                scale_y_loc: gl.get_uniform_location(program, "u_scale_y"),
                tex_x_max_loc: gl.get_uniform_location(program, "u_tex_x_max"),
                tex_y_max_loc: gl.get_uniform_location(program, "u_tex_y_max"),
                program,
            };
            gl.use_program(None);
            Ok(p)
        })()
        .map_err(|e| {
            log::warn!("[gles_oes] SDR→PQ program unavailable: {}", e);
            e
        })
        .ok();

        Ok(GlesOesRenderer {
            program,
            vao,
            _vbo: vbo,
            oes_texture,
            scale_x_loc,
            scale_y_loc,
            tex_x_max_loc,
            tex_y_max_loc,
            hdr,
            hdr_detect,
            sdr_to_pq,
            fn_get_native_client_buffer: fn_get_client,
            fn_egl_create_image: fn_create,
            fn_egl_destroy_image: fn_destroy,
            fn_gl_egl_image_target_texture: fn_target,
            fn_egl_presentation_time: fn_presentation_time,
            fn_egl_surface_attrib: fn_surface_attrib,
            egl_display: display,
            hdr_metadata_set: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Attach static HDR metadata (SMPTE 2086 mastering display + CTA-861.3
    /// content light level) to the current draw surface. Some HWCs only
    /// switch the HDMI output into HDR mode when the PQ layer carries this.
    /// BT.2020 primaries, D65 white point; luminance from the stream's SEI
    /// when available (caller passes nits), else 1000/0.005 defaults.
    unsafe fn set_surface_hdr_metadata(&self, max_nits: f32, max_cll: f32, max_fall: f32) {
        if self.fn_egl_surface_attrib == 0 {
            return;
        }
        let attrib: FnEglSurfaceAttrib = std::mem::transmute(self.fn_egl_surface_attrib);
        let display = self.egl_display as *mut c_void;
        let surface = eglGetCurrentSurface(EGL_DRAW);
        if surface.is_null() {
            return;
        }
        let s = EGL_METADATA_SCALING;
        // BT.2020 primaries + D65 white point; every attribute value is
        // "value × EGL_METADATA_SCALING_EXT" per the extension spec.
        let set = |attr: i32, v: f32| {
            attrib(display, surface, attr, v.round() as i32);
        };
        set(EGL_SMPTE2086_DISPLAY_PRIMARY_RX_EXT, 0.708 * s);
        set(EGL_SMPTE2086_DISPLAY_PRIMARY_RY_EXT, 0.292 * s);
        set(EGL_SMPTE2086_DISPLAY_PRIMARY_GX_EXT, 0.170 * s);
        set(EGL_SMPTE2086_DISPLAY_PRIMARY_GY_EXT, 0.797 * s);
        set(EGL_SMPTE2086_DISPLAY_PRIMARY_BX_EXT, 0.131 * s);
        set(EGL_SMPTE2086_DISPLAY_PRIMARY_BY_EXT, 0.046 * s);
        set(EGL_SMPTE2086_WHITE_POINT_X_EXT, 0.3127 * s);
        set(EGL_SMPTE2086_WHITE_POINT_Y_EXT, 0.3290 * s);
        set(EGL_SMPTE2086_MAX_LUMINANCE_EXT, max_nits * s);
        set(EGL_SMPTE2086_MIN_LUMINANCE_EXT, 0.005 * s);
        set(EGL_CTA861_3_MAX_CONTENT_LIGHT_LEVEL_EXT, max_cll * s);
        set(EGL_CTA861_3_MAX_FRAME_AVERAGE_LEVEL_EXT, max_fall * s);
        log::info!(
            "[gles_oes] static HDR metadata attached (mastering {} nits, MaxCLL {}, MaxFALL {})",
            max_nits, max_cll, max_fall
        );
    }

    /// Render one AHardwareBuffer frame to the currently-bound DRAW framebuffer.
    ///
    /// Called from the wgpu present hook after `make_current(window_surface)` and
    /// `glBindFramebuffer(DRAW_FRAMEBUFFER, 0)` — so FBO 0 (the EGL window surface)
    /// is the render target. eglSwapBuffers follows immediately after this returns.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn render(
        &self,
        gl: &glow::Context,
        ahb_ptr: *mut c_void,        // *mut AHardwareBuffer
        viewport_width: i32,
        viewport_height: i32,
        scale_x: f32,
        scale_y: f32,
        tex_x_max: f32,
        tex_y_max: f32,
        desired_present_ns: i64,     // CLOCK_MONOTONIC ns for this frame; 0 = unconstrained
        mode: OesRenderMode,
    ) -> Result<(), String> {
        let get_client: FnEglGetNativeClientBufferANDROID =
            std::mem::transmute(self.fn_get_native_client_buffer);
        let egl_create: FnEglCreateImageKHR = std::mem::transmute(self.fn_egl_create_image);
        let egl_destroy: FnEglDestroyImageKHR = std::mem::transmute(self.fn_egl_destroy_image);
        let gl_target: FnGlEglImageTargetTexture2DOES =
            std::mem::transmute(self.fn_gl_egl_image_target_texture);
        let display = self.egl_display as *mut c_void;

        // Step 1: AHardwareBuffer* → EGLClientBuffer
        // eglCreateImageKHR with EGL_NATIVE_BUFFER_ANDROID expects an EGLClientBuffer,
        // NOT a raw AHardwareBuffer*. eglGetNativeClientBufferANDROID does the conversion.
        let _ = eglGetError(); // clear any pending error
        let client_buffer = get_client(ahb_ptr as *const c_void);
        if client_buffer.is_null() {
            return Err(format!(
                "eglGetNativeClientBufferANDROID returned null (ahb={:?}, eglErr={:#x})",
                ahb_ptr,
                eglGetError()
            ));
        }

        // Step 2: EGLClientBuffer → EGLImageKHR (GPU-side zero-copy import)
        let _ = eglGetError(); // clear
        let egl_image = egl_create(
            display,
            std::ptr::null_mut(), // EGL_NO_CONTEXT
            EGL_NATIVE_BUFFER_ANDROID,
            client_buffer,
            std::ptr::null(), // no extra attributes
        );
        let egl_err = eglGetError();
        if egl_image.is_null() {
            return Err(format!(
                "eglCreateImageKHR returned EGL_NO_IMAGE_KHR (eglErr={:#x})",
                egl_err
            ));
        }
        log::debug!("[gles_oes] EGLImage={:?}", egl_image);

        // Step 3: draw OES texture directly to FBO 0 (window surface).
        // DRAW_FRAMEBUFFER is already bound to 0 by wgpu's present() before the hook fires.

        // Reset GL state that wgpu may have left in a draw-suppressing configuration.
        gl.color_mask(true, true, true, true);
        gl.disable(glow::DEPTH_TEST);
        gl.disable(glow::STENCIL_TEST);
        gl.disable(glow::CULL_FACE);
        gl.disable(glow::BLEND);
        gl.disable(glow::SCISSOR_TEST);

        // Clear to black first so letterbox bars around the video are black.
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);

        // Attach the EGL image to the OES texture and draw the fullscreen quad.
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(self.oes_texture));
        gl_target(GL_TEXTURE_EXTERNAL_OES, egl_image);
        let gl_err = gl.get_error();
        if gl_err != glow::NO_ERROR {
            log::warn!("[gles_oes] GL error after glEGLImageTargetTexture2DOES: {:#x}", gl_err);
        }

        // Scene peak/average detection — tonemapped HDR frames only (a
        // passthrough display does its own). Consumes the OES texture
        // bound above; clobbers FBO/viewport/program, so it runs before
        // the main draw's state setup and FBO 0 is restored here.
        let detected = if matches!(mode, OesRenderMode::TonemapHdr(_)) && self.hdr.is_some() {
            let threshold = match mode {
                OesRenderMode::TonemapHdr(p) => p.scene_threshold,
                _ => 0.2,
            };
            let r = self
                .hdr_detect
                .as_ref()
                .and_then(|d| d.run(gl, self.vao, tex_x_max, tex_y_max, threshold));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
            r
        } else {
            None
        };

        gl.viewport(0, 0, viewport_width, viewport_height);
        match (mode, &self.hdr, &self.sdr_to_pq) {
            (OesRenderMode::TonemapHdr(p), Some(h), _) => {
                // Detection (rolling-window scene statistics) supersedes
                // the seed once it has data — same takeover the desktop
                // detection performs after its first frame.
                let (peak, average) = detected.unwrap_or((p.peak, p.average));
                gl.use_program(Some(h.program));
                if let Some(ref loc) = h.scale_x_loc {
                    gl.uniform_1_f32(Some(loc), scale_x);
                }
                if let Some(ref loc) = h.scale_y_loc {
                    gl.uniform_1_f32(Some(loc), scale_y);
                }
                if let Some(ref loc) = h.tex_x_max_loc {
                    gl.uniform_1_f32(Some(loc), tex_x_max);
                }
                if let Some(ref loc) = h.tex_y_max_loc {
                    gl.uniform_1_f32(Some(loc), tex_y_max);
                }
                if let Some(ref loc) = h.tone_param_loc {
                    gl.uniform_1_f32(Some(loc), p.tone_param);
                }
                if let Some(ref loc) = h.desat_loc {
                    gl.uniform_1_f32(Some(loc), p.desat);
                }
                if let Some(ref loc) = h.peak_loc {
                    gl.uniform_1_f32(Some(loc), peak);
                }
                if let Some(ref loc) = h.average_loc {
                    gl.uniform_1_f32(Some(loc), average);
                }
            }
            (OesRenderMode::SdrToPq, _, Some(s)) => {
                gl.use_program(Some(s.program));
                if let Some(ref loc) = s.scale_x_loc {
                    gl.uniform_1_f32(Some(loc), scale_x);
                }
                if let Some(ref loc) = s.scale_y_loc {
                    gl.uniform_1_f32(Some(loc), scale_y);
                }
                if let Some(ref loc) = s.tex_x_max_loc {
                    gl.uniform_1_f32(Some(loc), tex_x_max);
                }
                if let Some(ref loc) = s.tex_y_max_loc {
                    gl.uniform_1_f32(Some(loc), tex_y_max);
                }
            }
            (m, _, _) => {
                // Sdr and PassthroughPq both sample-and-emit unchanged;
                // also the degraded fallbacks (TonemapHdr without the HDR
                // program, SdrToPq without the up-convert program).
                match m {
                    OesRenderMode::TonemapHdr(_) => {
                        static WARN_ONCE: std::sync::Once = std::sync::Once::new();
                        WARN_ONCE.call_once(|| {
                            log::warn!("[gles_oes] HDR frame but no HDR program — SDR passthrough");
                        });
                    }
                    OesRenderMode::SdrToPq => {
                        static WARN_ONCE: std::sync::Once = std::sync::Once::new();
                        WARN_ONCE.call_once(|| {
                            log::warn!("[gles_oes] SDR frame in PQ session but no up-convert program — plain draw");
                        });
                    }
                    _ => {}
                }
                gl.use_program(Some(self.program));
                if let Some(ref loc) = self.scale_x_loc {
                    gl.uniform_1_f32(Some(loc), scale_x);
                }
                if let Some(ref loc) = self.scale_y_loc {
                    gl.uniform_1_f32(Some(loc), scale_y);
                }
                if let Some(ref loc) = self.tex_x_max_loc {
                    gl.uniform_1_f32(Some(loc), tex_x_max);
                }
                if let Some(ref loc) = self.tex_y_max_loc {
                    gl.uniform_1_f32(Some(loc), tex_y_max);
                }
            }
        }
        gl.bind_vertex_array(Some(self.vao));
        gl.draw_arrays(glow::TRIANGLES, 0, 6);
        gl.bind_vertex_array(None);
        gl.use_program(None);

        let gl_err = gl.get_error();
        if gl_err != glow::NO_ERROR {
            log::warn!("[gles_oes] GL error after draw_arrays: {:#x}", gl_err);
        }

        // Static HDR metadata, once per surface, on the first passthrough
        // frame — the MTK HWC composites PQ layers into an SDR output
        // unless the buffer carries mastering metadata.
        if matches!(mode, OesRenderMode::PassthroughPq | OesRenderMode::SdrToPq)
            && !self
                .hdr_metadata_set
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            // 1000-nit mastering / CLL defaults — typical for the PQ ladder;
            // good enough for the HDMI InfoFrame (the TV tone-maps anyway).
            self.set_surface_hdr_metadata(1000.0, 1000.0, 400.0);
        }

        // Step 4: Set presentation timestamp so the compositor holds this
        // frame until the VSync at or after desired_present_ns.
        if self.fn_egl_presentation_time != 0 && desired_present_ns > 0 {
            let fn_pt: FnEglPresentationTimeANDROID =
                std::mem::transmute(self.fn_egl_presentation_time);
            let surface = eglGetCurrentSurface(EGL_DRAW);
            if !surface.is_null() {
                fn_pt(display, surface, desired_present_ns);
            }
        }

        // Step 5: cleanup. eglSwapBuffers (called by wgpu right after the hook returns)
        // handles GPU sync; no explicit gl.finish() needed here.
        gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, None);
        egl_destroy(display, egl_image);

        Ok(())
    }
}

unsafe fn compile_shader(
    gl: &glow::Context,
    shader_type: u32,
    source: &str,
) -> Result<glow::Shader, String> {
    let shader = gl.create_shader(shader_type).map_err(|e| e.to_string())?;
    gl.shader_source(shader, source);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        let log = gl.get_shader_info_log(shader);
        gl.delete_shader(shader);
        return Err(format!("shader compile error: {log}"));
    }
    Ok(shader)
}

unsafe fn link_program(
    gl: &glow::Context,
    vs: glow::Shader,
    fs: glow::Shader,
) -> Result<glow::Program, String> {
    let program = gl.create_program().map_err(|e| e.to_string())?;
    gl.attach_shader(program, vs);
    gl.attach_shader(program, fs);
    gl.bind_attrib_location(program, 0, "a_pos");
    gl.bind_attrib_location(program, 1, "a_tex");
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        let log = gl.get_program_info_log(program);
        gl.delete_program(program);
        return Err(format!("program link error: {log}"));
    }
    Ok(program)
}
