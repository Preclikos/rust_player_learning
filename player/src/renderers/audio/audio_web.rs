//! Browser PCM output via Web Audio — an `AudioWorkletNode`.
//!
//! The worklet processor (JS, embedded below and loaded from a Blob URL, so
//! the host page ships nothing) runs on the browser's audio rendering
//! thread. The main thread feeds it packed-stereo f32 chunks over its
//! `MessagePort`; it plays them back to back, keeps silent when paused or
//! starved, and reports what it actually played. Same contract as the cpal
//! backend: `samples_consumed` = samples the device presented (pause and
//! underrun silence do not count) and is the clock the video sync loop paces
//! against; flush generations are honoured (`FlushState`) so
//! `played_since_flush_ms` counts only THIS pipeline's audio.
//!
//! Why not `ScriptProcessorNode`: deprecated, and it ran on the main thread,
//! where a long task (a segment parse, a subtitle rasterization) became an
//! audible gap. The worklet keeps playing through main-thread stalls as long
//! as it has data — the pump keeps ~0.5 s ahead of it.
//!
//! The `AudioContext` is created at the host's request; browsers only let it
//! run after a user gesture, so the host page should construct the player
//! from a click handler (the demo does). `resume()` is called on creation
//! and again on every un-pause in case the context was suspended meanwhile.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use js_sys::{Array, Float32Array, Reflect};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Notify,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use super::{AudioRendererCommand, QUEUE_CHUNKS};
use crate::av_sync::{AudioChunk, FlushState};

/// How far ahead of the worklet the pump keeps it fed (frames at the output
/// rate). Enough to ride out a main-thread stall of that length; small enough
/// that a flush (seek / track switch) drops little and the volume/pause
/// messages take effect promptly.
const WORKLET_TARGET_FRAMES: u64 = 24_000; // 0.5 s at 48 kHz

/// The processor. `process()` runs per 128-frame render quantum on the audio
/// thread. Messages in: `pcm` (gen + interleaved stereo Float32Array), `flush`
/// (drop everything below gen), `pause`, `vol`. Messages out: `stats` with the
/// cumulative frames played, frames still buffered, frames of underrun
/// silence since the last report, and the generations whose first frame
/// started playing since the last report with the played-position at that
/// instant (the flush boundary the clock needs).
const PROCESSOR_JS: &str = r#"
class RustPlayerSink extends AudioWorkletProcessor {
  constructor() {
    super();
    this.q = []; this.off = 0; this.played = 0; this.buffered = 0; this.starved = 0;
    this.paused = false; this.vol = 1.0; this.lastGen = -1; this.starts = []; this.tick = 0;
    this.port.onmessage = (e) => {
      const m = e.data;
      if (m.t === 'pcm') { this.q.push({ gen: m.gen, d: m.d }); this.buffered += m.d.length >> 1; }
      else if (m.t === 'flush') {
        if (this.q.length && this.q[0].gen < m.gen) { this.off = 0; }
        this.q = this.q.filter(c => c.gen >= m.gen);
        let b = 0; for (const c of this.q) b += c.d.length >> 1; this.buffered = b - (this.off >> 1);
      }
      else if (m.t === 'pause') { this.paused = !!m.v; }
      else if (m.t === 'vol') { this.vol = m.v; }
    };
  }
  process(_inputs, outputs) {
    const out = outputs[0]; const L = out[0]; const R = out.length > 1 ? out[1] : out[0]; const n = L.length;
    let i = 0;
    if (!this.paused) {
      while (i < n && this.q.length) {
        const c = this.q[0]; const d = c.d;
        if (this.off === 0 && c.gen !== this.lastGen) { this.starts.push([c.gen, this.played + i]); this.lastGen = c.gen; }
        const v = this.vol;
        while (i < n && this.off < d.length) { L[i] = d[this.off] * v; R[i] = d[this.off + 1] * v; this.off += 2; i++; }
        if (this.off >= d.length) { this.q.shift(); this.off = 0; }
      }
      this.played += i; this.buffered -= i;
      if (i < n) this.starved += n - i;
    }
    for (; i < n; i++) { L[i] = 0; R[i] = 0; }
    this.tick++;
    if (this.starts.length || (this.tick & 7) === 0) {
      this.port.postMessage({ t: 'stats', played: this.played, buffered: this.buffered, starved: this.starved, starts: this.starts });
      this.starts = []; this.starved = 0;
    }
    return true;
  }
}
registerProcessor('rustplayer-sink', RustPlayerSink);
"#;

/// Everything that must stay alive for the stream to keep playing. Dropped
/// on `Stop`, which closes the context.
struct WebAudioOutput {
    context: web_sys::AudioContext,
    node: web_sys::AudioWorkletNode,
    _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

impl Drop for WebAudioOutput {
    fn drop(&mut self) {
        if let Ok(port) = self.node.port() {
            port.set_onmessage(None);
        }
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

fn describe(e: &JsValue) -> String {
    if let Some(ex) = e.dyn_ref::<web_sys::DomException>() {
        format!("{}: {}", ex.name(), ex.message())
    } else {
        e.as_string().unwrap_or_else(|| format!("{:?}", e))
    }
}

fn js_obj(pairs: &[(&str, JsValue)]) -> js_sys::Object {
    let o = js_sys::Object::new();
    for (k, v) in pairs {
        let _ = Reflect::set(&o, &JsValue::from_str(k), v);
    }
    o
}

fn get_f64(v: &JsValue, key: &str) -> Option<f64> {
    Reflect::get(v, &JsValue::from_str(key)).ok().and_then(|x| x.as_f64())
}

/// `AudioContext.baseLatency + outputLatency` in ms — how long after the
/// worklet writes a frame it is audible. Read reflectively (this web-sys has
/// no binding); 0 where the browser doesn't report it.
fn context_latency_ms(ctx: &web_sys::AudioContext) -> u64 {
    let base = get_f64(ctx, "baseLatency").unwrap_or(0.0);
    let out = get_f64(ctx, "outputLatency").unwrap_or(0.0);
    ((base + out) * 1000.0).round().max(0.0) as u64
}

/// Device-less fallback (no `AudioContext` / no worklet — e.g. a page
/// without audio permission): drain the queue at real-time pace so the
/// pipeline flows and video plays silently. Async twin of the cpal backend's
/// null sink, honouring the flush generations the same way.
fn start_null_sink(
    mut rx: Receiver<AudioChunk>,
    mut command_receiver: Receiver<AudioRendererCommand>,
    stop: Arc<Notify>,
    flush_state: Arc<FlushState>,
    paused_flag: Arc<AtomicBool>,
    samples_consumed: Arc<AtomicU64>,
) {
    crate::rt::spawn(async move {
        const RATE: f64 = 48_000.0 * 2.0; // samples/sec, packed stereo
        let tick = Duration::from_millis(10);
        let mut credit = 0f64;
        let mut consumed = 0u64;
        let mut cur: Option<(Vec<f32>, usize)> = None;
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
                if cur.as_ref().map(|(b, o)| *o >= b.len()).unwrap_or(true) {
                    cur = None;
                    match rx.try_recv() {
                        Ok(chunk) => {
                            if chunk.gen < flush_state.current_gen() {
                                continue;
                            }
                            if !flush_state.has_boundary(chunk.gen) {
                                flush_state.mark_boundary(chunk.gen, consumed);
                            }
                            cur = Some((chunk.samples, 0));
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => return,
                    }
                }
                if let Some((_, o)) = cur.as_mut() {
                    *o += 1;
                    consumed += 1;
                    credit -= 1.0;
                }
            }
            samples_consumed.store(consumed, Ordering::Release);
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
    command_receiver: Receiver<AudioRendererCommand>,
    stop: Arc<Notify>,
    flush_state: Arc<FlushState>,
    paused_flag: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    samples_consumed: Arc<AtomicU64>,
    output_latency_ms: Arc<AtomicU64>,
    // Flipped by the worklet's first report: the output is really running
    // (an `AudioContext` created outside a user gesture stays suspended and
    // never renders). See `AudioRenderer::output_running`.
    output_running: Arc<AtomicBool>,
) -> (Sender<AudioChunk>, u32) {
    let (sample_sender, sample_receiver) = mpsc::channel::<AudioChunk>(QUEUE_CHUNKS);

    let context = match web_sys::AudioContext::new() {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "[audio] AudioContext unavailable ({:?}) — NULL audio sink (silent playback, real-time drain)",
                e
            );
            // The null sink drains at real-time pace from the start: it IS the clock.
            output_running.store(true, Ordering::Relaxed);
            start_null_sink(sample_receiver, command_receiver, stop, flush_state, paused_flag, samples_consumed);
            return (sample_sender, 48_000);
        }
    };
    let out_rate = context.sample_rate().round() as u32;

    // Autoplay policy: a context created outside a user gesture starts
    // suspended. Ask; the host's un-pause asks again. Until the first report
    // the sink reports no clock (see `output_running`), so playback starts on
    // the wall clock, silent, instead of holding for 5 s.
    let _ = context.resume();
    if context.state() == web_sys::AudioContextState::Suspended {
        log::warn!(
            "[audio] AudioContext is suspended (no user gesture yet) — video runs on the wall clock, audio joins when the browser lets it"
        );
    }

    // The worklet module must be added asynchronously; everything from here
    // on is one task that sets the node up and then pumps chunks into it.
    crate::rt::spawn(pump(
        SendCell(context),
        out_rate,
        sample_receiver,
        command_receiver,
        stop,
        flush_state,
        paused_flag,
        volume,
        samples_consumed,
        output_latency_ms,
        output_running,
    ));

    (sample_sender, out_rate)
}

/// JS handle carried into a spawned future. Single thread (see rt/web.rs).
struct SendCell<T>(T);
unsafe impl<T> Send for SendCell<T> {}

#[allow(clippy::too_many_arguments)]
async fn pump(
    context: SendCell<web_sys::AudioContext>,
    out_rate: u32,
    mut rx: Receiver<AudioChunk>,
    mut command_receiver: Receiver<AudioRendererCommand>,
    stop: Arc<Notify>,
    flush_state: Arc<FlushState>,
    paused_flag: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    samples_consumed: Arc<AtomicU64>,
    output_latency_ms: Arc<AtomicU64>,
    output_running: Arc<AtomicBool>,
) {
    let context = context.0;
    let node = match setup_worklet(&context).await {
        Ok(n) => n,
        Err(e) => {
            log::warn!("[audio] AudioWorklet unavailable ({e}) — NULL audio sink (silent playback, real-time drain)");
            let _ = context.close();
            output_running.store(true, Ordering::Relaxed);
            start_null_sink(rx, command_receiver, stop, flush_state, paused_flag, samples_consumed);
            return;
        }
    };
    let port = match node.port() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[audio] worklet port unavailable ({}) — NULL audio sink", describe(&e));
            let _ = context.close();
            output_running.store(true, Ordering::Relaxed);
            start_null_sink(rx, command_receiver, stop, flush_state, paused_flag, samples_consumed);
            return;
        }
    };
    log::info!("[audio] Web Audio output {} Hz / 2 ch (AudioWorklet)", out_rate);

    // Worklet → main: played frames (the clock), buffered frames
    // (backpressure), generation starts (flush boundaries), underrun frames.
    let buffered = Rc::new(Cell::new(0u64));
    let report = Rc::new(Notify::new());
    let onmessage = {
        let buffered = Rc::clone(&buffered);
        let report = Rc::clone(&report);
        let flush_state = Arc::clone(&flush_state);
        let samples_consumed = Arc::clone(&samples_consumed);
        let output_running = Arc::clone(&output_running);
        let ctx = context.clone();
        let output_latency_ms = Arc::clone(&output_latency_ms);
        let mut diag_starved = 0u64;
        let mut diag_last = 0f64;
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            let m = ev.data();
            let Some(played) = get_f64(&m, "played") else { return };
            if !output_running.swap(true, Ordering::Relaxed) {
                log::info!("[audio] Web Audio output running (first worklet report)");
            }
            // Boundaries BEFORE the position so `played_since_flush` never
            // sees the new generation's frames against the old boundary.
            if let Ok(starts) = Reflect::get(&m, &JsValue::from_str("starts")) {
                if let Some(arr) = starts.dyn_ref::<Array>() {
                    for s in arr.iter() {
                        let pair: Array = s.unchecked_into();
                        let gen = pair.get(0).as_f64().unwrap_or(0.0) as u64;
                        let at = pair.get(1).as_f64().unwrap_or(0.0) as u64;
                        if !flush_state.has_boundary(gen) {
                            flush_state.mark_boundary(gen, at * 2);
                        }
                    }
                }
            }
            samples_consumed.store(played as u64 * 2, Ordering::Release);
            buffered.set(get_f64(&m, "buffered").unwrap_or(0.0).max(0.0) as u64);
            diag_starved += get_f64(&m, "starved").unwrap_or(0.0) as u64;
            output_latency_ms.store(context_latency_ms(&ctx), Ordering::Relaxed);
            report.notify_one();
            let now = ctx.current_time();
            if now - diag_last >= 5.0 {
                if diag_last > 0.0 {
                    log::debug!(
                        "[audio] web: played={}f buffered={}f starved={}f latency={}ms state={:?}",
                        played as u64,
                        buffered.get(),
                        diag_starved,
                        output_latency_ms.load(Ordering::Relaxed),
                        ctx.state()
                    );
                }
                diag_last = now;
                diag_starved = 0;
            }
        })
    };
    port.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    if let Err(e) = node.connect_with_audio_node(&context.destination()) {
        log::warn!("[audio] connect to destination failed: {:?}", e);
    }

    let output = Rc::new(WebAudioOutput {
        context: context.clone(),
        node,
        _onmessage: onmessage,
    });
    LIVE.with(|l| *l.borrow_mut() = Some(Rc::clone(&output)));

    // Main → worklet: chunks of the live generation, kept ~0.5 s ahead;
    // flush / pause / volume changes as they happen.
    let mut sent_gen = flush_state.current_gen();
    let mut sent_paused = paused_flag.load(Ordering::Relaxed);
    let mut sent_vol = f32::from_bits(volume.load(Ordering::Relaxed));
    let _ = port.post_message(&js_obj(&[("t", "pause".into()), ("v", JsValue::from_bool(sent_paused))]));
    let _ = port.post_message(&js_obj(&[("t", "vol".into()), ("v", JsValue::from_f64(sent_vol as f64))]));
    let mut pending: Option<AudioChunk> = None;
    loop {
        // Control changes first (cheap, and pause must not wait behind data).
        let live_gen = flush_state.current_gen();
        if live_gen != sent_gen {
            sent_gen = live_gen;
            let _ = port.post_message(&js_obj(&[("t", "flush".into()), ("gen", JsValue::from_f64(live_gen as f64))]));
        }
        let paused = paused_flag.load(Ordering::Relaxed);
        if paused != sent_paused {
            sent_paused = paused;
            let _ = port.post_message(&js_obj(&[("t", "pause".into()), ("v", JsValue::from_bool(paused))]));
        }
        let vol = f32::from_bits(volume.load(Ordering::Relaxed));
        if vol != sent_vol {
            sent_vol = vol;
            let _ = port.post_message(&js_obj(&[("t", "vol".into()), ("v", JsValue::from_f64(vol as f64))]));
        }

        // Backpressure: hold data while the worklet has enough. Wake on its
        // next report (or a short tick, so control changes still go out).
        if buffered.get() >= WORKLET_TARGET_FRAMES {
            tokio::select! {
                _ = report.notified() => {}
                _ = crate::rt::sleep(Duration::from_millis(50)) => {}
                cmd = command_receiver.recv() => {
                    if matches!(cmd, Some(AudioRendererCommand::Stop) | None) { break; }
                }
            }
            continue;
        }

        let chunk = match pending.take() {
            Some(c) => c,
            None => {
                tokio::select! {
                    got = rx.recv() => match got {
                        Some(c) => c,
                        None => break, // producer gone (sink torn down)
                    },
                    cmd = command_receiver.recv() => {
                        if matches!(cmd, Some(AudioRendererCommand::Stop) | None) { break; }
                        continue;
                    }
                    _ = crate::rt::sleep(Duration::from_millis(50)) => continue, // re-check controls
                }
            }
        };
        if chunk.gen < flush_state.current_gen() {
            continue; // stale — queued before the last flush
        }
        let data = Float32Array::from(&chunk.samples[..]);
        let msg = js_obj(&[
            ("t", "pcm".into()),
            ("gen", JsValue::from_f64(chunk.gen as f64)),
            ("d", data.clone().into()),
        ]);
        let transfer = Array::new();
        transfer.push(&data.buffer());
        if port.post_message_with_transferable(&msg, &transfer).is_err() {
            pending = Some(chunk);
            crate::rt::sleep(Duration::from_millis(20)).await;
            continue;
        }
        buffered.set(buffered.get() + (chunk.samples.len() / 2) as u64);
    }

    stop.notify_waiters();
    LIVE.with(|l| {
        let mut slot = l.borrow_mut();
        if slot.as_ref().is_some_and(|live| Rc::ptr_eq(live, &output)) {
            *slot = None;
        }
    });
    // `output` drops here: the context closes.
}

/// Load the processor from a Blob URL and create the node (0 inputs, one
/// stereo output).
async fn setup_worklet(context: &web_sys::AudioContext) -> Result<web_sys::AudioWorkletNode, String> {
    let worklet = context.audio_worklet().map_err(|e| describe(&e))?;
    let parts = Array::new();
    parts.push(&JsValue::from_str(PROCESSOR_JS));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("application/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &opts).map_err(|e| describe(&e))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|e| describe(&e))?;
    let added = worklet.add_module(&url).map_err(|e| describe(&e))?;
    let result = JsFuture::from(added).await;
    let _ = web_sys::Url::revoke_object_url(&url);
    result.map_err(|e| format!("addModule: {}", describe(&e)))?;

    let node_opts = web_sys::AudioWorkletNodeOptions::new();
    node_opts.set_number_of_inputs(0);
    node_opts.set_number_of_outputs(1);
    let channels = Array::new();
    channels.push(&JsValue::from_f64(2.0));
    node_opts.set_output_channel_count(&channels);
    web_sys::AudioWorkletNode::new_with_options(context, "rustplayer-sink", &node_opts).map_err(|e| describe(&e))
}
