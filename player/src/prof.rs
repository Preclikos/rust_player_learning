//! Where the time goes, per subsystem.
//!
//! The web build is single-threaded (`wasm32-unknown-unknown` has no
//! `atomics`), so demux, decrypt, audio conversion, pacing and GPU submission
//! all share one thread with the page. Measured from outside, that thread sits
//! near 100% of a core during playback at every resolution — 1.03 cores at
//! 720p and 0.97 at 2160p — while the GPU process idles at 0.03. Resolution
//! plainly is not what drives it, and no amount of reading the code settles
//! which of the remaining candidates does.
//!
//! So: cumulative microseconds per subsystem, read out of the page. Counters
//! only, no sampling, no allocation — the measurement has to be cheap enough
//! not to become the thing it measures.

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counters {
    ($($name:ident => $key:literal),* $(,)?) => {
        $(pub static $name: Counter = Counter::new($key);)*

        /// Every counter, for the JSON dump.
        pub const ALL: &[&Counter] = &[$(&$name),*];
    };
}

pub struct Counter {
    key: &'static str,
    micros: AtomicU64,
    calls: AtomicU64,
}

impl Counter {
    const fn new(key: &'static str) -> Self {
        Self { key, micros: AtomicU64::new(0), calls: AtomicU64::new(0) }
    }

    pub fn add(&self, micros: u64) {
        self.micros.fetch_add(micros, Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
    }

    pub fn key(&self) -> &'static str {
        self.key
    }

    pub fn micros(&self) -> u64 {
        self.micros.load(Ordering::Relaxed)
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

counters! {
    AUDIO_OUTPUT   => "audio_output",
    AUDIO_RESAMPLE => "audio_resample",
    VIDEO_RENDER   => "video_render",
    VIDEO_SUBMIT   => "video_submit",
    AUDIO_SUBMIT   => "audio_submit",
    VIDEO_DRAIN    => "video_drain",
    SEGMENT_PREP   => "segment_prep",
    DECRYPT        => "decrypt",
}

/// Times from construction to drop. For `async fn`s, where wrapping the body
/// in a closure would mean restructuring the function.
pub struct Timer(&'static Counter, crate::rt::Instant);

impl Timer {
    pub fn new(counter: &'static Counter) -> Self {
        Self(counter, crate::rt::Instant::now())
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.0.add(self.1.elapsed().as_micros() as u64);
    }
}

/// `{"audio_output":{"ms":123,"calls":456}, …}` — cumulative since load.
pub fn json() -> String {
    let body: Vec<String> = ALL
        .iter()
        .map(|c| {
            format!(
                r#""{}":{{"ms":{},"calls":{}}}"#,
                c.key(),
                c.micros() / 1000,
                c.calls()
            )
        })
        .collect();
    format!("{{{}}}", body.join(","))
}
