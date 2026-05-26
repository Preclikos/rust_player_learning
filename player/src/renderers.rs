pub mod audio;
pub mod subtitle;
pub mod video;

use std::future::Future;

use crate::decoders::DecodedVideoFrame;
use crate::parsers::vtt::VttCue;
use winit::dpi::PhysicalSize;

/// Receives decoded video frames and presents them to the display.
/// Implementations swap per platform (wgpu NV12 on desktop, GLES OES on Android).
pub trait VideoSink: Send + Sync + 'static {
    fn render_frame(&self, frame: DecodedVideoFrame) -> impl Future<Output = ()> + Send + '_;
    fn resize(&self, size: PhysicalSize<u32>) -> impl Future<Output = ()> + Send + '_;
    fn change_frame_size(&self, size: PhysicalSize<u32>) -> impl Future<Output = ()> + Send + '_;

    /// Install a font for subtitle rendering. No-op on sinks that don't
    /// render subtitles themselves. Returns Err on invalid font bytes.
    fn set_subtitle_font(&self, _bytes: Vec<u8>) -> Result<(), String> {
        Ok(())
    }

    /// Queue parsed cues for rendering. The sink keeps an internal list
    /// and picks the one active at the current playback PTS. Called by
    /// the text_play pipeline as cues arrive.
    fn queue_subtitle_cues(&self, _cues: Vec<VttCue>) {}

    /// Wipe any queued cues + cached rasterizations. Called on subtitle
    /// track switch or when the consumer disables subtitles.
    fn clear_subtitles(&self) {}

    /// Feed the current media-timeline PTS (ms, 0-based since start of
    /// content) into the subtitle overlay so its active-cue picker
    /// matches the cues' own timestamps. Called by the video sync loop
    /// each frame. MUST be the BMDT-adjusted PTS, not the raw DASH
    /// timestamp — otherwise cues fire at the wrong moment by however
    /// much the segment timeline is shifted (commonly 7-60 s in real
    /// streams).
    fn set_subtitle_pts(&self, _pts_ms: i64) {}
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
