//! Test doubles shared by unit tests in more than one module.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::renderers::AudioSink;

/// Audio sink whose reported position is whatever the test puts in it.
///
/// The clock and the audio-output watchdog both hinge on what this position
/// does — advancing, standing still, or absent — so both drive it from here.
pub(crate) struct TestSink {
    pub(crate) played_ms: AtomicU64,
    /// False makes `played_since_flush_ms` report "no clock at all".
    pub(crate) has_clock: AtomicBool,
}

impl TestSink {
    pub(crate) fn new(played_ms: u64) -> Self {
        Self {
            played_ms: AtomicU64::new(played_ms),
            has_clock: AtomicBool::new(true),
        }
    }
}

impl AudioSink for TestSink {
    fn put_samples<'a>(
        &'a self,
        _samples: &'a [f32],
    ) -> impl std::future::Future<Output = ()> + Send + 'a {
        async {}
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn played_since_flush_ms(&self) -> Option<u64> {
        self.has_clock
            .load(Ordering::Relaxed)
            .then(|| self.played_ms.load(Ordering::Relaxed))
    }
    fn flush(&self) {}
    fn stop(&self) -> impl std::future::Future<Output = ()> + Send + '_ {
        async {}
    }
    fn set_volume(&self, _volume: f32) {}
    fn get_volume(&self) -> f32 {
        1.0
    }
    fn set_paused(&self, _paused: bool) {}
}
