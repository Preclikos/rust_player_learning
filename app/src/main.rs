use std::sync::Arc;
use std::time::Duration;

use ffmpeg_next::software::scaling::Context;
use ffmpeg_next::util::frame::Video;
use player::Player;
use pollster::FutureExt;
use tokio::join;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use tokio::time::Instant;
use wgpu::util::DeviceExt;
use wgpu::{BindGroup, BindGroupLayout, RenderPipeline, Sampler, TextureFormat};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalSize, Size};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

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

struct State<'a> {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    frame_size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'a>,
    surface_format: TextureFormat,
    sampler: Sampler,
    render_texture: wgpu::Texture,
    vertex_buffer: wgpu::Buffer,
    texture_bind_group: BindGroup,
    texture_bind_group_layout: BindGroupLayout,
    render_pipeline: RenderPipeline,
    frame_scaler: Context,
}

impl<'a> State<'a> {
    async fn new(window: Arc<Window>) -> Self {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());

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
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await
            .unwrap();

        let size = window.inner_size();

        let cap = surface.get_capabilities(&adapter);
        let surface_format = cap.formats[0];

        let render_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Render Texture"),
            size: wgpu::Extent3d {
                width: 1280,
                height: 720,
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

        let state = State {
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
            frame_scaler,
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
            desired_maximum_frame_latency: 20,
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
        self.frame_scaler = ffmpeg_next::software::scaling::Context::get(
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

    fn render(&mut self, frame: Video) {
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

        if frame.width() != self.frame_size.width || frame.height() != self.frame_size.height {
            self.on_resize_frame(frame.clone());
        }

        let mut dst_frame = ffmpeg_next::util::frame::Video::new(
            self.frame_scaler.output().format,
            frame.width(),
            frame.height(),
        );

        _ = self.frame_scaler.run(&frame, &mut dst_frame);

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
        );

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
            render_pass.set_bind_group(0, &self.texture_bind_group, &[]);

            render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));

            render_pass.draw(0..6, 0..1);
        }

        // Submit the command in the queue to execute
        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        surface_texture.present();
    }
}

//#[derive(Default)]
struct App<'a> {
    state: Option<State<'a>>,
    receiver: Option<Receiver<Video>>,
    last_frame_time: Instant,
    frame_count: u32,
    player: Option<Player>,
}

impl ApplicationHandler for App<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (frame_tx, frame_rx) = mpsc::channel::<Video>(4);

        // Create window object
        let mut default_attrs = Window::default_attributes();
        default_attrs.inner_size = Some(Size::Physical(PhysicalSize::new(1280, 800)));
        let window = Arc::new(event_loop.create_window(default_attrs).unwrap());

        let mut player = Player::new(frame_tx);

        self.player = Some(player.clone());

        tokio::spawn(async move {
            //tearsofsteel_
            let _ = player
                .open_url("https://preclikos.cz/examples/tearsofsteel_raw/manifest.mpd")
                .await;

            let _ = player.prepare().await;

            let tracks = player.get_tracks();

            let tracks = tracks.unwrap();
            let selected_video = tracks.video.first().unwrap();
            let selected_video_representation = &selected_video.representations[2]; //.first().unwrap();

            player.set_video_track(selected_video, selected_video_representation);

            let selected_audio = tracks.audio.last().unwrap();
            let selected_audio_representation = &selected_audio.representations.last().unwrap();

            player.set_audio_track(selected_audio, selected_audio_representation);

            loop {
                let play = player.play();
                _ = join!(play.unwrap());
            }
        });

        let state = State::new(window.clone()).block_on();
        self.state = Some(state);

        self.receiver = Some(frame_rx);

        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let state: &mut State = self.state.as_mut().unwrap();
        let receiver = self.receiver.as_mut().unwrap();
        match event {
            WindowEvent::CloseRequested => {
                if let Some(player) = &self.player {
                    let mut player_clone = player.clone();
                    player_clone.stop().block_on();
                }
                println!("The close button was pressed; stopping");
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                let frame_duration = Duration::from_secs_f64(1.0 / 120.);

                self.frame_count += 1;

                if let Ok(frame) = receiver.try_recv() {
                    state.render(frame);
                }

                let elapsed = self.last_frame_time.elapsed();
                if elapsed < frame_duration {
                    std::thread::sleep(frame_duration - elapsed);
                }
                self.last_frame_time = Instant::now();

                state.get_window().request_redraw();
            }
            WindowEvent::Resized(size) => {
                state.resize(size);
            }
            WindowEvent::KeyboardInput {
                device_id: _,
                event,
                is_synthetic: _,
            } => match (event.physical_key, event.state) {
                (PhysicalKey::Code(KeyCode::Escape), ElementState::Pressed) => {
                    println!("Escape key pressed; exiting");
                    if let Some(player) = &self.player {
                        let mut player = player.clone();
                        player.stop().block_on();
                    }

                    event_loop.exit();
                }
                (PhysicalKey::Code(KeyCode::KeyF), ElementState::Pressed) => {
                    state
                        .get_window()
                        .set_fullscreen(Some(Fullscreen::Borderless(None)));
                }
                (PhysicalKey::Code(KeyCode::KeyW), ElementState::Pressed) => {
                    state.get_window().set_fullscreen(None);
                }
                (PhysicalKey::Code(KeyCode::KeyA), ElementState::Pressed) => {
                    let player = self.player.as_ref().unwrap();
                    player.volume(0.05);
                }
                (PhysicalKey::Code(KeyCode::KeyZ), ElementState::Pressed) => {
                    let player = self.player.as_ref().unwrap();
                    player.volume(-0.05);
                }
                _ => {}
            },
            _ => (),
        }
    }
}

#[tokio::main]
async fn main() {
    let event_loop = EventLoop::new().unwrap();

    // ControlFlow::Poll continuously runs the event loop, even if the OS hasn't
    // dispatched any events. This is ideal for games and similar applications.
    event_loop.set_control_flow(ControlFlow::Poll);

    // ControlFlow::Wait pauses the event loop if no events are available to process.
    // This is ideal for non-game applications that only update in response to user
    // input, and uses significantly less power/CPU time than ControlFlow::Poll.
    //event_loop.set_control_flow(ControlFlow::Wait);

    platform::prevent_screensaver();

    let mut app = App {
        state: None,
        receiver: None,
        last_frame_time: Instant::now(),
        frame_count: 0,
        player: None,
    };
    _ = event_loop.run_app(&mut app);
}

// Windows: Prevent sleep/screensaver
#[cfg(target_os = "windows")]
mod platform {
    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED,
    };

    pub fn prevent_screensaver() {
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED);
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::env;
    use std::ptr;

    extern crate x11;
    use x11::xlib::{XOpenDisplay, XResetScreenSaver};

    use wayland_client::Display;

    pub fn prevent_screensaver() {
        if let Ok(xdg_session_type) = env::var("XDG_SESSION_TYPE") {
            match xdg_session_type.as_str() {
                "x11" => prevent_screensaver_x11(),
                "wayland" => prevent_screensaver_wayland(),
                _ => eprintln!("Unsupported display server: {}", xdg_session_type),
            }
        } else {
            eprintln!("Failed to detect XDG_SESSION_TYPE");
        }
    }

    fn prevent_screensaver_x11() {
        unsafe {
            let display = XOpenDisplay(ptr::null());
            if !display.is_null() {
                XResetScreenSaver(display);
            } else {
                eprintln!("Failed to open X display");
            }
        }
    }

    fn prevent_screensaver_wayland() {
        if let Ok(display) = Display::connect_to_env() {
            eprintln!("Wayland support requires compositor-specific methods.");
        } else {
            eprintln!("Failed to connect to Wayland display");
        }
    }
}

// Default implementation for other platforms
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod platform {
    pub fn prevent_screensaver() {
        eprintln!("Screensaver prevention is not supported on this platform.");
    }
}
