//! Desktop (Windows/Linux/macOS) + iOS PCM output via cpal.
//!
//! The cpal output stream pulls resampled packed-stereo f32 from the channel in
//! its realtime callback; `samples_consumed` (frames the device actually took)
//! is the clock the video sync loop paces against. Android does NOT use this —
//! its AAudio stream gets stolen on some TV HALs, so it outputs via an
//! `AudioTrack` instead (see `audio_track_pcm`).
//!
//! Extracted verbatim from the old inline `AudioRenderer::{start_thread,
//! start_audio}` so each output backend lives in its own file (mirrors `video`).

use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, StreamConfig,
};
#[cfg(not(target_os = "ios"))]
use cpal::SupportedStreamConfig;
use pollster::FutureExt;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Notify,
};

use super::AudioRendererCommand;

/// iOS only: the OS-authoritative output sample rate, read from
/// `AVAudioSession.sharedInstance().sampleRate`.
///
/// cpal's `default_output_config()` reports a canonical rate on iOS (often
/// 48000) that does NOT necessarily match what RemoteIO actually runs at —
/// that's governed by the active `AVAudioSession`, which is 44100 on some
/// devices. Resampling to cpal's 48000 while the unit consumes at 44100 plays
/// audio slow and low-pitched ("deep voice"). The session is the truth, so we
/// read it directly and drive both the cpal stream and the resampler from it.
///
/// Returns `None` if the class/selector is unavailable or the value is absurd,
/// in which case the caller falls back to cpal's reported rate. AVFoundation is
/// linked by the iOS build (see app-ios/ios/build_sim.sh). Best read AFTER the
/// host has configured + activated the session, else it may report a default.
#[cfg(target_os = "ios")]
fn ios_output_sample_rate() -> Option<u32> {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    unsafe {
        let session: *mut AnyObject = msg_send![class!(AVAudioSession), sharedInstance];
        if session.is_null() {
            return None;
        }
        let rate: f64 = msg_send![session, sampleRate];
        if (8_000.0..=192_000.0).contains(&rate) {
            Some(rate.round() as u32)
        } else {
            None
        }
    }
}

/// iOS only: the live output route's channel count, read from
/// `AVAudioSession.sharedInstance().outputNumberOfChannels` (1 on the iPhone SE
/// built-in speaker, 2 on headphones / AirPods / stereo speakers).
///
/// Read per play so whatever the user currently has plugged in is honoured —
/// and crucially WITHOUT touching cpal's device/format enumeration, which
/// hangs on a second playback (see `start_thread`). Returns `None` if the
/// selector is unavailable or the value is absurd.
#[cfg(target_os = "ios")]
fn ios_output_channels() -> Option<u16> {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    unsafe {
        let session: *mut AnyObject = msg_send![class!(AVAudioSession), sharedInstance];
        if session.is_null() {
            return None;
        }
        let ch: i64 = msg_send![session, outputNumberOfChannels];
        if (1..=8).contains(&ch) {
            Some(ch as u16)
        } else {
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_audio(
    mut sample_receiver: Receiver<f32>,
    device: Device,
    // Rate + channel count to open the device at, resolved in `start_thread`
    // (rate from the iOS AVAudioSession; channels prefer stereo so the OS
    // owns routing/downmix). Kept in lock-step with the resampler.
    out_rate: u32,
    out_channels: u16,
    volume: Arc<AtomicU32>,
    stop: Arc<Notify>,
    flush_flag: Arc<AtomicBool>,
    paused_flag: Arc<AtomicBool>,
    samples_consumed: Arc<AtomicU64>,
    output_latency_ms: Arc<AtomicU64>,
) {
    // The resampler always emits packed STEREO. With a stereo stream
    // (the normal case) the callback copies 1:1; on a mono-only output it
    // downmixes (L+R)/2 — otherwise stereo fed 1:1 into a mono stream plays
    // at half speed (an octave low, "deep voice").
    let out_ch = out_channels as usize;
    let stream_config = StreamConfig {
        channels: out_channels,
        sample_rate: out_rate,
        buffer_size: cpal::BufferSize::Default,
    };

    let callback = move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
        // Output latency = (when this buffer's first sample is AUDIBLE)
        // − (now). The device buffer + DAC delay everything the callback
        // hands over by this much, so video paced to the wall clock
        // would lead audio by it. Captured here (stable per stream),
        // consumed by the video sync loop to delay video into alignment.
        // Backend may not support the timestamp (returns None / 0) — then
        // it stays 0 and behaviour is unchanged.
        let ts = info.timestamp();
        // cpal 0.18: duration_since takes StreamInstant by value and
        // saturates to a Duration. playback − callback = how long until
        // this buffer is audible (CoreAudio/WASAPI now fold in hardware
        // latency). Only adopt a real (nonzero, sane) reading — backends
        // that don't implement the playback timestamp give 0, which must
        // not clobber an earlier good value.
        let ms = ts.playback.duration_since(ts.callback).as_millis() as u64;
        if ms > 0 && ms <= 1000 {
            output_latency_ms.store(ms, Ordering::Relaxed);
        }
        if flush_flag.swap(false, Ordering::Relaxed) {
            while sample_receiver.try_recv().is_ok() {}
        }
        // While paused, emit silence WITHOUT draining the receiver —
        // resume picks up exactly where we left off.
        if paused_flag.load(Ordering::Relaxed) {
            for sample in data.iter_mut() {
                *sample = Sample::EQUILIBRIUM;
            }
            return;
        }
        let vol = f32::from_bits(volume.load(Ordering::Relaxed));
        let mut consumed = 0u64;
        if out_ch >= 2 {
            for sample in data.iter_mut() {
                let asample = match sample_receiver.try_recv() {
                    Ok(s) => {
                        consumed += 1;
                        s
                    }
                    Err(_) => Sample::EQUILIBRIUM,
                };
                *sample = asample * vol;
            }
        } else {
            // Mono output device: downmix the packed-stereo source (L+R)/2
            // per output sample so playback runs at the correct speed/pitch.
            for sample in data.iter_mut() {
                let l = match sample_receiver.try_recv() {
                    Ok(s) => {
                        consumed += 1;
                        s
                    }
                    Err(_) => Sample::EQUILIBRIUM,
                };
                let r = match sample_receiver.try_recv() {
                    Ok(s) => {
                        consumed += 1;
                        s
                    }
                    Err(_) => l,
                };
                *sample = (l + r) * 0.5 * vol;
            }
        }
        if consumed > 0 {
            samples_consumed.fetch_add(consumed, Ordering::Relaxed);
        }
    };

    // RealtimeDenied (AAudio couldn't grant the low-latency/realtime
    // path) is informational, not fatal — the stream falls back to the
    // normal mode and keeps playing. Log it quieter than real errors.
    let err_fn = |err: cpal::Error| {
        if err.to_string().contains("Realtime") {
            log::info!("audio: realtime/low-latency not granted, using normal mode");
        } else {
            log::error!("audio stream error: {}", err);
        }
    };

    let stream = device
        .build_output_stream(stream_config, callback, err_fn, Some(Duration::from_secs(20)))
        .expect("Failed to build audio stream");

    stream.play().expect("Failed to start audio stream");

    stop.notified().block_on();
}

/// Device-less audio path: a plain thread drains the sample channel at
/// real-time pace (48 kHz packed stereo — the resampler's output format for
/// the rate we report back), honoring pause/flush and counting consumption
/// exactly like the cpal callback would. Everything downstream behaves as if
/// a perfect silent device were attached; video plays, nothing is audible.
fn start_null_sink(
    mut sample_receiver: Receiver<f32>,
    mut command_receiver: Receiver<AudioRendererCommand>,
    stop: Arc<Notify>,
    flush_flag: Arc<AtomicBool>,
    paused_flag: Arc<AtomicBool>,
    samples_consumed: Arc<AtomicU64>,
) {
    std::thread::Builder::new()
        .name("bz-audio-null".into())
        .spawn(move || {
            const RATE: f64 = 48_000.0 * 2.0; // samples/sec, packed stereo
            let tick = std::time::Duration::from_millis(10);
            let mut credit = 0f64;
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(tick);
                if flush_flag.swap(false, Ordering::Relaxed) {
                    while sample_receiver.try_recv().is_ok() {}
                }
                let now = std::time::Instant::now();
                if paused_flag.load(Ordering::Relaxed) {
                    last = now;
                    continue;
                }
                credit += now.duration_since(last).as_secs_f64() * RATE;
                last = now;
                // Cap the backlog so a long descheduled stretch can't trigger
                // a burst-drain (mirrors a real device's bounded buffer).
                credit = credit.min(RATE);
                let mut consumed = 0u64;
                while credit >= 1.0 {
                    match sample_receiver.try_recv() {
                        Ok(_) => {
                            consumed += 1;
                            credit -= 1.0;
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => return,
                    }
                }
                if consumed > 0 {
                    samples_consumed.fetch_add(consumed, Ordering::Relaxed);
                }
            }
        })
        .expect("spawn null audio thread");

    tokio::spawn(async move {
        #[allow(clippy::never_loop)]
        while let Some(command) = command_receiver.recv().await {
            match command {
                AudioRendererCommand::Stop => {
                    stop.notify_waiters();
                    break;
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start_thread(
    mut command_receiver: Receiver<AudioRendererCommand>,
    stop: Arc<Notify>,
    flush_flag: Arc<AtomicBool>,
    paused_flag: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    samples_consumed: Arc<AtomicU64>,
    output_latency_ms: Arc<AtomicU64>,
) -> (Sender<f32>, u32) {
    let (sample_sender, sample_receiver) = mpsc::channel::<f32>(192_000);

    // No usable audio output (headless CI runner, server, unplugged dock):
    // don't panic the whole player — run a NULL sink that consumes samples at
    // real-time pace so the pipeline flows and video plays silently.
    let maybe_device = cpal::default_host().default_output_device();

    // Resolve the output rate + channel count.
    //
    // On iOS, read BOTH straight from the live AVAudioSession and do NOT
    // call cpal's `default_output_config()` / `supported_output_configs()`:
    // querying the audio device's formats right after the previous stream
    // stopped HANGS the setup on a second playback ("Loading…" forever — the
    // player rotates to landscape but never starts). AVAudioSession is the
    // OS truth anyway: the RemoteIO rate (44100 on the SE, which disagrees
    // with cpal's canonical 48000 → "deep voice" if mismatched) and the
    // live route's channel count (mono on the SE speaker, stereo on
    // headphones / AirPods). The resampler emits packed STEREO, so the
    // callback downmixes (L+R)/2 when the output is mono.
    #[cfg(target_os = "ios")]
    let resolved: Option<(Device, u32, u16)> = maybe_device.map(|d| {
        (
            d,
            ios_output_sample_rate().unwrap_or(48_000),
            ios_output_channels().unwrap_or(2),
        )
    });
    #[cfg(not(target_os = "ios"))]
    let resolved: Option<(Device, u32, u16)> = maybe_device.and_then(|d| {
        let config: SupportedStreamConfig = d.default_output_config().ok()?;
        let rc = (config.sample_rate(), config.channels().max(1));
        Some((d, rc.0, rc.1))
    });
    let Some((device, out_rate, out_channels)) = resolved else {
        log::warn!(
            "[audio] no usable output device — NULL audio sink (silent playback, real-time drain)"
        );
        start_null_sink(
            sample_receiver,
            command_receiver,
            stop,
            flush_flag,
            paused_flag,
            samples_consumed,
        );
        return (sample_sender, 48_000);
    };

    log::info!("[audio] opening output {} Hz / {} ch", out_rate, out_channels);

    let stop_cpal = stop.clone();
    // Run the cpal output stream on a DEDICATED OS thread, NOT a tokio
    // worker. `start_audio` parks (pollster `block_on`) on `stop` for the
    // whole playback; doing that on a tokio worker permanently consumes it.
    // On a low-core device (2-core iPhone SE) the pool is then exhausted
    // after the first play: the command loop that fires the stop
    // notification can't get a worker, so the previous stream never tears
    // down — and the SECOND play's spawned tasks (audio build, open_url)
    // never get scheduled → stuck on "Loading…" forever. A plain thread
    // keeps the blocking wait off the async runtime entirely. (Multi-core
    // simulators have spare workers, which is why it only bit on device.)
    let audio_fut = start_audio(
        sample_receiver,
        device,
        out_rate,
        out_channels,
        volume,
        stop_cpal,
        flush_flag,
        paused_flag,
        samples_consumed,
        output_latency_ms,
    );
    std::thread::Builder::new()
        .name("bz-audio-out".into())
        .spawn(move || audio_fut.block_on())
        .expect("spawn audio output thread");

    tokio::spawn(async move {
        // `Stop` is the only command today, so this drains exactly once —
        // but the loop is kept so adding non-terminal commands later is a
        // pure addition (new match arm) rather than a control-flow rewrite.
        #[allow(clippy::never_loop)]
        while let Some(command) = command_receiver.recv().await {
            match command {
                AudioRendererCommand::Stop => {
                    stop.notify_waiters();
                    break;
                }
            }
        }
    });

    (sample_sender, out_rate)
}
