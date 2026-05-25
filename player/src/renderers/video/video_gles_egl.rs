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
uniform float u_tex_y_max;
out vec2 v_tex;
void main() {
    gl_Position = vec4(a_pos.x * u_scale_x, a_pos.y * u_scale_y, 0.0, 1.0);
    v_tex = vec2(a_tex.x, a_tex.y * u_tex_y_max);
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

// libEGL.so is always present on Android.
// eglGetProcAddress loads EGL/GL extension entry points at runtime.
// eglGetCurrentDisplay returns the EGLDisplay for whichever context is current.
#[link(name = "EGL")]
extern "C" {
    fn eglGetProcAddress(procname: *const i8) -> Option<unsafe extern "system" fn()>;
    fn eglGetCurrentDisplay() -> *mut c_void;
    fn eglGetError() -> u32;
}

/// Frame data passed from the render thread to the present hook.
/// Stored as primitive types so the struct is Send (raw pointers are stored as usize).
pub struct GlesOesPendingFrame {
    pub ahb_ptr: usize,  // *mut AHardwareBuffer cast to usize
    pub scale_x: f32,
    pub scale_y: f32,
    pub tex_y_max: f32,
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
    tex_y_max_loc: Option<glow::UniformLocation>,
    // EGL extension function pointers stored as usize (makes the struct Send + Sync).
    fn_get_native_client_buffer: usize, // eglGetNativeClientBufferANDROID
    fn_egl_create_image: usize,         // eglCreateImageKHR
    fn_egl_destroy_image: usize,        // eglDestroyImageKHR
    fn_gl_egl_image_target_texture: usize, // glEGLImageTargetTexture2DOES
    // Raw EGLDisplay pointer captured at init time.
    egl_display: usize,
}

unsafe impl Send for GlesOesRenderer {}
unsafe impl Sync for GlesOesRenderer {}

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

        // Capture the EGLDisplay while the context is current.
        let display = eglGetCurrentDisplay() as usize;
        if display == 0 {
            return Err("eglGetCurrentDisplay returned EGL_NO_DISPLAY".to_string());
        }

        log::info!(
            "[gles_oes] init: eglGetNativeClientBufferANDROID={:#x} \
             eglCreateImageKHR={:#x} display={:#x}",
            fn_get_client,
            fn_create,
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
        let tex_y_max_loc = gl.get_uniform_location(program, "u_tex_y_max");
        gl.use_program(None);

        log::info!("[gles_oes] shader compiled, uniforms: scale_x={} scale_y={} tex_y_max={}",
            scale_x_loc.is_some(), scale_y_loc.is_some(), tex_y_max_loc.is_some());

        Ok(GlesOesRenderer {
            program,
            vao,
            _vbo: vbo,
            oes_texture,
            scale_x_loc,
            scale_y_loc,
            tex_y_max_loc,
            fn_get_native_client_buffer: fn_get_client,
            fn_egl_create_image: fn_create,
            fn_egl_destroy_image: fn_destroy,
            fn_gl_egl_image_target_texture: fn_target,
            egl_display: display,
        })
    }

    /// Render one AHardwareBuffer frame to the currently-bound DRAW framebuffer.
    ///
    /// Called from the wgpu present hook after `make_current(window_surface)` and
    /// `glBindFramebuffer(DRAW_FRAMEBUFFER, 0)` — so FBO 0 (the EGL window surface)
    /// is the render target. eglSwapBuffers follows immediately after this returns.
    pub unsafe fn render(
        &self,
        gl: &glow::Context,
        ahb_ptr: *mut c_void,        // *mut AHardwareBuffer
        viewport_width: i32,
        viewport_height: i32,
        scale_x: f32,
        scale_y: f32,
        tex_y_max: f32,
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

        gl.viewport(0, 0, viewport_width, viewport_height);
        gl.use_program(Some(self.program));
        if let Some(ref loc) = self.scale_x_loc {
            gl.uniform_1_f32(Some(loc), scale_x);
        }
        if let Some(ref loc) = self.scale_y_loc {
            gl.uniform_1_f32(Some(loc), scale_y);
        }
        if let Some(ref loc) = self.tex_y_max_loc {
            gl.uniform_1_f32(Some(loc), tex_y_max);
        }
        gl.bind_vertex_array(Some(self.vao));
        gl.draw_arrays(glow::TRIANGLES, 0, 6);
        gl.bind_vertex_array(None);
        gl.use_program(None);

        let gl_err = gl.get_error();
        if gl_err != glow::NO_ERROR {
            log::warn!("[gles_oes] GL error after draw_arrays: {:#x}", gl_err);
        }

        // Step 4: cleanup. eglSwapBuffers (called by wgpu right after the hook returns)
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
