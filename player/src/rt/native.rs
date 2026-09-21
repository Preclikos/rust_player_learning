//! Tokio-backed runtime facade (every non-wasm target).

pub use tokio::runtime::Handle;
pub use tokio::spawn;
pub use tokio::task::{block_in_place, spawn_blocking, JoinError, JoinHandle};
pub use tokio::time::error::Elapsed;
pub use tokio::time::{sleep, timeout, Instant, Sleep};

/// Yield to the host event loop so callback-driven decoders can deliver
/// their output. Native decoders are pulled synchronously (`try_recv` on
/// the codec itself), so there is nothing to yield to here; a no-op.
#[inline]
pub async fn cooperative_yield() {}
