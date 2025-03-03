use ffmpeg_next::{frame::Video, software::scaling::Context};
use std::ptr::NonNull;
use std::sync::Arc;
use wgpu::hal::api::Dx12;
use wgpu::hal::api::Vulkan;
use wgpu::Extent3d;
use wgpu::MemoryHints;
use wgpu::{Backends, Instance, InstanceDescriptor};

use wgpu::TextureFormat;
use wgpu::TextureUsages;
use wgpu::{util::DeviceExt, BindGroup, BindGroupLayout, RenderPipeline, Sampler};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, E_NOINTERFACE, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*, Dxgi::*};
use windows::Win32::System::Threading::{CreateEventA, WaitForSingleObject};
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

fn open_d3d11_device() -> ID3D11Device {
    unsafe {
        let mut d3d11_device: Option<ID3D11Device> = None;
        let mut d3d11_context: Option<ID3D11DeviceContext> = None;

        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE(std::ptr::null_mut()),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut d3d11_device),
            None,
            Some(&mut d3d11_context),
        )
        .unwrap();

        let d3d11_device = d3d11_device.unwrap();
        d3d11_device
    }
}

pub fn get_shared_texture_d3d11(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<HANDLE, Box<dyn std::error::Error>> {
    unsafe {
        // Try to open or create shared handle if possible
        if let Ok(dxgi_resource) = texture.cast::<IDXGIResource1>() {
            if let Ok(handle) = dxgi_resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            ) {
                if !handle.is_invalid() {
                    return Ok(handle);
                }
            }
        }

        // No shared handle and not possible to create one.
        // We need to create a new texture and use texture copy from our original one.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        desc.MiscFlags |= D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32
            | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32;

        let mut new_texture = None;
        device.CreateTexture2D(&desc, None, Some(&mut new_texture))?;
        if let Some(new_texture) = new_texture {
            let dxgi_resource: IDXGIResource1 = new_texture.cast::<IDXGIResource1>()?;
            let handle = dxgi_resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            )?;

            Ok(
                //(
                handle, /* ,
                       Some(DirectX11SharedTexture {
                           intermediate_texture: new_texture,
                           fence: DirectX11Fence::new(device)?,
                       })*/
            ) //)
        } else {
            Err("Call to CreateTexture2D failed".into())
        }
    }
}

fn dxgi_format_to_wgpu_format(dxgi_format: u32) -> wgpu::TextureFormat {
    match dxgi_format {
        // Common Formats
        28 => wgpu::TextureFormat::Rgba8Unorm, // DXGI_FORMAT_R8G8B8A8_UNORM
        87 => wgpu::TextureFormat::Bgra8Unorm, // DXGI_FORMAT_B8G8R8A8_UNORM
        22 => wgpu::TextureFormat::Rgba8Snorm, // DXGI_FORMAT_R8G8B8A8_SNORM
        23 => wgpu::TextureFormat::Rgba8Uint,  // DXGI_FORMAT_R8G8B8A8_UINT
        24 => wgpu::TextureFormat::Rgba8Sint,  // DXGI_FORMAT_R8G8B8A8_SINT
        25 => wgpu::TextureFormat::Rgba16Unorm, // DXGI_FORMAT_R16G16B16A16_UNORM
        26 => wgpu::TextureFormat::Rgba16Snorm, // DXGI_FORMAT_R16G16B16A16_SNORM
        27 => wgpu::TextureFormat::Rgba16Uint, // DXGI_FORMAT_R16G16B16A16_UINT
        30 => wgpu::TextureFormat::Rgba16Sint, // DXGI_FORMAT_R16G16B16A16_SINT
        32 => wgpu::TextureFormat::Rgba32Float, // DXGI_FORMAT_R32G32B32A32_FLOAT
        33 => wgpu::TextureFormat::Rgba32Uint, // DXGI_FORMAT_R32G32B32A32_UINT
        34 => wgpu::TextureFormat::Rgba32Sint, // DXGI_FORMAT_R32G32B32A32_SINT
        35 => wgpu::TextureFormat::R32Float,   // DXGI_FORMAT_R32G32B32_FLOAT
        36 => wgpu::TextureFormat::R32Uint,    // DXGI_FORMAT_R32G32B32_UINT
        37 => wgpu::TextureFormat::R32Sint,    // DXGI_FORMAT_R32G32B32_SINT
        38 => wgpu::TextureFormat::R16Float,   // DXGI_FORMAT_R16G16B16A16_FLOAT
        39 => wgpu::TextureFormat::R16Unorm,   // DXGI_FORMAT_R16G16B16A16_UNORM
        40 => wgpu::TextureFormat::R16Snorm,   // DXGI_FORMAT_R16G16B16A16_SNORM
        41 => wgpu::TextureFormat::R16Uint,    // DXGI_FORMAT_R16G16B16A16_UINT
        42 => wgpu::TextureFormat::R16Sint,    // DXGI_FORMAT_R16G16B16A16_SINT
        43 => wgpu::TextureFormat::R8Unorm,    // DXGI_FORMAT_R8G8B8A8_UNORM
        44 => wgpu::TextureFormat::R8Snorm,    // DXGI_FORMAT_R8G8B8A8_SNORM
        45 => wgpu::TextureFormat::R8Uint,     // DXGI_FORMAT_R8G8B8A8_UINT
        46 => wgpu::TextureFormat::R8Sint,     // DXGI_FORMAT_R8G8B8A8_SINT

        // Floating-point and compressed formats
        41 => wgpu::TextureFormat::Rg32Float, // DXGI_FORMAT_R32G32_FLOAT
        42 => wgpu::TextureFormat::Rg32Sint,  // DXGI_FORMAT_R32G32_SINT
        43 => wgpu::TextureFormat::Rg32Uint,  // DXGI_FORMAT_R32G32_UINT

        // Uncommon Formats
        100 => wgpu::TextureFormat::R32Float, // DXGI_FORMAT_R32_FLOAT
        101 => wgpu::TextureFormat::R32Uint,  // DXGI_FORMAT_R32_UINT
        102 => wgpu::TextureFormat::R32Sint,  // DXGI_FORMAT_R32_SINT
        //103 => wgpu::TextureFormat::R64Float, // DXGI_FORMAT_R64_FLOAT
        104 => wgpu::TextureFormat::R64Uint, // DXGI_FORMAT_R64_UINT
        //105 => wgpu::TextureFormat::R64Sint,  // DXGI_FORMAT_R64_SINT

        // Compressed formats
        113 => wgpu::TextureFormat::Bc1RgbaUnorm, // DXGI_FORMAT_BC1_UNORM
        114 => wgpu::TextureFormat::Bc1RgbaUnormSrgb, // DXGI_FORMAT_BC1_UNORM_SRGB
        115 => wgpu::TextureFormat::Bc2RgbaUnorm, // DXGI_FORMAT_BC2_UNORM
        116 => wgpu::TextureFormat::Bc2RgbaUnormSrgb, // DXGI_FORMAT_BC2_UNORM_SRGB
        117 => wgpu::TextureFormat::Bc3RgbaUnorm, // DXGI_FORMAT_BC3_UNORM
        118 => wgpu::TextureFormat::Bc3RgbaUnormSrgb, // DXGI_FORMAT_BC3_UNORM_SRGB
        119 => wgpu::TextureFormat::Bc4RUnorm,    // DXGI_FORMAT_BC4_UNORM
        120 => wgpu::TextureFormat::Bc4RSnorm,    // DXGI_FORMAT_BC4_SNORM
        121 => wgpu::TextureFormat::Bc5RgUnorm,   // DXGI_FORMAT_BC5_UNORM
        122 => wgpu::TextureFormat::Bc5RgSnorm,   // DXGI_FORMAT_BC5_SNORM
        123 => wgpu::TextureFormat::Bc6hRgbFloat, // DXGI_FORMAT_BC6H_UF16
        //124 => wgpu::TextureFormat::Bc6hRgbSfloat, // DXGI_FORMAT_BC6H_SF16
        125 => wgpu::TextureFormat::Bc7RgbaUnorm, // DXGI_FORMAT_BC7_UNORM
        126 => wgpu::TextureFormat::Bc7RgbaUnormSrgb, // DXGI_FORMAT_BC7_UNORM_SRGB

        // Depth-stencil formats
        45 => wgpu::TextureFormat::Depth24PlusStencil8, // DXGI_FORMAT_D24_UNORM_S8_UINT
        46 => wgpu::TextureFormat::Depth32Float,        // DXGI_FORMAT_D32_FLOAT
        47 => wgpu::TextureFormat::Depth32FloatStencil8, // DXGI_FORMAT_D32_FLOAT_S8X24_UINT
        48 => wgpu::TextureFormat::Depth16Unorm,        // DXGI_FORMAT_D16_UNORM

        842094158 => wgpu::TextureFormat::Rgb10a2Unorm,
        // Special case format: Unknown
        _ => panic!("Format error"),
    }
}
/*
fn create_texture_from_dxgi_desc(
    dxgi_desc: &D3DSURFACE_DESC, // Your DXGI description
    device: &wgpu::Device,
) -> wgpu::Texture {
    let format = dxgi_format_to_wgpu_format(dxgi_desc.Format.0);

    let texture_desc = wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: dxgi_desc.Width,
            height: dxgi_desc.Height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    };

    device.create_texture(&texture_desc)
}*/

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
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    frame_size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: TextureFormat,
    sampler: Sampler,
    render_texture: wgpu::Texture,
    vertex_buffer: wgpu::Buffer,
    texture_bind_group: BindGroup,
    texture_bind_group_layout: BindGroupLayout,
    render_pipeline: RenderPipeline,
    dx_device: ID3D11Device, //frame_scaler: Context,
}

impl VideoRenderer {
    pub async fn new(window: Arc<Window>) -> Self {
        let instance = Instance::new(&InstanceDescriptor {
            backends: Backends::DX12,
            flags: wgpu::InstanceFlags::DEBUG, // Force DirectX 12 backend
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
                    required_features: wgpu::Features::TEXTURE_FORMAT_NV12, // Enable NV12
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

        let dxdevice = open_d3d11_device();

        let render_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Render Texture"),
            size: wgpu::Extent3d {
                width: 1920,
                height: 1080,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: surface_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[surface_format],
        });

        let frame_size = winit::dpi::PhysicalSize::new(1, 1);

        //Need some dynamic
        let dst_format = ffmpeg_next::format::Pixel::BGRA;
        let frame_scaler = ffmpeg_next::software::scaling::Context::get(
            ffmpeg_next::format::Pixel::BGRA,
            frame_size.width,
            frame_size.height,
            dst_format,
            frame_size.width,
            frame_size.height,
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )
        .unwrap();

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
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
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
                label: Some("texture_bind_group_layout"),
            });

        let view = render_texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(surface_format),
            ..Default::default()
        });

        let texture_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
            label: Some("texture_bind_group"),
        });

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

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
            frame_size,
            surface,
            surface_format,
            sampler,
            render_texture,
            vertex_buffer,
            texture_bind_group,
            texture_bind_group_layout,
            render_pipeline,
            dx_device: dxdevice, //frame_scaler,
        };

        // Configure surface for the first time
        state.configure_surface();

        state
    }

    fn get_window(&self) -> &Window {
        &self.window
    }

    fn configure_surface(&self) {
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            view_formats: vec![self.surface_format],
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            width: self.size.width,
            height: self.size.height,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 10,
        };
        self.surface.configure(&self.device, &surface_config);
    }

    fn configure_vertext_buffer(&mut self, width: u32, height: u32) {
        let texture_aspect = self.frame_size.width as f32 / self.frame_size.height as f32;
        let window_aspect = width as f32 / height as f32;

        let (scale_x, scale_y) = if texture_aspect > window_aspect {
            (1.0, window_aspect / texture_aspect)
        } else {
            (texture_aspect / window_aspect, 1.0)
        };

        let vertices = generate_verticles(scale_x, scale_y);
        self.vertex_buffer.destroy();
        let vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Vertex Buffer"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

        self.vertex_buffer = vertex_buffer;
    }

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.size = new_size;
        if self.size.width > 0 && self.size.height > 0 {
            let width = self.size.width;
            let height = self.size.height;

            self.configure_vertext_buffer(width, height);
            self.configure_surface();
        }
    }

    fn on_resize_frame(&mut self, frame: Video) {
        let width = frame.width();
        let height = frame.height();

        self.frame_size = winit::dpi::PhysicalSize::new(width, height);

        let windows_size = self.get_window().inner_size();
        self.configure_vertext_buffer(windows_size.width, windows_size.height);

        let dst_format = ffmpeg_next::format::Pixel::BGRA;
        let frame_scaler = ffmpeg_next::software::scaling::Context::get(
            frame.format(),
            width,
            height,
            dst_format,
            width,
            height,
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )
        .unwrap();

        self.render_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Render Texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.surface_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[self.surface_format],
        });

        let view = self
            .render_texture
            .create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.surface_format),
                ..Default::default()
            });

        self.texture_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
            label: Some("texture_bind_group"),
        });
    }

    pub fn render(&self, frame: Arc<Video>) {
        let format = frame.format();
        unsafe {
            /*
                    let mut desc = D3DSURFACE_DESC::default();
                    unsafe {
                        let texture = (*frame.as_ptr()).data[3] as *mut _;

                        if let Err(e) = IDirect3DSurface9::from_raw_borrowed(&texture)
                            .unwrap()
                            .GetDesc(&mut desc)
                        {
                            //log::error!("Failed to get DXVA2 {}", e);
                            println!("Failed to get DXVA2 {}", e);
                        }
                    }
            */

            let frame_ptr = frame.as_ptr();
            // println!("Frame pointer: {:?}", frame_ptr);

            //let texture_ptr = (*frame_ptr).data[3] as *mut std::ffi::c_void;

            //let tex_ptr = (*frame_ptr).data[3] as *mut ID3D11Texture2D;
            //let tex: Option<&ID3D11Texture2D> = NonNull::new(tex_ptr).map(|ptr| &*ptr.as_ptr());

            let texture = (*frame_ptr).data[0] as *mut _;
            let texture = ID3D11Texture2D::from_raw_borrowed(&texture);
            //let mut desc11 = D3D11_TEXTURE2D_DESC::default();
            //tex.unwrap().GetDesc(&mut desc11);

            let shared_text = get_shared_texture_d3d11(&self.dx_device, texture.unwrap()).unwrap();

            let raw_image = self
                .device
                .as_hal::<Dx12, _, _>(|hdevice| {
                    hdevice.map(|hdevice| {
                        let raw_device = hdevice.raw_device();

                        let mut resource =
                            None::<windows::Win32::Graphics::Direct3D12::ID3D12Resource>;

                        match raw_device.OpenSharedHandle(shared_text, &mut resource) {
                            Ok(_) => Ok(resource.unwrap()),
                            Err(e) => Err(e),
                        }
                    })
                })
                .unwrap()
                .unwrap(); // TODO: unwrap

            let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;

            let desc = wgpu::TextureDescriptor {
                label: None,
                size: Extent3d {
                    width: 1920,
                    height: 896,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TextureFormat::NV12,
                usage,
                view_formats: &[],
            };

            let texture = <Dx12 as wgpu::hal::Api>::Device::texture_from_raw(
                raw_image,
                desc.format,
                desc.dimension,
                desc.size,
                1,
                1,
            );

            let texture = self.device.create_texture_from_hal::<Dx12>(texture, &desc);

            /* let view = self
            .render_texture
            .create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.surface_format),
                ..Default::default()
            });*/

            let y_plane_view = texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::R8Unorm), // Y plane
                aspect: wgpu::TextureAspect::Plane0,        // First plane (Y)
                ..Default::default()
            });

            let uv_plane_view = texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::Rg8Unorm), // UV plane
                aspect: wgpu::TextureAspect::Plane1,         // Second plane (UV)
                ..Default::default()
            });

            /*let view_heh = texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.surface_format),
                ..Default::default()
            });*/

            //let src_view: wgpu::TextureView =
            //    texture.create_view(&wgpu::TextureViewDescriptor::default());

            let texture_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &self.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&uv_plane_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
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
            /*
            if frame.width() != self.frame_size.width || frame.height() != self.frame_size.height {
                //self.on_resize_frame(frame.clone());
            }

            let dst_format = ffmpeg_next::format::Pixel::BGRA;
            let mut frame_scaler = ffmpeg_next::software::scaling::Context::get(
                frame.format(),
                frame.width(),
                frame.height(),
                dst_format,
                frame.width(),
                frame.height(),
                ffmpeg_next::software::scaling::Flags::BILINEAR,
            )
            .unwrap();

            let mut dst_frame = ffmpeg_next::util::frame::Video::new(
                frame_scaler.output().format,
                frame.width(),
                frame.height(),
            );

            _ = frame_scaler.run(&frame, &mut dst_frame);

            let width_bytes = dst_frame.stride(0);
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.render_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                dst_frame.data(0),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width_bytes as u32),
                    rows_per_image: Some(frame.height()),
                },
                wgpu::Extent3d {
                    width: frame.width(),
                    height: frame.height(),
                    depth_or_array_layers: 1,
                },
            );*/

            /*let texture_bind_group_layout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float {
                                    filterable: true,
                                },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                    label: Some("texture_bind_group_layout"),
                });*/

            let shader = self
                .device
                .create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

            // Create pipeline layout
            let pipeline_layout =
                self.device
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("Pipeline Layout"),
                        bind_group_layouts: &[&self.texture_bind_group_layout],
                        push_constant_ranges: &[],
                    });

            // Create render pipeline
            let render_pipeline =
                self.device
                    .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                                format: self.surface_format,
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

                render_pass.set_pipeline(&render_pipeline);
                render_pass.set_bind_group(0, &texture_bind_group, &[]);

                render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));

                render_pass.draw(0..6, 0..1);
            }

            // Submit the command in the queue to execute
            self.queue.submit([encoder.finish()]);
            self.window.pre_present_notify();
            surface_texture.present();
        }
    }
}
