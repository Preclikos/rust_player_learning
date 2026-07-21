//! Android PCM audio sink backed by a Java `AudioTrack` (instead of cpal/AAudio).
//!
//! Some Android TV audio HALs (e.g. the Google TV Streamer / MediaTek) *steal*
//! the app's AAudio output stream at `requestStart` — `open` succeeds but start
//! returns AAUDIO_ERROR_DISCONNECTED ("stream was probably stolen"), so cpal's
//! PCM output never plays and the audio clock never advances (video then starves
//! to a slideshow). The compressed-passthrough path works there because it uses
//! `AudioTrack`, not AAudio — so for non-passthrough PCM we use an `AudioTrack`
//! too, configured `ENCODING_PCM_16BIT` (see the constant for why not FLOAT).
//!
//! Clock semantics mirror the cpal sink exactly: `played_ms` is the track's
//! cumulative `getPlaybackHeadPosition` (frames presented since track creation),
//! and a seek/rebuild NEVER calls `AudioTrack.flush()` — it only drains the Rust
//! sample queue, leaving the small device buffer to play out (same tiny tail as
//! cpal's device buffer). So `MediaClock` baselines it identically. When the
//! stall heal replaces a wedged track (`recreate_track`) the discarded frames
//! are added to the clock so the position stays continuous.
//!
//! Raw JNI (jni 0.22), mirroring `audio_passthrough::AudioTrackSink`.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use jni::objects::JObject;
use jni::refs::GlobalRef;
use tokio::sync::mpsc::{self, Sender};

/// CLOCK_MONOTONIC now, in ns — the timebase of `AudioTimestamp.nanoTime`
/// (TIMEBASE_MONOTONIC, the default), so the timestamp's frame position can
/// be interpolated to the current instant. Same local copy as
/// audio_passthrough.rs.
fn clock_monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

// android.media.AudioFormat
/// 16-bit PCM — the conventional Android output format (what every ExoPlayer
/// app uses), adopted while chasing a wedge whose real cause turned out to be
/// the half-frame write spin (see the writer's `carry`). PCM_FLOAT likely
/// works too, but there is no reason to leave the well-trodden path.
const ENCODING_PCM_16BIT: i32 = 2;
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

/// A 16-bit PCM `AudioTrack`. The clock source for the non-passthrough Android path.
pub struct AudioTrackPcmSink {
    /// The live `AudioTrack`. Behind a mutex because the stall heal can swap in
    /// a freshly built track when the current one wedges beyond client-side
    /// repair (see `recreate_track`).
    track: Mutex<GlobalRef<JObject<'static>>>,
    sample_rate: u32,
    /// Final head positions of released (recreated-away) tracks — the new
    /// track's head restarts at 0, so `played_ms` adds this base to stay
    /// cumulative for `MediaClock`.
    head_base: AtomicU64,
    /// Set in Drop before stop/release so a late write/clock call no-ops rather
    /// than touching a released native object.
    stopped: AtomicBool,
    /// False until the first samples are written (log bookkeeping).
    started: AtomicBool,
    /// True once the CURRENT track holds enough PCM to survive its first mixer
    /// pulls (~100 ms written, the device buffer reporting full, or a 500 ms
    /// timeout). `play()` is never issued before this: starting a track with a
    /// near-empty buffer underruns instantly, and this device's AudioFlinger
    /// then drops the track from the mix and never recovers it (the client
    /// play state stays PLAYING throughout). Reset when the track is recreated
    /// so the replacement primes again.
    primed: AtomicBool,
    /// Frames accepted by `write()` into the CURRENT track (priming gauge;
    /// reset on recreate).
    track_written: AtomicU64,
    /// When the current track's first write was accepted (priming timeout).
    track_first_write: Mutex<Option<std::time::Instant>>,
    /// Cumulative frames accepted by `write()` across all tracks — used to
    /// compute how much queued-but-unplayed PCM a recreate discards.
    written_frames: AtomicU64,
    /// Frames discarded by heal `flush()`es, counted into `played_ms` so the
    /// clock skips over the dropped span instead of drifting behind the
    /// content by that much (lip-sync would be off forever otherwise).
    dropped_frames: AtomicU64,
    /// Desired paused state, honored across the lazy start (a pause that arrives
    /// before the first write must not be lost — av_sync starts audio at the
    /// first video frame via `set_paused(false)`).
    paused: AtomicBool,
    /// Playback-head rate watchdog state (see `check_stall`).
    stall: Mutex<StallState>,
    /// When healing failed even through a track recreate, the sink surrenders
    /// until this instant: writes are discarded (counted into
    /// `dropped_frames`), `played_ms` is `None` (video runs on the wall clock,
    /// silent, instead of starving into a slideshow against a frozen audio
    /// clock), and on expiry a fresh track is tried again.
    surrendered_until: Mutex<Option<std::time::Instant>>,
}

#[derive(Default)]
struct StallState {
    since: Option<std::time::Instant>,
    head: i64,
    strikes: u32,
}

// The GlobalRef + VM handle are safe to use from the audio writer thread.
unsafe impl Send for AudioTrackPcmSink {}
unsafe impl Sync for AudioTrackPcmSink {}

impl AudioTrackPcmSink {
    /// Create a paused 16-bit PCM `AudioTrack` at `sample_rate` / stereo. Returns
    /// `None` on any failure (caller then has no audio; video uses the wall clock).
    pub fn new(sample_rate: u32) -> Option<Self> {
        match Self::build_track(sample_rate) {
            Ok(track) => {
                log::info!("[audio-pcm] AudioTrack PCM_16BIT configured (paused): {}Hz stereo", sample_rate);
                Some(Self {
                    track: Mutex::new(track),
                    sample_rate,
                    head_base: AtomicU64::new(0),
                    stopped: AtomicBool::new(false),
                    started: AtomicBool::new(false),
                    primed: AtomicBool::new(false),
                    track_written: AtomicU64::new(0),
                    track_first_write: Mutex::new(None),
                    written_frames: AtomicU64::new(0),
                    dropped_frames: AtomicU64::new(0),
                    paused: AtomicBool::new(false),
                    stall: Mutex::new(StallState::default()),
                    surrendered_until: Mutex::new(None),
                })
            }
            Err(e) => {
                log::warn!("[audio-pcm] AudioTrack PCM init failed: {}", e);
                None
            }
        }
    }

    /// Build an initialized 16-bit PCM `AudioTrack` (used by `new` and by the
    /// stall heal's `recreate_track`).
    fn build_track(
        sample_rate: u32,
    ) -> Result<GlobalRef<JObject<'static>>, jni::errors::Error> {
        let mask = CHANNEL_OUT_STEREO;
        let vm = android_vm();
        vm.attach_current_thread(|env| {
                // A tight buffer keeps the post-seek tail small (we never flush the
                // track). 2× the HW minimum guards against startup underruns.
                let min_buf = env
                    .call_static_method(
                        jni::jni_str!("android/media/AudioTrack"),
                        jni::jni_str!("getMinBufferSize"),
                        jni::jni_sig!("(III)I"),
                        &[(sample_rate as i32).into(), mask.into(), ENCODING_PCM_16BIT.into()],
                    )?
                    .i()?;
                let buffer_bytes = if min_buf > 0 {
                    min_buf * 2
                } else {
                    // Fallback ≈ 100 ms of f32 stereo.
                    sample_rate as i32 * 2 * 4 / 10
                };

                // AudioFormat.Builder().setEncoding(PCM_16BIT).setSampleRate(sr).setChannelMask(mask).build()
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
                        &[ENCODING_PCM_16BIT.into()],
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
            })
    }

    /// Last-resort stall heal: the wedged track is unrecoverable client-side —
    /// bare `play()`, a pause/play cycle (re-adds it to the mix, AudioFlinger
    /// drops it again within ~0.5 s) and `flush()` were all measured dead ends
    /// on the Streamer; the server keeps seeing an empty buffer while the
    /// client sees it full. So build a NEW `AudioTrack` and swap it in. The
    /// old track's queued PCM dies with it: counted into `dropped_frames`, and
    /// its final head rolls into `head_base`, so `played_ms` stays cumulative
    /// and the clock skips over the lost span (lip-sync preserved).
    fn recreate_track(&self) -> bool {
        let old_head = self.head_frames().unwrap_or(0).max(0) as u64;
        // The replacement must prime again before it is played — starting it
        // with a near-empty buffer would repeat the exact wedge being healed.
        // Un-priming first also keeps other threads (set_paused, played_ms)
        // off the dying track through the standby wait below.
        self.primed.store(false, Ordering::Release);
        self.track_written.store(0, Ordering::Release);
        *self.track_first_write.lock().unwrap() = None;
        {
            let track = self.track.lock().unwrap();
            let vm = android_vm();
            let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
                env.call_method(track.as_obj(), jni::jni_str!("stop"), jni::jni_sig!("()V"), &[])?;
                env.call_method(track.as_obj(), jni::jni_str!("release"), jni::jni_sig!("()V"), &[])?;
                Ok(())
            });
        }
        // Wait out the output thread's standby delay (3 s on this HAL) with no
        // track alive: `outStream->standby()` is what resets the wedged HAL
        // stream. Rebuilding immediately lands the new track on the same
        // wedged stream — 10 back-to-back recreates in a wedged run measured
        // dead-on-arrival, while a fresh process (which takes >3 s to come up)
        // always got a working track.
        log::warn!("[audio-pcm] recreating track after standby settle…");
        for _ in 0..35 {
            if self.stopped.load(Ordering::Acquire) {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let new_track = match Self::build_track(self.sample_rate) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("[audio-pcm] recreate failed to build a new track: {e}");
                return false;
            }
        };
        *self.track.lock().unwrap() = new_track;
        let head_base = self.head_base.fetch_add(old_head, Ordering::AcqRel) + old_head;
        let dropped = self.dropped_frames.load(Ordering::Acquire);
        let queued = self
            .written_frames
            .load(Ordering::Acquire)
            .saturating_sub(head_base + dropped);
        self.dropped_frames.fetch_add(queued, Ordering::AcqRel);
        // PROBE the replacement with ~340 ms of silence before declaring it
        // alive: `primed` stays false (clock stays on the wall — no frozen
        // Some-static window pausing video) until the head is SEEN moving.
        // During hard episodes every replacement is dead on arrival; a dead
        // probe sends the ladder straight to surrender with zero clock impact.
        if self.paused.load(Ordering::Acquire) {
            log::info!("[audio-pcm] track recreated while paused — leaving unprimed");
            return true;
        }
        // Fill most of the device buffer (24624 frames) with silence: the
        // mixer pulls ~256 ms chunks and never touches a track holding less
        // than one chunk (BUFFER TIMEOUT removal).
        let silence = vec![0i16; 8192];
        let mut probe_samples = 0u64;
        for _ in 0..6 {
            // Non-blocking writes; a fresh track has ample space, so the
            // chunks land instantly. Errors just make the probe fail below.
            probe_samples += self.write_probe_chunk(&silence);
        }
        self.call_void(jni::jni_str!("play"));
        // The deep-buffer head updates only per HAL burst — allow ~2.5 s
        // before declaring the track dead (a live probe was once mislabeled
        // dead within 1 s while its frames demonstrably played out).
        let mut alive = false;
        for _ in 0..25 {
            if self.stopped.load(Ordering::Acquire) {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            if self.head_frames().unwrap_or(0) > 0 {
                alive = true;
                break;
            }
        }
        if alive {
            // The probe silence advances the head without carrying content —
            // take it back out of the accounting so `played_ms` stays aligned
            // with real PCM (`dropped` accumulated over the episode is far
            // larger than one probe, but guard the subtraction anyway).
            let probe_frames = probe_samples / 2;
            let _ = self.dropped_frames.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |d| Some(d.saturating_sub(probe_frames)),
            );
            self.primed.store(true, Ordering::Release);
            // NOTE: callers reset the stall state — check_stall holds its lock
            // across this call, so touching it here would self-deadlock.
            log::warn!(
                "[audio-pcm] track recreated and verified alive (old head {old_head}, {queued} queued frames skipped)"
            );
        } else {
            log::warn!(
                "[audio-pcm] recreated track is dead on arrival (head never moved on probe)"
            );
        }
        alive
    }

    /// Push one chunk of probe PCM with a short non-blocking retry budget.
    /// Returns the number of samples (shorts) the track accepted.
    fn write_probe_chunk(&self, chunk: &[i16]) -> u64 {
        let vm = android_vm();
        for _ in 0..5 {
            if self.stopped.load(Ordering::Acquire) {
                return 0;
            }
            let track = self.track.lock().unwrap();
            let written = vm
                .attach_current_thread(|env| -> Result<i32, jni::errors::Error> {
                    let arr = env.new_short_array(chunk.len())?;
                    env.set_short_array_region(&arr, 0, chunk)?;
                    let n = env
                        .call_method(
                            track.as_obj(),
                            jni::jni_str!("write"),
                            jni::jni_sig!("([SIII)I"),
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
            drop(track);
            if written != 0 {
                return written.max(0) as u64; // accepted or failed — done
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        0
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
        // Surrendered: discard at REALTIME pace (keeping the pipeline flowing,
        // the clock accounting intact, and decode from racing ahead of the
        // wall clock — an instant discard inflates `dropped_frames` beyond
        // realtime and the revival point leaps ahead, making video sprint to
        // catch up) until the retry deadline, then try a fresh track.
        {
            let mut sur = self.surrendered_until.lock().unwrap();
            if let Some(until) = *sur {
                if std::time::Instant::now() < until {
                    drop(sur);
                    self.discard_paced(samples.len(), abort);
                    return;
                }
                *sur = None;
                drop(sur);
                log::warn!("[audio-pcm] surrender expired — trying a fresh track");
                if self.recreate_track() {
                    *self.stall.lock().unwrap() = StallState::default();
                } else {
                    // Still dead — stay surrendered for another round.
                    *self.surrendered_until.lock().unwrap() = Some(
                        std::time::Instant::now() + std::time::Duration::from_secs(30),
                    );
                    self.discard_paced(samples.len(), abort);
                    return;
                }
            }
        }
        // Convert once for the whole batch — the track is 16-bit (see
        // ENCODING_PCM_16BIT); one short == one f32 sample, so all the offset
        // and frame math below is unchanged.
        let mut pcm: Vec<i16> = samples
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
        // Whole frames only — `write` never accepts a trailing half frame and
        // retrying it returns 0 forever. The writer thread already carries odd
        // samples between batches; this guard protects any other caller.
        if pcm.len() % 2 == 1 {
            log::warn!("[audio-pcm] odd batch of {} samples — truncating to whole frames", pcm.len());
            pcm.pop();
        }
        let mut zero_streak = 0u32;
        while off < pcm.len() {
            if self.stopped.load(Ordering::Acquire) || abort.load(Ordering::Relaxed) {
                return;
            }
            // A surrender can be declared by check_stall() mid-batch — bail out
            // (counting the remainder), don't keep retrying writes against the
            // dead track until the batch ends.
            if self.surrendered_until.lock().unwrap().is_some() {
                self.discard_paced(pcm.len() - off, abort);
                return;
            }
            let chunk = &pcm[off..];
            let vm = android_vm();
            let track = self.track.lock().unwrap();
            let written = vm
                .attach_current_thread(|env| -> Result<i32, jni::errors::Error> {
                    let arr = env.new_short_array(chunk.len())?;
                    env.set_short_array_region(&arr, 0, chunk)?;
                    // write(short[], offsetInShorts, sizeInShorts, WRITE_NON_BLOCKING)
                    let n = env
                        .call_method(
                            track.as_obj(),
                            jni::jni_str!("write"),
                            jni::jni_sig!("([SIII)I"),
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
            drop(track);
            if written < 0 {
                return; // JNI failure — drop the batch rather than spin
            }
            if written > 0 {
                off += written as usize;
                zero_streak = 0;
                let frames = written as u64 / 2;
                self.written_frames.fetch_add(frames, Ordering::AcqRel);
                if self.track_written.fetch_add(frames, Ordering::AcqRel) == 0 {
                    *self.track_first_write.lock().unwrap() = Some(std::time::Instant::now());
                }
                if !self.started.swap(true, Ordering::AcqRel) {
                    log::info!(
                        "[audio-pcm] first write accepted (n={written}) paused={}",
                        self.paused.load(Ordering::Acquire)
                    );
                }
                // Once primed, the track is supposed to be running — make sure
                // it actually IS (the unpark in set_paused and the lazy start
                // here can race; one JNI getPlayState per accepted batch, ~12/s,
                // recovers a lost transition within ~85 ms).
                if self.try_prime() && !self.paused.load(Ordering::Acquire) && !self.is_playing()
                {
                    log::info!("[audio-pcm] track has data but isn't playing — starting it");
                    self.call_void(jni::jni_str!("play"));
                }
            } else {
                // Device buffer full (or track parked): let it drain, staying
                // responsive to flush/teardown. A single zero write is NOT
                // proof of a full buffer (transient obtainBuffer misses were
                // seen with a few hundred frames in) — only treat a SUSTAINED
                // run of zeros as full, and only then consider it primed.
                zero_streak += 1;
                if zero_streak >= 50 && !self.primed.swap(true, Ordering::AcqRel) {
                    if !self.paused.load(Ordering::Acquire) && !self.is_playing() {
                        log::info!(
                            "[audio-pcm] device buffer full ({} frames) — starting playback",
                            self.track_written.load(Ordering::Acquire)
                        );
                        self.call_void(jni::jni_str!("play"));
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            self.check_stall();
        }
    }

    /// Discard `sample_count` interleaved samples while surrendered: count them
    /// into the clock accounting and sleep out their realtime duration (in
    /// abort/teardown-responsive slices), mimicking a consuming device.
    fn discard_paced(&self, sample_count: usize, abort: &AtomicBool) {
        let frames = (sample_count as u64) / 2;
        self.written_frames.fetch_add(frames, Ordering::AcqRel);
        self.dropped_frames.fetch_add(frames, Ordering::AcqRel);
        let mut left_ms = frames * 1000 / self.sample_rate.max(1) as u64;
        while left_ms > 0 {
            if self.stopped.load(Ordering::Acquire) || abort.load(Ordering::Relaxed) {
                return;
            }
            let slice = left_ms.min(15);
            std::thread::sleep(std::time::Duration::from_millis(slice));
            left_ms -= slice;
        }
    }

    /// Playback-head rate watchdog, run on every writer iteration. Once primed
    /// and unpaused, the head must advance at roughly the sample rate. The
    /// wedge has two faces on the Streamer — head fully static, or a ~2-4 %
    /// crawl (tiny writes keep being accepted, so a consecutive-zero-writes
    /// detector never fires) — and both mean AudioFlinger effectively dropped
    /// the track from the mix while the CLIENT play state stays PLAYING (so
    /// getPlayState can't see it). Escalation, one strike per ~second the head
    /// advances <10 % of realtime: two pause/play cycles (the PAUSED→PLAYING
    /// transition re-adds the track; a bare play() is a server-side no-op),
    /// then recreate the track outright.
    fn check_stall(&self) {
        if !self.primed.load(Ordering::Acquire) || self.paused.load(Ordering::Acquire) {
            *self.stall.lock().unwrap() = StallState::default();
            return;
        }
        let now = std::time::Instant::now();
        let mut st = self.stall.lock().unwrap();
        let Some(since) = st.since else {
            *st = StallState {
                since: Some(now),
                head: self.head_frames().unwrap_or(-1),
                ..StallState::default()
            };
            return;
        };
        let elapsed = now.duration_since(since);
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let head = self.head_frames().unwrap_or(-1);
        if elapsed > std::time::Duration::from_secs(3) {
            // The writer was idle (rebuffering / host pause) — a rate judgment
            // over that gap would be a false positive; rebaseline instead.
            *st = StallState { since: Some(now), head, strikes: st.strikes };
            return;
        }
        let expected = elapsed.as_millis() as i64 * self.sample_rate as i64 / 1000;
        let stalled = head >= 0 && st.head >= 0 && (head - st.head) * 10 < expected;
        if stalled {
            st.strikes += 1;
            log::warn!(
                "[audio-pcm] head advanced {}/{expected} frames in {elapsed:?} (strike {}) ",
                head - st.head,
                st.strikes
            );
            if st.strikes <= 2 {
                self.call_void(jni::jni_str!("pause"));
                self.call_void(jni::jni_str!("play"));
            } else if self.recreate_track() {
                // Fresh track probe-verified alive — rebaseline the watchdog.
                *st = StallState::default();
                return;
            } else {
                // Even a fresh post-standby track is dead: the device's audio
                // output is wedged beyond anything the app can do right now
                // (episodes observed box-wide, Dolby MS12 pipeline stuck). Go
                // silent but KEEP PLAYING — played_ms turns None so video runs
                // realtime on the wall clock — and retry with a fresh track in
                // 30 s.
                log::warn!(
                    "[audio-pcm] audio output unrecoverable — surrendering for 30 s (video continues, silent)"
                );
                self.primed.store(false, Ordering::Release);
                *self.surrendered_until.lock().unwrap() =
                    Some(now + std::time::Duration::from_secs(30));
                *st = StallState::default();
                return;
            }
        } else {
            st.strikes = 0;
        }
        st.since = Some(now);
        st.head = head;
    }

    /// Frames that must be written before `play()`. The deep-buffer/MS12 mixer
    /// on the Streamer pulls in ~256 ms chunks (`notificationFrames 12312`,
    /// frameCount 24624): a track started with less than one chunk never gets
    /// pulled and AudioFlinger removes it with `BUFFER TIMEOUT` — the whole
    /// "wedged track" family. ~340 ms (16384 frames @48k) is the amount the
    /// probe empirically delivered; prime to that.
    fn prime_frames(&self) -> u64 {
        self.sample_rate as u64 * 340 / 1000
    }

    /// Flip `primed` once the current track holds ≥ `prime_frames`, or the
    /// first write is ≥ 1.5 s old (slow decode — start with what we have
    /// rather than never). Returns the primed state.
    fn try_prime(&self) -> bool {
        if self.primed.load(Ordering::Acquire) {
            return true;
        }
        let buffered = self.track_written.load(Ordering::Acquire);
        let timed_out = self
            .track_first_write
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() >= std::time::Duration::from_millis(1500));
        if buffered >= self.prime_frames() || timed_out {
            self.primed.store(true, Ordering::Release);
            log::info!(
                "[audio-pcm] primed ({buffered} frames buffered{})",
                if timed_out { ", timeout" } else { "" }
            );
            true
        } else {
            false
        }
    }

    /// Raw cumulative `getPlaybackHeadPosition` of the CURRENT track (frames),
    /// `None` on JNI failure.
    fn head_frames(&self) -> Option<i64> {
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        let vm = android_vm();
        let track = self.track.lock().unwrap();
        vm.attach_current_thread(|env| -> Result<i64, jni::errors::Error> {
            let head = env
                .call_method(
                    track.as_obj(),
                    jni::jni_str!("getPlaybackHeadPosition"),
                    jni::jni_sig!("()I"),
                    &[],
                )?
                .i()? as u32 as i64;
            Ok(head)
        })
        .ok()
    }

    /// PRESENTED-frame position of the CURRENT track when available, else the
    /// consumed head position corrected by the learned output latency.
    ///
    /// `getPlaybackHeadPosition` counts frames CONSUMED by the mixer; on
    /// TV-class outputs (HDMI → TV processing → speakers, BT even more) the
    /// audible sound lags that by the output-path latency, so a video clock
    /// paced on the consumed position runs visibly AHEAD of the audio (the
    /// "picture slightly leads on PCM" report). This mirrors the passthrough
    /// sink's clock: `AudioTrack.getTimestamp()` gives the frame most recently
    /// PRESENTED at the output (interpolated to now); while it's not valid yet
    /// (first ~second of playback, or right after a heal flush) we fall back
    /// to head − cached latency, with the consumed→presented gap learned
    /// process-wide so later pipelines are corrected from their first frame
    /// (same handoff-continuity trick as audio_passthrough.rs).
    fn presented_frames(&self) -> Option<i64> {
        use std::sync::atomic::AtomicI64;
        // Learned CONSUMED − PRESENTED gap (output latency) in frames.
        // Process-wide: seeks/heals rebuild the sink, the device latency
        // doesn't change. 0 = not learned yet (raw head, today's behavior).
        static PCM_LATENCY_FRAMES: AtomicI64 = AtomicI64::new(0);
        // Rate limiter for the diagnostic clock log below (ns of last line).
        static LAST_CLOCK_LOG_NS: AtomicI64 = AtomicI64::new(0);
        let clock_log = |src: &str, head: i64, presented: i64, lat: i64| {
            let now = clock_monotonic_ns();
            let last = LAST_CLOCK_LOG_NS.load(Ordering::Relaxed);
            if now - last > 5_000_000_000 {
                LAST_CLOCK_LOG_NS.store(now, Ordering::Relaxed);
                let rate = self.sample_rate.max(1) as i64;
                log::info!(
                    "[audio-pcm] clock src={} head={} presented={} lat={}f/{}ms",
                    src, head, presented, lat, lat * 1000 / rate
                );
            }
        };

        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        let sample_rate = self.sample_rate as i64;
        let vm = android_vm();
        let track = self.track.lock().unwrap();
        let res = vm.attach_current_thread(
            |env| -> Result<(bool, i64, i64), jni::errors::Error> {
                let head = env
                    .call_method(
                        track.as_obj(),
                        jni::jni_str!("getPlaybackHeadPosition"),
                        jni::jni_sig!("()I"),
                        &[],
                    )?
                    .i()? as u32 as i64;
                let ts = env.new_object(
                    jni::jni_str!("android/media/AudioTimestamp"),
                    jni::jni_sig!("()V"),
                    &[],
                )?;
                let have_ts = env
                    .call_method(
                        track.as_obj(),
                        jni::jni_str!("getTimestamp"),
                        jni::jni_sig!("(Landroid/media/AudioTimestamp;)Z"),
                        &[(&ts).into()],
                    )?
                    .z()?;
                if have_ts {
                    let frame_pos = env
                        .get_field(&ts, jni::jni_str!("framePosition"), jni::jni_sig!("J"))?
                        .j()?;
                    let nano_time = env
                        .get_field(&ts, jni::jni_str!("nanoTime"), jni::jni_sig!("J"))?
                        .j()?;
                    let dt_ns = clock_monotonic_ns() - nano_time;
                    // A stale timestamp (old nanoTime — e.g. pre-flush) would
                    // extrapolate garbage; treat >1 s staleness as not valid.
                    if dt_ns >= 0 && dt_ns < 1_000_000_000 {
                        let presented = frame_pos + dt_ns * sample_rate / 1_000_000_000;
                        return Ok((true, presented, head));
                    }
                }
                Ok((false, head, head))
            },
        );
        match res {
            Ok((true, presented, head)) => {
                // Sanity: presented can never exceed consumed. A post-flush
                // transient where the timestamp still reflects the old track
                // shows up as presented > head — ignore it and use the
                // corrected head instead.
                if presented <= head {
                    let lat = head - presented;
                    if lat < sample_rate * 2 {
                        PCM_LATENCY_FRAMES.store(lat, Ordering::Relaxed);
                    }
                    clock_log("ts", head, presented, lat);
                    Some(presented.max(0))
                } else {
                    let lat = PCM_LATENCY_FRAMES.load(Ordering::Relaxed);
                    clock_log("ts-transient", head, head - lat, lat);
                    Some((head - lat).max(0))
                }
            }
            Ok((false, head, _)) => {
                let mut lat = PCM_LATENCY_FRAMES.load(Ordering::Relaxed);
                if lat == 0 {
                    // No valid AudioTimestamp yet (some TV outputs never
                    // deliver one for PCM) and nothing learned — seed the
                    // latency from the hidden AudioTrack.getLatency(), the
                    // same fallback ExoPlayer's position tracker uses. That
                    // value includes the track's own buffer, which the head
                    // has already consumed through, so subtract it.
                    if let Some(l) = self.reflected_latency_frames() {
                        lat = l;
                        PCM_LATENCY_FRAMES.store(l, Ordering::Relaxed);
                    }
                }
                clock_log("no-ts", head, head - lat, lat);
                Some((head - lat).max(0))
            }
            Err(_) => None,
        }
    }

    /// One-shot `AudioTrack.getLatency()` (hidden API, tolerated the same way
    /// ExoPlayer uses it) minus the track buffer's own duration, in frames.
    /// `None` when the method is unavailable or returns nonsense. Only ever
    /// consulted while no `AudioTimestamp` has validated — a learned/measured
    /// latency always wins.
    fn reflected_latency_frames(&self) -> Option<i64> {
        use std::sync::atomic::{AtomicBool, AtomicI64};
        // The JNI lookup either works forever or never on a given device —
        // don't re-attempt a failing hidden-API resolution on every clock read.
        static ATTEMPTED: AtomicBool = AtomicBool::new(false);
        static CACHED_FRAMES: AtomicI64 = AtomicI64::new(-1);
        if ATTEMPTED.swap(true, Ordering::Relaxed) {
            let f = CACHED_FRAMES.load(Ordering::Relaxed);
            return (f >= 0).then_some(f);
        }
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        let sample_rate = self.sample_rate as i64;
        let vm = android_vm();
        let track = self.track.lock().unwrap();
        let res = vm.attach_current_thread(|env| -> Result<i64, jni::errors::Error> {
            // Hidden API — may throw NoSuchMethodError under hidden-api
            // enforcement. Clear the pending exception so it can't poison
            // the next JNI call on this thread.
            let latency_ms = match env
                .call_method(track.as_obj(), jni::jni_str!("getLatency"), jni::jni_sig!("()I"), &[])
                .and_then(|v| v.i())
            {
                Ok(v) => v as i64,
                Err(e) => {
                    let _ = env.exception_clear();
                    return Err(e);
                }
            };
            let buffer_frames = env
                .call_method(
                    track.as_obj(),
                    jni::jni_str!("getBufferSizeInFrames"),
                    jni::jni_sig!("()I"),
                    &[],
                )?
                .i()? as i64;
            Ok(latency_ms * sample_rate / 1000 - buffer_frames)
        });
        match res {
            Ok(frames) if frames > 0 && frames < sample_rate * 2 => {
                log::info!(
                    "[audio-pcm] getLatency() fallback: {}f ({}ms) after buffer subtraction",
                    frames,
                    frames * 1000 / sample_rate.max(1)
                );
                CACHED_FRAMES.store(frames, Ordering::Relaxed);
                Some(frames)
            }
            _ => {
                log::info!("[audio-pcm] getLatency() fallback unavailable");
                None
            }
        }
    }

    /// Cumulative presented-frame position as ms — the PCM clock source. `None`
    /// until the track is actually playing, so `MediaClock` uses the wall clock
    /// during startup (and if audio never starts, video still plays).
    pub fn played_ms(&self) -> Option<u64> {
        if self.sample_rate == 0 || !self.primed.load(Ordering::Acquire) {
            return None;
        }
        let dropped = self.dropped_frames.load(Ordering::Acquire);
        let base = self.head_base.load(Ordering::Acquire);
        self.presented_frames()
            .map(|frames| (base + frames.max(0) as u64 + dropped) * 1000 / self.sample_rate as u64)
    }

    fn call_void(&self, method: &'static jni::strings::JNIStr) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        let vm = android_vm();
        let track = self.track.lock().unwrap();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            env.call_method(track.as_obj(), method, jni::jni_sig!("()V"), &[])?;
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
        let track = self.track.lock().unwrap();
        vm.attach_current_thread(|env| -> Result<bool, jni::errors::Error> {
            let state = env
                .call_method(
                    track.as_obj(),
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
        let primed = self.primed.load(Ordering::Acquire);
        if prev != paused {
            log::info!("[audio-pcm] set_paused({paused}) primed={primed}");
        }
        // Until the track is primed don't touch it — the lazy start in
        // write_floats() applies the remembered state once enough PCM is
        // buffered. (Unparking here used to start the track with a near-empty
        // buffer: it underran instantly and AudioFlinger dropped it from the
        // mix for good.)
        if !primed {
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
        let track = self.track.lock().unwrap();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            env.call_method(
                track.as_obj(),
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
        let track = self.track.lock().unwrap();
        let _ = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            env.call_method(track.as_obj(), jni::jni_str!("stop"), jni::jni_sig!("()V"), &[])?;
            env.call_method(track.as_obj(), jni::jni_str!("release"), jni::jni_sig!("()V"), &[])?;
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
                    // Odd sample held over so every batch is WHOLE FRAMES. The
                    // channel delivers individual samples, so a batch can end
                    // mid-frame; `AudioTrack.write` only accepts whole frames,
                    // and retrying a half-frame chunk returns 0 forever — the
                    // writer then spins on one batch for good (this WAS the
                    // probabilistic "audio never starts / wedges" bug). The
                    // leftover must carry into the next batch, not be dropped:
                    // dropping one sample swaps L/R for everything after it.
                    let mut carry: Option<f32> = None;
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
                            // The producer restarts at a frame boundary.
                            carry = None;
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
                                carry = None;
                                continue;
                            }
                        }
                        batch.clear();
                        if let Some(c) = carry.take() {
                            batch.push(c);
                        }
                        batch.push(first);
                        while batch.len() < 8192 {
                            match sample_receiver.try_recv() {
                                Ok(s) => batch.push(s),
                                Err(_) => break,
                            }
                        }
                        if batch.len() % 2 == 1 {
                            carry = batch.pop();
                        }
                        if batch.is_empty() {
                            continue;
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
