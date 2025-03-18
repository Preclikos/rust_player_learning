use ffmpeg_next::frame::Video;
use video::VideoFrame;
use wgpu::Backends;

mod video;
#[cfg(target_os = "windows")]
mod video_directx;
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
        }, // E
    ]
}

pub struct VideoRenderer {
    window: Arc<Window>,
    device: wgpu::Device,
    backend: wgpu::Backend,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    //frame_size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: TextureFormat,
    sampler: Sampler,
    vertex_buffer: wgpu::Buffer,
    texture_bind_group_layout: BindGroupLayout,
    render_pipeline: RenderPipeline,
}

impl VideoRenderer {
    pub async fn new(window: Arc<Window>) -> Self {
        let instance = Instance::new(&InstanceDescriptor {
            //backends: Backends::DX12,
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

        let size = window.inner_size();

        let cap = surface.get_capabilities(&adapter);
        let surface_format = cap.formats[0];

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
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
            width: 3840,
            height: 2160,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 10,
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

        VideoRenderer {
            window,
            device,
            backend,
            queue,
            size,
            //frame_size,
            surface,
            surface_format,
            sampler,
            vertex_buffer,
            texture_bind_group_layout,
            render_pipeline,
        }
    }

    fn get_window(&self) -> &Window {
        &self.window
    }

    pub fn render(&self, frame: Arc<Video>) {
        let video_frame = VideoFrame::new(self.device.clone(), self.backend);
        // unsafe {
        /* let frame_ptr = frame.as_ptr();

                    let hw_frames_ctx = (*frame_ptr).hw_frames_ctx;
                    if hw_frames_ctx.is_null() {
                        panic!("No hardware frames context associated with the frame.");
                    }

                    let hw_frames_ctx_ref = (*hw_frames_ctx).data as *mut AVHWFramesContext;
                    let hw_device_ctx = (*hw_frames_ctx_ref).device_ctx;
                    if hw_device_ctx.is_null() {
                        panic!("No hardware device context associated with the frames context.");
                    }

                    let hwctx = (*hw_device_ctx).hwctx as *mut AVD3D11VADeviceContext;

                    let d3d11_device = ID3D11Device::from_raw_borrowed(&(*hwctx).device).unwrap();
                    let d3d11_device_context =
                        ID3D11DeviceContext::from_raw_borrowed(&(*hwctx).device_context).unwrap();

                    let texture = (*frame.as_ptr()).data[0] as *mut _;
                    let index = (*frame.as_ptr()).data[1] as i32;
                    let texture_ref = ID3D11Texture2D::from_raw_borrowed(&texture).unwrap();

                    let (shared_handle, shared_text) =
                        get_shared_texture_d3d11(d3d11_device, texture_ref, frame.height()).unwrap();
                    let fence = DirectX11Fence::new(d3d11_device).unwrap();

                    let shared_tex = shared_text.unwrap();

                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    texture_ref.GetDesc(&mut desc);

                    if let Ok(mutex) = shared_tex.cast::<IDXGIKeyedMutex>() {
                        mutex.AcquireSync(0, 500).unwrap();

                        d3d11_device_context.CopySubresourceRegion(
                            &shared_tex,
                            0,
                            0,
                            0,
                            0,
                            texture_ref,
                            index as u32, // Copy from specific array slice
                            None,
                        );
                        fence.synchronize(d3d11_device_context).unwrap();

                        mutex.ReleaseSync(0).unwrap();
                    }

                    let raw_image = self
                        .device
                        .as_hal::<Dx12, _, _>(|hdevice| {
                            hdevice.map(|hdevice| {
                                let raw_device = hdevice.raw_device();

                                let mut resource =
                                    None::<windows::Win32::Graphics::Direct3D12::ID3D12Resource>;

                                match raw_device.OpenSharedHandle(shared_handle, &mut resource) {
                                    Ok(_) => Ok(resource.unwrap()),
                                    Err(e) => Err(e),
                                }
                            })
                        })
                        .unwrap()
                        .unwrap();

                    let desc = wgpu::TextureDescriptor {
                        label: None,
                        size: Extent3d {
                            width: desc.Width,
                            height: desc.Height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: format_dxgi_to_wgpu(desc.Format),
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    };

                    let dx_texture = <Dx12 as wgpu::hal::Api>::Device::texture_from_raw(
                        raw_image,
                        desc.format,
                        desc.dimension,
                        desc.size,
                        1,
                        1,
                    );

                    let texture = self
                        .device
                        .create_texture_from_hal::<Dx12>(dx_texture, &desc);
        */
        let texture = video_frame.get_texture(frame);

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

        let surface_texture = self
            .surface
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

            render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));

            render_pass.draw(0..6, 0..1);
        }

        // Submit the command in the queue to execute
        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        surface_texture.present();

        //_ = CloseHandle(shared_handle);

        drop(texture);
        //drop(frame);
        //}
    }
}
