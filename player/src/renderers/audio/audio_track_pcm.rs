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
const WRITE_NON_BLOCKING: i32 = 1;

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

    /// Write interleaved-stereo f32 samples with the same back-pressure
    /// behaviour as a blocking `AudioTrack.write` (the writer paces to the
    /// device's consumption rate, which makes the playback head a clock) — but
    /// implemented as NON-blocking writes in a retry loop that stays responsive:
    /// a blocking JNI write parked on a paused track wedged the writer thread,
    /// so a flush/rebuild issued during a long pause (the ABR tick fires there)
    /// was never serviced and resume played nothing. `abort` (the renderer's
    /// flush flag) abandons the remainder immediately — the caller drains the
    /// queue right after; teardown (`stopped`) exits too.
    pub fn write_floats(&self, samples: &[f32], abort: &AtomicBool) {
        let mut off = 0usize;
        while off < samples.len() {
            if self.stopped.load(Ordering::Acquire) || abort.load(Ordering::Relaxed) {
                return;
            }
            let chunk = &samples[off..];
            let vm = android_vm();
            let written = vm
                .attach_current_thread(|env| -> Result<i32, jni::errors::Error> {
                    let arr = env.new_float_array(chunk.len())?;
                    env.set_float_array_region(&arr, 0, chunk)?;
                    // write(float[], offsetInFloats, sizeInFloats, WRITE_NON_BLOCKING)
                    let n = env
                        .call_method(
                            self.track.as_obj(),
                            jni::jni_str!("write"),
                            jni::jni_sig!("([FIII)I"),
                            &[
                                (&arr).into(),
                                0i32.into(),
                                (chunk.len() as i32).into(),
                                WRITE_NON_BLOCKING.into(),
                            ],
                        )?
                        .i()?;
                    Ok(n)
                })
                .unwrap_or(-1);
            if written < 0 {
                return; // JNI failure — drop the batch rather than spin
            }
            if written > 0 {
                off += written as usize;
                if !self.started.swap(true, Ordering::AcqRel) {
                    log::info!(
                        "[audio-pcm] first write accepted (n={written}) paused={}",
                        self.paused.load(Ordering::Acquire)
                    );
                }
                // The track has data and is supposed to be running — make sure it
                // actually IS. The lazy first-write play() and the unpark in
                // set_paused can race each other at startup (av_sync's unpark and
                // the starvation gates fire around the same instant as the first
                // samples); if the play() is lost, the head never starts, the
                // audio clock freezes at 0 and the whole pipeline starves into
                // its 1 fps convoy. One JNI getPlayState per accepted batch
                // (~12/s) self-heals any such lost transition within ~85 ms.
                if !self.paused.load(Ordering::Acquire) && !self.is_playing() {
                    log::info!("[audio-pcm] track has data but isn't playing — starting it");
                    self.call_void(jni::jni_str!("play"));
                }
            } else {
                // Device buffer full (or track parked): let it drain, staying
                // responsive to flush/teardown.
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
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

    /// True when `AudioTrack.getPlayState() == PLAYSTATE_PLAYING` (3). Used by
    /// the writer's self-heal; errs on "playing" so a JNI failure can't spam
    /// play() calls.
    fn is_playing(&self) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return true;
        }
        let vm = android_vm();
        vm.attach_current_thread(|env| -> Result<bool, jni::errors::Error> {
            let state = env
                .call_method(
                    self.track.as_obj(),
                    jni::jni_str!("getPlayState"),
                    jni::jni_sig!("()I"),
                    &[],
                )?
                .i()?;
            Ok(state == 3) // AudioTrack.PLAYSTATE_PLAYING
        })
        .unwrap_or(true)
    }

    pub fn set_paused(&self, paused: bool) {
        let prev = self.paused.swap(paused, Ordering::AcqRel);
        let started = self.started.load(Ordering::Acquire);
        if prev != paused {
            log::info!("[audio-pcm] set_paused({paused}) started={started}");
        }
        // Before the first write the track has no data — don't touch it; the lazy
        // start in write_floats() applies the remembered state once it has data.
        if !started {
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
/// writer thread that pulls resampled stereo f32 from the channel and writes it
/// to the track. The write paces to the device rate (so the playback head is a
/// usable clock) but is built from NON-blocking writes internally and aborts on
/// the flush flag — a blocking write parked on a paused track wedged the writer,
/// so a flush/rebuild issued during a long pause was never serviced and resume
/// played nothing. Returns the sink so the renderer can read its clock + drive
/// pause. `None` sink ⇒ no audio (video falls back to the wall clock).
pub(super) fn start_output(
    flush_flag: Arc<AtomicBool>,
    host_paused: Arc<AtomicBool>,
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
                    // DIAG heartbeat (sparse): sink/track state while samples flow.
                    let mut last_beat = std::time::Instant::now();
                    // Blocks until samples arrive; exits when the sender (the
                    // AudioRenderer) is dropped on teardown.
                    while let Some(first) = sample_receiver.blocking_recv() {
                        if last_beat.elapsed() >= std::time::Duration::from_secs(5) {
                            last_beat = std::time::Instant::now();
                            log::info!(
                                "[audio-pcm] writer: host_paused={} sink_paused={} playing={} played_ms={:?}",
                                host_paused.load(Ordering::Relaxed),
                                sink.paused.load(Ordering::Acquire),
                                sink.is_playing(),
                                sink.played_ms(),
                            );
                        }
                        // Seek/rebuild: drop queued PCM. We deliberately do NOT
                        // flush the track — keeping its head cumulative so the
                        // clock baselines exactly like the cpal counter; the
                        // small device buffer plays out (cpal does the same).
                        if flush_flag.swap(false, Ordering::Relaxed) {
                            while sample_receiver.try_recv().is_ok() {}
                            continue;
                        }
                        // Host pause: hold this sample and don't consume further —
                        // queued samples must survive the pause so resume picks up
                        // exactly where it left off (the track itself was paused
                        // by the inherent set_paused). Without this gate the
                        // writer keeps feeding the buffered ~2 s into the track
                        // (audio bleeding on after the user paused). The flush
                        // flag stays serviced so a rebuild issued mid-pause (the
                        // ABR tick fires there) still drains.
                        if host_paused.load(Ordering::Relaxed) {
                            while host_paused.load(Ordering::Relaxed)
                                && !flush_flag.load(Ordering::Relaxed)
                            {
                                std::thread::sleep(std::time::Duration::from_millis(15));
                            }
                            if flush_flag.swap(false, Ordering::Relaxed) {
                                while sample_receiver.try_recv().is_ok() {}
                                continue;
                            }
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
                        // Paces like a blocking write but aborts on flush (the
                        // remainder is dropped; the NEXT loop turn swaps the flag
                        // and drains the queue).
                        sink.write_floats(&batch, &flush_flag);
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
