//! Debug lip-sync probe for the beep/flash test asset, inside the engine.
//!
//! The conformance harness measures lip-sync by wrapping the sinks, which
//! only works where the harness itself runs (desktop CI). This is the same
//! measurement built into the player so it also runs in the real apps —
//! Android first, where output route changes (Bluetooth headphones on/off)
//! move the audio clock and the HEALTH drift gauge (audio clock vs wall)
//! cannot tell a correct clock step from a broken lip-sync.
//!
//! The asset (`scripts/conformance/make-asset.ps1`) flashes the picture white
//! and starts a 1 kHz beep at every even second. Audio side: beep onsets are
//! found in the PCM the engine queues to the sink (content, not timestamps)
//! and dated by the sink's presented position — the same position the master
//! clock uses, so an engine that mis-accounts it (a recreated track, a flush
//! boundary, a lost buffer) shows up here. Video side: each flash frame is
//! dated at the instant it is meant to reach the display. `flash − beep` per
//! mark is the offset; positive = picture late (audio leads).
//!
//! Off unless enabled: `adb shell setprop debug.rustplayer.lipsync 1` on
//! Android, `RUST_PLAYER_LIPSYNC=1` elsewhere (read once, at first use).
//! Results go to the log (`[lipsync] …`) and the audio line of the debug HUD.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// Marks repeat every 2 s on the test asset.
const MARK_PERIOD_MS: u64 = 2_000;
/// A frame whose pts is within this of a mark is the flash (one 24 fps frame).
const FLASH_WINDOW_MS: u64 = 42;
/// Beep detector hysteresis (front-pair RMS per 1 ms block).
const BEEP_ON_RMS: f32 = 0.02;
const BEEP_OFF_RMS: f32 = 0.005;
/// A flash is paired once its beep can no longer still be on the way.
const PAIR_AFTER_MS: i64 = 1_000;

pub(crate) struct Probe {
    st: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// PCM queued to the sink since the last flush, ms (the axis the sink's
    /// `played_since_flush_ms` counts on).
    queued_ms: f64,
    loud: bool,
    /// Beep onsets on the queued axis, not yet presented.
    pending: VecDeque<f64>,
    /// Instants (probe ns) at which beeps became audible.
    audible: VecDeque<i64>,
    /// (pts ms, instant) of flash frames not yet paired.
    flashes: VecDeque<(u64, i64)>,
    last_mark: Option<u64>,
    /// Offsets measured so far (ms) and the latest (mark s, ms).
    offsets: Vec<i64>,
    last: Option<(u64, i64)>,
}

/// The probe, when enabled for this process.
pub(crate) fn probe() -> Option<&'static Probe> {
    static PROBE: OnceLock<Option<Probe>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            enabled().then(|| {
                log::info!("[lipsync] probe ON (beep/flash test asset expected)");
                Probe { st: Mutex::new(State::default()) }
            })
        })
        .as_ref()
}

#[cfg(target_os = "android")]
fn enabled() -> bool {
    let name = c"debug.rustplayer.lipsync";
    let mut buf = [0u8; 92]; // PROP_VALUE_MAX
    let n = unsafe { libc::__system_property_get(name.as_ptr(), buf.as_mut_ptr().cast()) };
    n > 0 && &buf[..n as usize] == b"1"
}

#[cfg(not(target_os = "android"))]
fn enabled() -> bool {
    std::env::var("RUST_PLAYER_LIPSYNC").is_ok_and(|v| v == "1")
}

impl Probe {
    /// A flush: the queued axis restarts with the next pipeline's audio.
    pub(crate) fn on_flush(&self) {
        let mut st = self.st.lock().unwrap();
        st.queued_ms = 0.0;
        st.pending.clear();
        st.loud = false;
    }

    /// PCM about to be queued to the sink (interleaved, `channels` wide).
    pub(crate) fn on_pcm(&self, samples: &[f32], channels: u16, rate: u32) {
        let ch = channels.max(1) as usize;
        let front = ch.min(2);
        let rate = rate.max(1) as f64;
        let block = ((rate / 1000.0) as usize).max(1) * ch;
        let mut st = self.st.lock().unwrap();
        let base = st.queued_ms;
        for (i, chunk) in samples.chunks(block).enumerate() {
            let (sum, n) = chunk
                .chunks(ch)
                .flat_map(|f| f.iter().take(front))
                .fold((0.0f32, 0usize), |(sum, n), s| (sum + s * s, n + 1));
            let rms = (sum / n.max(1) as f32).sqrt();
            if !st.loud && rms > BEEP_ON_RMS {
                st.loud = true;
                st.pending.push_back(base + i as f64);
            } else if st.loud && rms < BEEP_OFF_RMS {
                st.loud = false;
            }
        }
        st.queued_ms += (samples.len() / ch) as f64 * 1000.0 / rate;
    }

    /// Called per presented frame from the sync loop: dates beeps the sink has
    /// presented (`played` = its `played_since_flush_ms`, minus the output
    /// latency the clock subtracts too), records a flash frame, pairs.
    /// `shown_ns`: when the frame reaches the display, on the probe clock;
    /// `None` = now (renderers that present at once).
    pub(crate) fn on_frame(&self, pts_ms: u64, shown_ns: Option<i64>, played: Option<u64>, latency_ms: u64) {
        // CLOCK_MONOTONIC on Android = the timebase of the present stamps handed in.
        let now = crate::player::clock_monotonic_ns();
        let mut st = self.st.lock().unwrap();
        if let Some(played) = played {
            let played = played as f64;
            while let Some(&onset) = st.pending.front() {
                if played < onset {
                    break;
                }
                st.pending.pop_front();
                // Overshoot instead of "now": removes the position's update
                // granularity from the measurement.
                let ago_ms = (played - onset).min(500.0);
                let t = now - (ago_ms * 1e6) as i64 + latency_ms as i64 * 1_000_000;
                st.audible.push_back(t);
            }
        }
        // The pts-0 frame is painted at once as the start preview: skip it.
        if pts_ms % MARK_PERIOD_MS < FLASH_WINDOW_MS && pts_ms >= FLASH_WINDOW_MS {
            let mark = pts_ms / MARK_PERIOD_MS;
            if st.last_mark != Some(mark) {
                st.last_mark = Some(mark);
                st.flashes.push_back((pts_ms, shown_ns.unwrap_or(now)));
            }
        }
        // Pair flashes old enough that their beep has had time to be dated.
        while let Some(&(pts, t_flash)) = st.flashes.front() {
            if now - t_flash < PAIR_AFTER_MS * 1_000_000 {
                break;
            }
            st.flashes.pop_front();
            let nearest = st
                .audible
                .iter()
                .map(|&t_beep| (t_flash - t_beep) / 1_000_000)
                .min_by_key(|d| d.abs())
                .filter(|d| d.abs() < (MARK_PERIOD_MS / 2) as i64);
            match nearest {
                Some(d) => {
                    st.offsets.push(d);
                    st.last = Some((pts / 1000, d));
                    log::info!("[lipsync] flash@{}s: {:+} ms (+ = picture late)", pts / 1000, d);
                }
                None => log::info!("[lipsync] flash@{}s: no beep within ±1 s", pts / 1000),
            }
        }
        let horizon = now - 5_000_000_000;
        while st.audible.front().is_some_and(|&t| t < horizon) {
            st.audible.pop_front();
        }
    }

    /// One line for the debug HUD.
    pub(crate) fn hud(&self) -> String {
        let st = self.st.lock().unwrap();
        let Some((_, d)) = st.last else {
            return "lipsync -".into();
        };
        let mut sorted = st.offsets.clone();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];
        let max = sorted.iter().map(|d| d.abs()).max().unwrap_or(0);
        format!("lipsync {d:+} ms (median {median:+}, max {max}, n={})", sorted.len())
    }
}
