//! Async runtime facade — the one place the engine names an executor.
//!
//! Native targets run on a multi-threaded Tokio runtime and this module is a
//! set of re-exports: `spawn`, `sleep`, `Instant`, `JoinHandle` ARE Tokio's.
//! The browser has no Tokio runtime to drive (its timers need
//! `std::time::Instant`, which wasm32-unknown-unknown lacks, and a
//! `block_on` would starve the very event loop that delivers fetch bodies,
//! WebCodecs output and audio callbacks), so there the same names are backed
//! by `wasm-bindgen-futures` (the JS microtask queue), `setTimeout` and
//! `performance.now()`.
//!
//! Everything else the engine uses from Tokio — `sync::{mpsc, broadcast,
//! watch, oneshot, Notify, RwLock, Mutex}`, `select!`, `join!` — is
//! runtime-agnostic and keeps being used directly.
//!
//! Rule for engine code: never call `tokio::spawn`, `tokio::time::*`,
//! `tokio::task::*` or `std::time::Instant` directly; go through here.

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::*;
