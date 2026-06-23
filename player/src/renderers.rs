pub mod audio;
#[cfg(target_os = "android")]
pub mod audio_passthrough;
pub mod subtitle;
pub mod video;

use std::future::Future;

use crate::decoders::DecodedVideoFrame;
use crate::parsers::vtt::VttCue;
use crate::PhysicalSize;

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

    /// Apply subtitle styling (text/outline colour, size multiplier).
    /// Default no-op so sinks that don't render subtitles keep compiling.
    /// Sinks that own the overlay store it and invalidate any cached
    /// rasterization. Mirrors `set_hdr_tonemap_params`.
    fn set_subtitle_style(&self, _style: crate::SubtitleStyle) {}

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

    /// Apply runtime-tunable HDR→SDR tonemap parameters. Default no-op so
    /// alternate sinks (mocks, Apple's pre-tonemapped VideoToolbox path,
    /// and Android's not-yet-wired P010 path) keep compiling.
    /// Implementations that own the HDR shader (wgpu on Win/Linux) write
    /// the values into their per-frame uniform buffer.
    fn set_hdr_tonemap_params(&self, _params: crate::HdrTonemapParams) {}

    /// Bitmask of HDR formats the active display can present natively
    /// (bit 0 = Dolby Vision, 1 = HDR10, 2 = HLG, 3 = HDR10+ — the
    /// Android `Display.HdrCapabilities` order). Sinks that can hand the
    /// signal through to such a display (Android GLES: BT2020_PQ surface
    /// dataspace) use it to prefer passthrough over in-shader
    /// tonemapping. Default: ignored (tonemap-only sinks).
    fn set_display_hdr_types(&self, _mask: u32) {}

    /// Bottom safe-area inset (device px of the render surface) that
    /// subtitles must stay above — sourced from the host's `WindowInsets`
    /// (see `Player::set_subtitle_safe_insets`). The subtitle quad anchors
    /// its bottom edge here so cues clear TV overscan / system bars. Default:
    /// ignored (0 → renderer falls back to a 10% TV title-safe margin).
    fn set_subtitle_safe_bottom_px(&self, _px: u32) {}
}

/// A compressed-bitstream audio output (audio passthrough): the audio pipeline
/// writes raw access units, the device decodes them (HDMI → AVR/soundbar). When
/// one is installed on an [`AudioSink`] via [`AudioSink::set_passthrough`], the
/// sink delegates its clock/lifecycle here so the rest of the player drives it
/// through the same interface without knowing it's a bitstream. Object-safe so
/// the platform impl (Android `AudioTrackSink`) reaches the generic pipeline.
pub trait AudioPassthrough: Send + Sync {
    /// Write one compressed access unit (blocking — back-pressures the feed to
    /// the receiver's consumption rate, which makes the playback head a clock).
    fn write(&self, au: &[u8]);
    /// Playback-head position in media ms (the passthrough clock source).
    fn played_ms(&self) -> Option<u64>;
    fn output_latency_ms(&self) -> u64 {
        0
    }
    fn flush(&self);
    fn set_paused(&self, paused: bool);
    /// Whether the sink is currently paused. The feed consults this so it can
    /// tell "playback head hasn't started because we're paused" (legitimate —
    /// keep the buffer, wait for resume) from "head never started on a dead /
    /// stale sink" (abandon). Defaults to never-paused for trivial impls.
    fn is_paused(&self) -> bool {
        false
    }
}

/// Receives decoded PCM audio and feeds it to the output device.
/// Implementations swap per platform (cpal on desktop/Android).
pub trait AudioSink: Send + Sync + 'static {
    fn put_samples<'a>(&'a self, samples: &'a [f32]) -> impl Future<Output = ()> + Send + 'a;
    fn sample_rate(&self) -> u32;
    /// Media milliseconds the output device has actually PLAYED (samples
    /// consumed by the device callback; pause/underrun silence does not
    /// count). The device crystal is the clock the listener hears, so the
    /// video sync loop measures A/V drift against this. `None` = the sink
    /// can't measure (mocks, platform limitations) — drift tracking is
    /// then disabled.
    fn played_ms(&self) -> Option<u64> {
        None
    }
    /// Output-path latency in ms (device output buffer + DAC) — how long
    /// after the sink consumes a sample it becomes audible. The video
    /// sync loop subtracts this from its wall clock so a frame reaches the
    /// screen at the same instant its audio reaches the speaker. 0 = the
    /// sink can't report it (then video is paced to the bare wall clock,
    /// the previous behaviour).
    fn output_latency_ms(&self) -> u64 {
        0
    }
    fn flush(&self);
    fn stop(&self) -> impl Future<Output = ()> + Send + '_;
    /// Absolute volume in 0.0..=1.0. Implementations must clamp.
    fn set_volume(&self, volume: f32);
    /// Last value passed to `set_volume`, or the implementation's
    /// chosen default before the first set.
    fn get_volume(&self) -> f32;
    /// Relative adjustment. Default impl is `set_volume(get_volume() + diff)`
    /// clamped to 0.0..=1.0; suitable for hotkey-driven nudges.
    fn volume(&self, diff: f32) {
        let new = (self.get_volume() + diff).clamp(0.0, 1.0);
        self.set_volume(new);
    }
    /// Pause/resume the underlying audio device. When `true`, the
    /// implementation must stop pulling samples; when `false`, resume.
    /// `Player::pause()` calls this so the cpal stream halts and we don't
    /// burn through whatever was already queued.
    fn set_paused(&self, paused: bool);

    /// Install (or clear) a passthrough output. While set, the sink's PCM path
    /// is dormant and `played_ms`/`output_latency_ms`/`flush`/`set_paused`
    /// delegate to the passthrough, so `MediaClock` paces to the AVR's real
    /// output. Default: ignored (sinks without passthrough support).
    fn set_passthrough(&self, _pt: Option<std::sync::Arc<dyn AudioPassthrough>>) {}

    /// True while a passthrough output is engaged. The clock rebase differs:
    /// a passthrough sink's `played_ms` is already 0-based from THIS pipeline's
    /// start (a fresh AudioTrack per play), so its base is 0 — unlike the cpal
    /// counter, which is session-cumulative and must be baselined at the anchor.
    fn is_passthrough(&self) -> bool {
        false
    }

    /// Latest L/R peak in dB (range roughly -120..=0). `None` before the
    /// first audio frame has been pushed. Surfaced via `PlayerEvent::Stats`
    /// so the TUI can draw a tiny VU meter.
    fn last_peak_db(&self) -> Option<[f32; 2]> {
        None
    }
}
