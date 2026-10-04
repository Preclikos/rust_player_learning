//! Platform-agnostic bridge between the player engine and a host app: the
//! unified control surface ([`bridge`]) plus the smoke-test [`fixture`] the
//! example apps share.

// The bridge core. Re-exported at the crate root so consumers write
// `bridge::BridgeHost` / `bridge::start` (not `bridge::bridge::…`).
pub mod bridge;
pub use self::bridge::*;

pub mod fixture;
pub use self::fixture::*;
