use ffmpeg_next::frame::Video;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex, RwLock,
};
use video_frame::VideoFrame;
use wgpu::{Backends, Buffer};
use wgpu::{Device, SurfaceConfiguration};
use winit::dpi::PhysicalSize;

#[cfg(target_os = "windows")]
mod video_directx;
mod video_frame;
#[cfg(target_os = "linux")]
mod video_vaapi;
mod video_vulkan;

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
        let instance = Instance::new(&InstanceDescriptor {
            backends: Backends::DX12,
            ..Default::default()
        });

        #[cfg(target_os = "linux")]
        let instance = Instance::new(&InstanceDescriptor {
            backends: Backends::VULKAN,
            ..Default::default()
        });

        let surface = instance.create_surface(window.clone()).unwrap();

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();

        let backend = adapter.get_info().backend;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    required_features:/* wgpu::Features::TEXTURE_FORMAT_P010 // Enable P010
                        |*/  wgpu::Features::TEXTURE_FORMAT_NV12
                        | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM, // Enable NV12
                    required_limits: wgpu::Limits::default(),
                    label: Some("Device with NV12 support"),
                    memory_hints: MemoryHints::Performance,
                },
                None,
            )
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
            bind_group_layouts: &[&texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        // Create render pipeline
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Vertex::desc()],
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
            multiview: None,
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

            let surface_texture = surface
                .get_current_texture()
                .expect("failed to acquire next swapchain texture");

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
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });

                render_pass.set_pipeline(&self.render_pipeline);
                render_pass.set_bind_group(0, &texture_bind_group, &[]);

                render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));

                render_pass.draw(0..6, 0..1);
            }

            // Submit the command in the queue to execute
            self.queue.submit([encoder.finish()]);
            self.window.pre_present_notify();
            surface_texture.present();
        }
    }
}
