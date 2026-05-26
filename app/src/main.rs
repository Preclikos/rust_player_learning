use std::sync::Arc;
use std::time::Duration;

use player::Player;
use pollster::FutureExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::Instant;
use winit::application::ApplicationHandler;
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
        default_attrs.title = "Video Player".to_string();
        //default_attrs.inner_size = Some(Size::Physical(PhysicalSize::new(1280, 800)));
        let window = Arc::new(event_loop.create_window(default_attrs).unwrap());

        let player = Player::new(window.clone());

        self.player = Some(player.clone());

        // Hand the player off to the shared test-playback fixture, then
        // run the desktop-only stdin console against the same handle.
        let console_player = player.clone();
        tokio::spawn(async move {
            app_shared::run_test_playback(player).await;
            run_console(console_player).await;
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
                    player.stop().block_on();
                }
                log::info!("close button pressed; stopping");
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
                player.resize(size);
            }
            WindowEvent::KeyboardInput {
                device_id: _,
                event,
                is_synthetic: _,
            } => match (event.physical_key, event.state) {
                (PhysicalKey::Code(KeyCode::Escape), ElementState::Pressed) => {
                    log::info!("Escape pressed; exiting");
                    if let Some(player) = &self.player {
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
                (PhysicalKey::Code(KeyCode::ArrowLeft), ElementState::Pressed) => {
                    player.seek_relative(-10_000);
                }
                (PhysicalKey::Code(KeyCode::ArrowRight), ElementState::Pressed) => {
                    player.seek_relative(10_000);
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
    env_logger::init();
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

    // The play loop and stdin reader are tokio tasks that don't observe winit's exit.
    // Skip the runtime's Drop wait — we've already stopped the audio device.
    std::process::exit(0);
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

    use x11::xlib::{XOpenDisplay, XResetScreenSaver};

    use wayland_client::Connection;

    pub fn prevent_screensaver() {
        if let Ok(xdg_session_type) = env::var("XDG_SESSION_TYPE") {
            match xdg_session_type.as_str() {
                "x11" => prevent_screensaver_x11(),
                "wayland" => prevent_screensaver_wayland(),
                _ => log::warn!("Unsupported display server: {}", xdg_session_type),
            }
        } else {
            log::warn!("Failed to detect XDG_SESSION_TYPE");
        }
    }

    fn prevent_screensaver_x11() {
        unsafe {
            let display = XOpenDisplay(ptr::null());
            if !display.is_null() {
                XResetScreenSaver(display);
            } else {
                log::warn!("Failed to open X display");
            }
        }
    }

    fn prevent_screensaver_wayland() {
        if let Ok(_display) = Connection::connect_to_env() {
            log::warn!("Wayland support requires compositor-specific methods.");
        } else {
            log::warn!("Failed to connect to Wayland display");
        }
    }
}

// Default implementation for other platforms
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod platform {
    pub fn prevent_screensaver() {
        log::warn!("Screensaver prevention is not supported on this platform.");
    }
}

async fn run_console(player: Player) {
    print_menu(&player);
    println!("Commands: l = list, v <i> = video quality, a <i> = audio track");

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();

    while let Ok(Some(line)) = reader.next_line().await {
        let trimmed = line.trim();
        let mut parts = trimmed.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next();

        match cmd {
            "" => {}
            "l" | "list" => print_menu(&player),
            "v" => match arg.and_then(|s| s.parse::<usize>().ok()) {
                Some(i) => pick_video(&player, i),
                None => println!("usage: v <index>"),
            },
            "a" => match arg.and_then(|s| s.parse::<usize>().ok()) {
                Some(i) => pick_audio(&player, i),
                None => println!("usage: a <index>"),
            },
            other => println!("unknown command: {} (try l, v <i>, a <i>)", other),
        }
    }
}

fn print_menu(player: &Player) {
    let tracks = match player.get_tracks() {
        Ok(t) => t,
        Err(e) => {
            println!("no tracks: {}", e);
            return;
        }
    };
    let current_video_id = player.current_video_representation().map(|r| r.id);
    let current_audio_id = player.current_audio_representation().map(|r| r.id);

    println!("\n=== Available tracks ===");
    println!("Video:");
    let mut i = 0;
    for adaptation in &tracks.video {
        for repr in &adaptation.representations {
            let marker = if Some(repr.id) == current_video_id {
                " *"
            } else {
                ""
            };
            println!(
                "  [{}] {}x{}  bw={}  {}{}",
                i, repr.width, repr.height, repr.bandwidth, repr.codecs, marker
            );
            i += 1;
        }
    }
    println!("Audio:");
    let mut i = 0;
    for adaptation in &tracks.audio {
        for repr in &adaptation.representations {
            let marker = if Some(repr.id) == current_audio_id {
                " *"
            } else {
                ""
            };
            println!(
                "  [{}] lang={}  bw={}  {}{}",
                i, adaptation.lang, repr.bandwidth, repr.codecs, marker
            );
            i += 1;
        }
    }
    println!("========================");
}

fn pick_video(player: &Player, index: usize) {
    let tracks = match player.get_tracks() {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut i = 0;
    for adaptation in &tracks.video {
        for repr in &adaptation.representations {
            if i == index {
                println!(
                    "switching video to [{}] {}x{} bw={}",
                    i, repr.width, repr.height, repr.bandwidth
                );
                player.change_video_track(repr);
                return;
            }
            i += 1;
        }
    }
    println!("invalid video index");
}

fn pick_audio(player: &Player, index: usize) {
    let tracks = match player.get_tracks() {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut i = 0;
    for adaptation in &tracks.audio {
        for repr in &adaptation.representations {
            if i == index {
                println!(
                    "switching audio to [{}] lang={} bw={}",
                    i, adaptation.lang, repr.bandwidth
                );
                player.change_audio_track(adaptation, repr);
                return;
            }
            i += 1;
        }
    }
    println!("invalid audio index");
}
