use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use ffmpeg_next::util::frame::Video as Frame;
use player::Player;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use wgpu::{BindGroup, BindGroupLayout, RenderPipeline, Sampler, TextureFormat};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalSize, Size};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};
struct State {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: TextureFormat,
    sampler: Sampler,
    render_texture: wgpu::Texture,
    texture_bind_group: BindGroup,
    texture_bind_group_layout: BindGroupLayout,
    render_pipeline: RenderPipeline,
}

impl State {
    async fn new(window: Arc<Window>) -> State {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await
            .unwrap();

        let size = window.inner_size();

        let surface = instance.create_surface(window.clone()).unwrap();
        let cap = surface.get_capabilities(&adapter);
        let surface_format = cap.formats[0];

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

        // Create shader module
        let shader_source = r#"
        @group(0) @binding(0) var my_texture: texture_2d<f32>;
        @group(0) @binding(1) var my_sampler: sampler;

        struct VertexOutput {
            @builtin(position) position: vec4<f32>,
            @location(0) tex_coords: vec2<f32>,
        };

        @vertex
        fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
            var positions = array<vec2<f32>, 6>(
                vec2<f32>(-1.0, -1.0),
                vec2<f32>(1.0, -1.0),
                vec2<f32>(-1.0, 1.0),
                vec2<f32>(-1.0, 1.0),
                vec2<f32>(1.0, -1.0),
                vec2<f32>(1.0, 1.0)
            );
            var tex_coords = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0), // Invert Y coordinate
        vec2<f32>(1.0, 1.0), // Invert Y coordinate
        vec2<f32>(0.0, 0.0), // Invert Y coordinate
        vec2<f32>(0.0, 0.0), // Invert Y coordinate
        vec2<f32>(1.0, 1.0), // Invert Y coordinate
        vec2<f32>(1.0, 0.0)  // Invert Y coordinate
            );

            var output: VertexOutput;
            output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
            output.tex_coords = tex_coords[vertex_index];
            return output;
        }

        @fragment
        fn fs_main(@location(0) in_tex_coords: vec2<f32>) -> @location(0) vec4<f32> {
            return textureSample(my_texture, my_sampler, in_tex_coords);
        }
    "#;

        let shader_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Shader"),
            source: wgpu::ShaderSource::Wgsl(shader_source.into()),
        });

        // Create pipeline layout
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout"),
            bind_group_layouts: &[&texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline_compilation_options = wgpu::PipelineCompilationOptions {
            constants: &std::collections::HashMap::new(), // Use this to specify pipeline-overridable constants
            zero_initialize_workgroup_memory: true, // Set to true if you want to zero-initialize workgroup memory
        };

        // Create render pipeline
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader_module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: pipeline_compilation_options.clone(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader_module,
                entry_point: Some("fs_main"),
                compilation_options: pipeline_compilation_options.clone(),
                targets: &[Some(wgpu::ColorTargetState {
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                    format: surface_format,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let state = State {
            window,
            device,
            queue,
            size,
            surface,
            surface_format,
            sampler,
            render_texture,
            texture_bind_group,
            texture_bind_group_layout,
            render_pipeline,
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

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.size = new_size;

        self.render_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Render Texture"),
            size: wgpu::Extent3d {
                width: new_size.width & !1,
                height: new_size.height & !1,
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

        // Reconfigure the surface
        self.configure_surface();
    }

    fn render(&mut self, frame: Frame) {
        // Create texture view for rendering
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

        let width = self.size.width & !1;
        let height = self.size.height & !1;

        let dst_format = ffmpeg_next::format::Pixel::BGRA;

        let mut dst_frame = ffmpeg_next::util::frame::Video::new(dst_format, width, height);

        _ = ffmpeg_next::software::scaling::Context::get(
            frame.format(),
            frame.width(),
            frame.height(),
            dst_format,
            width,
            height,
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )
        .unwrap()
        .run(&frame, &mut dst_frame);

        // Upload the RGBA data to the copy texture
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
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        // Render the frame
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), // Clear with black
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // Bind the texture and render pipeline
            render_pass.set_pipeline(&self.render_pipeline);
            render_pass.set_bind_group(0, &self.texture_bind_group, &[]);
            render_pass.draw(0..6, 0..1);
        }

        // Submit the command in the queue to execute
        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        surface_texture.present();
    }
}

#[derive(Default)]
struct App {
    state: Option<State>,
    receiver: Option<Receiver<Frame>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>(3);
        // Create window object
        let mut default_attrs = Window::default_attributes();
        default_attrs.inner_size = Some(Size::Physical(PhysicalSize::new(1280, 800)));
        let window = Arc::new(event_loop.create_window(default_attrs).unwrap());

        tokio::spawn(async move {
            let mut player = Player::new(frame_tx);

            let _ = player
                .open_url("https://preclikos.cz/examples/raw/manifest.mpd")
                .await;

            let _ = player.prepare().await;

            let tracks = player.get_tracks();

            let tracks = tracks.unwrap();
            let selected_video = tracks.video.first().unwrap();
            let selected_representation = &selected_video.representations.last().unwrap();

            player.set_video_track(selected_video, selected_representation);

            let a = player.play().await;
        });

        let state = pollster::block_on(State::new(window.clone()));
        self.state = Some(state);

        self.receiver = Some(frame_rx);

        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let state = self.state.as_mut().unwrap();
        let receiver = self.receiver.as_mut().unwrap();
        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                if let Ok(frame) = receiver.try_recv() {
                    let frame_duration = Duration::from_millis(24000 / 1001);
                    sleep(frame_duration);
                    state.render(frame);
                }

                //state.render();
                // Emits a new redraw requested event.
                state.get_window().request_redraw();
            }
            WindowEvent::Resized(size) => {
                // Reconfigures the size of the surface. We do not re-render
                // here as this event is always followed up by redraw request.
                state.resize(size);
            }
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
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::default();
    _ = event_loop.run_app(&mut app);
}
