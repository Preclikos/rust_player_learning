use std::sync::Arc;

use ffmpeg_next::util::frame::Frame;
use player::Player;
use tokio::sync::mpsc::Receiver;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

struct State {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
}

impl State {
    async fn new(window: Arc<Window>) -> State {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor::default(),
                None, // Trace path
            )
            .await
            .unwrap();

        let size = window.inner_size();

        let surface = instance.create_surface(window.clone()).unwrap();
        let cap = surface.get_capabilities(&adapter);
        let surface_format = cap.formats[0];

        let state = State {
            window,
            device,
            queue,
            size,
            surface,
            surface_format,
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
            // Request compatibility with the sRGB-format texture view we‘re going to create later.
            view_formats: vec![self.surface_format.add_srgb_suffix()],
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            width: self.size.width,
            height: self.size.height,
            desired_maximum_frame_latency: 2,
            present_mode: wgpu::PresentMode::AutoVsync,
        };
        self.surface.configure(&self.device, &surface_config);
    }

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.size = new_size;

        // reconfigure the surface
        self.configure_surface();
    }

    /*fn convert_yuv_to_rgba(frame: Frame) -> Vec<u8> {
        let (width, height) = (frame.width(), frame.height());
        let yuv_data = frame.data(0); // Y plane
        let uv_data = frame.data(1); // U/V planes

        let mut rgba_data = Vec::with_capacity((width * height) * 4);

        for y in 0..height {
            for x in 0..width {
                // YUV420p to RGBA conversion
                let y_index = y * width + x;
                let u_index = ((y / 2) * (width / 2)) + (x / 2);
                let v_index = ((y / 2) * (width / 2)) + (x / 2);

                let y_val = yuv_data[y_index];
                let u_val = uv_data[u_index];
                let v_val = uv_data[v_index];

                // Convert to RGB using the standard YUV to RGB formula
                let r = (y_val as f32 + 1.402 * (v_val as f32 - 128.0)) as u8;
                let g = (y_val as f32
                    - 0.344136 * (u_val as f32 - 128.0)
                    - 0.714136 * (v_val as f32 - 128.0)) as u8;
                let b = (y_val as f32 + 1.772 * (u_val as f32 - 128.0)) as u8;

                rgba_data.push(r);
                rgba_data.push(g);
                rgba_data.push(b);
                rgba_data.push(255); // Alpha set to fully opaque
            }
        }

        rgba_data
    }*/

    fn render(&mut self, frame_rx: Receiver<Frame>) {
        // Create texture view
        let surface_texture = self
            .surface
            .get_current_texture()
            .expect("failed to acquire next swapchain texture");
        let texture_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                // Without add_srgb_suffix() the image we will be working with
                // might not be "gamma correct".
                format: Some(self.surface_format.add_srgb_suffix()),
                ..Default::default()
            });
        /*
        match frame_rx.try_recv() {
            Ok(frame) => {
                let rgba_frame = convert_yuv_to_rgba(frame);

                // Upload the RGBA data to the texture
                let rgba_data = rgba_frame.as_bytes();
                self.queue.write_texture(
                    wgpu::ImageCopyTexture {
                        texture: &surface_texture.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    rgba_data,
                    wgpu::ImageDataLayout {
                        offset: 0,
                        bytes_per_row: Some(4 * width), // RGBA has 4 bytes per pixel
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
                let renderpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
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

                // If you wanted to call any drawing commands, they would go here.

                // End the renderpass.
                drop(renderpass);

                // Submit the command in the queue to execute
                self.queue.submit([encoder.finish()]);
                self.window.pre_present_notify();
                surface_texture.present();
            }
            Err(e) => {}
        }*/
    }
}
/*
fn convert_yuv_to_rgba(frame: Frame) -> Vec<u8> {
    let (width, height) = (frame.width(), frame.height());
    let yuv_data = frame.data(0); // Y plane
    let uv_data = frame.data(1); // U/V planes

    let mut rgba_data = Vec::with_capacity((width * height) * 4);

    for y in 0..height {
        for x in 0..width {
            // YUV420p to RGBA conversion
            let y_index = y * width + x;
            let u_index = ((y / 2) * (width / 2)) + (x / 2);
            let v_index = ((y / 2) * (width / 2)) + (x / 2);

            let y_val = yuv_data[y_index];
            let u_val = uv_data[u_index];
            let v_val = uv_data[v_index];

            // Convert to RGB using the standard YUV to RGB formula
            let r = (y_val as f32 + 1.402 * (v_val as f32 - 128.0)) as u8;
            let g = (y_val as f32
                - 0.344136 * (u_val as f32 - 128.0)
                - 0.714136 * (v_val as f32 - 128.0)) as u8;
            let b = (y_val as f32 + 1.772 * (u_val as f32 - 128.0)) as u8;

            rgba_data.push(r);
            rgba_data.push(g);
            rgba_data.push(b);
            rgba_data.push(255); // Alpha set to fully opaque
        }
    }

    rgba_data
}
*/
#[derive(Default)]
struct App {
    state: Option<State>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Create window object
        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes())
                .unwrap(),
        );

        let state = pollster::block_on(State::new(window.clone()));
        self.state = Some(state);

        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let state = self.state.as_mut().unwrap();
        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
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
    let mut player = Player::new();

    let _ = player
        .open_url("https://preclikos.cz/examples/raw/manifest.mpd")
        .await;

    let _ = player.prepare().await;

    let tracks = player.get_tracks();

    let tracks = tracks.unwrap();
    let selected_video = tracks.video.first().unwrap();
    let selected_representation = selected_video.representations.last().unwrap();

    player.set_video_track(selected_video, selected_representation);

    //player.play().await;

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
