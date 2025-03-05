use ffmpeg_next::frame::Video;
use ffmpeg_sys_next::AVFrame;
use ffmpeg_sys_next::AVHWDeviceContext;
use ffmpeg_sys_next::AVHWDeviceType;
use ffmpeg_sys_next::AVHWFramesContext;
use std::sync::Arc;
use wgpu::hal::api::Dx12;
use wgpu::Extent3d;
use wgpu::MemoryHints;
use wgpu::{Backends, Instance, InstanceDescriptor};

extern crate ffmpeg_sys_next;

use wgpu::TextureFormat;
use wgpu::{util::DeviceExt, BindGroup, BindGroupLayout, RenderPipeline, Sampler};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, E_NOINTERFACE, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::{Direct3D11::*, Direct3D12, Dxgi::Common::*, Dxgi::*};
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*, Dxgi::*};
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

fn open_d3d11_device() -> (ID3D11Device, ID3D11DeviceContext) {
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
        let d3d11_context = d3d11_context.unwrap();
        (d3d11_device, d3d11_context)
    }
}

pub fn get_shared_texture_d3d11(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<(HANDLE, Option<ID3D11Texture2D>), Box<dyn std::error::Error>> {
    unsafe {
        // Try to open or create shared handle if possible
        if let Ok(dxgi_resource) = texture.cast::<IDXGIResource1>() {
            if let Ok(handle) = dxgi_resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            ) {
                if !handle.is_invalid() {
                    return Ok((handle, None));
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
    frame_size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: TextureFormat,
    sampler: Sampler,
    render_texture: wgpu::Texture,
    vertex_buffer: wgpu::Buffer,
    texture_bind_group: BindGroup,
    texture_bind_group_layout: BindGroupLayout,
    render_pipeline: RenderPipeline,
    dx_device: ID3D11Device,
    dx_context: ID3D11DeviceContext, //frame_scaler: Context,
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

        let features = adapter.features();

        if features.contains(wgpu::Features::TEXTURE_FORMAT_NV12) {
            println!("✅ NV12 supported!");
        } else {
            println!("❌ NV12 not supported! Falling back to manual YUV conversion.");
        }

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

        let (dxdevice, dxcontext) = open_d3d11_device();

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
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[surface_format],
        });

        let frame_size = winit::dpi::PhysicalSize::new(1920, 1080);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
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

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            view_formats: vec![surface_format],
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            width: 1920,
            height: 1080,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 10,
        };
        surface.configure(&device, &surface_config);

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

        let shader: wgpu::ShaderModule =
            device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

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
            dx_device: dxdevice,
            dx_context: dxcontext, //frame_scaler,
        };

        state
    }

    fn get_window(&self) -> &Window {
        &self.window
    }

    pub fn render(&self, frame: Arc<Video>) {
        //let format = frame.format();
        unsafe {
            let frame_ptr = frame.as_ptr();

            // Assume `frame` is your AVFrame
            let hw_frames_ctx = (*frame_ptr).hw_frames_ctx;
            if hw_frames_ctx.is_null() {
                panic!("No hardware frames context associated with the frame.");
            }

            // Get the AVHWFramesContext from the AVBufferRef
            let hw_frames_ctx_ref = (*hw_frames_ctx).data as *mut AVHWFramesContext;
            let hw_device_ctx = (*hw_frames_ctx_ref).device_ctx;
            if hw_device_ctx.is_null() {
                panic!("No hardware device context associated with the frames context.");
            }
            // Get the AVHWDeviceContext from the AVBufferRef
            let hwctx = (*hw_device_ctx).hwctx as *mut AVD3D11VADeviceContext;

            // Get the ID3D11Device and ID3D11DeviceContext
            //let d3d11_device: *mut ID3D11Device = (*hwctx).device;
            //let d3d11_device_context = (*hwctx).device_context;

            let d3d11_device = ID3D11Device::from_raw_borrowed(&(*hwctx).device).unwrap();
            let d3d11_device_context =
                ID3D11DeviceContext::from_raw_borrowed(&(*hwctx).device_context).unwrap();
            /*
            println!("ID3D11Device: {:?}", d3d11_device);
            println!("ID3D11DeviceContext: {:?}", d3d11_device_context);
            */
            let texture = (*frame_ptr).data[0] as *mut _;
            let texture_opt = ID3D11Texture2D::from_raw_borrowed(&texture);

            let (shared_handle, shared_text) =
                get_shared_texture_d3d11(d3d11_device, texture_opt.unwrap()).unwrap();
            let fence = DirectX11Fence::new(d3d11_device).unwrap();

            let shared_tex = shared_text.unwrap();
            let text = texture_opt.unwrap();

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            text.GetDesc(&mut desc);

            if let Ok(mutex) = shared_tex.cast::<IDXGIKeyedMutex>() {
                let acquire_result = mutex.AcquireSync(0, 500);

                if let Err(e) = acquire_result {
                    println!("❌ AcquireSync failed! Error: {:?}", e);
                }

                d3d11_device_context.CopyResource(text, &shared_tex);

                fence.synchronize(d3d11_device_context).unwrap();

                d3d11_device_context.Flush();

                let release_result = mutex.ReleaseSync(0);
                if let Err(e) = release_result {
                    println!("❌ ReleaseSync failed! Error: {:?}", e);
                }
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
                view_formats: &[wgpu::TextureFormat::Rgba8Unorm],
            };

            let texture = <Dx12 as wgpu::hal::Api>::Device::texture_from_raw(
                raw_image,
                desc.format,
                desc.dimension,
                desc.size,
                1,
                1,
            );

            let texture_asd = self.device.create_texture_from_hal::<Dx12>(texture, &desc);

            let y_plane_view = texture_asd.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::R8Unorm), // Y plane
                aspect: wgpu::TextureAspect::Plane0,        // First plane (Y)
                ..Default::default()
            });

            let uv_plane_view = texture_asd.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::Rg8Unorm), // UV plane
                aspect: wgpu::TextureAspect::Plane1,         // Second plane (UV)
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

            let shader = self
                .device
                .create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

            let pipeline_layout =
                self.device
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("Pipeline Layout"),
                        bind_group_layouts: &[&self.texture_bind_group_layout],
                        push_constant_ranges: &[],
                    });

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

            _ = CloseHandle(shared_handle);
            drop(frame);
        }
    }
}
