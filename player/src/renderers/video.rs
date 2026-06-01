#[cfg(any(target_os = "windows", target_os = "linux"))]
use ffmpeg_next::frame::Video;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex, RwLock,
};
#[cfg(any(target_os = "windows", target_os = "linux"))]
use video_frame::VideoFrame;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use video_metal::{MetalNV12Frame, MetalTextureCache};
use wgpu::{Backends, Buffer};
use wgpu::{Device, SurfaceConfiguration};

#[cfg(target_os = "windows")]
mod video_directx;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod video_frame;
#[cfg(target_os = "linux")]
mod video_vaapi;
// Vulkan import helper is shared by every backend that imports external GPU
// memory through VK_*_external_memory_*: Windows (D3D11 shared handle),
// Linux (DMA-BUF/VAAPI), and Android (AHardwareBuffer).
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "android"))]
mod video_vulkan;
#[cfg(target_os = "android")]
mod video_gles_egl;
#[cfg(target_os = "android")]
mod video_mediacodec;

/// Holds onto a GPU resource (AHB, VkDeviceMemory, …) for exactly long enough
/// that the previous frame's GPU work has flushed before MediaCodec is allowed
/// to recycle the source AHB. Each render path pushes its own concrete
/// keepalive into the per-renderer ring; the trait exists only so the ring
/// can store mixed types behind a single `Box<dyn ...>`.
#[cfg(target_os = "android")]
trait AndroidFrameKeepalive: Send + Sync {}

#[cfg(target_os = "android")]
impl AndroidFrameKeepalive for Arc<crate::decoders::SendableAhb> {}

/// Vulkan render path keepalive: holds the AHB ref AND the externally-imported
/// VkDeviceMemory that `create_vk_image_from_ahb` allocated. wgpu-hal destroys
/// the VkImage when our wgpu::Texture drops, but the imported memory is the
/// caller's responsibility — without an explicit `vkFreeMemory` here, Graphics
/// PSS climbs to multiple GB within a minute on a 24 fps stream.
#[cfg(target_os = "android")]
struct VulkanFrameKeepalive {
    _ahb: Arc<crate::decoders::SendableAhb>,
    device: wgpu::Device,
    memory: ash::vk::DeviceMemory,
}

#[cfg(target_os = "android")]
impl AndroidFrameKeepalive for VulkanFrameKeepalive {}

#[cfg(target_os = "android")]
impl Drop for VulkanFrameKeepalive {
    fn drop(&mut self) {
        use wgpu::hal::api::Vulkan;
        unsafe {
            if let Some(raw_dev) = self.device.as_hal::<Vulkan>() {
                raw_dev.raw_device().free_memory(self.memory, None);
            }
        }
    }
}
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod video_metal;

use std::sync::Arc;

use wgpu::MemoryHints;
use wgpu::RenderPipeline;
use wgpu::{Instance, InstanceDescriptor};

use wgpu::TextureFormat;
use wgpu::{util::DeviceExt, BindGroupLayout, Sampler};

use crate::PhysicalSize;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
// Raw-handle builders for the embedded Android path (host-provided ANativeWindow).
#[cfg(target_os = "android")]
use raw_window_handle::{AndroidDisplayHandle, AndroidNdkWindowHandle};

/// Hook invoked right before each present (desktop wires this to
/// `winit::window::Window::pre_present_notify`). The player crate stays
/// winit-free; embedded hosts leave it unset.
type PrePresentHook = Box<dyn Fn() + Send + Sync>;

/// Where the wgpu `Surface` is built from. Consumed synchronously at the top of
/// `new_with_surface` (before any await), so the raw pointer / handles never
/// cross an await point.
enum SurfaceSource {
    /// Desktop / any host with raw window+display handles (e.g. winit). Also
    /// the embedded Android path (an `ANativeWindow`-derived handle).
    RawHandle {
        window: RawWindowHandle,
        display: Option<RawDisplayHandle>,
    },
    /// Embedded Apple: a host-provided `CAMetalLayer*`.
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    MetalLayer(*mut std::ffi::c_void),
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 3],
    tex_coords: [f32; 2],
}

impl Vertex {
    fn desc() -> wgpu::VertexBufferLayout<'static> {
        use std::mem;
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    offset: mem::size_of::<[f32; 3]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
            ],
        }
    }
}

// tex_y_max: normally 1.0; set to content_height/buffer_height (<1.0) when
// the codec produces a taller buffer than the visible frame (e.g. 736 for 720p
// HEVC on Exynos) so we don't sample codec-alignment padding rows.
fn generate_verticles(scale_x: f32, scale_y: f32, tex_y_max: f32) -> [Vertex; 6] {
    [
        Vertex {
            position: [-1. * scale_x, -1. * scale_y, 0.0],
            tex_coords: [0., tex_y_max],
        }, // A
        Vertex {
            position: [1. * scale_x, -1. * scale_y, 0.0],
            tex_coords: [1., tex_y_max],
        }, // B
        Vertex {
            position: [-1. * scale_x, 1. * scale_y, 0.0],
            tex_coords: [0., 0.],
        }, // C
        Vertex {
            position: [-1. * scale_x, 1. * scale_y, 0.0],
            tex_coords: [0., 0.],
        }, // D
        Vertex {
            position: [1. * scale_x, -1. * scale_y, 0.0],
            tex_coords: [1., tex_y_max],
        }, // E
        Vertex {
            position: [1. * scale_x, 1. * scale_y, 0.0],
            tex_coords: [1., 0.],
        }, // F
    ]
}

fn select_preferred_format(
    available_formats: &[TextureFormat],
    preferred_formats: &[TextureFormat],
) -> Option<TextureFormat> {
    preferred_formats
        .iter()
        .find(|&&preferred| available_formats.contains(&preferred))
        .copied()
}

pub struct VideoRenderer {
    /// Last known drawable size (device pixels). There is no winit `Window` to
    /// query, so the host keeps this current via `Player::resize`.
    /// `std::sync::RwLock` (not tokio) so `inner_size()` stays a cheap
    /// synchronous read inside the async render path.
    surface_size: Arc<std::sync::RwLock<PhysicalSize<u32>>>,
    /// Optional pre-present hook (desktop wires it to winit's
    /// `pre_present_notify`). `std::sync::Mutex` because it's read inside the
    /// synchronous tail of the render path, just before `queue.present()`.
    pre_present: std::sync::Mutex<Option<PrePresentHook>>,
    device: wgpu::Device,
    // Used by the FFmpeg HW import path on Win/Linux to dispatch between
    // DX12 and Vulkan; the Apple Metal path doesn't need it.
    #[cfg_attr(any(target_os = "macos", target_os = "ios"), allow(dead_code))]
    backend: wgpu::Backend,
    queue: wgpu::Queue,
    /// Optional WebVTT subtitle overlay. Built lazily on the first
    /// `set_subtitle_font` call so the wgpu pipeline (which needs the
    /// surface format known at construction time) is created with the
    /// right target format. Drawn inside the desktop render pass after
    /// the main video quad. Android's GLES OES path doesn't call it
    /// yet — subtitle rendering there is a separate follow-up.
    subtitle_overlay: Arc<std::sync::Mutex<Option<Arc<super::subtitle::SubtitleOverlay>>>>,
    frame_size: Arc<RwLock<PhysicalSize<u32>>>,
    // Crop factor for the texture V-axis: content_height / buffer_height.
    // Always 1.0 on desktop; set to <1.0 on Android when the hardware codec
    // pads the output buffer taller than the visible frame (e.g. 736 for 720p).
    tex_y_max: Arc<RwLock<f32>>,
    surface: Arc<Mutex<wgpu::Surface<'static>>>,
    surface_format: TextureFormat,
    surface_config: Arc<RwLock<SurfaceConfiguration>>,
    sampler: Sampler,
    // None on devices where NV12 is unavailable (e.g. MT8696 Vulkan on Google
    // TV). Creating the NV12 shader/pipeline on those drivers triggers a crash
    // inside the driver's SPIR-V parser. The clear-color fallback path never
    // uses the pipeline, so skipping creation avoids the crash entirely.
    vertex_buffer: Option<Arc<RwLock<wgpu::Buffer>>>,
    texture_bind_group_layout: Option<BindGroupLayout>,
    render_pipeline: Option<RenderPipeline>,
    command_sender: Sender<VideoRendererCommand>,
    // GLES zero-copy OES renderer for devices without working Vulkan (e.g. Google TV MT8696).
    // Arc so the renderer can be shared with the present hook closure.
    #[cfg(target_os = "android")]
    gles_oes_renderer: Option<Arc<video_gles_egl::GlesOesRenderer>>,
    // Per-frame data written by render_android_gles() and consumed by the present hook.
    // std::sync::Mutex (not tokio) because the hook fires synchronously inside present().
    #[cfg(target_os = "android")]
    gles_oes_pending: Arc<std::sync::Mutex<Option<video_gles_egl::GlesOesPendingFrame>>>,
    // Ring buffer of recently-rendered AHardwareBuffer refs. eglSwapBuffers
    // returns before the GPU has finished sampling the OES texture; if we drop
    // the AHB immediately, MediaCodec recycles it for a new frame while the
    // GPU is still reading from it — the display shows torn / wrong-frame
    // content that looks like time-travel jumps. Keeping the last N AHBs
    // alive guarantees the GPU is done with each by the time it's dropped.
    //
    // On the Vulkan render path we also need to defer freeing the
    // externally-imported VkDeviceMemory for the same reason. Use a single
    // ring of trait-object keepalives so both paths share the depth-1
    // contract without each having to maintain its own queue.
    #[cfg(target_os = "android")]
    ahb_keepalive: Arc<std::sync::Mutex<std::collections::VecDeque<Box<dyn AndroidFrameKeepalive>>>>,
    /// Zero-copy CVPixelBuffer → MTLTexture cache. None when the wgpu backend
    /// isn't Metal (shouldn't happen on Apple platforms but the constructor
    /// is fallible).
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    metal_cache: Option<Arc<MetalTextureCache>>,
}

pub enum VideoRendererCommand {
    Resize(PhysicalSize<u32>),
    ChangeFrameSize(PhysicalSize<u32>),
}

impl VideoRenderer {
    /// Desktop / any host that can hand over raw window + display handles
    /// (e.g. winit). The player never touches winit; the host keeps the
    /// underlying window alive for the renderer's lifetime.
    pub async fn new_from_raw_handle(
        window_handle: RawWindowHandle,
        display_handle: RawDisplayHandle,
        width: u32,
        height: u32,
    ) -> Self {
        let size = PhysicalSize::new(width.max(1), height.max(1));
        Self::new_with_surface(
            size,
            SurfaceSource::RawHandle {
                window: window_handle,
                display: Some(display_handle),
            },
        )
        .await
    }

    /// Embedded Apple path: build the wgpu surface from a host-provided
    /// `CAMetalLayer*` (no winit, no `UIApplicationMain`). The host guarantees
    /// the layer outlives the renderer.
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub async fn new_from_metal_layer(
        layer: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> Self {
        let size = PhysicalSize::new(width.max(1), height.max(1));
        Self::new_with_surface(size, SurfaceSource::MetalLayer(layer)).await
    }

    /// Embedded Android path: build the wgpu surface from a host-provided
    /// `ANativeWindow*` (no winit `NativeActivity`). The host (JNI shim)
    /// acquires the window before this call and releases it after the renderer
    /// is dropped.
    #[cfg(target_os = "android")]
    pub async fn new_from_android_surface(
        native_window: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> Self {
        let size = PhysicalSize::new(width.max(1), height.max(1));
        let window = RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(
            std::ptr::NonNull::new(native_window).expect("null ANativeWindow"),
        ));
        let display = RawDisplayHandle::Android(AndroidDisplayHandle::new());
        Self::new_with_surface(
            size,
            SurfaceSource::RawHandle {
                window,
                display: Some(display),
            },
        )
        .await
    }

    /// Shared constructor body. Differs from the old `new` only in how the
    /// wgpu surface is created and where the initial size comes from — both are
    /// parameters now.
    async fn new_with_surface(size: PhysicalSize<u32>, surface_source: SurfaceSource) -> Self {
        #[cfg(target_os = "windows")]
        let backend = Backends::DX12;
        #[cfg(target_os = "linux")]
        let backend = Backends::VULKAN;
        // Android: request Vulkan first, fall back to GLES. wgpu's adapter
        // selection tries the backends in flag order and the first that
        // returns a working adapter wins — so capable devices (e.g. Mali-G78
        // on a S21) get the Vulkan zero-copy AHB-import path, while broken
        // drivers (Google TV MT8696 BILParseStream abort, emulator
        // vulkan.ranchu vkCreateInstance failure) silently slide onto GLES
        // and the GL_TEXTURE_EXTERNAL_OES path.
        #[cfg(target_os = "android")]
        let backend = Backends::VULKAN | Backends::GL;
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        let backend = Backends::METAL;

        let instance = Instance::new(InstanceDescriptor {
            backends: backend,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });

        let surface = match surface_source {
            SurfaceSource::RawHandle { window, display } => unsafe {
                instance
                    .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: display,
                        raw_window_handle: window,
                    })
                    .unwrap()
            },
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            SurfaceSource::MetalLayer(layer) => unsafe {
                instance
                    .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer))
                    .unwrap()
            },
        };

        #[cfg(target_os = "android")]
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .unwrap();

        #[cfg(not(target_os = "android"))]
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await
            .unwrap();

        let backend = adapter.get_info().backend;

        // NV12 / 16BIT_NORM are only used by the Vulkan AHB / D3D11 import
        // paths. GLES (Android emulator fallback) doesn't expose them and
        // request_device fails with UnsupportedFeature if we ask anyway.
        // Some Vulkan GPUs (e.g. PowerVR Rogue on Google TV) also don't expose
        // NV12, so intersect with what the adapter actually supports rather than
        // requesting blindly and unwrapping into a panic.
        // Windows/Linux use a single multi-planar NV12 wgpu texture (plane views
        // via aspect Plane0/Plane1). macOS Metal can't expose TEXTURE_FORMAT_NV12,
        // but the renderer still imports two separate Y+UV MTLTextures from
        // CVPixelBuffer planes, so it needs the pipeline created exactly like
        // the desktop NV12 path. `is_hw_backend` controls limits; the
        // pipeline-creation gate `has_nv12_feature` is widened to include Metal
        // below so the macOS path gets a working RenderPipeline.
        let is_hw_backend = backend == wgpu::Backend::Vulkan
            || backend == wgpu::Backend::Dx12
            || backend == wgpu::Backend::Metal;
        let required_features = if is_hw_backend && backend != wgpu::Backend::Metal {
            let desired = wgpu::Features::TEXTURE_FORMAT_NV12
                | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
            adapter.features() & desired
        } else {
            wgpu::Features::empty()
        };
        // Default limits require compute, which Android emulator GLES doesn't
        // expose; fall back to downlevel limits so request_device succeeds.
        // downlevel_defaults() still requires compute (ES 3.1); the
        // Android emulator only exposes ES 3.0. Start from the no-compute
        // WebGL2 baseline and lift max_texture_dimension_2d up to whatever
        // the adapter actually exposes so we can configure a surface
        // matching the device's screen resolution (1080×2400 etc.).
        let required_limits = if is_hw_backend {
            // Cap texture-dimension limits to what the adapter actually exposes.
            // wgpu::Limits::default() asks for max_texture_dimension_2d = 8192,
            // but some embedded Vulkan GPUs (e.g. PowerVR Rogue on Google TV)
            // only support 4096 and cause request_device to fail outright.
            //
            // The iOS Simulator's MoltenVK-backed GPU also caps
            // max_inter_stage_shader_variables at 15 (wgpu's default is 16),
            // so clamp every per-adapter scalar limit to the adapter's value
            // rather than asking blindly. We start from wgpu::Limits::default()
            // and floor each field to min(default, adapter), so request_device
            // never asks for more than the hardware promises.
            let adapter_limits = adapter.limits();
            let d = wgpu::Limits::default();
            wgpu::Limits {
                max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
                max_texture_dimension_1d: adapter_limits.max_texture_dimension_1d,
                max_inter_stage_shader_variables: d
                    .max_inter_stage_shader_variables
                    .min(adapter_limits.max_inter_stage_shader_variables),
                ..d
            }
        } else {
            let adapter_limits = adapter.limits();
            wgpu::Limits {
                max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
                max_texture_dimension_1d: adapter_limits.max_texture_dimension_1d,
                ..wgpu::Limits::downlevel_webgl2_defaults()
            }
        };

        // On Windows/Linux the pipeline only works when the adapter exposes
        // TEXTURE_FORMAT_NV12 (multi-planar texture). On macOS Metal the
        // pipeline binds two separate Y+UV textures (R8/RG8) imported from
        // CVPixelBuffer planes, so no NV12 feature is needed — but the
        // pipeline + vertex buffer still need to be created.
        let needs_pipeline = required_features.contains(wgpu::Features::TEXTURE_FORMAT_NV12)
            || backend == wgpu::Backend::Metal;
        let has_nv12_feature = needs_pipeline;
        log::info!(
            "[renderer] backend={:?} nv12={} adapter={}",
            backend,
            has_nv12_feature,
            adapter.get_info().name,
        );

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_features,
                required_limits,
                label: Some("Device with NV12 support"),
                memory_hints: MemoryHints::Performance,
                experimental_features: wgpu::ExperimentalFeatures::default(),
                trace: wgpu::Trace::Off,
            })
            .await
            .unwrap();

        let preferred_formats = vec![
            TextureFormat::Rgb10a2Unorm,
            TextureFormat::Rgba8Unorm,
            TextureFormat::Bgra8Unorm,
        ];

        let cap = surface.get_capabilities(&adapter);
        let preffered_sufrace_format = select_preferred_format(&cap.formats, &preferred_formats);

        let surface_format = match preffered_sufrace_format {
            Some(format) => format,
            None => cap.formats[0],
        };
        log::debug!("surface formats: {:?}", cap.formats);

        //let surface_format = cap.formats[4]; //.last().unwrap();

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            view_formats: vec![surface_format],
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        // Skip shader/pipeline/vertex-buffer creation when NV12 is unavailable.
        // On such devices (MT8696 Vulkan, GLES emulators) the driver crashes
        // inside its SPIR-V parser during vkCreateGraphicsPipeline. Since the
        // clear-color fallback path never touches these objects, omitting them
        // is safe and avoids the native abort.
        let (texture_bind_group_layout, render_pipeline, vertex_buffer) = if has_nv12_feature {
            let layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                    label: Some("texture_bind_group_layout"),
                });

            let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

            let pipeline_layout =
                device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("Pipeline Layout"),
                    bind_group_layouts: &[Some(&layout)],
                    immediate_size: 0,
                });

            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Render Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[Some(Vertex::desc())],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: surface_format,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

            let vertices = generate_verticles(1., 1., 1.);
            let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Vertex Buffer"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

            (Some(layout), Some(pipeline), Some(Arc::new(RwLock::new(vb))))
        } else {
            (None, None, None)
        };

        // Capacity sized for drag-resize bursts: Win32 generates many WM_SIZE
        // events per second while the user drags a window edge. The consumer
        // coalesces backlogged commands (see spawn_command_thread) so a deep
        // queue is fine — we just need send().await to never block.
        let (command_sender, command_receiver) = mpsc::channel(32);

        // On Android GLES path (no working Vulkan), build the OES renderer now while
        // the EGL context is available. We do this by accessing the hal device and
        // locking the EGL context to run the GL setup calls.
        #[cfg(target_os = "android")]
        let gles_oes_renderer = if backend == wgpu::Backend::Gl {
            unsafe {
                device.as_hal::<wgpu::hal::api::Gles>().and_then(|dev| {
                    let ctx = dev.context();
                    let gl = ctx.lock(); // makes EGL context current
                    match video_gles_egl::GlesOesRenderer::new(&gl) {
                        Ok(r) => Some(Arc::new(r)),
                        Err(e) => {
                            log::warn!("[renderer] GLES OES init failed: {}", e);
                            None
                        }
                    }
                })
            }
        } else {
            None
        };

        #[cfg(target_os = "android")]
        let gles_oes_pending = Arc::new(std::sync::Mutex::new(
            None::<video_gles_egl::GlesOesPendingFrame>,
        ));

        #[cfg(target_os = "android")]
        let ahb_keepalive = Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::<Box<dyn AndroidFrameKeepalive>>::with_capacity(4),
        ));

        // Install the present hook on the GLES surface so OES rendering happens
        // directly into FBO 0 (window surface) instead of into the swapchain renderbuffer.
        // The hook fires inside wgpu's present() after make_current(window_surface),
        // bypassing the PowerVR driver bug where rendering to sc.renderbuffer via a
        // PBuffer context silently produces no output.
        #[cfg(target_os = "android")]
        if backend == wgpu::Backend::Gl {
            if let Some(oes) = &gles_oes_renderer {
                let oes_arc = Arc::clone(oes);
                let pending_clone = Arc::clone(&gles_oes_pending);
                if let Some(s) = unsafe { surface.as_hal::<wgpu::hal::api::Gles>() } {
                    use std::ops::Deref;
                    let s_ref: &wgpu::hal::gles::Surface = s.deref();
                    s_ref.set_present_hook(Box::new(move |gl, w, h| {
                        let frame = pending_clone.lock().unwrap().take();
                        if let Some(f) = frame {
                            if let Err(e) = unsafe {
                                oes_arc.render(
                                    gl,
                                    f.ahb_ptr as *mut std::ffi::c_void,
                                    w as i32,
                                    h as i32,
                                    f.scale_x,
                                    f.scale_y,
                                    f.tex_y_max,
                                    f.desired_present_ns,
                                )
                            } {
                                log::warn!("[gles_oes] hook render failed: {}", e);
                            }
                        }
                    }));
                    log::info!("[gles_oes] present hook installed");
                }
            }
        }

        // Build the CVMetalTextureCache once per renderer so every frame's
        // import lookup hits the same cache (Apple caches the IOSurface →
        // MTLTexture binding internally, so per-frame creation would be
        // wasteful).
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let metal_cache = MetalTextureCache::new(&device);
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if metal_cache.is_none() {
            log::error!(
                "[renderer] CVMetalTextureCache init failed — zero-copy NV12 import will not work"
            );
        }

        let renderer = VideoRenderer {
            surface_size: Arc::new(std::sync::RwLock::new(size)),
            pre_present: std::sync::Mutex::new(None),
            device,
            backend,
            queue,
            frame_size: Arc::new(RwLock::new(size)),
            tex_y_max: Arc::new(RwLock::new(1.0_f32)),
            surface: Arc::new(Mutex::new(surface)),
            surface_format,
            surface_config: Arc::new(RwLock::new(surface_config)),
            sampler,
            vertex_buffer,
            texture_bind_group_layout,
            render_pipeline,
            command_sender,
            subtitle_overlay: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(target_os = "android")]
            gles_oes_renderer,
            #[cfg(target_os = "android")]
            gles_oes_pending,
            #[cfg(target_os = "android")]
            ahb_keepalive,
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            metal_cache,
        };

        renderer.spawn_command_thread(command_receiver);

        renderer
    }

    /// Last known drawable size (device pixels), kept current by the host via
    /// `resize`. Replaces the old `window.inner_size()`.
    #[inline]
    fn inner_size(&self) -> PhysicalSize<u32> {
        *self.surface_size.read().unwrap()
    }

    /// Invoke the host's pre-present hook if one is installed. No-op otherwise.
    #[inline]
    fn pre_present_notify(&self) {
        if let Some(f) = self.pre_present.lock().unwrap().as_ref() {
            f();
        }
    }

    /// Install the pre-present hook (see `Player::set_pre_present_hook`).
    pub fn set_pre_present_hook(&self, hook: PrePresentHook) {
        *self.pre_present.lock().unwrap() = Some(hook);
    }

    async fn change_vertex_buffer(
        device: &Device,
        window_size: PhysicalSize<u32>,
        frame_size: PhysicalSize<u32>,
        tex_y_max: f32,
        vertex_buffer: Arc<RwLock<Buffer>>,
    ) {
        let window_aspect = window_size.width as f32 / window_size.height as f32;

        let texture_aspect = frame_size.width as f32 / frame_size.height as f32;
        let (scale_x, scale_y) = if texture_aspect > window_aspect {
            (1.0, window_aspect / texture_aspect)
        } else {
            (texture_aspect / window_aspect, 1.0)
        };

        let vertices = generate_verticles(scale_x, scale_y, tex_y_max);
        let mut vertex_buffer_guard = vertex_buffer.write().await;
        vertex_buffer_guard.destroy();

        *vertex_buffer_guard = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
    }

    fn spawn_command_thread(&self, mut command_receiver: Receiver<VideoRendererCommand>) {
        let surface = Arc::clone(&self.surface);
        let config = Arc::clone(&self.surface_config);
        let frame_size = Arc::clone(&self.frame_size);
        let tex_y_max = Arc::clone(&self.tex_y_max);
        let vertex_buffer = self.vertex_buffer.clone(); // Option<Arc<...>>
        let device = self.device.clone();
        let surface_size = Arc::clone(&self.surface_size);
        tokio::spawn(async move {
            while let Some(initial) = command_receiver.recv().await {
                // Drain the channel and keep only the latest of each command
                // kind. During a drag-resize the producer floods us with
                // Resize events; every surface.configure() rebuilds the
                // DX12/Vulkan swapchain (10–20 ms on Windows), so processing
                // every intermediate size makes the window lag by hundreds of
                // ms. Only the final size is observable to the user.
                let mut latest_resize = None;
                let mut latest_frame_size = None;
                match initial {
                    VideoRendererCommand::Resize(s) => latest_resize = Some(s),
                    VideoRendererCommand::ChangeFrameSize(s) => latest_frame_size = Some(s),
                }
                while let Ok(next) = command_receiver.try_recv() {
                    match next {
                        VideoRendererCommand::Resize(s) => latest_resize = Some(s),
                        VideoRendererCommand::ChangeFrameSize(s) => latest_frame_size = Some(s),
                    }
                }

                // ChangeFrameSize first so frame_size reflects the new
                // content aspect before the Resize branch rebuilds the
                // vertex buffer for the latest window size.
                if let Some(new_frame_size) = latest_frame_size {
                    {
                        let mut size = frame_size.write().await;
                        *size = new_frame_size;
                    }
                    let window_size = *surface_size.read().unwrap();
                    if window_size.width > 0 && window_size.height > 0 {
                        // Release the config write lock before taking surface.lock() —
                        // the render path takes surface first, then config(read), so
                        // holding config(write) across surface.lock() would AB-BA
                        // deadlock with an in-flight render.
                        let new_config = {
                            let mut config_guard = config.write().await;
                            config_guard.width = window_size.width;
                            config_guard.height = window_size.height;
                            config_guard.clone()
                        };
                        surface.lock().await.configure(&device, &new_config);
                        if let Some(vb) = &vertex_buffer {
                            let ty_max = *tex_y_max.read().await;
                            Self::change_vertex_buffer(
                                &device,
                                window_size,
                                new_frame_size,
                                ty_max,
                                vb.clone(),
                            )
                            .await;
                        }
                    }
                }

                if let Some(new_size) = latest_resize {
                    if new_size.width > 0 && new_size.height > 0 {
                        log::info!("[renderer] resize {}x{}", new_size.width, new_size.height);
                        // Keep the size cell current so later inner_size() reads
                        // (render path, ChangeFrameSize) see the new layout.
                        *surface_size.write().unwrap() = new_size;
                        let new_config = {
                            let mut config_guard = config.write().await;
                            config_guard.width = new_size.width;
                            config_guard.height = new_size.height;
                            config_guard.clone()
                        };
                        surface.lock().await.configure(&device, &new_config);
                        if let Some(vb) = &vertex_buffer {
                            let frame_size = frame_size.read().await;
                            let ty_max = *tex_y_max.read().await;
                            Self::change_vertex_buffer(
                                &device,
                                new_size,
                                *frame_size,
                                ty_max,
                                vb.clone(),
                            )
                            .await;
                        }
                    }
                }
            }
        });
    }

    pub async fn resize(&self, new_size: PhysicalSize<u32>) {
        _ = self
            .command_sender
            .send(VideoRendererCommand::Resize(new_size))
            .await;
    }

    pub async fn change_frame_size(&self, new_size: PhysicalSize<u32>) {
        _ = self
            .command_sender
            .send(VideoRendererCommand::ChangeFrameSize(new_size))
            .await;
    }

    /// Unified render entry point used by the generic play loop.
    /// Dispatches to the platform-specific render path based on the PlatformFrame variant.
    pub async fn render_frame(&self, frame: crate::decoders::DecodedVideoFrame) {
        use crate::decoders::PlatformFrame;
        // NB: subtitle PTS is updated separately via set_subtitle_pts
        // by the video sync loop using the BMDT-adjusted timeline.
        // The raw frame.pts_us here would be off by the segment-base
        // offset (commonly several seconds in real DASH streams).
        #[cfg(target_os = "android")]
        let desired_present_ns = frame.desired_present_ns;
        match frame.native {
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            PlatformFrame::FfmpegVideo(ffmpeg_frame) => {
                self.render(ffmpeg_frame).await;
            }
            #[cfg(target_os = "android")]
            PlatformFrame::HardwareBuffer(ahb) => {
                self.render_android(ahb, desired_present_ns).await;
            }
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            PlatformFrame::CvPixelBuffer(cv_buf) => {
                self.render_cv_pixel_buffer(cv_buf).await;
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }

    /// Windows / Linux FFmpeg → wgpu native-import draw path. macOS / iOS
    /// route through `render_cv_pixel_buffer` instead (zero-copy
    /// CVPixelBuffer → MTLTexture).
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    pub async fn render(&self, frame: Arc<Video>) {
        let video_frame = VideoFrame::new(self.device.clone(), self.backend, frame.clone());

        let (y_plane_view, uv_plane_view) = {
            let texture = video_frame.get_texture();
            let y = match texture.format() {
                TextureFormat::P010 => texture.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(wgpu::TextureFormat::R16Unorm),
                    aspect: wgpu::TextureAspect::Plane0,
                    ..Default::default()
                }),
                TextureFormat::NV12 => texture.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(wgpu::TextureFormat::R8Unorm),
                    aspect: wgpu::TextureAspect::Plane0,
                    ..Default::default()
                }),
                _ => panic!("Not supported"),
            };
            let uv = match texture.format() {
                TextureFormat::P010 => texture.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(wgpu::TextureFormat::Rg16Unorm),
                    aspect: wgpu::TextureAspect::Plane1,
                    ..Default::default()
                }),
                TextureFormat::NV12 => texture.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(wgpu::TextureFormat::Rg8Unorm),
                    aspect: wgpu::TextureAspect::Plane1,
                    ..Default::default()
                }),
                _ => panic!("Not supported"),
            };
            (y, uv)
        };

        // Desktop path always has NV12 feature, so pipeline/buffer are always Some.
        let layout = self.texture_bind_group_layout.as_ref().expect("no bind group layout");
        let texture_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_plane_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_plane_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
            label: Some("texture_bind_group"),
        });

        {
            let surface = self.surface.lock().await;
            let vb_arc = self.vertex_buffer.as_ref().expect("no vertex buffer");
            let vertex_buffer = vb_arc.read().await;
            let render_pipeline = self.render_pipeline.as_ref().expect("no render pipeline");

            let surface_texture = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t) => t,
                wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                    // Suboptimal: surface is valid but swapchain config no longer
                    // perfectly matches (e.g. display transform changed). Render
                    // to it anyway to avoid dropping the frame; queue a resize so
                    // the config is corrected before the next frame.
                    log::warn!("[renderer] suboptimal surface — queuing resize, rendering anyway");
                    let _ = self.command_sender.try_send(
                        VideoRendererCommand::Resize(self.inner_size()),
                    );
                    t
                }
                other => {
                    log::warn!("surface texture not available: {:?}", other);
                    return;
                }
            };

            let texture_view = surface_texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor {
                    format: Some(self.surface_format),
                    ..Default::default()
                });

            // Resolve overlay + surface dims BEFORE begin_render_pass so
            // no async/mutex hazard happens inside the render-pass scope.
            let overlay_snapshot = self
                .subtitle_overlay
                .lock()
                .unwrap()
                .clone();
            let (surface_w, surface_h) = {
                let cfg = self.surface_config.read().await;
                (cfg.width, cfg.height)
            };

            let mut encoder = self.device.create_command_encoder(&Default::default());
            {
                let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &texture_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });

                render_pass.set_pipeline(render_pipeline);
                render_pass.set_bind_group(0, &texture_bind_group, &[]);
                render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                render_pass.draw(0..6, 0..1);

                if let Some(overlay) = overlay_snapshot {
                    overlay.draw_into(&mut render_pass, surface_w, surface_h);
                }
            }

            // Submit the command in the queue to execute
            self.queue.submit([encoder.finish()]);
            self.pre_present_notify();
            self.queue.present(surface_texture);
        }
    }

    /// Shared Apple Metal render path: draws an NV12 frame from two
    /// single-plane textures (Y = R8Unorm, UV = Rg8Unorm). Takes ownership
    /// of `metal_frame` and drops it after queue.submit — that's when the
    /// wgpu::Textures' MTLTexture retains are the only thing keeping the
    /// IOSurface alive through the in-flight GPU pass.
    ///
    /// MetalNV12Frame is `Send` but not `Sync` (raw CFTypeRef), so we
    /// take it by value instead of `&` to keep `render_frame`'s
    /// returned future `Send` across the await.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    async fn render_metal_nv12(&self, metal_frame: MetalNV12Frame) {
        let y_plane_view = metal_frame.y_texture.create_view(&Default::default());
        let uv_plane_view = metal_frame.uv_texture.create_view(&Default::default());

        let layout = self.texture_bind_group_layout.as_ref().expect("no bind group layout");
        let texture_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_plane_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_plane_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
            label: Some("metal_nv12_bind_group"),
        });

        let surface = self.surface.lock().await;
        let vb_arc = self.vertex_buffer.as_ref().expect("no vertex buffer");
        let vertex_buffer = vb_arc.read().await;
        let render_pipeline = self.render_pipeline.as_ref().expect("no render pipeline");

        let surface_texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                log::warn!("[renderer] suboptimal surface — queuing resize, rendering anyway");
                let _ = self
                    .command_sender
                    .try_send(VideoRendererCommand::Resize(self.inner_size()));
                t
            }
            other => {
                log::warn!("surface texture not available: {:?}", other);
                return;
            }
        };

        let texture_view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.surface_format),
            ..Default::default()
        });

        let overlay_snapshot = self.subtitle_overlay.lock().unwrap().clone();
        let (surface_w, surface_h) = {
            let cfg = self.surface_config.read().await;
            (cfg.width, cfg.height)
        };

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            render_pass.set_pipeline(render_pipeline);
            render_pass.set_bind_group(0, &texture_bind_group, &[]);
            render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            render_pass.draw(0..6, 0..1);

            if let Some(overlay) = overlay_snapshot {
                overlay.draw_into(&mut render_pass, surface_w, surface_h);
            }
        }

        self.queue.submit([encoder.finish()]);
        self.pre_present_notify();
        self.queue.present(surface_texture);
    }

    /// macOS / iOS: render a `CVPixelBufferOwned` from VTDecompressionSession.
    /// Wraps the buffer into a `MetalNV12Frame` (two zero-copy MTLTextures)
    /// and dispatches to the shared Metal NV12 helper.
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub async fn render_cv_pixel_buffer(&self, buf: crate::decoders::CvPixelBufferOwned) {
        let cache = match &self.metal_cache {
            Some(c) => c.clone(),
            None => {
                log::warn!("[renderer] metal_cache missing, dropping frame");
                return;
            }
        };
        let mf = match unsafe { MetalNV12Frame::new(&cache, &self.device, buf.as_ptr()) } {
            Ok(f) => f,
            Err(e) => {
                log::warn!("[renderer] MetalNV12Frame::new (iOS) failed: {}", e);
                return;
            }
        };
        // The wgpu::Textures inside `mf` hold a +1 retain on the MTLTexture,
        // so dropping `buf` early (the +1 from VTDecompressionSession) is
        // fine — Metal still has a live reference until queue.submit's
        // command buffer completes.
        drop(buf);
        self.render_metal_nv12(mf).await;
    }

    /// GLES zero-copy path: stores per-frame AHB data then calls queue.present().
    /// The present hook (installed once during VideoRenderer::new()) fires inside
    /// wgpu's present() after make_current(window_surface), draws the OES quad
    /// directly to FBO 0, then eglSwapBuffers presents it.
    /// Falls back to a blue-screen clear when the OES renderer is unavailable.
    #[cfg(target_os = "android")]
    async fn render_android_gles(&self, frame: crate::decoders::AndroidHardwareBufferFrame, desired_present_ns: i64) {
        // Update tex_y_max when the codec buffer is taller than the visible content
        // (e.g. PowerVR alignment padding: 1088-row buffer for 720p content).
        // Only fire when buffer > content (stored.height < frame.height): if stored.height
        // is still the window height (1080) and frame.height is content height (720),
        // the condition would be inverted and produce tex_y_max > 1.0 which corrupts output.
        {
            let stored = self.frame_size.read().await;
            if stored.height > 0 && frame.height > 0 && stored.height < frame.height {
                let new_ty = stored.height as f32 / frame.height as f32;
                let mut ty = self.tex_y_max.write().await;
                if (*ty - new_ty).abs() > 0.001 {
                    log::info!(
                        "[gles_oes] codec padding: content={}px buffer={}px tex_y_max={:.4}",
                        stored.height,
                        frame.height,
                        new_ty
                    );
                    *ty = new_ty;
                }
            }
        }

        let surface = self.surface.lock().await;
        let surface_texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => {
                log::warn!("[gles_oes] surface not available: {:?}", other);
                return;
            }
        };

        // OES renderer absent → fallback blue clear (OES init failed at startup).
        let Some(_oes) = &self.gles_oes_renderer else {
            static WARN_ONCE: std::sync::Once = std::sync::Once::new();
            WARN_ONCE.call_once(|| {
                log::warn!(
                    "[gles_oes] OES renderer unavailable (backend={:?}) — blue clear fallback",
                    self.backend
                );
            });
            let view = surface_texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor {
                    format: Some(self.surface_format),
                    ..Default::default()
                });
            let mut encoder = self.device.create_command_encoder(&Default::default());
            {
                let _rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("gles-fallback-clear"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.1,
                                g: 0.2,
                                b: 0.4,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
            self.queue.submit([encoder.finish()]);
            self.pre_present_notify();
            self.queue.present(surface_texture);
            return;
        };

        // Compute aspect-ratio-preserving scale factors.
        let window_size = self.inner_size();
        let frame_size = *self.frame_size.read().await;
        let tex_y_max = *self.tex_y_max.read().await;
        let (scale_x, scale_y) = if frame_size.width > 0 && frame_size.height > 0 {
            let wa = window_size.width as f32 / window_size.height as f32;
            let fa = frame_size.width as f32 / frame_size.height as f32;
            if fa > wa {
                (1.0_f32, wa / fa)
            } else {
                (fa / wa, 1.0_f32)
            }
        } else {
            (1.0_f32, 1.0_f32)
        };

        // Publish frame data for the present hook to consume.
        {
            let mut pending = self.gles_oes_pending.lock().unwrap();
            *pending = Some(video_gles_egl::GlesOesPendingFrame {
                ahb_ptr: frame.buffer.as_ptr() as usize,
                scale_x,
                scale_y,
                tex_y_max,
                desired_present_ns,
            });
        }

        // Submit empty wgpu work to keep the frame-tracking state machine consistent,
        // then present — the hook fires inside present() and draws the OES quad.
        self.queue.submit([]);
        self.pre_present_notify();
        self.queue.present(surface_texture);

        // Keep this AHB alive for one extra frame. The GPU may still be sampling
        // from the OES texture after eglSwapBuffers returns; if we drop the AHB
        // immediately, MediaCodec recycles it while the GPU reads it and the
        // display shows torn content. By the time the *next* render arrives,
        // wgpu has flushed the previous frame's GPU work, so dropping it then
        // is safe. Holding only 1 (not more) avoids starving MediaCodec's
        // 32-slot ImageReader pool at segment boundaries.
        {
            let mut keep = self.ahb_keepalive.lock().unwrap();
            keep.push_back(Box::new(Arc::clone(&frame.buffer)));
            while keep.len() > 1 {
                keep.pop_front();
            }
        }
    }

    #[cfg(target_os = "android")]
    pub async fn render_android(&self, frame: crate::decoders::AndroidHardwareBufferFrame, desired_present_ns: i64) {
        if self.backend == wgpu::Backend::Vulkan {
            self.render_android_vulkan(frame, desired_present_ns).await;
        } else {
            self.render_android_gles(frame, desired_present_ns).await;
        }
    }

    /// Vulkan zero-copy AHB path: imports the AHB into a Vulkan VkImage via
    /// VK_ANDROID_external_memory_android_hardware_buffer, wraps it as a
    /// wgpu NV12 texture, and draws it through the shared NV12 pipeline that
    /// the desktop VAAPI path also uses. `desired_present_ns` is currently
    /// unused on this path — Vulkan's analog is VK_GOOGLE_display_timing,
    /// which would need wiring through the queue's vkQueuePresentKHR call
    /// chain. Without it, the compositor schedules each frame at the next
    /// vsync after present() returns. The GLES path's eglPresentationTimeANDROID
    /// hook is the only place we currently steer compositor timing.
    #[cfg(target_os = "android")]
    async fn render_android_vulkan(
        &self,
        frame: crate::decoders::AndroidHardwareBufferFrame,
        _desired_present_ns: i64,
    ) {
        use ndk::hardware_buffer::HardwareBuffer;
        use video_mediacodec::create_vk_image_from_ahb;
        use video_vulkan::create_texture_from_vk_image;

        // Without the NV12 pipeline + bind-group layout the rest of this path
        // can't run — fall back to a blue clear so the swap chain keeps
        // ticking and we surface a clear visual signal that the renderer is
        // alive but un-pipelined.
        let (Some(bgl), Some(pipeline), Some(vbuf)) = (
            self.texture_bind_group_layout.as_ref(),
            self.render_pipeline.as_ref(),
            self.vertex_buffer.as_ref(),
        ) else {
            static WARN_ONCE: std::sync::Once = std::sync::Once::new();
            WARN_ONCE.call_once(|| {
                log::warn!(
                    "[android_vk] NV12 pipeline unavailable (adapter didn't expose TEXTURE_FORMAT_NV12) — blue clear"
                );
            });
            let surface = self.surface.lock().await;
            let surface_texture = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
                other => {
                    log::warn!("[android_vk] surface not available: {:?}", other);
                    return;
                }
            };
            let view = surface_texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor {
                    format: Some(self.surface_format),
                    ..Default::default()
                });
            let mut encoder = self.device.create_command_encoder(&Default::default());
            {
                let _rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("android-vk-fallback-clear"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.1,
                                g: 0.2,
                                b: 0.4,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
            self.queue.submit([encoder.finish()]);
            self.pre_present_notify();
            self.queue.present(surface_texture);
            return;
        };

        // Tight scope on the unowned HardwareBuffer view: it wraps a !Send
        // pointer, so we MUST drop it before the next .await or the
        // surrounding future loses its Send bound and the type-checker
        // refuses to compile this as an async fn. The owned `frame.buffer`
        // keeps the AHB alive across imports.
        let img_mem = {
            let hb_view = unsafe {
                HardwareBuffer::from_ptr(
                    std::ptr::NonNull::new(frame.buffer.as_ptr()).unwrap(),
                )
            };
            match create_vk_image_from_ahb(&self.device, &hb_view, frame.width, frame.height) {
                Ok(im) => im,
                Err(e) => {
                    log::warn!("[android_vk] AHB import failed: {}", e);
                    return;
                }
            }
        };

        // `drop=true` hands VkImage destruction off to wgpu-hal when the wgpu
        // texture below drops. The matching VkDeviceMemory is NOT released by
        // hal — externally-imported memory has to be freed by the caller —
        // so we stash it in `ahb_keepalive` alongside the AHB so its Drop
        // impl can call vkFreeMemory after the GPU is done.
        let raw_image = img_mem.raw_image;
        let raw_memory = img_mem.memory;
        let texture = create_texture_from_vk_image(
            &self.device,
            raw_image,
            frame.width,
            frame.height,
            TextureFormat::NV12,
            true,
            true,
        );

        let y_view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(TextureFormat::R8Unorm),
            aspect: wgpu::TextureAspect::Plane0,
            ..Default::default()
        });
        let uv_view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(TextureFormat::Rg8Unorm),
            aspect: wgpu::TextureAspect::Plane1,
            ..Default::default()
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("android_vk_bind_group"),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let surface = self.surface.lock().await;
        let vbuf_read = vbuf.read().await;
        let surface_texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                let _ = self.command_sender.try_send(VideoRendererCommand::Resize(
                    *self.surface_size.read().unwrap(),
                ));
                return;
            }
            other => {
                log::warn!("[android_vk] surface not available: {:?}", other);
                return;
            }
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.surface_format),
                ..Default::default()
            });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("android_vk_nv12"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(pipeline);
            rp.set_bind_group(0, &bind_group, &[]);
            rp.set_vertex_buffer(0, vbuf_read.slice(..));
            rp.draw(0..6, 0..1);
        }
        self.queue.submit([encoder.finish()]);
        self.pre_present_notify();
        self.queue.present(surface_texture);

        // Same one-frame keepalive contract as the GLES path, plus the
        // VkDeviceMemory cleanup the GLES path doesn't need: AHB stays
        // referenced until the GPU is done sampling, then VulkanFrameKeepalive's
        // Drop calls vkFreeMemory on the imported memory we allocated above.
        {
            let mut keep = self.ahb_keepalive.lock().unwrap();
            keep.push_back(Box::new(VulkanFrameKeepalive {
                _ahb: Arc::clone(&frame.buffer),
                device: self.device.clone(),
                memory: raw_memory,
            }));
            while keep.len() > 1 {
                keep.pop_front();
            }
        }
    }
}

impl VideoRenderer {
    fn ensure_subtitle_overlay(&self) -> Arc<super::subtitle::SubtitleOverlay> {
        let mut slot = self.subtitle_overlay.lock().unwrap();
        if let Some(ov) = slot.as_ref() {
            return ov.clone();
        }
        // Lazy first construction. Device + queue are clones of the
        // same underlying wgpu objects we use for video rendering.
        let overlay = Arc::new(super::subtitle::SubtitleOverlay::new(
            Arc::new(self.device.clone()),
            Arc::new(self.queue.clone()),
            self.surface_format,
        ));
        *slot = Some(overlay.clone());
        overlay
    }
}

impl super::VideoSink for VideoRenderer {
    fn render_frame(&self, frame: crate::decoders::DecodedVideoFrame) -> impl std::future::Future<Output = ()> + Send + '_ {
        VideoRenderer::render_frame(self, frame)
    }

    fn resize(&self, size: PhysicalSize<u32>) -> impl std::future::Future<Output = ()> + Send + '_ {
        VideoRenderer::resize(self, size)
    }

    fn change_frame_size(&self, size: PhysicalSize<u32>) -> impl std::future::Future<Output = ()> + Send + '_ {
        VideoRenderer::change_frame_size(self, size)
    }

    fn set_subtitle_font(&self, bytes: Vec<u8>) -> Result<(), String> {
        let overlay = self.ensure_subtitle_overlay();
        overlay.set_font(bytes)
    }

    fn queue_subtitle_cues(&self, cues: Vec<crate::parsers::vtt::VttCue>) {
        if cues.is_empty() {
            return;
        }
        let overlay = self.ensure_subtitle_overlay();
        overlay.queue_cues(cues);
    }

    fn clear_subtitles(&self) {
        if let Some(ov) = self.subtitle_overlay.lock().unwrap().as_ref() {
            ov.clear();
        }
    }

    fn set_subtitle_pts(&self, pts_ms: i64) {
        if let Some(ov) = self.subtitle_overlay.lock().unwrap().clone() {
            ov.set_pts_ms(pts_ms);
        }
    }
}
