use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
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
    Notify, RwLock,
};

pub struct AudioRenderer {
    command_sender: Sender<AudioRendererCommand>,
    sample_sender: Sender<f32>,
    sample_rate: u32,
    flush_flag: Arc<AtomicBool>,
    paused_flag: Arc<AtomicBool>,
    /// Last-frame peak dB per channel (f32 bits stored in AtomicU32).
    /// `peak_seen` flips on first push so we can return `None` until data
    /// is actually flowing — avoids reporting `-inf` at startup.
    peak_l_db: Arc<AtomicU32>,
    peak_r_db: Arc<AtomicU32>,
    peak_seen: Arc<AtomicBool>,
}

enum AudioRendererCommand {
    Stop,
    Volume(f32),
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

        let (command_sender, command_receiver) = mpsc::channel(4);
        let audio_thread =
            AudioRenderer::start_thread(command_receiver, stop, flush_flag.clone(), paused_flag.clone());

        AudioRenderer {
            command_sender,
            sample_sender: audio_thread.0,
            sample_rate: audio_thread.1,
            flush_flag,
            paused_flag,
            peak_l_db: Arc::new(AtomicU32::new(0)),
            peak_r_db: Arc::new(AtomicU32::new(0)),
            peak_seen: Arc::new(AtomicBool::new(false)),
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

    async fn start_audio(
        mut sample_receiver: Receiver<f32>,
        device: Device,
        config: SupportedStreamConfig,
        volume: Arc<RwLock<f32>>,
        stop: Arc<Notify>,
        flush_flag: Arc<AtomicBool>,
        paused_flag: Arc<AtomicBool>,
    ) {
        let stream_config = StreamConfig {
            channels: config.channels(),
            sample_rate: config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        // Diagnostic counters for the cpal callback. Aggregated per
        // 1-second window so we can see whether the audio thread is
        // (a) still firing, (b) finding samples in the channel, or
        // (c) underrunning. Help diagnose the "silent 2nd play" path.
        use std::sync::atomic::AtomicU64 as DiagU64;
        let diag_calls = Arc::new(DiagU64::new(0));
        let diag_real = Arc::new(DiagU64::new(0));
        let diag_silent = Arc::new(DiagU64::new(0));
        let diag_calls_cb = Arc::clone(&diag_calls);
        let diag_real_cb = Arc::clone(&diag_real);
        let diag_silent_cb = Arc::clone(&diag_silent);
        let paused_flag_rep = Arc::clone(&paused_flag);

        let callback = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            if flush_flag.swap(false, Ordering::Relaxed) {
                while sample_receiver.try_recv().is_ok() {}
            }
            diag_calls_cb.fetch_add(1, Ordering::Relaxed);
            // While paused, emit silence WITHOUT draining the receiver —
            // resume picks up exactly where we left off.
            if paused_flag.load(Ordering::Relaxed) {
                for sample in data.iter_mut() {
                    *sample = Sample::EQUILIBRIUM;
                }
                return;
            }
            let vol = volume.blocking_read();
            let mut real = 0u64;
            let mut silent = 0u64;
            for sample in data.iter_mut() {
                match sample_receiver.try_recv() {
                    Ok(asample) => {
                        *sample = asample * *vol;
                        real += 1;
                    }
                    Err(_) => {
                        *sample = Sample::EQUILIBRIUM;
                        silent += 1;
                    }
                }
            }
            diag_real_cb.fetch_add(real, Ordering::Relaxed);
            diag_silent_cb.fetch_add(silent, Ordering::Relaxed);
        };

        // Periodic diagnostic reporter — runs on the tokio runtime, dumps
        // the callback counters every second.
        let diag_calls_rep = Arc::clone(&diag_calls);
        let diag_real_rep = Arc::clone(&diag_real);
        let diag_silent_rep = Arc::clone(&diag_silent);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let calls = diag_calls_rep.swap(0, Ordering::Relaxed);
                let real = diag_real_rep.swap(0, Ordering::Relaxed);
                let silent = diag_silent_rep.swap(0, Ordering::Relaxed);
                let paused = paused_flag_rep.load(Ordering::Relaxed);
                log::info!(
                    "[cpal] last 1s: calls={} real_samples={} silent_samples={} paused={}",
                    calls, real, silent, paused
                );
            }
        });

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
    ) -> (Sender<f32>, u32) {
        let (sample_sender, sample_receiver) = mpsc::channel::<f32>(192_000);

        let device = cpal::default_host()
            .default_output_device()
            .expect("No output device");

        let config: SupportedStreamConfig = device
            .default_output_config()
            .expect("Failed to get default config");

        let volume = Arc::new(RwLock::new(0.15_f32));

        let stop_cpal = stop.clone();
        let config_cpal = config.clone();
        let volume_cpal = Arc::clone(&volume);
        tokio::spawn(AudioRenderer::start_audio(
            sample_receiver,
            device,
            config_cpal,
            volume_cpal,
            stop_cpal,
            flush_flag,
            paused_flag,
        ));

        let volume_handle = Arc::clone(&volume);
        tokio::spawn(async move {
            while let Some(command) = command_receiver.recv().await {
                match command {
                    AudioRendererCommand::Stop => {
                        stop.notify_waiters();
                        break;
                    }
                    AudioRendererCommand::Volume(new_volume) => {
                        let mut vol = volume_handle.write().await;
                        *vol += new_volume;
                        log::debug!("volume: {}", *vol);
                    }
                }
            }
        });

        (sample_sender, config.sample_rate())
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

    pub async fn volume(&self, volume_diff: f32) {
        _ = self
            .command_sender
            .send(AudioRendererCommand::Volume(volume_diff))
            .await;
    }
}

impl super::AudioSink for AudioRenderer {
    fn put_samples<'a>(&'a self, samples: &'a [f32]) -> impl std::future::Future<Output = ()> + Send + 'a {
        self.put_samples_raw(samples)
    }

    fn sample_rate(&self) -> u32 {
        AudioRenderer::sample_rate(self)
    }

    fn flush(&self) {
        AudioRenderer::flush(self)
    }

    fn stop(&self) -> impl std::future::Future<Output = ()> + Send + '_ {
        AudioRenderer::stop(self)
    }

    fn volume(&self, diff: f32) -> impl std::future::Future<Output = ()> + Send + '_ {
        AudioRenderer::volume(self, diff)
    }

    fn set_paused(&self, paused: bool) {
        AudioRenderer::set_paused(self, paused)
    }

    fn last_peak_db(&self) -> Option<[f32; 2]> {
        AudioRenderer::last_peak_db(self)
    }
}
