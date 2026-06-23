//! Android audio passthrough sink.
//!
//! Feeds a compressed bitstream (E-AC-3 / AC-3) straight to an `AudioTrack`
//! configured with the matching `ENCODING_*`, so the HDMI AVR/soundbar decodes
//! it — instead of decoding to PCM here. The playback head of the track is the
//! clock source: `played_ms()` lets `MediaClock` pace video to the receiver's
//! real output position, exactly as it does for the cpal PCM sink.
//!
//! Raw `ndk`-less JNI (jni 0.22, mirroring `decoders::mediacodec`): there's no
//! NDK API for compressed AudioTrack, so we drive the Java `AudioTrack` through
//! the VM from `ndk_context`. Each call attaches the current thread (cheap when
//! already attached); writes happen at the audio access-unit rate (~30/s).

use jni::objects::JObject;
use jni::refs::GlobalRef;

// android.media.AudioFormat
pub const ENCODING_AC3: i32 = 5;
pub const ENCODING_E_AC3: i32 = 6;
const CHANNEL_OUT_STEREO: i32 = 12;
const CHANNEL_OUT_5POINT1: i32 = 252;
// android.media.AudioAttributes
const USAGE_MEDIA: i32 = 1;
const CONTENT_TYPE_MOVIE: i32 = 3;
// android.media.AudioTrack
const MODE_STREAM: i32 = 1;
const STATE_INITIALIZED: i32 = 1;
// A *direct* E-AC-3 track won't begin output until enough compressed data is
// buffered to cross its start threshold (~1–2 s of media observed on the Google
// TV Streamer + HDMI AVR). The buffer MUST exceed that threshold: the feed's
// prime phase relies on writing past it before `write` blocks, otherwise the
// track never starts and the head-paced feed deadlocks. 256 KiB ≈ 2.7 s at the
// 768 kbps test bitrate — comfortably above the observed ~2.5 s start point.
const BUFFER_SIZE_BYTES: i32 = 256 * 1024;

fn android_vm() -> jni::JavaVM {
    let ctx = ndk_context::android_context();
    unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
}

/// CLOCK_MONOTONIC now, in ns — the timebase of `AudioTimestamp.nanoTime`
/// (TIMEBASE_MONOTONIC, the default), so we can interpolate the timestamp's
/// frame position to the current instant.
fn clock_monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// A compressed-bitstream `AudioTrack`. The clock source for passthrough.
pub struct AudioTrackSink {
    track: GlobalRef<JObject<'static>>,
    sample_rate: u32,
    /// Set in Drop before stop/release; write/played_ms/lifecycle become no-ops
    /// so a feed or the clock touching the track mid-teardown can't hit a
    /// stopped/released native object (the seek-time crash).
    stopped: std::sync::atomic::AtomicBool,
    /// False until the first AU is written, at which point we lazily `play()`.
    /// We must NOT `play()` an empty direct track nor call `getTimestamp` before
    /// it's playing — the framework reports "dead IAudioTrack" and recreates it
    /// in a loop (no audio). The feed gates the first write on video readiness,
    /// so playback begins with the first real audio, in step with video.
    started: std::sync::atomic::AtomicBool,
    /// Desired paused state, honored ACROSS the lazy start. A `set_paused(true)`
    /// before the first AU can't touch the not-yet-playing track, so we remember
    /// it here and the lazy start in `write` skips `play()` while paused — else
    /// pausing during a seek/buffer (before audio begins) would be silently
    /// dropped and the first AU would start the track anyway (audio plays while
    /// the user paused; the clock then runs ahead of the parked video).
    paused: std::sync::atomic::AtomicBool,
}

// The GlobalRef + VM handle are thread-safe to use from the audio task.
unsafe impl Send for AudioTrackSink {}
unsafe impl Sync for AudioTrackSink {}

impl AudioTrackSink {
    /// Create + start an AudioTrack for the given compressed `encoding`
    /// (`ENCODING_E_AC3` / `ENCODING_AC3`). Returns `None` on any failure
    /// (caller falls back to PCM decode).
    pub fn new(encoding: i32, sample_rate: u32, channels: u16) -> Option<Self> {
        let mask = if channels > 2 {
            CHANNEL_OUT_5POINT1
        } else {
            CHANNEL_OUT_STEREO
        };
        let vm = android_vm();
        let res: Result<GlobalRef<JObject<'static>>, jni::errors::Error> = vm.attach_current_thread(|env| {
            // AudioFormat.Builder().setEncoding(enc).setSampleRate(sr).setChannelMask(mask).build()
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
                    &[encoding.into()],
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

            // new AudioTrack.Builder().setAudioAttributes(a).setAudioFormat(f)
            //     .setBufferSizeInBytes(n).setTransferMode(STREAM).build()
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
                    &[BUFFER_SIZE_BYTES.into()],
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
            // Do NOT play() here. av_sync starts the track (via set_paused
            // (false)) at the SAME instant it renders the first video frame, so
            // audio can't race seconds ahead during the video-decode startup
            // (the "audio way ahead" bug). The feed primes the buffer meanwhile
            // (write blocks on the full stopped-track buffer until play()).
            env.new_global_ref(&track)
        });
        match res {
            Ok(track) => {
                log::info!(
                    "[audio-pt] AudioTrack passthrough configured (paused): encoding={} {}Hz {}ch",
                    encoding, sample_rate, channels
                );
                Some(Self {
                    track,
                    sample_rate,
                    stopped: std::sync::atomic::AtomicBool::new(false),
                    started: std::sync::atomic::AtomicBool::new(false),
                    paused: std::sync::atomic::AtomicBool::new(false),
                })
            }
            Err(e) => {
                log::warn!("[audio-pt] AudioTrack passthrough init failed: {}", e);
                None
            }
        }
    }

    /// Write one compressed access unit (blocking — back-pressures the feed to
    /// the receiver's consumption rate, which is what makes the playback head a
    /// usable clock).
    pub fn write(&self, au: &[u8]) {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let vm = android_vm();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            let arr = env.byte_array_from_slice(au)?;
            // write(byte[], offsetInBytes, sizeInBytes): blocking in STREAM mode.
            env.call_method(
                self.track.as_obj(),
                jni::jni_str!("write"),
                jni::jni_sig!("([BII)I"),
                &[(&arr).into(), 0i32.into(), (au.len() as i32).into()],
            )?;
            Ok(())
        });
        // Lazily start playback on the first AU: the track now has real data, so
        // play() begins output with actual audio (no silent pre-roll counted by
        // the clock, no "dead IAudioTrack" from getTimestamp on an idle track).
        // Honor a pause that arrived before audio began (seek/buffer): mark the
        // track started (so a later set_paused(false) can play it) but DON'T
        // play() while paused — otherwise audio runs under a user pause and the
        // clock runs ahead of the parked video.
        if !self.started.swap(true, std::sync::atomic::Ordering::AcqRel)
            && !self.paused.load(std::sync::atomic::Ordering::Acquire)
        {
            self.call_void(jni::jni_str!("play"));
        }
    }

    /// Presented-frame position as milliseconds — the passthrough clock source.
    ///
    /// Uses `AudioTrack.getTimestamp()` (precise presentation `framePosition`),
    /// which gives a smooth, accurate position regardless of how coarsely the
    /// playback head updates — `getPlaybackHeadPosition` alone is bursty for
    /// compressed passthrough, so MediaClock's interpolation froze between
    /// updates and video stuttered. Falls back to the head before the first
    /// timestamp is available.
    pub fn played_ms(&self) -> Option<u64> {
        // Before the first AU (track not playing) getTimestamp churns the track
        // ("dead IAudioTrack"); report no clock so MediaClock uses its wall
        // fallback until audio actually starts.
        if self.sample_rate == 0
            || !self.started.load(std::sync::atomic::Ordering::Acquire)
            || self.stopped.load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let vm = android_vm();
        let res = vm.attach_current_thread(|env| -> Result<(i64, bool, i64), jni::errors::Error> {
            let ts = env.new_object(
                jni::jni_str!("android/media/AudioTimestamp"),
                jni::jni_sig!("()V"),
                &[],
            )?;
            let have_ts = env
                .call_method(
                    self.track.as_obj(),
                    jni::jni_str!("getTimestamp"),
                    jni::jni_sig!("(Landroid/media/AudioTimestamp;)Z"),
                    &[(&ts).into()],
                )?
                .z()?;
            if have_ts {
                // AudioTimestamp public fields (long): the presented frame at
                // CLOCK_MONOTONIC instant `nanoTime`. Interpolate to NOW —
                // returning the stale framePosition jitters the clock by the
                // (now − nanoTime) staleness, which judders the video.
                let frame_pos = env
                    .get_field(&ts, jni::jni_str!("framePosition"), jni::jni_sig!("J"))?
                    .j()?;
                let nano_time = env
                    .get_field(&ts, jni::jni_str!("nanoTime"), jni::jni_sig!("J"))?
                    .j()?;
                let dt_ns = clock_monotonic_ns() - nano_time;
                Ok((frame_pos + dt_ns * self.sample_rate as i64 / 1_000_000_000, true, frame_pos))
            } else {
                let head = env
                    .call_method(
                        self.track.as_obj(),
                        jni::jni_str!("getPlaybackHeadPosition"),
                        jni::jni_sig!("()I"),
                        &[],
                    )?
                    .i()?;
                Ok((head as u32 as i64, false, head as u32 as i64))
            }
        });
        match res {
            Ok((frames, have_ts, raw_frame_pos)) => {
                // Rate-limited trace — whether the playback head is advancing
                // (the prime/start-threshold behaviour, see audio_passthrough_task).
                use std::sync::atomic::{AtomicU64, Ordering};
                static N: AtomicU64 = AtomicU64::new(0);
                if N.fetch_add(1, Ordering::Relaxed) % 30 == 0 {
                    log::debug!(
                        "[audio-pt] played_ms probe: have_ts={} raw_frame_pos={} interp_frames={} -> {}ms",
                        have_ts, raw_frame_pos, frames,
                        (frames.max(0) as u64) * 1000 / self.sample_rate as u64
                    );
                }
                Some((frames.max(0) as u64) * 1000 / self.sample_rate as u64)
            }
            Err(_) => None,
        }
    }

    fn call_void(&self, method: &'static jni::strings::JNIStr) {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let vm = android_vm();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            env.call_method(self.track.as_obj(), method, jni::jni_sig!("()V"), &[])?;
            Ok(())
        });
    }

    pub fn pause(&self) {
        self.call_void(jni::jni_str!("pause"));
    }
    pub fn play(&self) {
        self.call_void(jni::jni_str!("play"));
    }
    pub fn flush(&self) {
        self.call_void(jni::jni_str!("flush"));
    }
}

impl crate::renderers::AudioPassthrough for AudioTrackSink {
    fn write(&self, au: &[u8]) {
        AudioTrackSink::write(self, au)
    }
    fn played_ms(&self) -> Option<u64> {
        AudioTrackSink::played_ms(self)
    }
    fn flush(&self) {
        AudioTrackSink::flush(self)
    }
    fn is_paused(&self) -> bool {
        self.paused.load(std::sync::atomic::Ordering::Acquire)
    }
    fn set_paused(&self, paused: bool) {
        // Always record the desired state — the lazy start in write() reads it,
        // so a pause/resume that arrives BEFORE the first AU (during a seek or
        // initial buffer) isn't lost.
        self.paused
            .store(paused, std::sync::atomic::Ordering::Release);
        // Until the first AU is written the track must not play (empty direct
        // track → "dead IAudioTrack"); the lazy start in write() applies the
        // remembered state once it has data.
        if !self.started.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        if paused {
            self.pause()
        } else {
            self.play()
        }
    }
    fn recover_stall(&self) {
        // Only meaningful once playing; never override a real pause.
        if !self.started.load(std::sync::atomic::Ordering::Acquire)
            || self.paused.load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        // Toggle pause→play to kick a track whose head wedged (AVR lost lock).
        self.pause();
        self.play();
    }
    fn head_debug(&self) -> (bool, i64) {
        if !self.started.load(std::sync::atomic::Ordering::Acquire)
            || self.stopped.load(std::sync::atomic::Ordering::Acquire)
        {
            return (false, 0);
        }
        let vm = android_vm();
        vm.attach_current_thread(|env| -> Result<(bool, i64), jni::errors::Error> {
            let ts = env.new_object(
                jni::jni_str!("android/media/AudioTimestamp"),
                jni::jni_sig!("()V"),
                &[],
            )?;
            let have_ts = env
                .call_method(
                    self.track.as_obj(),
                    jni::jni_str!("getTimestamp"),
                    jni::jni_sig!("(Landroid/media/AudioTimestamp;)Z"),
                    &[(&ts).into()],
                )?
                .z()?;
            let fp = if have_ts {
                env.get_field(&ts, jni::jni_str!("framePosition"), jni::jni_sig!("J"))?
                    .j()?
            } else {
                env.call_method(
                    self.track.as_obj(),
                    jni::jni_str!("getPlaybackHeadPosition"),
                    jni::jni_sig!("()I"),
                    &[],
                )?
                .i()? as u32 as i64
            };
            Ok((have_ts, fp))
        })
        .unwrap_or((false, 0))
    }
}

impl Drop for AudioTrackSink {
    fn drop(&mut self) {
        // Block concurrent write/clock callers first, then stop+release the
        // native track DIRECTLY (the guarded call_void would now no-op).
        self.stopped.store(true, std::sync::atomic::Ordering::Release);
        let vm = android_vm();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            env.call_method(self.track.as_obj(), jni::jni_str!("stop"), jni::jni_sig!("()V"), &[])?;
            env.call_method(self.track.as_obj(), jni::jni_str!("release"), jni::jni_sig!("()V"), &[])?;
            Ok(())
        });
    }
}
