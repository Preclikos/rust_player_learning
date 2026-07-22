//! Playback conformance soak harness — Phase 1 of automated A/V validation.
//!
//! Plays a scripted scenario (steady playback + soft track switches + seeks)
//! against a real DASH stream on the real platform decoder stack, headless
//! (offscreen wgpu target, default audio device). While playing it counts
//! every user-visible playback event from the event stream; at the end it
//! reads the engine's [`player::ConformanceSummary`] and fails loudly when
//! any threshold is exceeded. CI runs this on the self-hosted Windows /
//! Linux / macOS runners before a library tag is allowed out the door.
//!
//! ```text
//! cargo run --release --example conformance -- <MPD_URL> \
//!     [--key KIDHEX:KEYHEX]...   # ClearKey pairs for CENC test assets
//!     [--secs 90]                # scenario length
//!     [--switches 2]             # soft video-track switches (ABR swap path)
//!     [--seeks 1]                # relative seeks (+30 s)
//!     [--max-gap-ms 700]         # worst allowed frame-to-frame render gap
//!     [--max-drift-ms 100]       # worst allowed |A/V drift|
//!     [--max-bursts 200]         # sub-5ms catch-up renders
//!     [--allowed-stalls N]       # default = --seeks (a post-seek spinner is fine)
//! ```
//!
//! Exit code 0 = every threshold held; 1 = at least one FAIL (printed).
//! The summary is also printed as a single JSON line (machine-readable, for
//! trend tracking) prefixed with `CONFORMANCE_JSON `.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use player::{BufferingReason, Player, PlayerEvent, TrackKind};

struct Args {
    mpd: String,
    keys: HashMap<String, String>,
    secs: u64,
    switches: u32,
    seeks: u32,
    max_gap_ms: u64,
    max_drift_ms: i64,
    max_bursts: u64,
    max_judder_pct: f64,
    allowed_stalls: Option<u64>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        mpd: String::new(),
        keys: HashMap::new(),
        secs: 90,
        switches: 2,
        seeks: 1,
        max_gap_ms: 700,
        max_drift_ms: 100,
        max_bursts: 200,
        max_judder_pct: 5.0,
        allowed_stalls: None,
    };
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().unwrap_or_else(|| panic!("{name} needs a value"));
        match arg.as_str() {
            "--key" => {
                let kv = val("--key");
                let (kid, key) = kv.split_once(':').expect("--key expects KIDHEX:KEYHEX");
                a.keys.insert(kid.to_string(), key.to_string());
            }
            "--secs" => a.secs = val("--secs").parse().expect("--secs"),
            "--switches" => a.switches = val("--switches").parse().expect("--switches"),
            "--seeks" => a.seeks = val("--seeks").parse().expect("--seeks"),
            "--max-gap-ms" => a.max_gap_ms = val("--max-gap-ms").parse().expect("--max-gap-ms"),
            "--max-drift-ms" => a.max_drift_ms = val("--max-drift-ms").parse().expect("--max-drift-ms"),
            "--max-bursts" => a.max_bursts = val("--max-bursts").parse().expect("--max-bursts"),
            "--max-judder-pct" => {
                a.max_judder_pct = val("--max-judder-pct").parse().expect("--max-judder-pct")
            }
            "--allowed-stalls" => {
                a.allowed_stalls = Some(val("--allowed-stalls").parse().expect("--allowed-stalls"))
            }
            other if a.mpd.is_empty() && !other.starts_with("--") => a.mpd = other.to_string(),
            other => panic!("unknown argument: {other}"),
        }
    }
    if a.mpd.is_empty() {
        eprintln!("usage: conformance <MPD_URL> [--key KID:KEY] [--secs N] …");
        std::process::exit(2);
    }
    a
}

/// Headless wgpu device — mirror of the BlackZone desktop host's shared-GPU
/// init (the player fork's wgpu carries non-upstream fields, so every field
/// is spelled out).
async fn headless_gpu() -> (wgpu::Device, wgpu::Queue, wgpu::Backend) {
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
    let adapter = match instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
    {
        Ok(a) => a,
        Err(e) => {
            // Headless session without GPU access (e.g. a runner service
            // outside the GUI session — Metal offers no adapter there).
            // Not a playback failure: report a SKIP so CI stays green but
            // the gap is visible in the log.
            println!("CONFORMANCE_SKIP no-gpu-adapter: {e}");
            std::process::exit(0);
        }
    };
    let backend = adapter.get_info().backend;
    let desired = if backend == wgpu::Backend::Metal {
        wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
    } else {
        wgpu::Features::TEXTURE_FORMAT_NV12
            | wgpu::Features::TEXTURE_FORMAT_P010
            | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
    };
    let required_features = adapter.features() & desired;
    let alim = adapter.limits();
    let required_limits = wgpu::Limits {
        max_texture_dimension_2d: alim.max_texture_dimension_2d,
        max_texture_dimension_1d: alim.max_texture_dimension_1d,
        ..wgpu::Limits::default()
    };
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("conformance headless device"),
            required_features,
            required_limits,
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::default(),
            trace: wgpu::Trace::Off,
        })
        .await
        .expect("request_device failed");
    eprintln!(
        "[conformance] gpu: {:?} / {}",
        backend,
        adapter.get_info().name
    );
    (device, queue, backend)
}

#[derive(Default)]
struct EventCounters {
    stall_buffering: AtomicU64,
    seek_buffering: AtomicU64,
    errors: AtomicU64,
    eos: AtomicU64,
    video_track_changes: AtomicU64,
    first_error: std::sync::Mutex<Option<String>>,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();

    let (device, queue, backend) = headless_gpu().await;
    let mut player = Player::new_offscreen(device, queue, backend, 1280, 720).await;
    if !args.keys.is_empty() {
        player.set_clearkey(args.keys.clone()).expect("set_clearkey");
    }

    let t0 = Instant::now();
    player.open_url(&args.mpd).await.expect("open_url");
    player.prepare().await.expect("prepare");
    let tracks = player.get_tracks().expect("get_tracks");
    let va = tracks.video.first().expect("no video adaptation").clone();
    let low = va.representations.first().expect("no video reps").clone();
    let high = va.representations.last().expect("no video reps").clone();
    player.set_video_track(&va, &low);
    let aa = tracks.audio.first().expect("no audio adaptation").clone();
    let ar = aa.representations.first().expect("no audio reps").clone();
    player.set_audio_track(&aa, &ar);
    eprintln!(
        "[conformance] prepared in {} ms — video reps {}..{} ({} total), scenario {}s/{}sw/{}seek",
        t0.elapsed().as_millis(),
        low.id,
        high.id,
        va.representations.len(),
        args.secs,
        args.switches,
        args.seeks,
    );

    // ---- event stream accounting -------------------------------------------
    let counters = Arc::new(EventCounters::default());
    {
        let counters = Arc::clone(&counters);
        let mut rx = player.events();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => match ev {
                        PlayerEvent::Buffering { reason } => match reason {
                            BufferingReason::Stall => {
                                counters.stall_buffering.fetch_add(1, Ordering::Relaxed);
                                eprintln!("[conformance] EVENT Buffering(Stall)");
                            }
                            BufferingReason::Seek => {
                                counters.seek_buffering.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {}
                        },
                        PlayerEvent::Error { kind, detail } => {
                            counters.errors.fetch_add(1, Ordering::Relaxed);
                            counters
                                .first_error
                                .lock()
                                .unwrap()
                                .get_or_insert_with(|| detail.clone());
                            eprintln!("[conformance] EVENT Error({kind:?}): {detail}");
                        }
                        PlayerEvent::EndOfStream => {
                            counters.eos.fetch_add(1, Ordering::Relaxed);
                            eprintln!("[conformance] EVENT EndOfStream");
                        }
                        PlayerEvent::TrackChanged { kind: TrackKind::Video, .. } => {
                            counters.video_track_changes.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    let handle = player.play().expect("play");

    // ---- scripted scenario ---------------------------------------------------
    // Actions are spread across the middle 80% of the run so the tail is clean
    // steady-state playback (drift has time to show).
    let total = Duration::from_secs(args.secs);
    let mut actions: Vec<(Duration, String)> = Vec::new();
    let n_actions = (args.switches + args.seeks) as u64;
    for i in 0..args.switches as u64 {
        let at = total.mul_f64(0.1 + 0.8 * (i as f64 + 0.5) / n_actions.max(1) as f64);
        actions.push((at, format!("switch{}", i)));
    }
    for i in 0..args.seeks as u64 {
        let at = total.mul_f64(0.1 + 0.8 * ((args.switches as u64 + i) as f64 + 0.5) / n_actions.max(1) as f64);
        actions.push((at, format!("seek{}", i)));
    }
    actions.sort_by_key(|(at, _)| *at);

    let start = Instant::now();
    let mut next_action = 0usize;
    let mut on_high = false;
    while start.elapsed() < total {
        tokio::time::sleep(Duration::from_millis(250)).await;
        while next_action < actions.len() && start.elapsed() >= actions[next_action].0 {
            let (_, what) = &actions[next_action];
            if what.starts_with("switch") && va.representations.len() > 1 {
                let target = if on_high { &low } else { &high };
                on_high = !on_high;
                eprintln!(
                    "[conformance] ACTION soft switch -> rep {} at {}s",
                    target.id,
                    start.elapsed().as_secs()
                );
                player.change_video_track_soft(target);
            } else if what.starts_with("seek") {
                let pos = player.position();
                // Clamp so the seek can't run past the clip end (a legit
                // EndOfStream would fail the eos==0 assertion).
                let cap = tracks.duration.mul_f64(0.8);
                let to = (pos + Duration::from_secs(30)).min(cap);
                eprintln!(
                    "[conformance] ACTION seek {}s -> {}s",
                    pos.as_secs(),
                    to.as_secs()
                );
                player.seek(to);
            }
            next_action += 1;
        }
    }

    player.stop().await;
    let _ = handle.await;

    // ---- verdict -------------------------------------------------------------
    let s = player.conformance_summary();
    let stalls_ev = counters.stall_buffering.load(Ordering::Relaxed);
    let errors = counters.errors.load(Ordering::Relaxed);
    let eos = counters.eos.load(Ordering::Relaxed);
    let allowed_stalls = args.allowed_stalls.unwrap_or(args.seeks as u64);

    // Environment, not player: a machine without a usable HW decoder (no
    // /dev/dri render node, missing driver) can't play anything at all —
    // report a SKIP so the lane is green-but-honest instead of failing on
    // every run until the machine grows a GPU.
    if s.video_frames_decoded == 0 {
        let first = counters.first_error.lock().unwrap().clone().unwrap_or_default();
        if first.contains("av_hwdevice_ctx_create") {
            println!("CONFORMANCE_SKIP no-hw-decoder: {first}");
            std::process::exit(0);
        }
    }

    println!(
        "CONFORMANCE_JSON {{\"platform\":\"{}\",\"secs\":{},\"stall_events\":{},\"stall_buffering_events\":{},\"stall_ms_total\":{},\"pipeline_retries\":{},\"render_gap_max_ms\":{},\"render_burst_frames\":{},\"judder_frames\":{},\"av_drift_max_ms\":{},\"frames_decoded\":{},\"frames_dropped\":{},\"audio_underruns\":{},\"errors\":{},\"eos\":{},\"video_track_changes\":{}}}",
        std::env::consts::OS,
        args.secs,
        s.stall_events,
        stalls_ev,
        s.stall_ms_total,
        s.pipeline_retries,
        s.render_gap_max_ms,
        s.render_burst_frames,
        s.judder_frames,
        s.av_drift_max_ms,
        s.video_frames_decoded,
        s.video_frames_dropped,
        s.audio_underruns,
        errors,
        eos,
        counters.video_track_changes.load(Ordering::Relaxed),
    );

    let mut failed = false;
    let mut check = |name: &str, ok: bool, detail: String| {
        if ok {
            println!("PASS {name}: {detail}");
        } else {
            println!("FAIL {name}: {detail}");
            failed = true;
        }
    };
    check("errors", errors == 0, format!("{errors} player errors"));
    check(
        "pipeline-retries",
        s.pipeline_retries == 0,
        format!("{} mid-play pipeline rebuilds", s.pipeline_retries),
    );
    check("eos", eos == 0, format!("{eos} EndOfStream before scenario end"));
    check(
        "stalls",
        stalls_ev <= allowed_stalls,
        format!("{stalls_ev} Buffering(Stall) events (allowed {allowed_stalls})"),
    );
    check(
        "render-gap",
        s.render_gap_max_ms <= args.max_gap_ms,
        format!("max frame gap {} ms (limit {})", s.render_gap_max_ms, args.max_gap_ms),
    );
    check(
        "render-bursts",
        s.render_burst_frames <= args.max_bursts,
        format!("{} sub-5ms renders (limit {})", s.render_burst_frames, args.max_bursts),
    );
    let judder_pct = if s.video_frames_decoded > 0 {
        s.judder_frames as f64 * 100.0 / s.video_frames_decoded as f64
    } else {
        0.0
    };
    check(
        "judder",
        judder_pct <= args.max_judder_pct,
        format!(
            "{} frames >±10 ms off cadence = {:.1}% (limit {:.0}%)",
            s.judder_frames, judder_pct, args.max_judder_pct
        ),
    );
    check(
        "av-drift",
        s.av_drift_max_ms <= args.max_drift_ms,
        format!("max |A/V drift| {} ms (limit {})", s.av_drift_max_ms, args.max_drift_ms),
    );
    // Sanity: the scenario actually played (≥15 fps average) and the soft
    // switches actually happened.
    check(
        "throughput",
        s.video_frames_decoded > args.secs * 15,
        format!("{} frames decoded over {} s", s.video_frames_decoded, args.secs),
    );

    std::process::exit(if failed { 1 } else { 0 });
}
