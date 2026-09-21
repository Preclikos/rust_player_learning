//! Browser PCM output via Web Audio.
//!
//! A `ScriptProcessorNode` pulls packed-stereo f32 from the chunk queue in
//! its `audioprocess` callback (main thread, driven by the audio clock) and
//! writes it into the output buffer. Same contract as the cpal backend:
//! `samples_consumed` counts what the device actually took (pause /
//! underrun silence does not advance it) and is the clock the video sync
//! loop paces against; `output_latency_ms` is how far ahead of "now" the
//! buffer being filled will be audible (`playbackTime − currentTime`).
//!
//! `ScriptProcessorNode` is deprecated in favour of `AudioWorklet`, but it
//! is universally shipped, needs no separate worklet module file, and runs
//! its callback on the thread our queue lives on — which is what makes it a
//! one-file drop-in here. Swapping to a worklet is a contained follow-up.
//!
//! The `AudioContext` is created at the host's request; browsers only let it
//! run after a user gesture, so the host page should construct the player
//! from a click handler (the demo does). `resume()` is called on creation
//! and again on every un-pause in case the context was suspended meanwhile.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Notify,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::{AudioRendererCommand, QUEUE_CHUNKS};
use crate::av_sync::{AudioChunk, ChunkCursor, FlushState};

/// Frames per `audioprocess` callback. 2048 @ 48 kHz ≈ 43 ms: low enough
/// that A/V alignment stays tight, high enough that a busy main thread
/// (a segment decrypt, a subtitle rasterization) doesn't underrun.
const BUFFER_FRAMES: u32 = 2048;

/// Everything that must stay alive for the stream to keep playing. Dropped
/// on `Stop`, which closes the context.
struct WebAudioOutput {
    context: web_sys::AudioContext,
    node: web_sys::ScriptProcessorNode,
    _onaudioprocess: Closure<dyn FnMut(web_sys::AudioProcessingEvent)>,
}

impl Drop for WebAudioOutput {
    fn drop(&mut self) {
        self.node.set_onaudioprocess(None);
        let _ = self.node.disconnect();
        let _ = self.context.close();
    }
}

thread_local! {
    /// The live output, so `resume_if_suspended` (called from un-pause) can
    /// reach the context without threading it through the renderer.
    static LIVE: RefCell<Option<Rc<WebAudioOutput>>> = const { RefCell::new(None) };
}

/// Kick a context the browser suspended (autoplay policy) back into running.
pub(super) fn resume_if_suspended() {
    LIVE.with(|l| {
        if let Some(out) = l.borrow().as_ref() {
            if out.context.state() == web_sys::AudioContextState::Suspended {
                let _ = out.context.resume();
            }
        }
    });
}

/// Device-less fallback (no `AudioContext` — e.g. a page without audio
/// permission): drain the queue at real-time pace so the pipeline flows and
/// video plays silently. Async twin of the cpal backend's null sink.
fn start_null_sink(
    sample_receiver: Receiver<AudioChunk>,
    mut command_receiver: Receiver<AudioRendererCommand>,
    stop: Arc<Notify>,
    flush_state: Arc<FlushState>,
    paused_flag: Arc<AtomicBool>,
    samples_consumed: Arc<AtomicU64>,
) {
    crate::rt::spawn(async move {
        const RATE: f64 = 48_000.0 * 2.0; // samples/sec, packed stereo
        let tick = Duration::from_millis(10);
        let mut cursor = ChunkCursor::new(sample_receiver, flush_state, samples_consumed);
        let mut credit = 0f64;
        let mut last = crate::rt::Instant::now();
        loop {
            crate::rt::sleep(tick).await;
            let now = crate::rt::Instant::now();
            if paused_flag.load(Ordering::Relaxed) {
                last = now;
                continue;
            }
            credit += now.duration_since(last).as_secs_f64() * RATE;
            last = now;
            credit = credit.min(RATE);
            while credit >= 1.0 {
                match cursor.next_sample() {
                    Some(_) => credit -= 1.0,
                    None if cursor.is_closed() => return,
                    None => break,
                }
            }
            cursor.commit();
        }
    });
    crate::rt::spawn(async move {
        #[allow(clippy::never_loop)]
        while let Some(command) = command_receiver.recv().await {
            match command {
                AudioRendererCommand::Stop => {
                    stop.notify_waiters();
                    break;
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start_thread(
    mut command_receiver: Receiver<AudioRendererCommand>,
    stop: Arc<Notify>,
    flush_state: Arc<FlushState>,
    paused_flag: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    samples_consumed: Arc<AtomicU64>,
    output_latency_ms: Arc<AtomicU64>,
) -> (Sender<AudioChunk>, u32) {
    let (sample_sender, sample_receiver) = mpsc::channel::<AudioChunk>(QUEUE_CHUNKS);

    let context = match web_sys::AudioContext::new() {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "[audio] AudioContext unavailable ({:?}) — NULL audio sink (silent playback, real-time drain)",
                e
            );
            start_null_sink(
                sample_receiver,
                command_receiver,
                stop,
                flush_state,
                paused_flag,
                samples_consumed,
            );
            return (sample_sender, 48_000);
        }
    };
    let out_rate = context.sample_rate().round() as u32;
    let node = match context
        .create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(
            BUFFER_FRAMES,
            0,
            2,
        ) {
        Ok(n) => n,
        Err(e) => {
            log::warn!("[audio] createScriptProcessor failed ({:?}) — NULL audio sink", e);
            let _ = context.close();
            start_null_sink(
                sample_receiver,
                command_receiver,
                stop,
                flush_state,
                paused_flag,
                samples_consumed,
            );
            return (sample_sender, 48_000);
        }
    };
    log::info!("[audio] Web Audio output {} Hz / 2 ch, {} frames per callback", out_rate, BUFFER_FRAMES);

    let mut cursor = ChunkCursor::new(sample_receiver, flush_state, samples_consumed);
    let ctx_for_cb = context.clone();
    let mut left: Vec<f32> = vec![0.0; BUFFER_FRAMES as usize];
    let mut right: Vec<f32> = vec![0.0; BUFFER_FRAMES as usize];
    let onaudioprocess = Closure::<dyn FnMut(web_sys::AudioProcessingEvent)>::new(
        move |ev: web_sys::AudioProcessingEvent| {
            let Ok(out) = ev.output_buffer() else { return };
            let frames = out.length() as usize;
            if left.len() != frames {
                left.resize(frames, 0.0);
                right.resize(frames, 0.0);
            }
            // How long until this buffer is audible — the device-side
            // latency the video sync loop compensates for.
            let ahead = ev.playback_time() - ctx_for_cb.current_time();
            let ms = (ahead * 1000.0) as i64;
            if ms > 0 && ms <= 1000 {
                output_latency_ms.store(ms as u64, Ordering::Relaxed);
            }
            if paused_flag.load(Ordering::Relaxed) {
                // Silence WITHOUT consuming — resume picks up exactly here.
                left.iter_mut().for_each(|s| *s = 0.0);
                right.iter_mut().for_each(|s| *s = 0.0);
            } else {
                let vol = f32::from_bits(volume.load(Ordering::Relaxed));
                for i in 0..frames {
                    left[i] = cursor.next_sample().unwrap_or(0.0) * vol;
                    right[i] = cursor.next_sample().unwrap_or(0.0) * vol;
                }
                cursor.commit();
            }
            let _ = out.copy_to_channel(&mut left, 0);
            let _ = out.copy_to_channel(&mut right, 1);
        },
    );
    node.set_onaudioprocess(Some(onaudioprocess.as_ref().unchecked_ref()));
    if let Err(e) = node.connect_with_audio_node(&context.destination()) {
        log::warn!("[audio] connect to destination failed: {:?}", e);
    }
    // Autoplay policy: a context created outside a user gesture starts
    // suspended. Ask; the host's un-pause asks again.
    let _ = context.resume();

    let output = Rc::new(WebAudioOutput {
        context,
        node,
        _onaudioprocess: onaudioprocess,
    });
    LIVE.with(|l| *l.borrow_mut() = Some(Rc::clone(&output)));

    // `Stop` is the only command today (kept as a loop so adding commands is
    // a new match arm). Dropping the output closes the context.
    let holder = SendCell(output);
    crate::rt::spawn(async move {
        let holder = holder;
        #[allow(clippy::never_loop)]
        while let Some(command) = command_receiver.recv().await {
            match command {
                AudioRendererCommand::Stop => {
                    stop.notify_waiters();
                    break;
                }
            }
        }
        LIVE.with(|l| {
            let mut slot = l.borrow_mut();
            if slot.as_ref().is_some_and(|live| Rc::ptr_eq(live, &holder.0)) {
                *slot = None;
            }
        });
        drop(holder);
    });

    (sample_sender, out_rate)
}

/// `Rc<JS handles>` moved into a spawned task. Single thread (see rt/web.rs).
struct SendCell(Rc<WebAudioOutput>);
unsafe impl Send for SendCell {}
