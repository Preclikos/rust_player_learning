//! Rust media player — DASH playback with hardware decode.
//!
//! The crate root is declarations and the public surface only; the player
//! itself and its pipeline live in [`player`].

mod abr;
mod player;
#[cfg(test)]
mod test_support;
mod av_sync;
mod capabilities;
mod crypto;
#[cfg(target_arch = "wasm32")]
mod crypto_web;
mod decoders;
mod events;
mod ffmpeg_log;
mod hdr_tonemap;
mod manifest;
mod net;
mod parsers;
pub mod prof;
mod renderers;
pub mod rt;
#[doc(hidden)]
pub mod shader_src;
mod subtitle_style;
mod tracks;
mod utils;

// Public re-exports so downstream consumers (BlackZone Console etc.) can
// implement RequestInterceptor / LicenseResolver against the player's
// canonical types — see PLAYER_INTEGRATION.md.
pub use abr::{AbrStrategy, AbrVideoProfile};
pub use capabilities::{capabilities, probe_capabilities, PlayerCapabilities};
/// The track tree returned by [`Player::get_tracks`]. Adaptation/representation
/// types stay reachable through its public `video`/`audio`/`text` fields — a
/// consumer reads them via inference (no need to name the inner types).
pub use tracks::Tracks;
pub use events::{
    BufferingReason, Fps, PlayerErrorKind, PlayerEvent, TrackInfo, TrackKind,
};
pub use ffmpeg_log::{set_log_level, LogLevel};
pub use hdr_tonemap::HdrTonemapParams;
pub use subtitle_style::SubtitleStyle;
/// Host-supplied sidecar subtitles — see
/// [`Player::add_external_subtitle_track`].
pub use parsers::sidecar::{SidecarError, SubtitleFormat};
pub use net::{
    tls_client, BoxError, HttpClient, LicenseResolver, NoopInterceptor, PreparedRequest,
    RequestInterceptor, RequestKind, RetryPolicy,
};
/// Physical (device-pixel) size of the render target. A tiny owned type so the

// Re-exported so hosts can name the handle types they pass to
// `Player::new_from_raw_handle` without pinning their own raw-window-handle.
pub use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
// Additive: re-export the offscreen ring handle + a convenience alias. Offscreen
// (in-app) video reuses `VideoRenderer` with an offscreen target, so the in-app
// player is the same concrete type as the windowed desktop player.
pub use renderers::video_offscreen::OffscreenTarget;
/// The stock sinks + the sink traits, so a host can wrap them (see
/// [`Player::with_sinks`]): a decorator that forwards to the real renderer
/// while observing every frame / sample is how the conformance harness
/// measures lip-sync independently of the engine clock.
pub use renderers::{audio::AudioRenderer, video::VideoRenderer, AudioPassthrough, AudioSink, VideoSink};
pub use decoders::DecodedVideoFrame;
pub use parsers::vtt::VttCue;

/// Types defined by the player module itself, re-exported so the public
/// surface is unchanged by where they live.
pub use player::{
    ConformanceSummary, ExternalSubtitleOptions, OffscreenPlayer, PhysicalSize, Player,
};
