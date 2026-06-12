use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, StreamConfig, SupportedStreamConfig,
};
#[cfg(any(target_os = "windows", target_os = "linux"))]
use ffmpeg_next::frame::Audio;
use pollster::FutureExt;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Notify,
};

pub struct AudioRenderer {
    command_sender: Sender<AudioRendererCommand>,
    sample_sender: Sender<f32>,
    sample_rate: u32,
    flush_flag: Arc<AtomicBool>,
    paused_flag: Arc<AtomicBool>,
    /// Volume gain in 0.0..=1.0, stored as `f32::to_bits` so the cpal
    /// output callback (which runs on a non-tokio audio thread) can read
    /// it without blocking. Writes go through `set_volume`; the integration
    /// layer is expected to restore any persisted user value on startup.
    volume: Arc<AtomicU32>,
    /// Last-frame peak dB per channel (f32 bits stored in AtomicU32).
    /// `peak_seen` flips on first push so we can return `None` until data
    /// is actually flowing — avoids reporting `-inf` at startup.
    peak_l_db: Arc<AtomicU32>,
    peak_r_db: Arc<AtomicU32>,
    peak_seen: Arc<AtomicBool>,
    /// Samples (interleaved f32s, post-resample at the OUTPUT rate) the
    /// cpal callback has actually consumed from the channel. The DEVICE
    /// clock — silence emitted during pause/underrun does not advance it,
    /// so `consumed / 2 / out_rate` is exactly how much media time the
    /// listener has heard. Drives the A/V drift measurement in the video
    /// sync loop (the device crystal and CLOCK_MONOTONIC disagree by
    /// 10-100 ppm — minutes-long playback drifts audibly without it).
    samples_consumed: Arc<AtomicU64>,
}

enum AudioRendererCommand {
    Stop,
}

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

impl AudioRenderer {
    pub fn new() -> Self {
        let stop = Arc::new(Notify::new());
        let flush_flag = Arc::new(AtomicBool::new(false));
        // Start paused so cpal emits silence (without draining the
        // mpsc) until av_sync_handler is ready to start playback. Once
        // the av_sync handler observes both decoders' first frame, it
        // sets the wall-clock start_time and unpauses audio in lock-
        // step. Without this, cpal would consume samples the moment
        // the decoder produced them — making the speaker output begin
        // BEFORE video showed its first frame, perceived as audio
        // leading or (after the decoder queue fills) lagging by an
        // unpredictable amount.
        let paused_flag = Arc::new(AtomicBool::new(true));

        // Unity gain default. The integration layer (UI) overrides this
        // by calling set_volume() after construction once it has loaded
        // the persisted user value — defaulting low silently masked
        // decode/sync issues.
        let volume = Arc::new(AtomicU32::new(1.0_f32.to_bits()));

        let samples_consumed = Arc::new(AtomicU64::new(0));
        let (command_sender, command_receiver) = mpsc::channel(4);
        let audio_thread = AudioRenderer::start_thread(
            command_receiver,
            stop,
            flush_flag.clone(),
            paused_flag.clone(),
            volume.clone(),
            samples_consumed.clone(),
        );

        AudioRenderer {
            command_sender,
            sample_sender: audio_thread.0,
            sample_rate: audio_thread.1,
            flush_flag,
            paused_flag,
            volume,
            peak_l_db: Arc::new(AtomicU32::new(0)),
            peak_r_db: Arc::new(AtomicU32::new(0)),
            peak_seen: Arc::new(AtomicBool::new(false)),
            samples_consumed,
        }
    }

    /// Compute interleaved-stereo peak in dB and stash it for the next
    /// `last_peak_db()` poll. Cheap — one abs+max per sample.
    fn update_peaks(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let mut max_l = 0.0_f32;
        let mut max_r = 0.0_f32;
        for chunk in samples.chunks_exact(2) {
            max_l = max_l.max(chunk[0].abs());
            max_r = max_r.max(chunk[1].abs());
        }
        // 20 * log10(|s|). Floor at -120 dB to avoid log(0) = -inf.
        let to_db = |v: f32| -> f32 {
            if v <= 1.0e-6 { -120.0 } else { 20.0 * v.log10() }
        };
        self.peak_l_db
            .store(to_db(max_l).to_bits(), Ordering::Relaxed);
        self.peak_r_db
            .store(to_db(max_r).to_bits(), Ordering::Relaxed);
        self.peak_seen.store(true, Ordering::Relaxed);
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

        let callback = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
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

        let err_fn = |err| log::error!("audio stream error: {}", err);

        let stream = device
            .build_output_stream(
                &stream_config,
                callback,
                err_fn,
                Some(Duration::from_secs(20)),
            )
            .expect("Failed to build audio stream");

        stream.play().expect("Failed to start audio stream");

        stop.notified().block_on();
    }

    fn start_thread(
        mut command_receiver: Receiver<AudioRendererCommand>,
        stop: Arc<Notify>,
        flush_flag: Arc<AtomicBool>,
        paused_flag: Arc<AtomicBool>,
        volume: Arc<AtomicU32>,
        samples_consumed: Arc<AtomicU64>,
    ) -> (Sender<f32>, u32) {
        let (sample_sender, sample_receiver) = mpsc::channel::<f32>(192_000);

        let device = cpal::default_host()
            .default_output_device()
            .expect("No output device");

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
        let (out_rate, out_channels): (u32, u16) = (
            ios_output_sample_rate().unwrap_or(48_000),
            ios_output_channels().unwrap_or(2),
        );
        #[cfg(not(target_os = "ios"))]
        let (out_rate, out_channels): (u32, u16) = {
            let config: SupportedStreamConfig = device
                .default_output_config()
                .expect("Failed to get default config");
            (config.sample_rate(), config.channels().max(1))
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
        let audio_fut = AudioRenderer::start_audio(
            sample_receiver,
            device,
            out_rate,
            out_channels,
            volume,
            stop_cpal,
            flush_flag,
            paused_flag,
            samples_consumed,
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

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    pub async fn put_sample(&self, frame: Audio) {
        let expected_bytes =
            frame.samples() * frame.channels() as usize * std::mem::size_of::<f32>();
        let cpal_sample_data: &[f32] = bytemuck::cast_slice(&frame.data(0)[..expected_bytes]);

        for &sample in cpal_sample_data {
            let _ = self.sample_sender.send(sample).await;
        }
    }

    pub async fn put_samples_raw(&self, samples: &[f32]) {
        self.update_peaks(samples);
        for &s in samples {
            let _ = self.sample_sender.send(s).await;
        }
    }

    /// Returns the last computed L/R peak in dB, or `None` before the
    /// first audio frame has been pushed.
    pub fn last_peak_db(&self) -> Option<[f32; 2]> {
        if !self.peak_seen.load(Ordering::Relaxed) {
            return None;
        }
        let l = f32::from_bits(self.peak_l_db.load(Ordering::Relaxed));
        let r = f32::from_bits(self.peak_r_db.load(Ordering::Relaxed));
        Some([l, r])
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub async fn stop(&self) {
        _ = self.command_sender.send(AudioRendererCommand::Stop).await;
    }

    pub fn flush(&self) {
        self.flush_flag.store(true, Ordering::Relaxed);
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused_flag.store(paused, Ordering::Relaxed);
    }

    pub fn get_volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn set_volume(&self, volume: f32) {
        let v = volume.clamp(0.0, 1.0);
        self.volume.store(v.to_bits(), Ordering::Relaxed);
        log::debug!("volume: {}", v);
    }

    pub fn volume(&self, diff: f32) {
        let new = (self.get_volume() + diff).clamp(0.0, 1.0);
        self.set_volume(new);
    }
}

impl super::AudioSink for AudioRenderer {
    fn put_samples<'a>(&'a self, samples: &'a [f32]) -> impl std::future::Future<Output = ()> + Send + 'a {
        self.put_samples_raw(samples)
    }

    fn sample_rate(&self) -> u32 {
        AudioRenderer::sample_rate(self)
    }

    fn played_ms(&self) -> Option<u64> {
        if self.sample_rate == 0 {
            return None;
        }
        // Interleaved stereo: 2 f32s per frame at the OUTPUT rate (the
        // resampler preserves duration, so output time = media time).
        let frames = self.samples_consumed.load(Ordering::Relaxed) / 2;
        Some(frames * 1000 / self.sample_rate as u64)
    }

    fn flush(&self) {
        AudioRenderer::flush(self)
    }

    fn stop(&self) -> impl std::future::Future<Output = ()> + Send + '_ {
        AudioRenderer::stop(self)
    }

    fn set_volume(&self, volume: f32) {
        AudioRenderer::set_volume(self, volume)
    }

    fn get_volume(&self) -> f32 {
        AudioRenderer::get_volume(self)
    }

    fn volume(&self, diff: f32) {
        AudioRenderer::volume(self, diff)
    }

    fn set_paused(&self, paused: bool) {
        AudioRenderer::set_paused(self, paused)
    }

    fn last_peak_db(&self) -> Option<[f32; 2]> {
        AudioRenderer::last_peak_db(self)
    }
}
