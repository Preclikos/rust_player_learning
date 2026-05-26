#[cfg(any(target_os = "windows", target_os = "linux"))]
use ffmpeg_next::frame::Video;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex, RwLock,
};
#[cfg(any(target_os = "windows", target_os = "linux"))]
use video_frame::VideoFrame;
use wgpu::{Backends, Buffer};
use wgpu::{Device, SurfaceConfiguration};
use winit::dpi::PhysicalSize;

#[cfg(target_os = "windows")]
mod video_directx;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod video_frame;
#[cfg(target_os = "linux")]
mod video_vaapi;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod video_vulkan;
#[cfg(target_os = "android")]
mod video_gles_egl;

use std::sync::Arc;

use wgpu::MemoryHints;
use wgpu::RenderPipeline;
use wgpu::{Instance, InstanceDescriptor};

use wgpu::TextureFormat;
use wgpu::{util::DeviceExt, BindGroupLayout, Sampler};
use winit::window::Window;

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
    window: Arc<Window>,
    device: wgpu::Device,
    backend: wgpu::Backend,
    // True only when the adapter exposed TEXTURE_FORMAT_NV12 (required for AHB
    // import on Android). PowerVR Rogue on Google TV does not; falls back to
    // blue-screen clear so the renderer survives without crashing.
    has_nv12_feature: bool,
    queue: wgpu::Queue,
    /// Optional WebVTT subtitle overlay. Built lazily on the first
    /// `set_subtitle_font` call so the wgpu pipeline (which needs the
    /// surface format known at construction time) is created with the
    /// right target format. Drawn inside the desktop render pass after
    /// the main video quad. Android's GLES OES path doesn't call it
    /// yet — subtitle rendering there is a separate follow-up.
    subtitle_overlay: Arc<std::sync::Mutex<Option<Arc<super::subtitle::SubtitleOverlay>>>>,
    frame_size: Arc<RwLock<winit::dpi::PhysicalSize<u32>>>,
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
    #[cfg(target_os = "android")]
    ahb_keepalive: Arc<std::sync::Mutex<std::collections::VecDeque<Arc<crate::decoders::SendableAhb>>>>,
}

pub enum VideoRendererCommand {
    Resize(PhysicalSize<u32>),
    ChangeFrameSize(PhysicalSize<u32>),
}

impl VideoRenderer {
    pub async fn new(window: Arc<Window>) -> Self {
        #[cfg(target_os = "windows")]
        let backend = Backends::DX12;
        #[cfg(target_os = "linux")]
        let backend = Backends::VULKAN;
        // Android: always use GLES. The AHB zero-copy path works directly with
        // GL_TEXTURE_EXTERNAL_OES — no Vulkan AHB import needed. This avoids
        // device-specific Vulkan driver bugs (MT8696 abort in BILParseStream,
        // emulator vkCreateInstance failures) and follows the recommended path.
        #[cfg(target_os = "android")]
        let backend = Backends::GL;
        #[cfg(target_os = "ios")]
        let backend = Backends::METAL;

        let instance = Instance::new(InstanceDescriptor {
            backends: backend,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });

        let surface = instance.create_surface(window.clone()).unwrap();

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
        let is_hw_backend = backend == wgpu::Backend::Vulkan || backend == wgpu::Backend::Dx12;
        let required_features = if is_hw_backend {
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
            let adapter_limits = adapter.limits();
            wgpu::Limits {
                max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
                max_texture_dimension_1d: adapter_limits.max_texture_dimension_1d,
                ..wgpu::Limits::default()
            }
        } else {
            let adapter_limits = adapter.limits();
            wgpu::Limits {
                max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
                max_texture_dimension_1d: adapter_limits.max_texture_dimension_1d,
                ..wgpu::Limits::downlevel_webgl2_defaults()
            }
        };

        let has_nv12_feature = required_features.contains(wgpu::Features::TEXTURE_FORMAT_NV12);
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

        let size: PhysicalSize<u32> = window.inner_size();

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

        let (command_sender, command_receiver) = mpsc::channel(4);

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
            std::collections::VecDeque::<Arc<crate::decoders::SendableAhb>>::with_capacity(4),
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

        let renderer = VideoRenderer {
            window,
            device,
            backend,
            has_nv12_feature,
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
        };

        renderer.spawn_command_thread(command_receiver);

        renderer
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
        let window = self.window.clone();
        tokio::spawn(async move {
            while let Some(command) = command_receiver.recv().await {
                match command {
                    VideoRendererCommand::ChangeFrameSize(new_frame_size) => {
                        {
                            let mut size = frame_size.write().await;
                            *size = new_frame_size;
                        }
                        let window_size = window.inner_size();
                        if window_size.width > 0 && window_size.height > 0 {
                            let mut config_guard = config.write().await;
                            config_guard.width = window_size.width;
                            config_guard.height = window_size.height;
                            {
                                surface.lock().await.configure(&device, &config_guard);
                            }
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
                    VideoRendererCommand::Resize(new_size) => {
                        if new_size.width > 0 && new_size.height > 0 {
                            log::info!("[renderer] resize {}x{}", new_size.width, new_size.height);
                            let mut config_guard = config.write().await;
                            config_guard.width = new_size.width;
                            config_guard.height = new_size.height;
                            {
                                surface.lock().await.configure(&device, &config_guard);
                            }
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
            #[cfg(target_os = "ios")]
            PlatformFrame::CvPixelBuffer { .. } => {
                log::warn!("render_frame: iOS CvPixelBuffer not implemented");
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    pub async fn render(&self, frame: Arc<Video>) {
        let video_frame = VideoFrame::new(self.device.clone(), self.backend, frame.clone());

        let texture = video_frame.get_texture();

        let y_plane_view = match texture.format() {
            TextureFormat::P010 => {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(wgpu::TextureFormat::R16Unorm), // Y plane (16-bit to match 10-bit data)
                    aspect: wgpu::TextureAspect::Plane0,
                    ..Default::default()
                })
            }
            TextureFormat::NV12 => {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(wgpu::TextureFormat::R8Unorm), // Y plane (8-bit to match 8-bit data)
                    aspect: wgpu::TextureAspect::Plane0,
                    ..Default::default()
                })
            }
            _ => panic!("Not supported"),
        };

        let uv_plane_view = match texture.format() {
            TextureFormat::P010 => {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(wgpu::TextureFormat::Rg16Unorm), // Y plane (16-bit to match 10-bit data)
                    aspect: wgpu::TextureAspect::Plane1,
                    ..Default::default()
                })
            }
            TextureFormat::NV12 => {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(wgpu::TextureFormat::Rg8Unorm), // Y plane (8-bit to match 8-bit data)
                    aspect: wgpu::TextureAspect::Plane1,
                    ..Default::default()
                })
            }
            _ => panic!("Not supported"),
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
                        VideoRendererCommand::Resize(self.window.inner_size()),
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
            self.window.pre_present_notify();
            self.queue.present(surface_texture);
        }
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
            self.window.pre_present_notify();
            self.queue.present(surface_texture);
            return;
        };

        // Compute aspect-ratio-preserving scale factors.
        let window_size = self.window.inner_size();
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
        self.window.pre_present_notify();
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
            keep.push_back(Arc::clone(&frame.buffer));
            while keep.len() > 1 {
                keep.pop_front();
            }
        }
    }

    #[cfg(target_os = "android")]
    pub async fn render_android(&self, frame: crate::decoders::AndroidHardwareBufferFrame, desired_present_ns: i64) {
        self.render_android_gles(frame, desired_present_ns).await;
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

    fn resize(&self, size: winit::dpi::PhysicalSize<u32>) -> impl std::future::Future<Output = ()> + Send + '_ {
        VideoRenderer::resize(self, size)
    }

    fn change_frame_size(&self, size: winit::dpi::PhysicalSize<u32>) -> impl std::future::Future<Output = ()> + Send + '_ {
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
