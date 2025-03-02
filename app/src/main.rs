use std::sync::Arc;
use std::time::Duration;

use player::Player;
use pollster::FutureExt;
use tokio::join;
use tokio::time::Instant;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalSize, Size};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

struct App {
    window: Option<Arc<Window>>,
    last_frame_time: Instant,
    frame_count: u32,
    player: Option<Player>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Create window object
        let mut default_attrs = Window::default_attributes();
        default_attrs.inner_size = Some(Size::Physical(PhysicalSize::new(1280, 800)));
        let window = Arc::new(event_loop.create_window(default_attrs).unwrap());

        let mut player = Player::new(window.clone());

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

        self.window = Some(window.clone());

        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let player: &mut Player = self.player.as_mut().unwrap();
        let window = Arc::clone(self.window.as_ref().unwrap());

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

                let elapsed = self.last_frame_time.elapsed();
                if elapsed < frame_duration {
                    std::thread::sleep(frame_duration - elapsed);
                }
                self.last_frame_time = Instant::now();

                window.request_redraw();
            }
            WindowEvent::Resized(size) => {
                //state.resize(size);
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
                    window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                }
                (PhysicalKey::Code(KeyCode::KeyW), ElementState::Pressed) => {
                    window.set_fullscreen(None);
                }
                (PhysicalKey::Code(KeyCode::KeyA), ElementState::Pressed) => {
                    player.volume(0.05);
                }
                (PhysicalKey::Code(KeyCode::KeyZ), ElementState::Pressed) => {
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
        window: None,
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
