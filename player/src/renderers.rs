pub mod audio;
pub mod video;

use std::future::Future;

use crate::decoders::DecodedVideoFrame;
use winit::dpi::PhysicalSize;

/// Receives decoded video frames and presents them to the display.
/// Implementations swap per platform (wgpu NV12 on desktop, GLES OES on Android).
pub trait VideoSink: Send + Sync + 'static {
    fn render_frame(&self, frame: DecodedVideoFrame) -> impl Future<Output = ()> + Send + '_;
    fn resize(&self, size: PhysicalSize<u32>) -> impl Future<Output = ()> + Send + '_;
    fn change_frame_size(&self, size: PhysicalSize<u32>) -> impl Future<Output = ()> + Send + '_;
}

/// Receives decoded PCM audio and feeds it to the output device.
/// Implementations swap per platform (cpal on desktop/Android).
pub trait AudioSink: Send + Sync + 'static {
    fn put_samples<'a>(&'a self, samples: &'a [f32]) -> impl Future<Output = ()> + Send + 'a;
    fn sample_rate(&self) -> u32;
    fn flush(&self);
    fn stop(&self) -> impl Future<Output = ()> + Send + '_;
    fn volume(&self, diff: f32) -> impl Future<Output = ()> + Send + '_;
    /// Pause/resume the underlying audio device. When `true`, the
    /// implementation must stop pulling samples; when `false`, resume.
    /// `Player::pause()` calls this so the cpal stream halts and we don't
    /// burn through whatever was already queued.
    fn set_paused(&self, paused: bool);

    /// Latest L/R peak in dB (range roughly -120..=0). `None` before the
    /// first audio frame has been pushed. Surfaced via `PlayerEvent::Stats`
    /// so the TUI can draw a tiny VU meter.
    fn last_peak_db(&self) -> Option<[f32; 2]> {
        None
    }
}
