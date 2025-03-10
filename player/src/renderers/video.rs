use ffmpeg_next::frame::Video;

use ffmpeg_sys_next::AVHWFramesContext;
use std::sync::Arc;
use wgpu::hal::api::Dx12;
use wgpu::Extent3d;
use wgpu::MemoryHints;
use wgpu::RenderPipeline;
use wgpu::{Backends, Instance, InstanceDescriptor};
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*, Dxgi::*};

//extern crate ffmpeg_sys_next;

use wgpu::TextureFormat;
use wgpu::{util::DeviceExt, BindGroupLayout, Sampler};
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE};

use windows::Win32::System::Threading::{CreateEventA, WaitForSingleObject};
use winit::window::Window;

// Define a raw struct for AVD3D11VAContext if it's not exposed in the bindings
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut std::ffi::c_void,
    device_context: *mut std::ffi::c_void,
    video_device: *mut ID3D11VideoDevice,
    video_context: *mut ID3D11VideoContext,
    lock: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    lock_ctx: *mut std::ffi::c_void,
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

pub fn get_shared_texture_d3d11(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    height: u32,
) -> Result<(HANDLE, Option<ID3D11Texture2D>), Box<dyn std::error::Error>> {
    unsafe {
        // Try to open or create shared handle if possible
        /*if let Ok(dxgi_resource) = texture.cast::<IDXGIResource1>() {
            if let Ok(handle) = dxgi_resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            ) {
                if !handle.is_invalid() {
                    return Ok((handle, None));
                }
            }
        }*/

        // No shared handle and not possible to create one.
        // We need to create a new texture and use texture copy from our original one.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        desc.MiscFlags |= D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32
            | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32;

        desc.Height = height;
        desc.ArraySize = 1;

        let mut new_texture = None;
        device.CreateTexture2D(&desc, None, Some(&mut new_texture))?;
        if let Some(new_texture) = new_texture {
            let dxgi_resource: IDXGIResource1 = new_texture.cast::<IDXGIResource1>()?;
            let handle = dxgi_resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            )?;

            Ok((handle, Some(new_texture)))
        } else {
            Err("Call to CreateTexture2D failed".into())
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

pub struct DirectX11Fence {
    fence: ID3D11Fence,
    event: HANDLE,
    fence_value: std::sync::atomic::AtomicU64,
}
unsafe impl Send for DirectX11Fence {}
impl DirectX11Fence {
    pub fn new(device: &ID3D11Device) -> windows::core::Result<Self> {
        unsafe {
            let device = device.cast::<ID3D11Device5>()?;
            let mut fence: Option<ID3D11Fence> = None;

            device.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)?;
            let fence = fence.ok_or(windows::core::Error::new(E_FAIL, "Failed to create fence"))?;

            let event = CreateEventA(None, false, false, windows::core::PCSTR::null())?;

            Ok(Self {
                fence,
                event,
                fence_value: Default::default(),
            })
        }
    }
    pub fn synchronize(&self, context: &ID3D11DeviceContext) -> windows::core::Result<()> {
        let context = context.cast::<ID3D11DeviceContext4>()?;
        let v = self
            .fence_value
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        unsafe {
            context.Signal(&self.fence, v)?;
            self.fence.SetEventOnCompletion(v, self.event)?;
            WaitForSingleObject(self.event, 5000);
        }
        Ok(())
    }
}
impl Drop for DirectX11Fence {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

pub struct VideoRenderer {
    window: Arc<Window>,
    device: wgpu::Device,
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
            backends: Backends::DX12,
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

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    required_features: wgpu::Features::TEXTURE_FORMAT_P010 // Enable P010
                        | wgpu::Features::TEXTURE_FORMAT_NV12
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

        let state = VideoRenderer {
            window,
            device,
            queue,
            size,
            //frame_size,
            surface,
            surface_format,
            sampler,
            vertex_buffer,
            texture_bind_group_layout,
            render_pipeline,
        };

        state
    }

    fn get_window(&self) -> &Window {
        &self.window
    }

    pub fn render(&self, frame: Arc<Video>) {
        unsafe {
            let frame_ptr = frame.as_ptr();

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

            let y_plane_view = match desc.format {
                TextureFormat::P010 => {
                    texture.create_view(&wgpu::TextureViewDescriptor {
                        format: Some(wgpu::TextureFormat::R16Unorm), // Y plane (16-bit to match 10-bit data)
                        aspect: wgpu::TextureAspect::Plane0,
                        ..Default::default()
                    })
                }
                TextureFormat::NV12 => {
                    texture.create_view(&wgpu::TextureViewDescriptor {
                        format: Some(wgpu::TextureFormat::R8Unorm), // Y plane (16-bit to match 10-bit data)
                        aspect: wgpu::TextureAspect::Plane0,
                        ..Default::default()
                    })
                }
                _ => panic!("Not supported"),
            };

            let uv_plane_view = match desc.format {
                TextureFormat::P010 => {
                    texture.create_view(&wgpu::TextureViewDescriptor {
                        format: Some(wgpu::TextureFormat::Rg16Unorm), // Y plane (16-bit to match 10-bit data)
                        aspect: wgpu::TextureAspect::Plane1,
                        ..Default::default()
                    })
                }
                TextureFormat::NV12 => {
                    texture.create_view(&wgpu::TextureViewDescriptor {
                        format: Some(wgpu::TextureFormat::Rg8Unorm), // Y plane (16-bit to match 10-bit data)
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

            /*
                        let texture_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                            layout: &self.texture_bind_group_layout,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(&y_plane_view),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                                },
                            ],
                            label: Some("texture_bind_group"),
                        });
            */
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

            _ = CloseHandle(shared_handle);

            drop(texture);
            drop(frame);
        }
    }
}

pub fn format_dxgi_to_wgpu(format: DXGI_FORMAT) -> TextureFormat {
    match format {
        DXGI_FORMAT_NV12 => TextureFormat::NV12,
        DXGI_FORMAT_P010 => TextureFormat::P010,
        DXGI_FORMAT_R8_UNORM => TextureFormat::R8Unorm,
        DXGI_FORMAT_R8_SNORM => TextureFormat::R8Snorm,
        DXGI_FORMAT_R8_UINT => TextureFormat::R8Uint,
        DXGI_FORMAT_R8_SINT => TextureFormat::R8Sint,
        DXGI_FORMAT_R16_UINT => TextureFormat::R16Uint,
        DXGI_FORMAT_R16_SINT => TextureFormat::R16Sint,
        DXGI_FORMAT_R16_UNORM => TextureFormat::R16Unorm,
        DXGI_FORMAT_R16_SNORM => TextureFormat::R16Snorm,
        DXGI_FORMAT_R16_FLOAT => TextureFormat::R16Float,
        DXGI_FORMAT_R8G8_UNORM => TextureFormat::Rg8Unorm,
        DXGI_FORMAT_R8G8_SNORM => TextureFormat::Rg8Snorm,
        DXGI_FORMAT_R8G8_UINT => TextureFormat::Rg8Uint,
        DXGI_FORMAT_R8G8_SINT => TextureFormat::Rg8Sint,
        DXGI_FORMAT_R16G16_UNORM => TextureFormat::Rg16Unorm,
        DXGI_FORMAT_R16G16_SNORM => TextureFormat::Rg16Snorm,
        DXGI_FORMAT_R32_UINT => TextureFormat::R32Uint,
        DXGI_FORMAT_R32_SINT => TextureFormat::R32Sint,
        DXGI_FORMAT_R32_FLOAT => TextureFormat::R32Float,
        DXGI_FORMAT_R16G16_UINT => TextureFormat::Rg16Uint,
        DXGI_FORMAT_R16G16_SINT => TextureFormat::Rg16Sint,
        DXGI_FORMAT_R16G16_FLOAT => TextureFormat::Rg16Float,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => TextureFormat::Rgba8Unorm,
        DXGI_FORMAT_R8G8B8A8_UNORM => TextureFormat::Rgba8Unorm,
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => TextureFormat::Rgba8UnormSrgb,
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => TextureFormat::Bgra8UnormSrgb,
        DXGI_FORMAT_R8G8B8A8_SNORM => TextureFormat::Rgba8Snorm,
        DXGI_FORMAT_B8G8R8A8_UNORM => TextureFormat::Bgra8Unorm,
        DXGI_FORMAT_R8G8B8A8_UINT => TextureFormat::Rgba8Uint,
        DXGI_FORMAT_R8G8B8A8_SINT => TextureFormat::Rgba8Sint,
        DXGI_FORMAT_R10G10B10A2_UNORM => TextureFormat::Rgb10a2Unorm,
        DXGI_FORMAT_R10G10B10A2_UINT => TextureFormat::Rgb10a2Uint,
        DXGI_FORMAT_R11G11B10_FLOAT => TextureFormat::Rg11b10Ufloat,
        DXGI_FORMAT_R32G32_UINT => TextureFormat::Rg32Uint,
        DXGI_FORMAT_R32G32_SINT => TextureFormat::Rg32Sint,
        DXGI_FORMAT_R32G32_FLOAT => TextureFormat::Rg32Float,
        DXGI_FORMAT_R16G16B16A16_UINT => TextureFormat::Rgba16Uint,
        DXGI_FORMAT_R16G16B16A16_SINT => TextureFormat::Rgba16Sint,
        DXGI_FORMAT_R16G16B16A16_UNORM => TextureFormat::Rgba16Unorm,
        DXGI_FORMAT_R16G16B16A16_SNORM => TextureFormat::Rgba16Snorm,
        DXGI_FORMAT_R16G16B16A16_FLOAT => TextureFormat::Rgba16Float,
        DXGI_FORMAT_R32G32B32A32_UINT => TextureFormat::Rgba32Uint,
        DXGI_FORMAT_R32G32B32A32_SINT => TextureFormat::Rgba32Sint,
        DXGI_FORMAT_R32G32B32A32_FLOAT => TextureFormat::Rgba32Float,
        DXGI_FORMAT_D32_FLOAT => TextureFormat::Depth32Float,
        DXGI_FORMAT_D32_FLOAT_S8X24_UINT => TextureFormat::Depth32FloatStencil8,
        DXGI_FORMAT_R9G9B9E5_SHAREDEXP => TextureFormat::Rgb9e5Ufloat,
        DXGI_FORMAT_BC1_UNORM => TextureFormat::Bc1RgbaUnorm,
        DXGI_FORMAT_BC1_UNORM_SRGB => TextureFormat::Bc1RgbaUnormSrgb,
        DXGI_FORMAT_BC2_UNORM => TextureFormat::Bc2RgbaUnorm,
        DXGI_FORMAT_BC2_UNORM_SRGB => TextureFormat::Bc2RgbaUnormSrgb,
        DXGI_FORMAT_BC3_UNORM => TextureFormat::Bc3RgbaUnorm,
        DXGI_FORMAT_BC3_UNORM_SRGB => TextureFormat::Bc3RgbaUnormSrgb,
        DXGI_FORMAT_BC4_UNORM => TextureFormat::Bc4RUnorm,
        DXGI_FORMAT_BC4_SNORM => TextureFormat::Bc4RSnorm,
        DXGI_FORMAT_BC5_UNORM => TextureFormat::Bc5RgUnorm,
        DXGI_FORMAT_BC5_SNORM => TextureFormat::Bc5RgSnorm,
        DXGI_FORMAT_BC6H_UF16 => TextureFormat::Bc6hRgbUfloat,
        DXGI_FORMAT_BC6H_SF16 => TextureFormat::Bc6hRgbFloat,
        DXGI_FORMAT_BC7_UNORM => TextureFormat::Bc7RgbaUnorm,
        DXGI_FORMAT_BC7_UNORM_SRGB => TextureFormat::Bc7RgbaUnormSrgb,
        _ => panic!("Unsupported texture format: {:?}", format),
    }
}
