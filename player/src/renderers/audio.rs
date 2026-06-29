use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

#[cfg(any(target_os = "windows", target_os = "linux"))]
use ffmpeg_next::frame::Audio;
use tokio::sync::{
    mpsc::{self, Sender},
    Notify,
};

// Per-platform output backends, each in its own file (mirrors the `video`
// module's per-backend split): cpal PCM on desktop/iOS, an AudioTrack PCM sink
// on Android (cpal/AAudio is stolen on some TV HALs), and an AudioTrack
// bitstream sink for compressed passthrough.
#[cfg(not(target_os = "android"))]
mod audio_cpal;
#[cfg(target_os = "android")]
pub mod audio_passthrough;
#[cfg(target_os = "android")]
mod audio_track_pcm;

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
    /// Output-path latency in ms (device buffer + DAC), from the cpal
    /// callback timestamp. 0 until the first callback / when the backend
    /// can't report it. The video sync loop subtracts this from its wall
    /// clock so video reaches the screen when the matching audio reaches
    /// the speaker — otherwise video leads by this much at every
    /// (re)start, the "audio delayed after a seek/switch" symptom.
    output_latency_ms: Arc<AtomicU64>,
    /// When set (audio passthrough engaged), the cpal PCM path is dormant and
    /// this bitstream output is the real output + clock source: `played_ms` /
    /// `output_latency_ms` / `flush` / `set_paused` delegate to it, so
    /// `MediaClock` and the rest of the player drive it through the same
    /// `AudioSink` interface without knowing it's a compressed bitstream.
    passthrough: std::sync::Mutex<Option<Arc<dyn super::AudioPassthrough>>>,
    /// Android only: the AudioTrack PCM output (cpal/AAudio is stolen on some TV
    /// HALs). When present it is the PCM output + clock source; `played_ms` /
    /// `set_paused` delegate to it. `None` if the track couldn't be created.
    #[cfg(target_os = "android")]
    pcm_sink: Option<Arc<audio_track_pcm::AudioTrackPcmSink>>,
}

enum AudioRendererCommand {
    Stop,
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
        // Populated by the cpal callback from the backend's playback
        // timestamp where the platform reports it (desktop / iOS CoreAudio);
        // stays 0 on backends that don't (Android oboe), where the video
        // sync loop instead anchors its clock to when audio actually starts
        // playing — both universal, no per-device constants.
        let output_latency_ms = Arc::new(AtomicU64::new(0));
        let (command_sender, command_receiver) = mpsc::channel(4);

        #[cfg(not(target_os = "android"))]
        let (sample_sender, sample_rate) = {
            let t = audio_cpal::start_thread(
                command_receiver,
                stop,
                flush_flag.clone(),
                paused_flag.clone(),
                volume.clone(),
                samples_consumed.clone(),
                output_latency_ms.clone(),
            );
            (t.0, t.1)
        };
        // Android outputs PCM through an AudioTrack (cpal/AAudio is stolen on some
        // TV HALs); the cpal stop/command machinery is unused on this path.
        #[cfg(target_os = "android")]
        let (sample_sender, sample_rate, pcm_sink) = {
            drop(command_receiver);
            drop(stop);
            audio_track_pcm::start_output(flush_flag.clone(), volume.clone())
        };

        AudioRenderer {
            command_sender,
            sample_sender,
            sample_rate,
            flush_flag,
            paused_flag,
            volume,
            peak_l_db: Arc::new(AtomicU32::new(0)),
            peak_r_db: Arc::new(AtomicU32::new(0)),
            peak_seen: Arc::new(AtomicBool::new(false)),
            samples_consumed,
            output_latency_ms,
            passthrough: std::sync::Mutex::new(None),
            #[cfg(target_os = "android")]
            pcm_sink,
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

    /// Output-path latency in ms (device buffer + DAC) reported by the cpal
    /// backend; 0 until the first callback or when unsupported.
    pub fn output_latency_ms(&self) -> u64 {
        self.output_latency_ms.load(Ordering::Relaxed)
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
        // Passthrough: the bitstream output's playback head is the clock source.
        if let Some(pt) = self.passthrough.lock().unwrap().as_ref() {
            return pt.played_ms();
        }
        // Android: PCM is output via AudioTrack; its cumulative playback head is
        // the clock (same session-cumulative semantics as the cpal counter).
        #[cfg(target_os = "android")]
        {
            return self.pcm_sink.as_ref().and_then(|s| s.played_ms());
        }
        #[cfg(not(target_os = "android"))]
        {
            if self.sample_rate == 0 {
                return None;
            }
            // Interleaved stereo: 2 f32s per frame at the OUTPUT rate (the
            // resampler preserves duration, so output time = media time).
            let frames = self.samples_consumed.load(Ordering::Relaxed) / 2;
            Some(frames * 1000 / self.sample_rate as u64)
        }
    }

    fn output_latency_ms(&self) -> u64 {
        if let Some(pt) = self.passthrough.lock().unwrap().as_ref() {
            return pt.output_latency_ms();
        }
        // Android AudioTrack: played_ms is the presented head, so no extra
        // output-path latency to fold in.
        #[cfg(target_os = "android")]
        {
            0
        }
        #[cfg(not(target_os = "android"))]
        {
            AudioRenderer::output_latency_ms(self)
        }
    }

    fn flush(&self) {
        if let Some(pt) = self.passthrough.lock().unwrap().as_ref() {
            pt.flush();
            return;
        }
        AudioRenderer::flush(self)
    }

    fn is_passthrough(&self) -> bool {
        self.passthrough.lock().unwrap().is_some()
    }

    fn set_passthrough(&self, pt: Option<Arc<dyn super::AudioPassthrough>>) {
        // Engaging passthrough silences the cpal PCM path so it doesn't fight
        // the compressed AudioTrack for the HDMI output (concurrent PCM
        // disconnects the bitstream stream — the EPIPE / underrun spam).
        let engaging = pt.is_some();
        *self.passthrough.lock().unwrap() = pt;
        self.paused_flag.store(engaging, Ordering::Relaxed);
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
        if let Some(pt) = self.passthrough.lock().unwrap().as_ref() {
            pt.set_paused(paused);
            return;
        }
        // Android AudioTrack output: drive its play/pause (lazy-started).
        #[cfg(target_os = "android")]
        if let Some(s) = self.pcm_sink.as_ref() {
            s.set_paused(paused);
        }
        AudioRenderer::set_paused(self, paused)
    }

    fn last_peak_db(&self) -> Option<[f32; 2]> {
        AudioRenderer::last_peak_db(self)
    }
}
