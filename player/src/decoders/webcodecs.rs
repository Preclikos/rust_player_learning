//! Browser decoders on WebCodecs (`VideoDecoder` / `AudioDecoder`).
//!
//! Both are callback-driven: the browser hands decoded output to a JS
//! closure whenever its event loop runs. The engine's decode loop yields to
//! that loop after every `submit` (`rt::cooperative_yield`) and pulls the
//! results out with `try_recv`, so the `HwVideoDecoder` / `AudioDecoder`
//! contracts stay the pull-based shape every other platform implements.
//!
//! Video output stays on the GPU: the `VideoFrame` is handed downstream as
//! [`WebVideoFrame`] and the renderer copies it GPU→GPU with
//! `copyExternalImageToTexture` — the browser converts Y'CbCr → R'G'B' on
//! the GPU, no CPU byte per pixel. Frames are closed when the renderer (or a
//! LATE drop) is done with them; the in-flight pacing below keeps the
//! browser's decoder pool from filling up.
//!
//! HDR frames take the same path and get the BROWSER's tone-map: measured in
//! Chrome, both `copyExternalImageToTexture` and `importExternalTexture`
//! hand over already tone-mapped values whatever the canvas mode, and
//! hardware-decoded frames are opaque (`format` null) so their planes can't
//! be read out either. The web shell therefore plays the SDR ladder by
//! default (see platform/web).
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
    AudioCodec, AudioDecoder, AudioDecoderParams, DecodedAudioFrame, DecodedVideoFrame,
    DecoderError, HwVideoDecoder, PlatformFrame, VideoColorInfo, VideoDecoderParams,
    WebVideoFrame,
};
use crate::parsers::hevc::{hevc_codec_string, hvcc_nal_length_size, is_irap_sample};

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
    /// Frames ready for `try_recv`, in output order.
    ready: RefCell<VecDeque<DecodedVideoFrame>>,
    /// First error reported by the decoder; surfaced on the next
    /// `submit` / `try_recv` so the pipeline restarts.
    error: RefCell<Option<String>>,
    color: Cell<VideoColorInfo>,
    /// Frames the browser has handed to the output callback.
    delivered: Cell<u64>,
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
                error: RefCell::new(None),
                color: Cell::new(VideoColorInfo::default()),
                delivered: Cell::new(0),
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

/// Output callback: wrap the GPU-resident frame and queue it for `try_recv`.
fn on_video_output(shared: &Rc<VideoShared>, frame: web_sys::VideoFrame) {
    shared.delivered.set(shared.delivered.get() + 1);
    let pts_us = frame.timestamp() as i64;
    let wrapped = WebVideoFrame::new(frame);
    let (width, height) = (wrapped.width, wrapped.height);
    shared.ready.borrow_mut().push_back(DecodedVideoFrame {
        pts_us,
        width,
        height,
        native: PlatformFrame::WebVideoFrame(wrapped),
        desired_present_ns: 0,
        color: shared.color.get(),
        hdr_meta: None,
    });
}

impl HwVideoDecoder for WebCodecsVideoDecoder {
    fn name(&self) -> &'static str {
        "WebCodecs HEVC"
    }

    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError> {
        let record = &params.decoder_config_record;
        let codec = hevc_codec_string(record)
            .ok_or_else(|| -> DecoderError { "webcodecs: no hvcC record in init segment".into() })?;
        self.nal_len_size = hvcc_nal_length_size(record);
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
        log::debug!("[webcodecs] video decoder closed after {} frames", self.shared.delivered.get());
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
