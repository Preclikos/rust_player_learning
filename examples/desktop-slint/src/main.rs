//! Slint in-app host for the player — the composition BlackZoneDesktop ships,
//! reduced to what matters for reproducing and measuring it:
//!
//! * ONE wgpu device shared by Slint (FemtoVG on wgpu) and the player
//!   (`Player::new_offscreen`), same backend/features as BlackZone's gpu.rs;
//! * the video reaches the window as `slint::Image::try_from(texture)`, pulled
//!   in every Slint render tick (BlackZone's pull mode) plus the frame-ready
//!   push;
//! * BlackZone's UI-present gauge (Slint render time, present cadence, UI
//!   judder) next to the player's own Stats, logged and summarised as JSON.
//!
//! The windowed `examples/desktop` never goes through this path, which is
//! why subtitle/UI problems seen in BlackZone did not show up there.
//!
//! ```text
//! cargo run --release -- [--url MPD] [--key KID:KEY]... [--sub file.vtt] [--sub-offset-ms N]
//!                        [--secs N] [--ui-load-ms N] [--no-abr] [--no-play]
//!                        [--json out.json]
//! cargo run --release -- --bench [--json out.json]
//! ```
//! * `--ui-load-ms N` stalls every Slint render by a random 0..N ms on the UI
//!   thread — a slow machine on demand. `--secs 0` runs until the window closes.
//! * `--no-play`: UI only, no decoder — tells UI-side issues from player ones.
//! * `--bench`: per-operation cost of the device's memory policy (small
//!   buffer writes, texture uploads/creation), then exit — the A/B that showed
//!   DX12 without suballocation costing ~2 ms per Slint frame.
//!
//! Own workspace (Slint stays out of the engine's): build from this directory.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use player::{AbrStrategy, ExternalSubtitleOptions, Player, PlayerEvent};
use slint::ComponentHandle;

slint::slint! {
    export component App inherits Window {
        title: "rust_player — Slint in-app harness";
        preferred-width: 1280px;
        preferred-height: 720px;
        background: #000000;
        in property <image> video_frame;
        in property <string> hud;
        callback video_size_changed(float, float);

        // Same structure as BlackZone's player screen: black rect, the video
        // Image fitted inside, scrims + a seek bar drawn over it.
        Rectangle {
            background: #000000;
            init => { root.video_size_changed(self.width / 1px, self.height / 1px); }
            changed width => { root.video_size_changed(self.width / 1px, self.height / 1px); }
            changed height => { root.video_size_changed(self.width / 1px, self.height / 1px); }
            Image {
                width: 100%;
                height: 100%;
                image-fit: contain;
                source: root.video_frame;
            }
            Rectangle {
                y: 0;
                height: 96px;
                background: @linear-gradient(180deg, #000000CC, #00000000);
            }
            Rectangle {
                y: parent.height - 96px;
                height: 96px;
                background: @linear-gradient(0deg, #000000CC, #00000000);
                Rectangle {
                    x: 24px;
                    y: parent.height - 30px;
                    width: parent.width - 48px;
                    height: 4px;
                    background: #FFFFFF40;
                    Rectangle { x: 0; width: parent.width * 0.35; background: #E50914; }
                }
            }
            Text {
                x: 12px;
                y: 12px;
                text: root.hud;
                color: rgb(153, 238, 153);
                font-size: 12px;
                font-family: "Consolas";
            }
        }
    }
}

const TEST_MANIFEST_URL: &str = "https://preclikos.cz/examples/encrypted/manifest.mpd";

struct Args {
    url: String,
    keys: HashMap<String, String>,
    sub: Option<String>,
    sub_offset_ms: i64,
    secs: u64,
    ui_load_ms: u64,
    abr: bool,
    json: Option<String>,
    no_play: bool,
    bench: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        url: TEST_MANIFEST_URL.into(),
        keys: HashMap::new(),
        sub: None,
        sub_offset_ms: 0,
        secs: 60,
        ui_load_ms: 0,
        abr: true,
        json: None,
        no_play: false,
        bench: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--url" => a.url = val(),
            "--key" => {
                let kv = val();
                let (k, v) = kv.split_once(':').expect("--key expects KIDHEX:KEYHEX");
                a.keys.insert(k.to_string(), v.to_string());
            }
            "--sub" => a.sub = Some(val()),
            "--sub-offset-ms" => a.sub_offset_ms = val().parse().expect("--sub-offset-ms"),
            "--secs" => a.secs = val().parse().expect("--secs"),
            "--ui-load-ms" => a.ui_load_ms = val().parse().expect("--ui-load-ms"),
            "--json" => a.json = Some(val()),
            "--no-abr" => a.abr = false,
            "--no-play" => a.no_play = true,
            "--bench" => a.bench = true,
            other => panic!("unknown argument {other}"),
        }
    }
    if a.keys.is_empty() && a.url == TEST_MANIFEST_URL {
        a.keys.insert("0fd37dac41c0e987e68d43b801b1210c".into(), "fd8d9f408c2bd702970afcd3b219e791".into());
        a.keys.insert("519af81ab2d284f52aa8257d96b5e4bd".into(), "627ef72b42d98770dec20ecab46cd1f4".into());
    }
    a
}

// ---------------------------------------------------------------------------
// Shared device — field-for-field BlackZoneDesktop/app/src/gpu.rs.
// ---------------------------------------------------------------------------

struct SharedGpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    backend: wgpu::Backend,
}

async fn shared_gpu() -> SharedGpu {
    #[cfg(target_os = "windows")]
    let backends = wgpu::Backends::DX12;
    #[cfg(target_os = "linux")]
    let backends = wgpu::Backends::VULKAN;
    #[cfg(target_os = "macos")]
    let backends = wgpu::Backends::METAL;
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    });
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .expect("no suitable wgpu adapter");
    let backend = adapter.get_info().backend;
    let desired = if backend == wgpu::Backend::Metal {
        wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
    } else {
        wgpu::Features::TEXTURE_FORMAT_NV12
            | wgpu::Features::TEXTURE_FORMAT_P010
            | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
    };
    let alim = adapter.limits();
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("desktop-slint shared device (slint + player)"),
            required_features: adapter.features() & desired,
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: alim.max_texture_dimension_2d,
                max_texture_dimension_1d: alim.max_texture_dimension_1d,
                ..wgpu::Limits::default()
            },
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::default(),
            trace: wgpu::Trace::Off,
        })
        .await
        .expect("request_device failed");
    // BlackZone logs and carries on; count them here so a run can fail on it.
    device.on_uncaptured_error(Arc::new(|e: wgpu::Error| {
        WGPU_ERRORS.fetch_add(1, Ordering::Relaxed);
        log::error!("wgpu error (continuing): {e}");
    }));
    log::info!("shared gpu: backend={backend:?} adapter={}", adapter.get_info().name);
    SharedGpu { instance, adapter, device, queue, backend }
}

// ---------------------------------------------------------------------------
// UI-present gauge — the same counters BlackZone keeps (main.rs UI_PRESENT).
// ---------------------------------------------------------------------------

static WGPU_ERRORS: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct UiPresent {
    pending_since_ns: AtomicU64,
    render_t0_ns: AtomicU64,
    renders: AtomicU64,
    render_ms_max: AtomicU64,
    render_us_ewma: AtomicU64,
    slow_renders: AtomicU64,
    presented: AtomicU64,
    late_frames: AtomicU64,
    lag_max_ms: AtomicU64,
    last_present_ns: AtomicU64,
    interval_ewma_us: AtomicU64,
    ui_judder: AtomicU64,
    gaps: AtomicU64,
    int_lt25: AtomicU64,
    int_25_41: AtomicU64,
    int_42_58: AtomicU64,
    int_gt58: AtomicU64,
}

static UI: OnceLock<UiPresent> = OnceLock::new();
fn ui_gauge() -> &'static UiPresent {
    UI.get_or_init(UiPresent::default)
}

fn mono_ns() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_nanos() as u64 + 1
}

fn load(a: &AtomicU64) -> u64 {
    a.load(Ordering::Relaxed)
}

/// The player's newest `Stats` event (cumulative counters).
#[derive(Default, Clone)]
struct PlayerStats {
    position_ms: u64,
    decoded: u64,
    dropped: u64,
    late: u64,
    judder: u64,
    render_gap_max_ms: u64,
    stalls: u64,
    hist: [u64; 4],
    decoder: String,
    resolution: Option<(u32, u32)>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        "warn,player=info,player::renderers::subtitle=debug,desktop_slint=info",
    ))
    .init();
    let args = parse_args();

    let gpu = pollster::block_on(shared_gpu());
    if args.bench {
        let out = bench(&gpu.device, &gpu.queue);
        println!("{out}");
        if let Some(path) = &args.json {
            let _ = std::fs::write(path, &out);
        }
        return;
    }
    slint::BackendSelector::new()
        .require_wgpu_29(slint::wgpu_29::WGPUConfiguration::Manual {
            instance: gpu.instance.clone(),
            adapter: gpu.adapter.clone(),
            device: gpu.device.clone(),
            queue: gpu.queue.clone(),
        })
        .select()
        .expect("slint backend with the shared wgpu device");
    let ui = App::new().expect("create window");

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let mut player = rt.block_on(Player::new_offscreen(
        gpu.device.clone(),
        gpu.queue.clone(),
        gpu.backend,
        1280,
        720,
    ));
    if !args.keys.is_empty() {
        player.set_clearkey(args.keys.clone()).expect("set_clearkey");
    }

    // Player Stats → shared snapshot.
    let stats = Arc::new(Mutex::new(PlayerStats::default()));
    {
        let stats = Arc::clone(&stats);
        let mut rx = player.events();
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(PlayerEvent::Stats {
                        position_ms,
                        video_frames_decoded,
                        video_frames_dropped,
                        video_late_frames,
                        stall_events,
                        render_gap_max_ms,
                        judder_frames,
                        interval_hist,
                        decoder_name,
                        current_resolution,
                        ..
                    }) => {
                        *stats.lock().unwrap() = PlayerStats {
                            position_ms,
                            decoded: video_frames_decoded,
                            dropped: video_frames_dropped,
                            late: video_late_frames,
                            judder: judder_frames,
                            render_gap_max_ms,
                            stalls: stall_events,
                            hist: interval_hist,
                            decoder: decoder_name,
                            resolution: current_resolution,
                        };
                    }
                    Ok(PlayerEvent::Error { kind, detail }) => log::error!("player error {kind:?}: {detail}"),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    // Offscreen render target follows the on-screen video size (BlackZone's
    // video_size_changed → player.resize).
    {
        let p = player.clone();
        let ui_weak = ui.as_weak();
        ui.on_video_size_changed(move |w, h| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let sf = ui.window().scale_factor();
            p.resize(player::PhysicalSize::new(((w * sf) as u32).max(1), ((h * sf) as u32).max(1)));
        });
    }

    // Push: frame-ready → re-wrap the published texture (BlackZone does both
    // this and the pull below).
    {
        let ui_weak = ui.as_weak();
        let p = player.clone();
        player.set_frame_ready_callback(move || {
            ui_gauge().pending_since_ns.store(mono_ns(), Ordering::Relaxed);
            let ui_weak = ui_weak.clone();
            let p = p.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    match slint::Image::try_from(p.current_video_texture()) {
                        Ok(img) => ui.set_video_frame(img),
                        Err(e) => log::error!("Image::try_from: {e:?}"),
                    }
                }
            });
        });
    }

    // Pull mode + UI-present gauge, as in BlackZone's rendering notifier.
    {
        let ui_weak = ui.as_weak();
        let p = player.clone();
        let ui_load_ms = args.ui_load_ms;
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        if let Err(e) = ui.window().set_rendering_notifier(move |state, _| {
            let g = ui_gauge();
            match state {
                slint::RenderingState::BeforeRendering => {
                    g.render_t0_ns.store(mono_ns(), Ordering::Relaxed);
                    if let Some(ui) = ui_weak.upgrade() {
                        if let Ok(img) = slint::Image::try_from(p.current_video_texture()) {
                            ui.set_video_frame(img);
                        }
                    }
                    if ui_load_ms > 0 {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        std::thread::sleep(Duration::from_millis((seed >> 33) % (ui_load_ms + 1)));
                    }
                }
                slint::RenderingState::AfterRendering => {
                    let now = mono_ns();
                    let t0 = g.render_t0_ns.swap(0, Ordering::Relaxed);
                    if t0 > 0 {
                        let dur_us = now.saturating_sub(t0) / 1_000;
                        g.renders.fetch_add(1, Ordering::Relaxed);
                        g.render_ms_max.fetch_max(dur_us / 1_000, Ordering::Relaxed);
                        if dur_us >= 100_000 {
                            g.slow_renders.fetch_add(1, Ordering::Relaxed);
                            log::warn!("[ui] slow render: {} ms", dur_us / 1_000);
                        }
                        let ewma = g.render_us_ewma.load(Ordering::Relaxed);
                        g.render_us_ewma
                            .store(if ewma == 0 { dur_us } else { (ewma * 7 + dur_us) / 8 }, Ordering::Relaxed);
                    }
                    let since = g.pending_since_ns.swap(0, Ordering::Relaxed);
                    if since > 0 {
                        let lag_ms = now.saturating_sub(since) / 1_000_000;
                        g.lag_max_ms.fetch_max(lag_ms, Ordering::Relaxed);
                        if lag_ms > 20 {
                            g.late_frames.fetch_add(1, Ordering::Relaxed);
                        }
                        g.presented.fetch_add(1, Ordering::Relaxed);
                        let last = g.last_present_ns.swap(now, Ordering::Relaxed);
                        if last > 0 {
                            let interval_us = now.saturating_sub(last) / 1_000;
                            if (100_000..200_000).contains(&interval_us) {
                                g.gaps.fetch_add(1, Ordering::Relaxed);
                                log::warn!("[ui] video present gap {} ms", interval_us / 1_000);
                            }
                            if interval_us < 200_000 {
                                let ms = interval_us / 1_000;
                                let bucket = match ms {
                                    0..=24 => &g.int_lt25,
                                    25..=41 => &g.int_25_41,
                                    42..=58 => &g.int_42_58,
                                    _ => &g.int_gt58,
                                };
                                bucket.fetch_add(1, Ordering::Relaxed);
                                let ewma = g.interval_ewma_us.load(Ordering::Relaxed);
                                if ewma > 0 && (interval_us as i64 - ewma as i64).unsigned_abs() / 1_000 > 10 {
                                    g.ui_judder.fetch_add(1, Ordering::Relaxed);
                                }
                                g.interval_ewma_us.store(
                                    if ewma == 0 { interval_us } else { (ewma * 7 + interval_us) / 8 },
                                    Ordering::Relaxed,
                                );
                            }
                        }
                    }
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.window().request_redraw();
                    }
                }
                _ => {}
            }
        }) {
            panic!("rendering notifier unsupported: {e:?}");
        }
    }

    // Open → prepare → pick tracks → sidecar → play, on the runtime.
    let started = Arc::new(Mutex::new(None::<Instant>));
    if args.no_play {
        // UI only: no decoder, no D3D11VA, no video texture updates.
        *started.lock().unwrap() = Some(Instant::now());
    } else {
        let started = Arc::clone(&started);
        let url = args.url.clone();
        let sub = args.sub.clone();
        let sub_offset_ms = args.sub_offset_ms;
        let abr = args.abr;
        let p = &mut player;
        rt.block_on(async {
            p.open_url(&url).await.expect("open_url");
            p.prepare().await.expect("prepare");
        });
        let tracks = player.get_tracks().expect("get_tracks");
        let va = tracks.video.first().expect("no video adaptation").clone();
        // Start like BlackZone does: a mid rung, ABR from there.
        let mut reps = va.representations.clone();
        reps.sort_by_key(|r| r.bandwidth);
        let start = reps
            .iter()
            .rev()
            .find(|r| r.bandwidth <= 6_000_000)
            .or(reps.first())
            .expect("no video reps")
            .clone();
        player.set_video_track(&va, &start);
        if abr {
            player.set_abr_strategy(AbrStrategy::BandwidthEwma { safety_factor: 1.25 });
        }
        let aa = tracks.audio.first().expect("no audio adaptation").clone();
        let ar = aa.representations.first().expect("no audio reps").clone();
        player.set_audio_track(&aa, &ar);
        if let Some(path) = sub {
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let rep = player
                .add_external_subtitle_track(
                    &bytes,
                    ExternalSubtitleOptions { time_offset_ms: sub_offset_ms, ..Default::default() },
                )
                .expect("subtitle file");
            player.set_subtitle_track(&rep);
        } else if let Some(t) = tracks.text.first().and_then(|a| a.representations.first().cloned()) {
            player.set_subtitle_track(&t);
        }
        let _guard = rt.enter();
        let handle = player.play().expect("play");
        std::mem::forget(handle); // runs for the process lifetime
        *started.lock().unwrap() = Some(Instant::now());
    }

    // HUD + periodic log line + end of run.
    let hud_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        let stats = Arc::clone(&stats);
        let started = Arc::clone(&started);
        let secs = args.secs;
        let json = args.json.clone();
        let ui_load_ms = args.ui_load_ms;
        let p = player.clone();
        let mut tick = 0u64;
        hud_timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
            tick += 1;
            let s = stats.lock().unwrap().clone();
            let g = ui_gauge();
            let text = format!(
                "pos {:.1}s  {}  {}\n\
                 player: decoded {} dropped {} late {} judder {} gap_max {} ms stalls {}  hist {:?}\n\
                 ui: renders {} (avg {:.1} ms, max {} ms, slow {})  presented {} late {} lag_max {} ms  judder {} gaps {}\n\
                 ui intervals <25 {} 25-41 {} 42-58 {} >58 {}   wgpu errors {}   ui-load {} ms",
                s.position_ms as f64 / 1000.0,
                s.decoder,
                s.resolution.map(|(w, h)| format!("{w}x{h}")).unwrap_or_default(),
                s.decoded, s.dropped, s.late, s.judder, s.render_gap_max_ms, s.stalls, s.hist,
                load(&g.renders), load(&g.render_us_ewma) as f64 / 1000.0, load(&g.render_ms_max),
                load(&g.slow_renders), load(&g.presented), load(&g.late_frames), load(&g.lag_max_ms),
                load(&g.ui_judder), load(&g.gaps),
                load(&g.int_lt25), load(&g.int_25_41), load(&g.int_42_58), load(&g.int_gt58),
                load(&WGPU_ERRORS), ui_load_ms,
            );
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_hud(text.clone().into());
            }
            if tick % 10 == 0 {
                log::info!("[harness] {}", text.replace('\n', " | "));
            }
            let elapsed = started.lock().unwrap().map(|t| t.elapsed()).unwrap_or_default();
            if secs > 0 && elapsed >= Duration::from_secs(secs) {
                let summary = summary_json(&s, g, &p);
                println!("{summary}");
                if let Some(path) = &json {
                    if let Err(e) = std::fs::write(path, &summary) {
                        log::error!("writing {path}: {e}");
                    }
                }
                let _ = slint::quit_event_loop();
            }
        });
    }

    ui.run().expect("event loop");
    drop(rt);
}

fn summary_json(s: &PlayerStats, g: &UiPresent, p: &Player) -> String {
    format!(
        "{{\"player\":{{\"position_ms\":{},\"decoded\":{},\"dropped\":{},\"late\":{},\"judder\":{},\
         \"render_gap_max_ms\":{},\"stalls\":{},\"interval_hist\":[{},{},{},{}],\"decoder\":{:?}}},\
         \"ui\":{{\"renders\":{},\"render_avg_ms\":{:.2},\"render_max_ms\":{},\"slow_renders\":{},\
         \"presented\":{},\"late\":{},\"lag_max_ms\":{},\"judder\":{},\"gaps\":{},\
         \"intervals\":[{},{},{},{}]}},\"wgpu_errors\":{},\"debug\":{}}}",
        s.position_ms, s.decoded, s.dropped, s.late, s.judder, s.render_gap_max_ms, s.stalls,
        s.hist[0], s.hist[1], s.hist[2], s.hist[3], s.decoder,
        load(&g.renders), load(&g.render_us_ewma) as f64 / 1000.0, load(&g.render_ms_max),
        load(&g.slow_renders), load(&g.presented), load(&g.late_frames), load(&g.lag_max_ms),
        load(&g.ui_judder), load(&g.gaps),
        load(&g.int_lt25), load(&g.int_25_41), load(&g.int_42_58), load(&g.int_gt58),
        load(&WGPU_ERRORS),
        p.debug_snapshot().to_json(),
    )
}

/// `--bench`: what the device's memory policy costs per operation, on the
/// shared device, before any UI exists. Each case is timed end to end
/// (encode + submit + wait), the way a frame pays for it.
fn bench(dev: &wgpu::Device, queue: &wgpu::Queue) -> String {
    fn wait(dev: &wgpu::Device) {
        let _ = dev.poll(wgpu::PollType::wait_indefinitely());
    }
    fn time_us(n: u32, mut f: impl FnMut(u32)) -> f64 {
        for i in 0..(n / 10).max(3) {
            f(i); // warm-up
        }
        let t = Instant::now();
        for i in 0..n {
            f(i);
        }
        t.elapsed().as_secs_f64() * 1e6 / n as f64
    }

    let ubo = dev.create_buffer(&wgpu::BufferDescriptor {
        label: Some("bench ubo"),
        size: 64 * 1024,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let small = vec![7u8; 256];
    let vbuf = vec![7u8; 64 * 1024];
    let tex_desc = |w: u32, h: u32| wgpu::TextureDescriptor {
        label: Some("bench tex"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        // What FemtoVG uses for its images.
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    };
    let img = vec![128u8; 512 * 512 * 4];
    let fixed = dev.create_texture(&tex_desc(512, 512));
    let write_tex = |t: &wgpu::Texture, w: u32, h: u32| {
        queue.write_texture(
            t.as_image_copy(),
            &img[..(w * h * 4) as usize],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    };

    // Per-frame patterns: a few small uniform/vertex uploads, one submit.
    let frame_small = time_us(2000, |_| {
        for _ in 0..8 {
            queue.write_buffer(&ubo, 0, &small);
        }
        queue.submit([]);
        wait(dev);
    });
    let frame_vertex = time_us(1000, |_| {
        queue.write_buffer(&ubo, 0, &vbuf);
        queue.submit([]);
        wait(dev);
    });
    // Re-upload into an existing texture (glyph atlas / cue texture update).
    let upload_existing = time_us(500, |_| {
        write_tex(&fixed, 512, 512);
        queue.submit([]);
        wait(dev);
    });
    // New texture + first upload (image load, cue of a new size).
    let create_small = time_us(300, |_| {
        let t = dev.create_texture(&tex_desc(128, 128));
        write_tex(&t, 128, 128);
        queue.submit([]);
        wait(dev);
    });
    let create_large = time_us(100, |_| {
        let t = dev.create_texture(&tex_desc(512, 512));
        write_tex(&t, 512, 512);
        queue.submit([]);
        wait(dev);
    });
    // A full-HD render target (offscreen ring resize) - no upload.
    let create_rt_1080 = time_us(30, |_| {
        let t = dev.create_texture(&wgpu::TextureDescriptor {
            label: Some("bench rt"),
            size: wgpu::Extent3d { width: 1920, height: 1080, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = t.create_view(&Default::default());
        let mut enc = dev.create_command_encoder(&Default::default());
        drop(enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        }));
        queue.submit([enc.finish()]);
        wait(dev);
    });
    format!(
        "{{\"bench_us\":{{\"frame_8x256B_writes\":{frame_small:.1},\"frame_64KiB_write\":{frame_vertex:.1},\
         \"reupload_512px\":{upload_existing:.1},\"new_tex_128px\":{create_small:.1},\
         \"new_tex_512px\":{create_large:.1},\"new_rt_1080p\":{create_rt_1080:.1}}},\"wgpu_errors\":{}}}",
        load(&WGPU_ERRORS)
    )
}
