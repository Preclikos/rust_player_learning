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
    channels: u16,
    flush_flag: Arc<AtomicBool>,
    paused_flag: Arc<AtomicBool>,
    /// Number of interleaved samples (f32 slots) cpal should silently
    /// drop from the front of the device queue on its next invocation.
    /// Used by `drop_ms` to fast-forward audio when video_sync_loop
    /// catches up by dropping frames.
    drop_pending: Arc<std::sync::atomic::AtomicU64>,
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
        let drop_pending = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
        let audio_thread = AudioRenderer::start_thread(
            command_receiver,
            stop,
            flush_flag.clone(),
            paused_flag.clone(),
            drop_pending.clone(),
        );

        AudioRenderer {
            command_sender,
            sample_sender: audio_thread.0,
            sample_rate: audio_thread.1,
            channels: audio_thread.2,
            flush_flag,
            paused_flag,
            drop_pending,
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
        drop_pending: Arc<std::sync::atomic::AtomicU64>,
    ) {
        let stream_config = StreamConfig {
            channels: config.channels(),
            sample_rate: config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        let callback = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            if flush_flag.swap(false, Ordering::Relaxed) {
                while sample_receiver.try_recv().is_ok() {}
            }
            // `drop_pending` is set by `drop_ms` whenever video_sync_loop
            // catches up by skipping frames — we drain that many samples
            // from the front of the queue so the audio jumps forward by
            // the same wall-clock amount.
            let pending_drop = drop_pending.swap(0, Ordering::Relaxed);
            for _ in 0..pending_drop {
                if sample_receiver.try_recv().is_err() {
                    break;
                }
            }
            // While paused, emit silence WITHOUT draining the receiver —
            // resume picks up exactly where we left off.
            if paused_flag.load(Ordering::Relaxed) {
                for sample in data.iter_mut() {
                    *sample = Sample::EQUILIBRIUM;
                }
                return;
            }
            let vol = volume.blocking_read();
            for sample in data.iter_mut() {
                let asample = sample_receiver.try_recv().unwrap_or(Sample::EQUILIBRIUM);
                *sample = asample * *vol;
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
        drop_pending: Arc<std::sync::atomic::AtomicU64>,
    ) -> (Sender<f32>, u32, u16) {
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
            drop_pending,
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

        (sample_sender, config.sample_rate(), config.channels())
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

    /// Schedule the next cpal callback to discard ~`ms` of buffered
    /// audio from the front of the device queue. Convert ms → device
    /// frames → interleaved sample count (one slot per channel per
    /// frame). The atomic is `fetch_add`-ed so repeated drops during a
    /// run of late video frames accumulate correctly.
    pub fn drop_ms(&self, ms: u64) {
        let drop_samples = ms
            .saturating_mul(self.sample_rate as u64)
            .saturating_mul(self.channels as u64)
            / 1000;
        if drop_samples > 0 {
            self.drop_pending
                .fetch_add(drop_samples, Ordering::Relaxed);
        }
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

    fn drop_ms(&self, ms: u64) {
        AudioRenderer::drop_ms(self, ms)
    }

    fn last_peak_db(&self) -> Option<[f32; 2]> {
        AudioRenderer::last_peak_db(self)
    }
}
