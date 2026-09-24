//! Browser runtime facade: `wasm-bindgen-futures` for tasks, `setTimeout`
//! for timers, `performance.now()` for the monotonic clock.
//!
//! wasm32-unknown-unknown (no `atomics`) is single-threaded. Several types
//! here wrap JS handles that are `!Send` and are marked `Send`/`Sync`
//! anyway — the same "fragile" contract wgpu offers on this target
//! (`fragile-send-sync-non-atomic-wasm`): there is exactly one thread, so a
//! `Send` bound can never be exercised. The engine keeps its native `Send`
//! bounds unchanged and one set of code compiles for both worlds.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::future::{AbortHandle, Abortable};
use tokio::sync::oneshot;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

pub use web_time::Instant;

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// Error from awaiting a [`JoinHandle`]: the task was aborted (or its
/// output was dropped without being sent).
#[derive(Debug)]
pub struct JoinError {
    cancelled: bool,
}

impl JoinError {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
    /// Unwinding is not available on wasm32; a panic aborts the module, so
    /// no task ever fails "by panic" here.
    pub fn is_panic(&self) -> bool {
        false
    }
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.cancelled {
            write!(f, "task was cancelled")
        } else {
            write!(f, "task dropped its output")
        }
    }
}

impl std::error::Error for JoinError {}

/// Handle to a task spawned with [`spawn`]: awaitable and abortable, like
/// Tokio's.
pub struct JoinHandle<T> {
    rx: oneshot::Receiver<T>,
    abort: AbortHandle,
    finished: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.abort.abort();
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.rx).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(v)) => Poll::Ready(Ok(v)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(JoinError {
                cancelled: this.cancelled.load(Ordering::Relaxed),
            })),
        }
    }
}

// Single thread: see the module docs.
unsafe impl<T> Send for JoinHandle<T> {}
unsafe impl<T> Sync for JoinHandle<T> {}

/// Spawn a task onto the browser's microtask queue.
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (tx, rx) = oneshot::channel();
    let (abort, registration) = AbortHandle::new_pair();
    let finished = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let fin = Arc::clone(&finished);
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(v) = Abortable::new(fut, registration).await {
            let _ = tx.send(v);
        }
        fin.store(true, Ordering::Relaxed);
    });
    JoinHandle {
        rx,
        abort,
        finished,
        cancelled,
    }
}

/// There is no blocking pool in the browser: the closure runs inline on the
/// caller and the handle is already complete. Callers on this target keep
/// the work small (one segment's decrypt + parse) — anything longer would
/// show as a frame hitch rather than a stall of another thread.
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + 'static,
    R: 'static,
{
    let (tx, rx) = oneshot::channel();
    let (abort, _registration) = AbortHandle::new_pair();
    let _ = tx.send(f());
    JoinHandle {
        rx,
        abort,
        finished: Arc::new(AtomicBool::new(true)),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

/// See [`spawn_blocking`]: runs inline.
pub fn block_in_place<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

/// Stand-in for `tokio::runtime::Handle`: the engine stores one so control
/// calls can spawn from any host thread. In the browser there is only the
/// one event loop, so this is a unit handle over [`spawn`].
#[derive(Clone, Debug, Default)]
pub struct Handle;

impl Handle {
    pub fn current() -> Self {
        Handle
    }

    pub fn spawn<F>(&self, fut: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        spawn(fut)
    }
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// A future that resolves when a JS promise settles (either way). Used for
/// `setTimeout` sleeps and the event-loop trampoline below.
pub struct Sleep(JsFuture);

// Single thread: see the module docs.
unsafe impl Send for Sleep {}
unsafe impl Sync for Sleep {}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match Pin::new(&mut self.get_mut().0).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(()),
        }
    }
}

/// `setTimeout` on whichever global scope we run in (window or worker).
fn set_timeout(cb: &js_sys::Function, ms: i32) {
    let global = js_sys::global();
    if let Some(win) = global.dyn_ref::<web_sys::Window>() {
        let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(cb, ms);
    } else if let Some(scope) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
        let _ = scope.set_timeout_with_callback_and_timeout_and_arguments_0(cb, ms);
    } else {
        // No timer facility: resolve immediately rather than hang forever.
        let _ = cb.call0(&JsValue::UNDEFINED);
    }
}

pub fn sleep(duration: Duration) -> Sleep {
    let ms = duration.as_millis().min(i32::MAX as u128) as i32;
    let promise = js_sys::Promise::new(&mut |resolve, _reject| set_timeout(&resolve, ms));
    Sleep(JsFuture::from(promise))
}

/// The future did not complete within the timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed(());

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

pub async fn timeout<F: Future>(duration: Duration, fut: F) -> Result<F::Output, Elapsed> {
    tokio::select! {
        v = fut => Ok(v),
        _ = sleep(duration) => Err(Elapsed(())),
    }
}

/// Resolves at the browser's next animation frame with its timestamp
/// (`DOMHighResTimeStamp`, ms): the tick just before the display's next
/// vsync, the moment to draw a frame that should show on that vsync. Where
/// there is no `Window` (a worker) it degrades to a ~16 ms sleep.
pub struct AnimationFrame(JsFuture);

// Single thread: see the module docs.
unsafe impl Send for AnimationFrame {}
unsafe impl Sync for AnimationFrame {}

impl Future for AnimationFrame {
    type Output = f64;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<f64> {
        match Pin::new(&mut self.get_mut().0).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(v) => Poll::Ready(v.ok().and_then(|v| v.as_f64()).unwrap_or(0.0)),
        }
    }
}

pub fn animation_frame() -> AnimationFrame {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let global = js_sys::global();
        if let Some(win) = global.dyn_ref::<web_sys::Window>() {
            if win.request_animation_frame(&resolve).is_ok() {
                return;
            }
        }
        set_timeout(&resolve, 16);
    });
    AnimationFrame(JsFuture::from(promise))
}

// ---------------------------------------------------------------------------
// Cooperative yield
// ---------------------------------------------------------------------------

/// `MessageChannel` used as a macrotask trampoline: a `postMessage` to
/// ourselves is the fastest way back to the event loop after the browser has
/// run its pending tasks (WebCodecs output callbacks, audio process events).
/// `setTimeout(0)` is clamped to ≥4 ms once nested, which at one yield per
/// submitted sample would cost a third of a segment's duration.
struct Trampoline {
    sender: web_sys::MessagePort,
    /// Resolvers waiting for the next message, FIFO — a port delivers
    /// messages in order, so the oldest waiter takes the next one.
    waiters: Rc<RefCell<VecDeque<js_sys::Function>>>,
    _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

impl Trampoline {
    fn new() -> Option<Self> {
        let channel = web_sys::MessageChannel::new().ok()?;
        let receiver = channel.port1();
        let waiters: Rc<RefCell<VecDeque<js_sys::Function>>> = Default::default();
        let onmessage = {
            let waiters = Rc::clone(&waiters);
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |_ev| {
                let next = waiters.borrow_mut().pop_front();
                if let Some(resolve) = next {
                    let _ = resolve.call0(&JsValue::UNDEFINED);
                }
            })
        };
        receiver.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        receiver.start();
        Some(Trampoline {
            sender: channel.port2(),
            waiters,
            _onmessage: onmessage,
        })
    }

    /// Queue `resolve` to run on the next macrotask. `false` if the post
    /// failed (caller falls back to `setTimeout`).
    fn bounce(&self, resolve: &js_sys::Function) -> bool {
        self.waiters.borrow_mut().push_back(resolve.clone());
        if self.sender.post_message(&JsValue::UNDEFINED).is_ok() {
            true
        } else {
            self.waiters.borrow_mut().pop_back();
            false
        }
    }
}

thread_local! {
    static TRAMPOLINE: Option<Trampoline> = Trampoline::new();
}

/// Yield once to the browser event loop (a macrotask boundary) so callback
/// driven decoders — WebCodecs — get to deliver their output before the
/// engine feeds the next sample. Native decoders are pulled synchronously
/// and this is a no-op there.
pub fn cooperative_yield() -> Sleep {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let bounced = TRAMPOLINE.with(|t| t.as_ref().map(|t| t.bounce(&resolve)).unwrap_or(false));
        if !bounced {
            set_timeout(&resolve, 0);
        }
    });
    Sleep(JsFuture::from(promise))
}
