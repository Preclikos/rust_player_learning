//! Apple direct mode: compressed samples go straight into a host-provided
//! `AVSampleBufferDisplayLayer`, the OS video pipeline decodes and shows
//! them — HDR10, HLG and Dolby Vision (RPU dynamic metadata, profile 5
//! included) reach the display exactly as AVFoundation delivers them. The
//! Apple analog of Android's direct mode (`set_video_output_window`).
//!
//! Shape:
//!   - [`AppleDirectOutput`] (one per `Player`, `output.rs`) owns the layer
//!     and its `controlTimebase`. The A/V sync loop keeps running
//!     unchanged: for every frame it would have shown it "renders" a stamp
//!     ([`AppleDirectFrame`]) whose only effect is re-anchoring the
//!     timebase so that frame's PTS lands on the sync loop's present time.
//!     The layer then shows each enqueued sample when the timebase reaches
//!     its PTS — the audio clock stays the master, no second clock exists.
//!     When stamps stop (pause, buffering, seek) a guard thread stops the
//!     timebase within two frame intervals.
//!   - [`AppleVideoDecoder`] (`decoder.rs`) is what the player's decoder
//!     factory hands out on macOS / iOS. At `configure` it picks direct
//!     mode when the host installed a layer, the display reported HDR (or
//!     the `RUST_PLAYER_DIRECT` override forces it) and direct mode has not
//!     failed; otherwise it is the plain VideoToolbox decoder feeding the
//!     player's own renderer (EDR output or tonemap).
//!   - Failover: a layer that reports `status == failed` marks direct mode
//!     dead for this player and fails the pipeline; the supervisor's
//!     rebuild then gets a VideoToolbox decoder — the tonemap path. An
//!     interruption the OS asks us to flush away (app backgrounded) only
//!     flushes and rebuilds, it doesn't disable direct mode.
//!   - `ffi.rs` holds the CoreMedia / CoreFoundation bindings behind small
//!     RAII wrappers so the two files above contain no raw `CFRelease`.

#![cfg(any(target_os = "macos", target_os = "ios"))]

mod decoder;
mod ffi;
mod output;

pub use decoder::AppleVideoDecoder;
pub use output::{AppleDirectFrame, AppleDirectOutput};
