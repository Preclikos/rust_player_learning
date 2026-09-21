//! Browser decoders on WebCodecs (`VideoDecoder` / `AudioDecoder`).
//!
//! Both are callback-driven: the browser hands decoded output to a JS
//! closure whenever its event loop runs. The engine's decode loop yields to
//! that loop after every `submit` (`rt::cooperative_yield`) and pulls the
//! results out with `try_recv`, so the `HwVideoDecoder` / `AudioDecoder`
//! contracts stay the pull-based shape every other platform implements.
//!
//! Video output is copied OUT of the `VideoFrame` into CPU memory
//! ([`CpuPlanarFrame`]: tightly packed Y + interleaved UV) the moment it is
//! delivered, and the `VideoFrame` is closed. Holding decoder frames would
//! stall the browser's decoder pool; copying keeps every frame downstream a
//! plain `Send` Rust value that the renderer uploads as two textures — the
//! same two-plane shape the Apple path samples, so the NV12 / P010 shaders
//! and the HDR tonemap are shared.
//!
//! `unsafe impl Send`: wasm32-unknown-unknown without `atomics` is a single
//! thread, so the `Send` bounds on the decoder traits can never be exercised.
//! See `rt/web.rs` for the same contract.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use js_sys::{Float32Array, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use super::pcm::{downmix_to_stereo, interleave, LinearResampler};
use super::{
    AudioCodec, AudioDecoder, AudioDecoderParams, CpuPlanarFrame, DecodedAudioFrame,
    DecodedVideoFrame, DecoderError, HwVideoDecoder, PlatformFrame, VideoColorInfo,
    VideoDecoderParams,
};
use crate::parsers::hevc::nal_unit_type;

fn describe(e: &JsValue) -> String {
    if let Some(ex) = e.dyn_ref::<web_sys::DomException>() {
        format!("{}: {}", ex.name(), ex.message())
    } else if let Some(s) = e.as_string() {
        s
    } else {
        format!("{:?}", e)
    }
}

fn js_err(prefix: &str, e: JsValue) -> DecoderError {
    format!("{prefix}: {}", describe(&e)).into()
}

// ---------------------------------------------------------------------------
// hvcC helpers
// ---------------------------------------------------------------------------

/// Codec parameter string (ISO/IEC 14496-15 Annex E.3) from an
/// HEVCDecoderConfigurationRecord: `hvc1.<profile>.<compat>.<tier><level>[.<constraints>]`,
/// e.g. `hvc1.1.6.L120.B0` for Main@L4.0 or `hvc1.2.4.L153.B0` for Main 10@L5.1.
pub fn hevc_codec_string(hvcc: &[u8]) -> Option<String> {
    if hvcc.len() < 23 {
        return None;
    }
    let profile_space = hvcc[1] >> 6;
    let tier = (hvcc[1] >> 5) & 1;
    let profile_idc = hvcc[1] & 0x1f;
    // The 32 compatibility flags are written in REVERSE bit order, as hex.
    let compat = u32::from_be_bytes([hvcc[2], hvcc[3], hvcc[4], hvcc[5]]).reverse_bits();
    let constraints = &hvcc[6..12];
    let level_idc = hvcc[12];

    let mut s = String::from("hvc1.");
    match profile_space {
        1 => s.push('A'),
        2 => s.push('B'),
        3 => s.push('C'),
        _ => {}
    }
    s.push_str(&profile_idc.to_string());
    s.push_str(&format!(".{:X}", compat));
    s.push_str(&format!(".{}{}", if tier == 1 { 'H' } else { 'L' }, level_idc));
    // Constraint bytes with trailing zero bytes omitted.
    let mut end = constraints.len();
    while end > 0 && constraints[end - 1] == 0 {
        end -= 1;
    }
    for b in &constraints[..end] {
        s.push_str(&format!(".{:X}", b));
    }
    Some(s)
}

/// `lengthSizeMinusOne + 1` from the record: the byte width of the NAL
/// length prefixes in the samples (4 for every stream we have seen).
fn nal_length_size(hvcc: &[u8]) -> usize {
    hvcc.get(21).map(|b| (b & 0x03) as usize + 1).unwrap_or(4)
}

/// Whether a length-prefixed HEVC sample contains an IRAP picture (BLA /
/// IDR / CRA, nal_unit_type 16..=23). WebCodecs must be told which chunks
/// are key frames and refuses a delta chunk right after `configure`.
fn is_irap_sample(sample: &[u8], len_size: usize) -> bool {
    let mut i = 0usize;
    while i + len_size <= sample.len() {
        let mut n = 0usize;
        for k in 0..len_size {
            n = (n << 8) | sample[i + k] as usize;
        }
        i += len_size;
        if n == 0 || i + n > sample.len() {
            break;
        }
        if let Some(t) = nal_unit_type(&sample[i..i + n]) {
            if (16..=23).contains(&t) {
                return true;
            }
        }
        i += n;
    }
    false
}

/// Kick off a codec `flush()` and hand back the flag its promise flips when
/// every pending output has been emitted (also on rejection — a closed
/// codec has nothing left to deliver). `None` (no decoder) is done already.
fn start_flush<P: Into<JsValue>>(flush: Option<P>) -> Rc<Cell<bool>> {
    let done = Rc::new(Cell::new(flush.is_none()));
    if let Some(promise) = flush {
        let flag = Rc::clone(&done);
        let promise: js_sys::Promise = promise.into().unchecked_into();
        let started = crate::rt::Instant::now();
        wasm_bindgen_futures::spawn_local(async move {
            let r = JsFuture::from(promise).await;
            log::debug!(
                "[webcodecs] flush {} after {}ms",
                if r.is_ok() { "resolved" } else { "rejected" },
                started.elapsed().as_millis()
            );
            flag.set(true);
        });
    }
    done
}

/// In-flight bound (samples submitted minus outputs delivered) past which
/// the decode loop yields to the event loop. See `wants_event_loop` on the
/// two decoders. Each turn of the event loop costs ~10 ms of other queued
/// work and delivers whatever the codec finished meanwhile, so the bound is
/// the depth that lets a turn carry several outputs: a hardware video
/// decoder pipelines a handful of frames; 48 audio AUs are ~0.5 s of media
/// (measured: 8 gave ~90 % of real time, still starving the output).
const VIDEO_IN_FLIGHT: u64 = 6;
const AUDIO_IN_FLIGHT: u64 = 96;

// ---------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------

/// State shared between the decoder object and the JS output / error
/// callbacks (which own `Rc` clones).
struct VideoShared {
    /// Frames copied out and ready for `try_recv`, in output order.
    ready: RefCell<VecDeque<DecodedVideoFrame>>,
    /// Frames delivered by WebCodecs, awaiting the (async) copy-out. Drained
    /// sequentially by one pump task so output order is preserved.
    pending: RefCell<VecDeque<web_sys::VideoFrame>>,
    pump_active: Cell<bool>,
    /// First error reported by the decoder; surfaced on the next
    /// `submit` / `try_recv` so the pipeline restarts.
    error: RefCell<Option<String>>,
    color: Cell<VideoColorInfo>,
    /// Log an unsupported pixel format once, not per frame.
    warned_format: Cell<bool>,
    /// Frames the browser has handed to the output callback (before copy-out).
    delivered: Cell<u64>,
    frames_out: Cell<u64>,
}

impl VideoShared {
    fn fail(&self, msg: String) {
        log::error!("[webcodecs] video: {msg}");
        let mut e = self.error.borrow_mut();
        if e.is_none() {
            *e = Some(msg);
        }
    }
}

pub struct WebCodecsVideoDecoder {
    decoder: Option<web_sys::VideoDecoder>,
    shared: Rc<VideoShared>,
    nal_len_size: usize,
    /// WebCodecs needs a key frame first (after configure and after flush);
    /// delta chunks before that are dropped instead of erroring the codec.
    await_key: bool,
    /// Chunks handed to `decode()`; with `shared.delivered` gives the
    /// in-flight count that paces the decode loop.
    submitted: Cell<u64>,
    /// End of input signalled: `Some(done)` where `done` flips once the
    /// codec's `flush()` promise resolved — every queued output has been
    /// delivered. While draining, `wants_event_loop` follows this instead
    /// of the in-flight bound (an AU the codec swallowed, e.g. priming,
    /// would otherwise keep the count above zero forever).
    draining: Option<Rc<Cell<bool>>>,
    /// Closures must outlive the decoder they are registered on.
    _output_cb: Option<Closure<dyn FnMut(web_sys::VideoFrame)>>,
    _error_cb: Option<Closure<dyn FnMut(JsValue)>>,
}

// Single thread (see module docs).
unsafe impl Send for WebCodecsVideoDecoder {}

impl WebCodecsVideoDecoder {
    pub fn new() -> Self {
        Self {
            decoder: None,
            shared: Rc::new(VideoShared {
                ready: RefCell::new(VecDeque::new()),
                pending: RefCell::new(VecDeque::new()),
                pump_active: Cell::new(false),
                error: RefCell::new(None),
                color: Cell::new(VideoColorInfo::default()),
                warned_format: Cell::new(false),
                delivered: Cell::new(0),
                frames_out: Cell::new(0),
            }),
            nal_len_size: 4,
            await_key: true,
            submitted: Cell::new(0),
            draining: None,
            _output_cb: None,
            _error_cb: None,
        }
    }

    fn check_error(&self) -> Result<(), DecoderError> {
        match self.shared.error.borrow().as_ref() {
            Some(e) => Err(e.clone().into()),
            None => Ok(()),
        }
    }
}

impl Default for WebCodecsVideoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Output callback: queue the frame and make sure the copy-out pump runs.
fn on_video_output(shared: &Rc<VideoShared>, frame: web_sys::VideoFrame) {
    shared.delivered.set(shared.delivered.get() + 1);
    shared.pending.borrow_mut().push_back(frame);
    if shared.pump_active.replace(true) {
        return;
    }
    let s = Rc::clone(shared);
    wasm_bindgen_futures::spawn_local(async move {
        loop {
            let next = s.pending.borrow_mut().pop_front();
            let Some(frame) = next else { break };
            let pts_us = frame.timestamp() as i64;
            match copy_out(&frame, &s).await {
                Ok(Some(planes)) => {
                    let (width, height) = (planes.width, planes.height);
                    s.frames_out.set(s.frames_out.get() + 1);
                    s.ready.borrow_mut().push_back(DecodedVideoFrame {
                        pts_us,
                        width,
                        height,
                        native: PlatformFrame::CpuPlanes(planes),
                        desired_present_ns: 0,
                        color: s.color.get(),
                        hdr_meta: None,
                    });
                }
                Ok(None) => {}
                Err(e) => s.fail(format!("copy-out: {e}")),
            }
            frame.close();
        }
        s.pump_active.set(false);
    });
}

/// Read the `PlaneLayout[]` a `copyTo` resolved with.
fn plane_layouts(v: JsValue) -> Result<Vec<(usize, usize)>, String> {
    let arr: js_sys::Array = v.dyn_into().map_err(|_| "copyTo did not return an array")?;
    Ok(arr
        .iter()
        .map(|p| {
            let p: web_sys::PlaneLayout = p.unchecked_into();
            (p.get_offset() as usize, p.get_stride() as usize)
        })
        .collect())
}

/// Copy one row-padded plane into a tightly packed one.
fn pack_plane(src: &[u8], offset: usize, stride: usize, row_bytes: usize, rows: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(row_bytes * rows);
    for r in 0..rows {
        let start = offset + r * stride;
        let end = (start + row_bytes).min(src.len());
        if start >= end {
            out.resize(out.len() + row_bytes, 0);
            continue;
        }
        out.extend_from_slice(&src[start..end]);
        if end - start < row_bytes {
            out.resize(out.len() + (row_bytes - (end - start)), 0);
        }
    }
    out
}

/// Interleave separate 8-bit U and V planes into NV12's UV plane.
fn interleave_uv8(src: &[u8], u: (usize, usize), v: (usize, usize), w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 2];
    for r in 0..h {
        for c in 0..w {
            let iu = u.0 + r * u.1 + c;
            let iv = v.0 + r * v.1 + c;
            out[(r * w + c) * 2] = src.get(iu).copied().unwrap_or(128);
            out[(r * w + c) * 2 + 1] = src.get(iv).copied().unwrap_or(128);
        }
    }
    out
}

/// 16-bit little-endian planar samples, low-aligned (`I420P10` stores a
/// 10-bit code in the low bits) → MSB-aligned like P010 (`code << shift`),
/// tightly packed. `shift` = 16 − bit depth.
fn pack_plane16_msb(src: &[u8], offset: usize, stride: usize, w: usize, h: usize, shift: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 2);
    for r in 0..h {
        let row = offset + r * stride;
        for c in 0..w {
            let i = row + c * 2;
            let v = if i + 1 < src.len() {
                u16::from_le_bytes([src[i], src[i + 1]])
            } else {
                0
            };
            out.extend_from_slice(&(v << shift).to_le_bytes());
        }
    }
    out
}

fn interleave_uv16_msb(src: &[u8], u: (usize, usize), v: (usize, usize), w: usize, h: usize, shift: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 4);
    let rd = |off: usize| -> u16 {
        if off + 1 < src.len() {
            u16::from_le_bytes([src[off], src[off + 1]]) << shift
        } else {
            0
        }
    };
    for r in 0..h {
        for c in 0..w {
            out.extend_from_slice(&rd(u.0 + r * u.1 + c * 2).to_le_bytes());
            out.extend_from_slice(&rd(v.0 + r * v.1 + c * 2).to_le_bytes());
        }
    }
    out
}

/// Copy the frame's visible rectangle out of the browser into a
/// [`CpuPlanarFrame`]. `Ok(None)` = a pixel format we don't handle (logged
/// once; the frame is dropped).
async fn copy_out(frame: &web_sys::VideoFrame, shared: &VideoShared) -> Result<Option<CpuPlanarFrame>, String> {
    use web_sys::VideoPixelFormat as F;
    let Some(format) = frame.format() else {
        return Err("frame has no pixel format".into());
    };
    let (w, h) = match frame.visible_rect() {
        Some(r) => (r.width() as u32, r.height() as u32),
        None => (frame.coded_width(), frame.coded_height()),
    };
    if w == 0 || h == 0 {
        return Err("zero-sized frame".into());
    }
    let (bit_depth, shift) = match format {
        F::Nv12 | F::I420 => (8u8, 0u32),
        F::I420p10 => (10, 6),
        F::I420p12 => (12, 4),
        other => {
            if !shared.warned_format.replace(true) {
                log::warn!(
                    "[webcodecs] unsupported VideoFrame format {:?} — frames dropped (only NV12 / I420 / I420P10 / I420P12 are handled)",
                    other
                );
            }
            return Ok(None);
        }
    };

    // Copy into a JS-owned buffer (not a view of wasm memory: the copy is
    // async and wasm memory may move under a view while it's in flight),
    // then pull it into Rust.
    let size = frame.allocation_size().map_err(|e| describe(&e))? as usize;
    let js_buf = Uint8Array::new_with_length(size as u32);
    let layouts = JsFuture::from(frame.copy_to_with_buffer_source(&js_buf))
        .await
        .map_err(|e| describe(&e))?;
    let layouts = plane_layouts(JsValue::from(layouts))?;
    let mut raw = vec![0u8; size];
    js_buf.copy_to(&mut raw);

    let (wu, hu) = ((w as usize).div_ceil(2), (h as usize).div_ceil(2));
    let (w, h) = (w as usize, h as usize);
    let planes = match format {
        F::Nv12 => {
            let (y, uv) = (layouts.first().copied().ok_or("NV12: no Y layout")?, layouts.get(1).copied().ok_or("NV12: no UV layout")?);
            CpuPlanarFrame {
                width: w as u32,
                height: h as u32,
                bit_depth: 8,
                y: pack_plane(&raw, y.0, y.1, w, h),
                uv: pack_plane(&raw, uv.0, uv.1, wu * 2, hu),
            }
        }
        F::I420 => {
            let (y, u, v) = (
                layouts.first().copied().ok_or("I420: no Y layout")?,
                layouts.get(1).copied().ok_or("I420: no U layout")?,
                layouts.get(2).copied().ok_or("I420: no V layout")?,
            );
            CpuPlanarFrame {
                width: w as u32,
                height: h as u32,
                bit_depth: 8,
                y: pack_plane(&raw, y.0, y.1, w, h),
                uv: interleave_uv8(&raw, u, v, wu, hu),
            }
        }
        _ => {
            // I420P10 / I420P12: 16-bit LE planar, low-aligned.
            let (y, u, v) = (
                layouts.first().copied().ok_or("I420Pxx: no Y layout")?,
                layouts.get(1).copied().ok_or("I420Pxx: no U layout")?,
                layouts.get(2).copied().ok_or("I420Pxx: no V layout")?,
            );
            CpuPlanarFrame {
                width: w as u32,
                height: h as u32,
                bit_depth,
                y: pack_plane16_msb(&raw, y.0, y.1, w, h, shift),
                uv: interleave_uv16_msb(&raw, u, v, wu, hu, shift),
            }
        }
    };
    Ok(Some(planes))
}

impl HwVideoDecoder for WebCodecsVideoDecoder {
    fn name(&self) -> &'static str {
        "WebCodecs HEVC"
    }

    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError> {
        let record = &params.decoder_config_record;
        let codec = hevc_codec_string(record)
            .ok_or_else(|| -> DecoderError { "webcodecs: no hvcC record in init segment".into() })?;
        self.nal_len_size = nal_length_size(record);
        self.shared.color.set(params.color);

        let shared = Rc::clone(&self.shared);
        let output_cb = Closure::<dyn FnMut(web_sys::VideoFrame)>::new(move |frame| {
            on_video_output(&shared, frame);
        });
        let shared = Rc::clone(&self.shared);
        let error_cb = Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
            shared.fail(format!("decoder error: {}", describe(&e)));
        });

        let init = web_sys::VideoDecoderInit::new(
            error_cb.as_ref().unchecked_ref(),
            output_cb.as_ref().unchecked_ref(),
        );
        let decoder = web_sys::VideoDecoder::new(&init).map_err(|e| js_err("VideoDecoder::new", e))?;

        let config = web_sys::VideoDecoderConfig::new(&codec);
        config.set_description_u8_array(&Uint8Array::from(&record[..]));
        if params.width > 0 && params.height > 0 {
            config.set_coded_width(params.width);
            config.set_coded_height(params.height);
        }
        config.set_hardware_acceleration(web_sys::HardwareAcceleration::PreferHardware);
        decoder
            .configure(&config)
            .map_err(|e| js_err(&format!("VideoDecoder::configure({codec})"), e))?;
        log::info!(
            "[webcodecs] video configured: {} {}x{} {}-bit {:?}",
            codec,
            params.width,
            params.height,
            params.color.bit_depth,
            params.color.transfer
        );

        self.decoder = Some(decoder);
        self._output_cb = Some(output_cb);
        self._error_cb = Some(error_cb);
        self.await_key = true;
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        self.check_error()?;
        let decoder = self
            .decoder
            .as_ref()
            .ok_or_else(|| -> DecoderError { "webcodecs: submit before configure".into() })?;
        let key = is_irap_sample(sample, self.nal_len_size);
        if self.await_key && !key {
            log::debug!("[webcodecs] dropping delta sample before first key frame (pts {}ms)", pts_us / 1000);
            return Ok(());
        }
        self.await_key = false;
        let ty = if key {
            web_sys::EncodedVideoChunkType::Key
        } else {
            web_sys::EncodedVideoChunkType::Delta
        };
        let data = Uint8Array::from(sample);
        let init = web_sys::EncodedVideoChunkInit::new(&data, 0, ty);
        init.set_timestamp_f64(pts_us as f64);
        let chunk = web_sys::EncodedVideoChunk::new(&init).map_err(|e| js_err("EncodedVideoChunk", e))?;
        decoder.decode(&chunk).map_err(|e| js_err("VideoDecoder::decode", e))?;
        self.submitted.set(self.submitted.get() + 1);
        Ok(())
    }

    /// Yield once the decoder holds more than a pipeline's worth of frames
    /// we have not been handed back. Output is only delivered while the
    /// event loop runs, so this both bounds the in-flight queue (a flooded
    /// WebCodecs decoder stops delivering) and gives the copy-out pump its
    /// turn. Never wedges: a yield always returns, and an AU that produces
    /// no output just costs one extra turn.
    fn wants_event_loop(&self) -> bool {
        if let Some(done) = &self.draining {
            return !done.get();
        }
        self.submitted.get().saturating_sub(self.shared.delivered.get()) >= VIDEO_IN_FLIGHT
    }

    fn signal_end_of_stream(&mut self) {
        if self.draining.is_some() {
            return;
        }
        self.draining = Some(start_flush(self.decoder.as_ref().map(|d| d.flush())));
    }

    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        self.check_error()?;
        Ok(self.shared.ready.borrow_mut().pop_front())
    }

    fn flush(&mut self) -> Result<(), DecoderError> {
        if let Some(d) = &self.decoder {
            // Emits every pending output through the callback; the next
            // chunk after a flush must be a key frame again.
            let _ = d.flush();
            self.await_key = true;
        }
        Ok(())
    }
}

impl Drop for WebCodecsVideoDecoder {
    fn drop(&mut self) {
        if let Some(d) = self.decoder.take() {
            if d.state() != web_sys::CodecState::Closed {
                let _ = d.close();
            }
        }
        for f in self.shared.pending.borrow_mut().drain(..) {
            f.close();
        }
        log::debug!("[webcodecs] video decoder closed after {} frames", self.shared.frames_out.get());
    }
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

struct AudioShared {
    ready: RefCell<VecDeque<DecodedAudioFrame>>,
    error: RefCell<Option<String>>,
    output_rate: Cell<u32>,
    frames_out: Cell<u64>,
    /// Built on the first output (that is when the real input rate is
    /// known); keeps its phase across AUs so the output frame count is exact.
    resampler: RefCell<Option<LinearResampler>>,
}

impl AudioShared {
    fn fail(&self, msg: String) {
        log::error!("[webcodecs] audio: {msg}");
        let mut e = self.error.borrow_mut();
        if e.is_none() {
            *e = Some(msg);
        }
    }
}

pub struct WebCodecsAudioDecoder {
    decoder: Option<web_sys::AudioDecoder>,
    shared: Rc<AudioShared>,
    /// Chunks handed to `decode()`; with `shared.frames_out` (outputs
    /// delivered) gives the in-flight count that paces the decode loop.
    submitted: Cell<u64>,
    /// See the video decoder's `draining`.
    draining: Option<Rc<Cell<bool>>>,
    _output_cb: Option<Closure<dyn FnMut(web_sys::AudioData)>>,
    _error_cb: Option<Closure<dyn FnMut(JsValue)>>,
}

// Single thread (see module docs).
unsafe impl Send for WebCodecsAudioDecoder {}

impl WebCodecsAudioDecoder {
    pub fn new() -> Self {
        Self {
            decoder: None,
            shared: Rc::new(AudioShared {
                ready: RefCell::new(VecDeque::new()),
                error: RefCell::new(None),
                output_rate: Cell::new(48_000),
                frames_out: Cell::new(0),
                resampler: RefCell::new(None),
            }),
            submitted: Cell::new(0),
            draining: None,
            _output_cb: None,
            _error_cb: None,
        }
    }

    fn check_error(&self) -> Result<(), DecoderError> {
        match self.shared.error.borrow().as_ref() {
            Some(e) => Err(e.clone().into()),
            None => Ok(()),
        }
    }
}

impl Default for WebCodecsAudioDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// WebCodecs codec string for the track. AAC carries its audio object type
/// in the first 5 bits of the AudioSpecificConfig (`2` = LC, `5` = HE-AAC,
/// `29` = HE-AACv2).
fn audio_codec_string(codec: AudioCodec, asc: &[u8]) -> String {
    match codec {
        AudioCodec::Aac => {
            let aot = asc.first().map(|b| b >> 3).filter(|&t| t != 0 && t != 31).unwrap_or(2);
            format!("mp4a.40.{aot}")
        }
        AudioCodec::Ac3 => "ac-3".to_string(),
        AudioCodec::Eac3 => "ec-3".to_string(),
    }
}

fn on_audio_output(shared: &AudioShared, data: web_sys::AudioData) {
    let channels = data.number_of_channels() as usize;
    let frames = data.number_of_frames() as usize;
    let rate = data.sample_rate().round() as u32;
    let pts_ms = (data.timestamp() / 1000.0) as i64;
    let n = shared.frames_out.get();
    shared.frames_out.set(n + 1);
    if n < 3 || n % 200 == 0 {
        log::debug!(
            "[webcodecs] audio out #{n}: {frames} frames {channels}ch {rate}Hz ts={pts_ms}ms dur={:.2}ms fmt={:?}",
            data.duration() / 1000.0,
            data.format()
        );
    }
    let mut planes: Vec<Vec<f32>> = Vec::with_capacity(channels);
    for c in 0..channels {
        // Ask for f32-planar explicitly: every implementation must support
        // converting to it, whatever the decoder's native layout.
        let opts = web_sys::AudioDataCopyToOptions::new(c as u32);
        opts.set_format(web_sys::AudioSampleFormat::F32Planar);
        let js = Float32Array::new_with_length(frames as u32);
        if let Err(e) = data.copy_to_with_buffer_source(&js, &opts) {
            data.close();
            shared.fail(format!("AudioData.copyTo: {}", describe(&e)));
            return;
        }
        let mut v = vec![0f32; frames];
        js.copy_to(&mut v);
        planes.push(v);
    }
    data.close();
    let interleaved = interleave(&planes);
    let stereo = downmix_to_stereo(&interleaved, channels);
    let mut rs = shared.resampler.borrow_mut();
    let resampler = rs.get_or_insert_with(|| LinearResampler::new(2, rate, shared.output_rate.get()));
    let samples = resampler.process(&stereo);
    shared.ready.borrow_mut().push_back(DecodedAudioFrame { pts_ms, samples });
}

impl AudioDecoder for WebCodecsAudioDecoder {
    fn configure(&mut self, params: AudioDecoderParams) -> Result<(), DecoderError> {
        let codec = audio_codec_string(params.codec, &params.codec_specific_data);
        self.shared.output_rate.set(params.output_sample_rate.max(1));
        *self.shared.resampler.borrow_mut() = None;

        let shared = Rc::clone(&self.shared);
        let output_cb = Closure::<dyn FnMut(web_sys::AudioData)>::new(move |data| {
            on_audio_output(&shared, data);
        });
        let shared = Rc::clone(&self.shared);
        let error_cb = Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
            shared.fail(format!("decoder error: {}", describe(&e)));
        });

        let init = web_sys::AudioDecoderInit::new(
            error_cb.as_ref().unchecked_ref(),
            output_cb.as_ref().unchecked_ref(),
        );
        let decoder = web_sys::AudioDecoder::new(&init).map_err(|e| js_err("AudioDecoder::new", e))?;
        let config = web_sys::AudioDecoderConfig::new(
            &codec,
            params.input_channels.max(1) as u32,
            params.input_sample_rate.max(1),
        );
        if matches!(params.codec, AudioCodec::Aac) && !params.codec_specific_data.is_empty() {
            config.set_description_u8_array(&Uint8Array::from(&params.codec_specific_data[..]));
        }
        decoder
            .configure(&config)
            .map_err(|e| js_err(&format!("AudioDecoder::configure({codec})"), e))?;
        log::info!(
            "[webcodecs] audio configured: {} {}Hz {}ch → {}Hz stereo",
            codec,
            params.input_sample_rate,
            params.input_channels,
            params.output_sample_rate
        );

        self.decoder = Some(decoder);
        self._output_cb = Some(output_cb);
        self._error_cb = Some(error_cb);
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        self.check_error()?;
        let decoder = self
            .decoder
            .as_ref()
            .ok_or_else(|| -> DecoderError { "webcodecs: submit before configure".into() })?;
        let data = Uint8Array::from(sample);
        let init = web_sys::EncodedAudioChunkInit::new(&data, 0, web_sys::EncodedAudioChunkType::Key);
        init.set_timestamp_f64(pts_us as f64);
        let chunk = web_sys::EncodedAudioChunk::new(&init).map_err(|e| js_err("EncodedAudioChunk", e))?;
        decoder.decode(&chunk).map_err(|e| js_err("AudioDecoder::decode", e))?;
        self.submitted.set(self.submitted.get() + 1);
        Ok(())
    }

    /// Yield when more than [`AUDIO_IN_FLIGHT`] AUs are submitted but not
    /// yet delivered back. Measured failure modes this rules out: a turn per
    /// AU cannot keep up with ~100 AUs/s when each turn costs 5–15 ms of
    /// other queued work (~75 % throughput, starving the output); and
    /// feeding thousands of AUs without a turn makes the decoder stop
    /// delivering output altogether.
    fn wants_event_loop(&self) -> bool {
        if let Some(done) = &self.draining {
            return !done.get();
        }
        self.submitted.get().saturating_sub(self.shared.frames_out.get()) >= AUDIO_IN_FLIGHT
    }

    fn signal_end_of_stream(&mut self) {
        if self.draining.is_some() {
            return;
        }
        self.draining = Some(start_flush(self.decoder.as_ref().map(|d| d.flush())));
    }

    fn try_recv(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        self.check_error()?;
        Ok(self.shared.ready.borrow_mut().pop_front())
    }

    fn flush(&mut self) -> Result<(), DecoderError> {
        if let Some(d) = &self.decoder {
            let _ = d.flush();
        }
        Ok(())
    }
}

impl Drop for WebCodecsAudioDecoder {
    fn drop(&mut self) {
        if let Some(d) = self.decoder.take() {
            if d.state() != web_sys::CodecState::Closed {
                let _ = d.close();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_string_main_profile() {
        // configurationVersion=1, profile_space=0 tier=0 profile_idc=1,
        // compat flags 0x60000000, constraints 90 00 00 00 00 00, level 120.
        let mut hvcc = vec![1u8, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 120];
        hvcc.resize(23, 0);
        hvcc[21] = 0x0f; // lengthSizeMinusOne = 3
        assert_eq!(hevc_codec_string(&hvcc).as_deref(), Some("hvc1.1.6.L120.90"));
        assert_eq!(nal_length_size(&hvcc), 4);
    }

    #[test]
    fn irap_detection_walks_length_prefixed_nals() {
        // 4-byte length, one AUD (type 35) then one IDR_W_RADL (type 19).
        let aud = [0u8, 0, 0, 2, 35 << 1, 0x01];
        let idr = [0u8, 0, 0, 3, 19 << 1, 0x01, 0xAF];
        let mut sample = aud.to_vec();
        assert!(!is_irap_sample(&sample, 4));
        sample.extend_from_slice(&idr);
        assert!(is_irap_sample(&sample, 4));
    }
}
