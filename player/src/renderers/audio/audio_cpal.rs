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
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, FromSample, SampleFormat, SizedSample, StreamConfig,
};
#[cfg(not(target_os = "ios"))]
use cpal::SupportedStreamConfig;
use pollster::FutureExt;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Notify,
};

use super::{AudioRendererCommand, QUEUE_CHUNKS};
use crate::av_sync::{AudioChunk, ChunkCursor, FlushState};

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
/// linked by the iOS build (see examples/ios/build_sim.sh). Best read AFTER the
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

/// The device may go this long without calling back before the stream is
/// taken for dead and rebuilt. The callback runs while paused too (it
/// writes silence), so only a dead stream is ever this quiet. Well above a
/// device period (tens of ms), and below `AUDIO_OUTPUT_DEAD_MS` (1.5 s), so
/// the stream is usually back before the watchdog rebuilds the pipeline —
/// which cannot help here anyway: the output stream outlives pipelines.
#[cfg(not(target_os = "ios"))]
const STREAM_DEAD_MS: u64 = 1_000;

/// Retry period while no stream can be opened (device gone, server down).
const STREAM_RETRY: Duration = Duration::from_secs(1);

/// State the output callback shares with the thread that owns the stream,
/// kept across stream rebuilds so the queue and the clock carry on.
struct OutputShared {
    cursor: Mutex<ChunkCursor>,
    volume: Arc<AtomicU32>,
    paused_flag: Arc<AtomicBool>,
    output_latency_ms: Arc<AtomicU64>,
    epoch: Instant,
    /// `epoch`-relative ms of the latest callback (or of the stream build).
    last_callback_ms: AtomicU64,
    /// Set by the error callback: the stream reported itself broken.
    failed: AtomicBool,
}

impl OutputShared {
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }
}

/// Build and start an output stream in the device's own sample format.
fn open_stream(
    device: &Device,
    config: StreamConfig,
    shared: &Arc<OutputShared>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    // iOS: RemoteIO always takes f32, and querying the device's formats
    // there hangs a second playback (see `start_thread`).
    #[cfg(target_os = "ios")]
    let format = SampleFormat::F32;
    #[cfg(not(target_os = "ios"))]
    let format = device
        .default_output_config()
        .map(|c| c.sample_format())
        .unwrap_or(SampleFormat::F32);
    let stream = match format {
        SampleFormat::I16 => build_stream::<i16>(device, config, shared)?,
        SampleFormat::I32 => build_stream::<i32>(device, config, shared)?,
        SampleFormat::U16 => build_stream::<u16>(device, config, shared)?,
        _ => build_stream::<f32>(device, config, shared)?,
    };
    shared.last_callback_ms.store(shared.now_ms(), Ordering::Relaxed);
    shared.failed.store(false, Ordering::Relaxed);
    stream.play()?;
    Ok(stream)
}

fn build_stream<T: SizedSample + FromSample<f32>>(
    device: &Device,
    config: StreamConfig,
    shared: &Arc<OutputShared>,
) -> Result<cpal::Stream, cpal::Error> {
    let cb = Arc::clone(shared);
    let callback = move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
        cb.last_callback_ms.store(cb.now_ms(), Ordering::Relaxed);
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
            cb.output_latency_ms.store(ms, Ordering::Relaxed);
        }
        // Only this callback locks the cursor, one stream at a time.
        let mut cursor = cb.cursor.lock().unwrap();
        // While paused, emit silence WITHOUT consuming live PCM — resume
        // picks up exactly where we left off. Chunks a flush has already
        // superseded ARE thrown away here, or a seek (flush + pause) leaves
        // the bounded queue full of the old pipeline's audio and the new
        // pipeline can never queue the pre-roll that unpauses the output
        // (see `ChunkCursor::drop_stale`).
        if cb.paused_flag.load(Ordering::Relaxed) {
            cursor.drop_stale();
            for sample in data.iter_mut() {
                *sample = T::EQUILIBRIUM;
            }
            return;
        }
        let vol = f32::from_bits(cb.volume.load(Ordering::Relaxed));
        for sample in data.iter_mut() {
            *sample = T::from_sample(cursor.next_sample().unwrap_or(0.0) * vol);
        }
        // Publish the consumed-sample count once per callback (the clock).
        cursor.commit();
    };

    // RealtimeDenied (AAudio couldn't grant the low-latency/realtime
    // path) is informational, not fatal — the stream falls back to the
    // normal mode and keeps playing. Log it quieter than real errors.
    let err = Arc::clone(shared);
    let err_fn = move |e: cpal::Error| {
        if e.to_string().contains("Realtime") {
            log::info!("audio: realtime/low-latency not granted, using normal mode");
        } else {
            log::error!("audio stream error: {}", e);
            err.failed.store(true, Ordering::Relaxed);
        }
    };

    device.build_output_stream(config, callback, err_fn, Some(Duration::from_secs(20)))
}

/// Owns the cpal output stream for the whole playback, on its own thread.
///
/// A stream can die under a running player without cpal saying so: a
/// PipeWire restart or an unplugged USB / Bluetooth headset just stops the
/// callbacks. The consumed position then stands, the audio-disciplined
/// clock stands with it, and a pipeline rebuild does not help because it
/// reuses this stream. So the stream is rebuilt here — on the current
/// default device — when it reports an error or stops calling back; the
/// queue and the consumed count carry over. A stream that cannot be opened
/// (no device right now, a format the device refuses) is retried instead of
/// panicking the thread.
#[allow(clippy::too_many_arguments)]
fn start_audio(
    sample_receiver: Receiver<AudioChunk>,
    device: Device,
    // Rate + channel count to open the device at, resolved in `start_thread`
    // (rate from the iOS AVAudioSession; channels prefer stereo so the OS
    // owns routing/downmix). Kept in lock-step with the resampler.
    out_rate: u32,
    out_channels: u16,
    volume: Arc<AtomicU32>,
    stop: Arc<Notify>,
    flush_state: Arc<FlushState>,
    paused_flag: Arc<AtomicBool>,
    samples_consumed: Arc<AtomicU64>,
    output_latency_ms: Arc<AtomicU64>,
) {
    // Generation-filtering cursor over the chunk queue: drops PCM queued
    // before the last flush and records the consumed-sample position at which
    // each new generation begins (the post-flush clock boundary).
    let cursor = ChunkCursor::new(sample_receiver, flush_state, samples_consumed);
    let shared = Arc::new(OutputShared {
        cursor: Mutex::new(cursor),
        volume,
        paused_flag,
        output_latency_ms,
        epoch: Instant::now(),
        last_callback_ms: AtomicU64::new(0),
        failed: AtomicBool::new(false),
    });
    // The decoders mix to exactly `out_channels` (AudioSink::channels()), so
    // the callback copies 1:1 whatever the device layout — stereo, mono, 5.1.
    // (Before, a stereo stream was copied 1:1 into a 6-channel device buffer:
    // wrong speed and channel order on every 5.1-configured PC.)
    let stream_config = StreamConfig {
        channels: out_channels,
        sample_rate: out_rate,
        buffer_size: cpal::BufferSize::Default,
    };

    // `stop` is async; wait for it on a helper thread that wakes this one.
    let stopped = Arc::new(AtomicBool::new(false));
    {
        let stopped = Arc::clone(&stopped);
        let owner = std::thread::current();
        std::thread::Builder::new()
            .name("bz-audio-stop".into())
            .spawn(move || {
                stop.notified().block_on();
                stopped.store(true, Ordering::Relaxed);
                owner.unpark();
            })
            .expect("spawn audio stop thread");
    }

    let mut device = Some(device);
    let mut stream: Option<cpal::Stream> = None;
    let mut failures = 0u32;
    while !stopped.load(Ordering::Relaxed) {
        if stream.is_none() {
            let opened = device
                .take()
                .or_else(|| cpal::default_host().default_output_device())
                .ok_or_else(|| "no output device".into())
                .and_then(|d| open_stream(&d, stream_config, &shared));
            match opened {
                Ok(s) => {
                    if failures > 0 {
                        log::info!(
                            "[audio] output stream reopened after {failures} failed attempt(s)"
                        );
                    }
                    failures = 0;
                    stream = Some(s);
                }
                Err(e) => {
                    if failures == 0 {
                        log::warn!("[audio] cannot open the output stream, retrying: {e}");
                    }
                    failures += 1;
                    std::thread::park_timeout(STREAM_RETRY);
                    continue;
                }
            }
        }
        std::thread::park_timeout(Duration::from_millis(250));

        #[cfg(not(target_os = "ios"))]
        {
            let quiet_ms = shared
                .now_ms()
                .saturating_sub(shared.last_callback_ms.load(Ordering::Relaxed));
            let failed = shared.failed.load(Ordering::Relaxed);
            if failed || quiet_ms > STREAM_DEAD_MS {
                let why = if failed {
                    "stream error".to_string()
                } else {
                    format!("no callback for {quiet_ms} ms")
                };
                log::warn!("[audio] output stream dead ({why}) — rebuilding it on the default device");
                stream = None;
            }
        }
    }
    drop(stream);
}

/// Device-less audio path: a plain thread drains the sample channel at
/// real-time pace (48 kHz packed stereo — the resampler's output format for
/// the rate we report back), honoring pause/flush and counting consumption
/// exactly like the cpal callback would. Everything downstream behaves as if
/// a perfect silent device were attached; video plays, nothing is audible.
fn start_null_sink(
    sample_receiver: Receiver<AudioChunk>,
    mut command_receiver: Receiver<AudioRendererCommand>,
    stop: Arc<Notify>,
    flush_state: Arc<FlushState>,
    paused_flag: Arc<AtomicBool>,
    samples_consumed: Arc<AtomicU64>,
) {
    // The thread polls this instead of `stop` (it has no async context).
    let ended = Arc::new(AtomicBool::new(false));
    let ended_thread = ended.clone();
    std::thread::Builder::new()
        .name("bz-audio-null".into())
        .spawn(move || {
            const RATE: f64 = 48_000.0 * 2.0; // samples/sec, packed stereo
            let tick = std::time::Duration::from_millis(10);
            let mut cursor = ChunkCursor::new(sample_receiver, flush_state, samples_consumed);
            let mut credit = 0f64;
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(tick);
                if ended_thread.load(Ordering::Relaxed) {
                    return;
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
                while credit >= 1.0 {
                    match cursor.next_sample() {
                        Some(_) => credit -= 1.0,
                        None if cursor.is_closed() => return,
                        None => break,
                    }
                }
                cursor.commit();
            }
        })
        .expect("spawn null audio thread");

    crate::rt::spawn(async move {
        #[allow(clippy::never_loop)]
        while let Some(command) = command_receiver.recv().await {
            match command {
                AudioRendererCommand::Stop => break,
            }
        }
        // Stop, or the AudioRenderer was dropped (sender closed). A paused
        // sink never reads the closed sample channel, so without this the
        // thread outlived the player.
        stop.notify_one();
        ended.store(true, Ordering::Relaxed);
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start_thread(
    mut command_receiver: Receiver<AudioRendererCommand>,
    stop: Arc<Notify>,
    flush_state: Arc<FlushState>,
    paused_flag: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    samples_consumed: Arc<AtomicU64>,
    output_latency_ms: Arc<AtomicU64>,
) -> (Sender<AudioChunk>, u32, u16) {
    let (sample_sender, sample_receiver) = mpsc::channel::<AudioChunk>(QUEUE_CHUNKS);

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
            flush_state,
            paused_flag,
            samples_consumed,
        );
        return (sample_sender, 48_000, 2);
    };

    // Layouts the decoders know how to mix to; anything odd (3, 5, 7 …)
    // falls back to stereo and the OS/driver spreads it.
    let out_channels = match out_channels {
        1 | 2 | 6 | 8 => out_channels,
        _ => 2,
    };
    log::info!("[audio] opening output {} Hz / {} ch", out_rate, out_channels);

    let stop_cpal = stop.clone();
    // Run the cpal output stream on a DEDICATED OS thread, NOT a tokio
    // worker. `start_audio` blocks for the whole playback (it owns the
    // stream and waits on `stop`); doing that on a tokio worker permanently consumes it.
    // On a low-core device (2-core iPhone SE) the pool is then exhausted
    // after the first play: the command loop that fires the stop
    // notification can't get a worker, so the previous stream never tears
    // down — and the SECOND play's spawned tasks (audio build, open_url)
    // never get scheduled → stuck on "Loading…" forever. A plain thread
    // keeps the blocking wait off the async runtime entirely. (Multi-core
    // simulators have spare workers, which is why it only bit on device.)
    std::thread::Builder::new()
        .name("bz-audio-out".into())
        .spawn(move || {
            start_audio(
                sample_receiver,
                device,
                out_rate,
                out_channels,
                volume,
                stop_cpal,
                flush_state,
                paused_flag,
                samples_consumed,
                output_latency_ms,
            )
        })
        .expect("spawn audio output thread");

    crate::rt::spawn(async move {
        // `Stop` is the only command today, so this drains exactly once —
        // but the loop is kept so adding non-terminal commands later is a
        // pure addition (new match arm) rather than a control-flow rewrite.
        #[allow(clippy::never_loop)]
        while let Some(command) = command_receiver.recv().await {
            match command {
                AudioRendererCommand::Stop => break,
            }
        }
        // Stop, or the AudioRenderer was dropped (sender closed): end the
        // stream either way. Only an explicit Stop used to, so every dropped
        // player left its output thread, cpal stream and OS audio unit
        // (AURemoteIO on iOS) running, with its queued PCM, for good.
        // notify_one keeps the permit: a stop arriving while the output
        // thread is still inside build_output_stream (up to 20 s) used to be
        // lost, leaking the thread + stream.
        stop.notify_one();
    });

    (sample_sender, out_rate, out_channels)
}
