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
// Compressed frames are tiny (E-AC-3 ≤ ~2 KiB); this buffers ~tens of frames.
const BUFFER_SIZE_BYTES: i32 = 64 * 1024;

fn android_vm() -> jni::JavaVM {
    let ctx = ndk_context::android_context();
    unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
}

/// A compressed-bitstream `AudioTrack`. The clock source for passthrough.
pub struct AudioTrackSink {
    track: GlobalRef<JObject<'static>>,
    sample_rate: u32,
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
            env.call_method(&track, jni::jni_str!("play"), jni::jni_sig!("()V"), &[])?;
            env.new_global_ref(&track)
        });
        match res {
            Ok(track) => {
                log::info!(
                    "[audio-pt] AudioTrack passthrough started: encoding={} {}Hz {}ch",
                    encoding, sample_rate, channels
                );
                Some(Self { track, sample_rate })
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
    }

    /// Frames played, as milliseconds — the passthrough clock source.
    pub fn played_ms(&self) -> Option<u64> {
        if self.sample_rate == 0 {
            return None;
        }
        let vm = android_vm();
        let res = vm.attach_current_thread(|env| -> Result<i32, jni::errors::Error> {
            Ok(env
                .call_method(
                    self.track.as_obj(),
                    jni::jni_str!("getPlaybackHeadPosition"),
                    jni::jni_sig!("()I"),
                    &[],
                )?
                .i()?)
        });
        match res {
            Ok(frames) => Some((frames as u32 as u64) * 1000 / self.sample_rate as u64),
            Err(_) => None,
        }
    }

    fn call_void(&self, method: &'static jni::strings::JNIStr) {
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

impl Drop for AudioTrackSink {
    fn drop(&mut self) {
        self.call_void(jni::jni_str!("stop"));
        self.call_void(jni::jni_str!("release"));
    }
}
