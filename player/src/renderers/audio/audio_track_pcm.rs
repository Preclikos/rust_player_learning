//! Android PCM audio sink backed by a Java `AudioTrack` (instead of cpal/AAudio).
//!
//! Some Android TV audio HALs (e.g. the Google TV Streamer / MediaTek) *steal*
//! the app's AAudio output stream at `requestStart` — `open` succeeds but start
//! returns AAUDIO_ERROR_DISCONNECTED ("stream was probably stolen"), so cpal's
//! PCM output never plays and the audio clock never advances (video then starves
//! to a slideshow). The compressed-passthrough path works there because it uses
//! `AudioTrack`, not AAudio — so for non-passthrough PCM we use an `AudioTrack`
//! too, configured `ENCODING_PCM_FLOAT`.
//!
//! Clock semantics mirror the cpal sink exactly: `played_ms` is the track's
//! cumulative `getPlaybackHeadPosition` (frames presented since track creation),
//! and we NEVER call `AudioTrack.flush()` — a seek/rebuild only drains the Rust
//! sample queue, leaving the small device buffer to play out (same tiny tail as
//! cpal's device buffer). So `MediaClock` baselines it identically.
//!
//! Raw JNI (jni 0.22), mirroring `audio_passthrough::AudioTrackSink`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use jni::objects::JObject;
use jni::refs::GlobalRef;
use tokio::sync::mpsc::{self, Sender};

// android.media.AudioFormat
const ENCODING_PCM_FLOAT: i32 = 4;
const CHANNEL_OUT_STEREO: i32 = 12;
// android.media.AudioAttributes
const USAGE_MEDIA: i32 = 1;
const CONTENT_TYPE_MOVIE: i32 = 3;
// android.media.AudioTrack
const MODE_STREAM: i32 = 1;
const STATE_INITIALIZED: i32 = 1;
const WRITE_BLOCKING: i32 = 0;

fn android_vm() -> jni::JavaVM {
    let ctx = ndk_context::android_context();
    unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
}

/// A PCM-float `AudioTrack`. The clock source for the non-passthrough Android path.
pub struct AudioTrackPcmSink {
    track: GlobalRef<JObject<'static>>,
    sample_rate: u32,
    /// Set in Drop before stop/release so a late write/clock call no-ops rather
    /// than touching a released native object.
    stopped: AtomicBool,
    /// False until the first samples are written, at which point we lazily
    /// `play()` (honoring `paused`). Avoids playing / polling an empty track.
    started: AtomicBool,
    /// Desired paused state, honored across the lazy start (a pause that arrives
    /// before the first write must not be lost — av_sync starts audio at the
    /// first video frame via `set_paused(false)`).
    paused: AtomicBool,
}

// The GlobalRef + VM handle are safe to use from the audio writer thread.
unsafe impl Send for AudioTrackPcmSink {}
unsafe impl Sync for AudioTrackPcmSink {}

impl AudioTrackPcmSink {
    /// Create a paused PCM-float `AudioTrack` at `sample_rate` / stereo. Returns
    /// `None` on any failure (caller then has no audio; video uses the wall clock).
    pub fn new(sample_rate: u32) -> Option<Self> {
        let mask = CHANNEL_OUT_STEREO;
        let vm = android_vm();
        let res: Result<GlobalRef<JObject<'static>>, jni::errors::Error> =
            vm.attach_current_thread(|env| {
                // A tight buffer keeps the post-seek tail small (we never flush the
                // track). 2× the HW minimum guards against startup underruns.
                let min_buf = env
                    .call_static_method(
                        jni::jni_str!("android/media/AudioTrack"),
                        jni::jni_str!("getMinBufferSize"),
                        jni::jni_sig!("(III)I"),
                        &[(sample_rate as i32).into(), mask.into(), ENCODING_PCM_FLOAT.into()],
                    )?
                    .i()?;
                let buffer_bytes = if min_buf > 0 {
                    min_buf * 2
                } else {
                    // Fallback ≈ 100 ms of f32 stereo.
                    sample_rate as i32 * 2 * 4 / 10
                };

                // AudioFormat.Builder().setEncoding(PCM_FLOAT).setSampleRate(sr).setChannelMask(mask).build()
                let fb = env.new_object(
                    jni::jni_str!("android/media/AudioFormat$Builder"),
                    jni::jni_sig!("()V"),
                    &[],
                )?;
                let fb = env
                    .call_method(
                        &fb,
                        jni::jni_str!("setEncoding"),
                        jni::jni_sig!("(I)Landroid/media/AudioFormat$Builder;"),
                        &[ENCODING_PCM_FLOAT.into()],
                    )?
                    .l()?;
                let fb = env
                    .call_method(
                        &fb,
                        jni::jni_str!("setSampleRate"),
                        jni::jni_sig!("(I)Landroid/media/AudioFormat$Builder;"),
                        &[(sample_rate as i32).into()],
                    )?
                    .l()?;
                let fb = env
                    .call_method(
                        &fb,
                        jni::jni_str!("setChannelMask"),
                        jni::jni_sig!("(I)Landroid/media/AudioFormat$Builder;"),
                        &[mask.into()],
                    )?
                    .l()?;
                let format = env
                    .call_method(
                        &fb,
                        jni::jni_str!("build"),
                        jni::jni_sig!("()Landroid/media/AudioFormat;"),
                        &[],
                    )?
                    .l()?;

                // AudioAttributes.Builder().setUsage(MEDIA).setContentType(MOVIE).build()
                let ab = env.new_object(
                    jni::jni_str!("android/media/AudioAttributes$Builder"),
                    jni::jni_sig!("()V"),
                    &[],
                )?;
                let ab = env
                    .call_method(
                        &ab,
                        jni::jni_str!("setUsage"),
                        jni::jni_sig!("(I)Landroid/media/AudioAttributes$Builder;"),
                        &[USAGE_MEDIA.into()],
                    )?
                    .l()?;
                let ab = env
                    .call_method(
                        &ab,
                        jni::jni_str!("setContentType"),
                        jni::jni_sig!("(I)Landroid/media/AudioAttributes$Builder;"),
                        &[CONTENT_TYPE_MOVIE.into()],
                    )?
                    .l()?;
                let attrs = env
                    .call_method(
                        &ab,
                        jni::jni_str!("build"),
                        jni::jni_sig!("()Landroid/media/AudioAttributes;"),
                        &[],
                    )?
                    .l()?;

                // new AudioTrack.Builder()…build()
                let tb = env.new_object(
                    jni::jni_str!("android/media/AudioTrack$Builder"),
                    jni::jni_sig!("()V"),
                    &[],
                )?;
                let tb = env
                    .call_method(
                        &tb,
                        jni::jni_str!("setAudioAttributes"),
                        jni::jni_sig!("(Landroid/media/AudioAttributes;)Landroid/media/AudioTrack$Builder;"),
                        &[(&attrs).into()],
                    )?
                    .l()?;
                let tb = env
                    .call_method(
                        &tb,
                        jni::jni_str!("setAudioFormat"),
                        jni::jni_sig!("(Landroid/media/AudioFormat;)Landroid/media/AudioTrack$Builder;"),
                        &[(&format).into()],
                    )?
                    .l()?;
                let tb = env
                    .call_method(
                        &tb,
                        jni::jni_str!("setBufferSizeInBytes"),
                        jni::jni_sig!("(I)Landroid/media/AudioTrack$Builder;"),
                        &[buffer_bytes.into()],
                    )?
                    .l()?;
                let tb = env
                    .call_method(
                        &tb,
                        jni::jni_str!("setTransferMode"),
                        jni::jni_sig!("(I)Landroid/media/AudioTrack$Builder;"),
                        &[MODE_STREAM.into()],
                    )?
                    .l()?;
                let track = env
                    .call_method(
                        &tb,
                        jni::jni_str!("build"),
                        jni::jni_sig!("()Landroid/media/AudioTrack;"),
                        &[],
                    )?
                    .l()?;

                let state = env
                    .call_method(&track, jni::jni_str!("getState"), jni::jni_sig!("()I"), &[])?
                    .i()?;
                if state != STATE_INITIALIZED {
                    return Err(jni::errors::Error::JavaException);
                }
                env.new_global_ref(&track)
            });
        match res {
            Ok(track) => {
                log::info!("[audio-pcm] AudioTrack PCM_FLOAT configured (paused): {}Hz stereo", sample_rate);
                Some(Self {
                    track,
                    sample_rate,
                    stopped: AtomicBool::new(false),
                    started: AtomicBool::new(false),
                    paused: AtomicBool::new(false),
                })
            }
            Err(e) => {
                log::warn!("[audio-pcm] AudioTrack PCM init failed: {}", e);
                None
            }
        }
    }

    /// Write interleaved-stereo f32 samples (blocking — back-pressures the writer
    /// to the device's consumption rate, which makes the playback head a clock).
    pub fn write_floats(&self, samples: &[f32]) {
        if self.stopped.load(Ordering::Acquire) || samples.is_empty() {
            return;
        }
        let vm = android_vm();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            let arr = env.new_float_array(samples.len())?;
            env.set_float_array_region(&arr, 0, samples)?;
            // write(float[], offsetInFloats, sizeInFloats, WRITE_BLOCKING)
            env.call_method(
                self.track.as_obj(),
                jni::jni_str!("write"),
                jni::jni_sig!("([FIII)I"),
                &[
                    (&arr).into(),
                    0i32.into(),
                    (samples.len() as i32).into(),
                    WRITE_BLOCKING.into(),
                ],
            )?;
            Ok(())
        });
        // Lazily start on the first real samples, honoring a pause that arrived
        // first (seek/buffer): mark started so a later set_paused(false) can play,
        // but don't play() while paused.
        if !self.started.swap(true, Ordering::AcqRel) && !self.paused.load(Ordering::Acquire) {
            self.call_void(jni::jni_str!("play"));
        }
    }

    /// Cumulative presented-frame position as ms — the PCM clock source. `None`
    /// until the track is actually playing, so `MediaClock` uses the wall clock
    /// during startup (and if audio never starts, video still plays).
    pub fn played_ms(&self) -> Option<u64> {
        if self.sample_rate == 0
            || !self.started.load(Ordering::Acquire)
            || self.stopped.load(Ordering::Acquire)
        {
            return None;
        }
        let vm = android_vm();
        let res = vm.attach_current_thread(|env| -> Result<i64, jni::errors::Error> {
            let head = env
                .call_method(
                    self.track.as_obj(),
                    jni::jni_str!("getPlaybackHeadPosition"),
                    jni::jni_sig!("()I"),
                    &[],
                )?
                .i()? as u32 as i64;
            Ok(head)
        });
        match res {
            Ok(frames) => Some((frames.max(0) as u64) * 1000 / self.sample_rate as u64),
            Err(_) => None,
        }
    }

    fn call_void(&self, method: &'static jni::strings::JNIStr) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        let vm = android_vm();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            env.call_method(self.track.as_obj(), method, jni::jni_sig!("()V"), &[])?;
            Ok(())
        });
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
        // Before the first write the track has no data — don't touch it; the lazy
        // start in write_floats() applies the remembered state once it has data.
        if !self.started.load(Ordering::Acquire) {
            return;
        }
        if paused {
            self.call_void(jni::jni_str!("pause"));
        } else {
            self.call_void(jni::jni_str!("play"));
        }
    }

    pub fn set_volume(&self, volume: f32) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        let vm = android_vm();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            env.call_method(
                self.track.as_obj(),
                jni::jni_str!("setVolume"),
                jni::jni_sig!("(F)I"),
                &[volume.into()],
            )?;
            Ok(())
        });
    }
}

impl Drop for AudioTrackPcmSink {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let vm = android_vm();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            env.call_method(self.track.as_obj(), jni::jni_str!("stop"), jni::jni_sig!("()V"), &[])?;
            env.call_method(self.track.as_obj(), jni::jni_str!("release"), jni::jni_sig!("()V"), &[])?;
            Ok(())
        });
    }
}

/// Start the Android PCM output: create the [`AudioTrackPcmSink`] and a dedicated
/// writer thread that pulls resampled stereo f32 from the channel and
/// blocking-writes it to the track (the blocking write back-pressures the
/// producer to the device rate, so the playback head is a usable clock). Returns
/// the sink so the renderer can read its clock + drive pause. `None` sink ⇒ no
/// audio (video falls back to the wall clock).
pub(super) fn start_output(
    flush_flag: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
) -> (Sender<f32>, u32, Option<Arc<AudioTrackPcmSink>>) {
    let out_rate = 48_000u32;
    let (sample_sender, mut sample_receiver) = mpsc::channel::<f32>(192_000);
    let sink = AudioTrackPcmSink::new(out_rate).map(Arc::new);

    match sink.clone() {
        Some(sink) => {
            std::thread::Builder::new()
                .name("bz-audio-pcm-out".into())
                .spawn(move || {
                    let mut batch: Vec<f32> = Vec::with_capacity(8192);
                    // Blocks until samples arrive; exits when the sender (the
                    // AudioRenderer) is dropped on teardown.
                    while let Some(first) = sample_receiver.blocking_recv() {
                        // Seek/rebuild: drop queued PCM. We deliberately do NOT
                        // flush the track — keeping its head cumulative so the
                        // clock baselines exactly like the cpal counter; the
                        // small device buffer plays out (cpal does the same).
                        if flush_flag.swap(false, Ordering::Relaxed) {
                            while sample_receiver.try_recv().is_ok() {}
                            continue;
                        }
                        batch.clear();
                        batch.push(first);
                        while batch.len() < 8192 {
                            match sample_receiver.try_recv() {
                                Ok(s) => batch.push(s),
                                Err(_) => break,
                            }
                        }
                        let vol = f32::from_bits(volume.load(Ordering::Relaxed));
                        if (vol - 1.0).abs() > f32::EPSILON {
                            for s in batch.iter_mut() {
                                *s *= vol;
                            }
                        }
                        sink.write_floats(&batch);
                    }
                })
                .expect("spawn audio pcm output thread");
        }
        None => log::error!(
            "[audio-pcm] no AudioTrack output — playback will be silent (video uses wall clock)"
        ),
    }

    (sample_sender, out_rate, sink)
}
