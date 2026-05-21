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
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "android"))]
mod video_vulkan;
#[cfg(target_os = "android")]
mod video_mediacodec;

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

fn generate_verticles(scale_x: f32, scale_y: f32) -> [Vertex; 6] {
    [
        Vertex {
            position: [-1. * scale_x, -1. * scale_y, 0.0],
            tex_coords: [0., 1.],
        }, // A
        Vertex {
            position: [1. * scale_x, -1. * scale_y, 0.0],
            tex_coords: [1., 1.],
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
            tex_coords: [1., 1.],
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
    queue: wgpu::Queue,
    frame_size: Arc<RwLock<winit::dpi::PhysicalSize<u32>>>,
    surface: Arc<Mutex<wgpu::Surface<'static>>>,
    surface_format: TextureFormat,
    surface_config: Arc<RwLock<SurfaceConfiguration>>,
    sampler: Sampler,
    vertex_buffer: Arc<RwLock<wgpu::Buffer>>,
    texture_bind_group_layout: BindGroupLayout,
    render_pipeline: RenderPipeline,
    command_sender: Sender<VideoRendererCommand>,
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
        // Android emulators on preview API levels sometimes ship a Vulkan
        // ICD that rejects vkCreateInstance with ERROR_INITIALIZATION_FAILED.
        // Keep GLES in the request set so the Instance can still come up
        // (and Player::new doesn't panic). On real devices Vulkan wins, which
        // is what render_android's AHB→VkImage zero-copy path requires.
        #[cfg(target_os = "android")]
        let backend = Backends::VULKAN | Backends::GL;
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
        let is_hw_backend = backend == wgpu::Backend::Vulkan || backend == wgpu::Backend::Dx12;
        let required_features = if is_hw_backend {
            wgpu::Features::TEXTURE_FORMAT_NV12 | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
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
            wgpu::Limits::default()
        } else {
            let adapter_limits = adapter.limits();
            wgpu::Limits {
                max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
                max_texture_dimension_1d: adapter_limits.max_texture_dimension_1d,
                ..wgpu::Limits::downlevel_webgl2_defaults()
            }
        };

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
        println!("{:?}", cap.formats);

        //let surface_format = cap.formats[4]; //.last().unwrap();

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let texture_bind_group_layout =
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

        let shader: wgpu::ShaderModule =
            device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

        /*let shader: wgpu::ShaderModule =
                    device.create_shader_module(wgpu::include_wgsl!("shader_hdr.wgsl"));
        */
        // Create pipeline layout
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout"),
            bind_group_layouts: &[Some(&texture_bind_group_layout)],
            immediate_size: 0,
        });

        // Create render pipeline
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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

        let vertices = generate_verticles(1., 1.);
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let (command_sender, command_receiver) = mpsc::channel(4);

        let renderer = VideoRenderer {
            window,
            device,
            backend,
            queue,
            frame_size: Arc::new(RwLock::new(size)),
            surface: Arc::new(Mutex::new(surface)),
            surface_format,
            surface_config: Arc::new(RwLock::new(surface_config)),
            sampler,
            vertex_buffer: Arc::new(RwLock::new(vertex_buffer)),
            texture_bind_group_layout,
            render_pipeline,
            command_sender,
        };

        renderer.spawn_command_thread(command_receiver);

        renderer
    }

    async fn change_vertex_buffer(
        device: &Device,
        window_size: PhysicalSize<u32>,
        frame_size: PhysicalSize<u32>,
        vertex_buffer: Arc<RwLock<Buffer>>,
    ) {
        let window_aspect = window_size.width as f32 / window_size.height as f32;

        let texture_aspect = frame_size.width as f32 / frame_size.height as f32;
        let (scale_x, scale_y) = if texture_aspect > window_aspect {
            (1.0, window_aspect / texture_aspect)
        } else {
            (texture_aspect / window_aspect, 1.0)
        };

        let vertices = generate_verticles(scale_x, scale_y);
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
        let vertex_buffer = Arc::clone(&self.vertex_buffer);
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
                            Self::change_vertex_buffer(
                                &device,
                                window_size,
                                new_frame_size,
                                vertex_buffer.clone(),
                            )
                            .await;
                        }
                    }
                    VideoRendererCommand::Resize(new_size) => {
                        if new_size.width > 0 && new_size.height > 0 {
                            let mut config_guard = config.write().await;
                            config_guard.width = new_size.width;
                            config_guard.height = new_size.height;
                            {
                                surface.lock().await.configure(&device, &config_guard);
                            }
                            let window_size = window.inner_size();
                            if window_size.width > 0 && window_size.height > 0 {
                                let frame_size = frame_size.read().await;
                                Self::change_vertex_buffer(
                                    &device,
                                    window_size,
                                    *frame_size,
                                    vertex_buffer.clone(),
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
        match frame.native {
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            PlatformFrame::FfmpegVideo(ffmpeg_frame) => {
                self.render(ffmpeg_frame).await;
            }
            #[cfg(target_os = "android")]
            PlatformFrame::HardwareBuffer(ahb) => {
                self.render_android(ahb).await;
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

        let texture_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.texture_bind_group_layout,
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
            let vertex_buffer = self.vertex_buffer.read().await;

            let surface_texture = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
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

                render_pass.set_pipeline(&self.render_pipeline);
                render_pass.set_bind_group(0, &texture_bind_group, &[]);

                render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));

                render_pass.draw(0..6, 0..1);
            }

            // Submit the command in the queue to execute
            self.queue.submit([encoder.finish()]);
            self.window.pre_present_notify();
            self.queue.present(surface_texture);
        }
    }

    /// Android render path. Imports the MediaCodec-produced AHardwareBuffer
    /// into Vulkan as a VkImage with `VK_FORMAT_G8_B8R8_2PLANE_420_UNORM`
    /// (Vulkan NV12), wraps it as `wgpu::Texture`, samples Y + UV planes
    /// through the same NV12 shader the desktop path uses. Zero-copy.
    #[cfg(target_os = "android")]
    pub async fn render_android(&self, frame: crate::decoders::AndroidHardwareBufferFrame) {
        use ndk::hardware_buffer::HardwareBuffer;
        use video_mediacodec::create_vk_image_from_ahb;
        use video_vulkan::create_texture_from_vk_image;

        // AHB→VkImage zero-copy import requires the Vulkan backend. On
        // emulators that fall back to GLES, drop the frame's GPU import
        // but still clear-and-present the surface so the swapchain keeps
        // ticking and we can see the renderer is alive (otherwise the
        // window stays black and looks frozen).
        if self.backend != wgpu::Backend::Vulkan {
            static WARN_ONCE: std::sync::Once = std::sync::Once::new();
            WARN_ONCE.call_once(|| {
                log::warn!(
                    "render_android: backend is {:?}, not Vulkan — AHB import unavailable, presenting clear color only",
                    self.backend
                );
            });

            let surface = self.surface.lock().await;
            let surface_texture = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
                other => {
                    log::warn!("surface texture not available: {:?}", other);
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
                    label: Some("android-fallback-clear"),
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
        }

        // `HardwareBuffer` is the unowned view; scope it tightly so the
        // !Send pointer doesn't outlive the import call (otherwise the
        // surrounding future loses its Send bound when we .await below).
        // The owned ref in `frame.buffer` keeps the AHB alive after the
        // import — Vulkan adds its own refcount once the memory is allocated.
        let img_mem = {
            let hb_view = unsafe {
                HardwareBuffer::from_ptr(std::ptr::NonNull::new(frame.buffer.as_ptr()).unwrap())
            };
            match create_vk_image_from_ahb(
                &self.device,
                &hb_view,
                frame.width,
                frame.height,
            ) {
                Ok(im) => im,
                Err(e) => {
                    log::warn!("AHB import failed: {}", e);
                    return;
                }
            }
        };

        let texture = create_texture_from_vk_image(
            &self.device,
            img_mem.raw_image,
            frame.width,
            frame.height,
            TextureFormat::NV12,
            true,
            true,
        );

        let y_plane_view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(TextureFormat::R8Unorm),
            aspect: wgpu::TextureAspect::Plane0,
            ..Default::default()
        });
        let uv_plane_view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(TextureFormat::Rg8Unorm),
            aspect: wgpu::TextureAspect::Plane1,
            ..Default::default()
        });

        let texture_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.texture_bind_group_layout,
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
            label: Some("texture_bind_group_android"),
        });

        {
            let surface = self.surface.lock().await;
            let vertex_buffer = self.vertex_buffer.read().await;

            let surface_texture = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
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

                render_pass.set_pipeline(&self.render_pipeline);
                render_pass.set_bind_group(0, &texture_bind_group, &[]);
                render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                render_pass.draw(0..6, 0..1);
            }

            self.queue.submit([encoder.finish()]);
            self.window.pre_present_notify();
            self.queue.present(surface_texture);
        }

        // Defer freeing the imported VkDeviceMemory until the GPU has
        // finished with this submission. Freeing immediately after
        // queue.present() races the GPU and poisons the next
        // get_current_texture() with a validation error.
        // on_submitted_work_done fires once the just-submitted batch
        // (including this frame's NV12 sample reads) retires; subsequent
        // queue.submit / device.poll calls drive it forward.
        drop(texture);
        let device_for_free = self.device.clone();
        let memory_to_free = img_mem.memory;
        self.queue.on_submitted_work_done(move || unsafe {
            if let Some(raw_dev) = device_for_free.as_hal::<wgpu::hal::api::Vulkan>() {
                raw_dev.raw_device().free_memory(memory_to_free, None);
            }
        });
    }
}
