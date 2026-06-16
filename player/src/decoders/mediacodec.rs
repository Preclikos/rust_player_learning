// Android MediaCodec-based HwVideoDecoder. Zero-copy via:
//
//   AMediaCodec
//     ── decoded output ──▶ Surface (AImageReader::window())
//                              │
//                              ▼
//                          AImageReader::acquire_latest_image()
//                              │
//                              ▼
//                          AImage::hardware_buffer()  →  AHardwareBuffer
//
// The AHB is then imported into Vulkan as VkImage via
// VK_ANDROID_external_memory_android_hardware_buffer, and the resulting
// VkImage becomes a `wgpu::Texture` (same pattern as `video_vaapi.rs`'s
// DMA-BUF path and `video_directx.rs`'s D3D11 shared-handle path).
//
// The CPU never touches the pixels; this is what makes 4K HEVC playback
// viable on Android.

#![cfg(target_os = "android")]

use std::sync::Arc;
use std::time::Duration;

use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;

use crate::parsers::hevc;
use crate::parsers::mp4::parse_hevc_nalu;

use super::{
    AndroidHardwareBufferFrame, DecodedVideoFrame, DecoderError, HdrFrameMeta, HwVideoDecoder,
    PlatformFrame, SendableAhb, VideoCodec, VideoDecoderParams,
};

pub struct MediaCodecDecoder {
    // ImageReader owns the consumer surface. It's behind Arc so we can
    // hold a stable reference for the lifetime of the codec — MediaCodec
    // keeps the Surface internally too via its NativeWindow.
    reader: Option<Arc<ImageReader>>,
    codec: Option<MediaCodec>,
    /// Direct mode (raw FFI codec rendering into the app's video Surface).
    /// Mutually exclusive with `codec`/`reader`.
    direct: Option<Arc<SharedDirectCodec>>,
    /// `ANativeWindow*` (as usize) for direct mode; 0 = ImageReader mode.
    direct_window: usize,
    width: u32,
    height: u32,
    last_decoded_pts_us: i64,
    decoded_frame_idx: u64,
    /// Stamped onto every decoded frame (from configure params).
    color: crate::decoders::VideoColorInfo,
    /// HDR10+ dynamic metadata parsed from the SEI of submitted samples,
    /// keyed by the sample's pts so it can be re-attached to the decoded
    /// frame on output (MediaCodec doesn't carry SEI through the Surface
    /// path). Bounded — see submit().
    pending_hdr_meta: std::collections::BTreeMap<i64, HdrFrameMeta>,
    /// Static mastering-display / MaxCLL metadata (usually only on IDR
    /// SEIs) — the fallback for frames without a dynamic entry.
    static_hdr_meta: Option<HdrFrameMeta>,
    /// Last MaxCLL (content light level) seen, persisted across frames. Some
    /// streams carry MaxCLL and the mastering-display SEI on ALTERNATING
    /// frames, so recomputing peak = MaxCLL.or(mastering) per frame made it
    /// flip-flop (and re-log). Once MaxCLL is seen we keep it — it's the
    /// tonemap-relevant peak — and only fall back to mastering until then.
    seen_max_cll_nits: Option<f32>,
    /// Keep Dolby Vision RPU/EL NALs in the bitstream (platform DV
    /// decoder configured — it needs them). False = strip them (plain
    /// HEVC decoders may choke on unspecified NAL types).
    keep_dv_nalus: bool,
    /// Pipeline stop signal (direct mode). The `submit_direct` input-buffer
    /// spin checks it so a teardown (seek/track-switch) doesn't leave the
    /// decode task wedged in the spin when the codec is being torn down.
    stop_signal: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

// ---------------------------------------------------------------------------
// Direct mode: MediaCodec renders straight into the host's video Surface.
// Raw ndk_sys FFI — the safe `ndk` wrapper hides the output-buffer index,
// which is exactly what the deferred timed release needs.
// ---------------------------------------------------------------------------

/// [C2] Serializes direct codecs on the shared video Surface: only ONE codec
/// may own the `ANativeWindow` producer connection at a time. A new codec must
/// wait until the previous one's `AMediaCodec_delete` (in
/// `SharedDirectCodec::drop`) has released the connection — otherwise it
/// configures into a half-released Surface and emits NO output (`produced=0`),
/// which stalls forever (`video_ready` never fires → av_sync blocks). Held for
/// the codec's whole lifetime; released in Drop.
static DIRECT_WINDOW_BUSY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The codec shared between the decoder (input/dequeue side) and in-flight
/// frames (release side). AMediaCodec is not documented thread-safe, so
/// every call goes through `call_lock`; each call is microseconds.
pub struct SharedDirectCodec {
    raw: *mut ndk_sys::AMediaCodec,
    call_lock: std::sync::Mutex<()>,
    /// Set before stop() during teardown — release handles become no-ops
    /// (their buffer indices died with the stop).
    stopped: std::sync::atomic::AtomicBool,
}

unsafe impl Send for SharedDirectCodec {}
unsafe impl Sync for SharedDirectCodec {}

impl SharedDirectCodec {
    /// Queue the buffer to the video Surface for display at `present_ns`
    /// (CLOCK_MONOTONIC). `present_ns <= now` displays ASAP.
    fn release_at(&self, index: usize, present_ns: i64) {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let _l = self.call_lock.lock().unwrap();
        let st = unsafe {
            ndk_sys::AMediaCodec_releaseOutputBufferAtTime(self.raw, index, present_ns)
        };
        if st != ndk_sys::media_status_t::AMEDIA_OK {
            log::trace!("[mc-direct] releaseAtTime({}) -> {:?}", index, st);
        }
    }

    /// Return the buffer to the codec without rendering (dropped frame).
    fn release_drop(&self, index: usize) {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let _l = self.call_lock.lock().unwrap();
        let _ = unsafe { ndk_sys::AMediaCodec_releaseOutputBuffer(self.raw, index, false) };
    }

    /// Teardown: invalidate all outstanding release handles, then stop.
    fn stop(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::Release);
        let _l = self.call_lock.lock().unwrap();
        unsafe {
            ndk_sys::AMediaCodec_stop(self.raw);
        }
    }
}

impl Drop for SharedDirectCodec {
    fn drop(&mut self) {
        // Last reference (decoder + every in-flight frame) gone.
        unsafe {
            ndk_sys::AMediaCodec_delete(self.raw);
        }
        // [C2] The Surface producer connection is now released (delete is
        // synchronous) — let the next direct codec configure onto it.
        DIRECT_WINDOW_BUSY.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// A decoded frame living inside the codec: the renderer releases it to
/// the video Surface with a presentation timestamp; dropping it unrendered
/// hands the buffer back to the codec (LATE drains, teardown).
pub struct DirectVideoFrame {
    codec: Arc<SharedDirectCodec>,
    index: usize,
    released: std::sync::atomic::AtomicBool,
}

impl DirectVideoFrame {
    /// Queue for display at `present_ns` (CLOCK_MONOTONIC nanoseconds;
    /// values in the past display at the next vsync).
    pub fn render_at(&self, present_ns: i64) {
        if !self.released.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.codec.release_at(self.index, present_ns);
        }
    }
}

impl Drop for DirectVideoFrame {
    fn drop(&mut self) {
        if !self.released.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.codec.release_drop(self.index);
        }
    }
}

// MediaCodec/ImageReader wrap NonNull pointers and aren't auto-Send.
// Single-threaded ownership inside the decoder task is fine.
unsafe impl Send for MediaCodecDecoder {}

impl MediaCodecDecoder {
    pub fn new() -> Self {
        Self {
            reader: None,
            codec: None,
            direct: None,
            direct_window: 0,
            width: 0,
            height: 0,
            last_decoded_pts_us: -1,
            decoded_frame_idx: 0,
            color: Default::default(),
            pending_hdr_meta: Default::default(),
            static_hdr_meta: None,
            seen_max_cll_nits: None,
            keep_dv_nalus: false,
            stop_signal: None,
        }
    }

    /// Resolve the platform decoder NAME for (mime, profile) via the Java
    /// `MediaCodecList.findDecoderForFormat` — the NDK has no codec
    /// enumeration, and `createDecoderByType` just takes the FIRST decoder
    /// registered for the mime (on MTK that picks the AVC-based
    /// `dvav.ser` for video/dolby-vision even for HEVC profiles). Returns
    /// None on any JNI trouble; caller falls back to createDecoderByType.
    fn find_decoder_name(mime: &str, profile: i32, width: i32, height: i32) -> Option<String> {
        let ctx = ndk_context::android_context();
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) };
        // jni 0.22 dropped the AttachGuard-returning attach in favour of a
        // scoped callback handed a `&mut Env`. All the calls below return
        // jni::errors::Error, so we `?` inside and collapse the
        // Result<Option<_>> back to an Option for the caller (any JNI trouble
        // → None → caller falls back to createDecoderByType).
        // jni 0.22 also requires class/method names and signatures to be
        // null-terminated JNIStr/MethodSignature (built at compile time by the
        // jni_str!/jni_sig! macros) instead of plain &str, and JObject ->
        // JString goes through env.cast_local rather than a From impl.
        vm.attach_current_thread(|env| -> Result<Option<String>, jni::errors::Error> {
            let jmime = env.new_string(mime)?;
            let fmt = env
                .call_static_method(
                    jni::jni_str!("android/media/MediaFormat"),
                    jni::jni_str!("createVideoFormat"),
                    jni::jni_sig!("(Ljava/lang/String;II)Landroid/media/MediaFormat;"),
                    &[(&jmime).into(), width.into(), height.into()],
                )?
                .l()?;
            let kprofile = env.new_string("profile")?;
            env.call_method(
                &fmt,
                jni::jni_str!("setInteger"),
                jni::jni_sig!("(Ljava/lang/String;I)V"),
                &[(&kprofile).into(), profile.into()],
            )?;

            // MediaCodecList(MediaCodecList.ALL_CODECS = 1)
            let list = env.new_object(
                jni::jni_str!("android/media/MediaCodecList"),
                jni::jni_sig!("(I)V"),
                &[1i32.into()],
            )?;
            let name = env
                .call_method(
                    &list,
                    jni::jni_str!("findDecoderForFormat"),
                    jni::jni_sig!("(Landroid/media/MediaFormat;)Ljava/lang/String;"),
                    &[(&fmt).into()],
                )?
                .l()?;
            if name.is_null() {
                return Ok(None);
            }
            let jstr = env.cast_local::<jni::objects::JString>(name)?;
            let s: String = env.get_string(&jstr)?.into();
            Ok(Some(s))
        })
        .ok()
        .flatten()
    }

    /// Direct-mode configure: raw FFI codec attached to the video Surface.
    fn configure_direct(&mut self, params: &VideoDecoderParams, mime: &str) -> Result<(), DecoderError> {
        use std::ffi::CString;
        unsafe {
            let mime_c = CString::new(mime).unwrap();
            // Profile-aware decoder resolution (Java MediaCodecList) with
            // createDecoderByType as the fallback. Essential for DV, where
            // several profile-specific decoders register the same mime.
            let profile_const: i32 = match params.dovi_profile {
                Some(4) => 0x10,
                Some(5) => 0x20,
                Some(6) => 0x40,
                Some(7) => 0x80,
                Some(8) => 0x100,
                Some(9) => 0x200,
                _ => 0,
            };
            let resolved = if mime == "video/dolby-vision" && profile_const != 0 {
                Self::find_decoder_name(
                    mime,
                    profile_const,
                    params.width as i32,
                    params.height as i32,
                )
            } else {
                None
            };
            let codec = match &resolved {
                Some(name) => {
                    log::info!("MediaCodecDecoder: resolved decoder {} for {} profile {:?}",
                        name, mime, params.dovi_profile);
                    let name_c = CString::new(name.as_str()).unwrap();
                    ndk_sys::AMediaCodec_createCodecByName(name_c.as_ptr())
                }
                None => ndk_sys::AMediaCodec_createDecoderByType(mime_c.as_ptr()),
            };
            if codec.is_null() {
                return Err(format!(
                    "decoder creation failed (mime {}, resolved {:?})",
                    mime, resolved
                )
                .into());
            }

            let format = ndk_sys::AMediaFormat_new();
            let key_mime = CString::new("mime").unwrap();
            ndk_sys::AMediaFormat_setString(format, key_mime.as_ptr(), mime_c.as_ptr());
            let key_w = CString::new("width").unwrap();
            let key_h = CString::new("height").unwrap();
            ndk_sys::AMediaFormat_setInt32(format, key_w.as_ptr(), params.width as i32);
            ndk_sys::AMediaFormat_setInt32(format, key_h.as_ptr(), params.height as i32);
            if mime == "video/dolby-vision" {
                // MediaCodecInfo.CodecProfileLevel DolbyVisionProfile*
                // constants — DV decoders key their mode off this.
                let profile_const: i32 = match params.dovi_profile {
                    Some(4) => 0x10,  // DvheDtr
                    Some(5) => 0x20,  // DvheStn
                    Some(6) => 0x40,  // DvheDth
                    Some(7) => 0x80,  // DvheDtb
                    Some(8) => 0x100, // DvheSt
                    Some(9) => 0x200, // DvavSe
                    _ => 0x100,
                };
                let key_profile = CString::new("profile").unwrap();
                ndk_sys::AMediaFormat_setInt32(format, key_profile.as_ptr(), profile_const);
            }
            if !params.hvcc_nalus.is_empty() {
                let mut csd = Vec::new();
                for n in &params.hvcc_nalus {
                    csd.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                    csd.extend_from_slice(n);
                }
                let key_csd = CString::new("csd-0").unwrap();
                ndk_sys::AMediaFormat_setBuffer(
                    format,
                    key_csd.as_ptr(),
                    csd.as_ptr() as *const _,
                    csd.len(),
                );
            }

            // [C2] Claim the shared Surface before connecting our producer to
            // it (AMediaCodec_configure binds the codec to the window). Wait
            // for any prior direct codec to finish its AMediaCodec_delete
            // (which clears the flag in Drop) so we never configure into a
            // half-released Surface. Bounded — a leaked frame ref can't wedge
            // configure forever; the produced=0 watchdog then recovers.
            {
                use std::sync::atomic::Ordering;
                let mut waited = 0u32;
                while DIRECT_WINDOW_BUSY
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    if waited >= 500 {
                        log::warn!(
                            "[mc-direct] prior codec still owns the Surface after ~1s; configuring anyway"
                        );
                        DIRECT_WINDOW_BUSY.store(true, Ordering::Release);
                        break;
                    }
                    waited += 1;
                    std::thread::sleep(Duration::from_millis(2));
                }
            }

            let st = ndk_sys::AMediaCodec_configure(
                codec,
                format,
                self.direct_window as *mut ndk_sys::ANativeWindow,
                std::ptr::null_mut(),
                0,
            );
            ndk_sys::AMediaFormat_delete(format);
            if st != ndk_sys::media_status_t::AMEDIA_OK {
                ndk_sys::AMediaCodec_delete(codec);
                // No SharedDirectCodec will be created to release the gate.
                DIRECT_WINDOW_BUSY.store(false, std::sync::atomic::Ordering::Release);
                return Err(format!("AMediaCodec_configure(direct): {:?}", st).into());
            }
            let st = ndk_sys::AMediaCodec_start(codec);
            if st != ndk_sys::media_status_t::AMEDIA_OK {
                ndk_sys::AMediaCodec_delete(codec);
                DIRECT_WINDOW_BUSY.store(false, std::sync::atomic::Ordering::Release);
                return Err(format!("AMediaCodec_start(direct): {:?}", st).into());
            }

            self.direct = Some(Arc::new(SharedDirectCodec {
                raw: codec,
                call_lock: std::sync::Mutex::new(()),
                stopped: std::sync::atomic::AtomicBool::new(false),
            }));
        }
        log::info!(
            "MediaCodecDecoder: configured {} DIRECT to video surface, {}x{}",
            mime,
            params.width,
            params.height
        );
        Ok(())
    }

    /// Direct-mode submit: same Annex-B conversion, raw FFI input queue.
    fn submit_direct(&mut self, annex_b: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        // Frames the codec has emitted so far this session. Frozen during the
        // dequeue_input spin (try_recv runs on the same task), so logging it at
        // a stall tells us whether the codec ever produced output: 0 after a
        // seek/start-at-offset == the codec accepts input but emits nothing
        // (lifecycle/Surface issue), vs >0 == it flowed then backed up.
        let produced = self.decoded_frame_idx;
        let direct = self.direct.as_ref().unwrap();
        unsafe {
            let idx = {
                let mut retries = 0u32;
                loop {
                    // [B] Bail promptly on teardown (seek/track-switch) instead
                    // of spinning a codec that's being torn down — otherwise
                    // this blocking spin strands the decode task and the
                    // pipeline rebuild can't join it. The supervisor treats an
                    // error while stop_flag is set as a clean teardown.
                    if let Some(stop) = &self.stop_signal {
                        if stop.load(std::sync::atomic::Ordering::Relaxed) {
                            return Err("submit_direct: stop signalled".into());
                        }
                    }
                    let idx = {
                        let _l = direct.call_lock.lock().unwrap();
                        ndk_sys::AMediaCodec_dequeueInputBuffer(direct.raw, 5_000)
                    };
                    if idx >= 0 {
                        break idx as usize;
                    }
                    if idx != -1 {
                        // Anything but AMEDIACODEC_INFO_TRY_AGAIN_LATER (-1).
                        return Err(format!("dequeueInputBuffer(direct): {}", idx).into());
                    }
                    retries += 1;
                    // [A] produced==0 watchdog: a freshly (re)configured codec
                    // that accepts no more input AND has never emitted output is
                    // wedged (the lifecycle/Surface race — produced=0). Bail
                    // (~2s) so the supervisor rebuilds the pipeline rather than
                    // spinning forever. produced>0 is ordinary backpressure (it
                    // recovers once the renderer drains output buffers) → keep
                    // waiting; don't false-trip on it.
                    if produced == 0 && retries >= 400 {
                        return Err(format!(
                            "submit_direct: codec produced no output {}ms after configure (wedged Surface)",
                            retries * 5
                        )
                        .into());
                    }
                    if retries % 200 == 0 {
                        log::warn!(
                            "[mc-direct] dequeue_input stall {}x5ms pts={} produced={} (produced=0 => codec emits no output after this seek/start)",
                            retries,
                            pts_us / 1000,
                            produced
                        );
                    }
                    std::thread::yield_now();
                }
            };
            let mut cap: usize = 0;
            let buf = {
                let _l = direct.call_lock.lock().unwrap();
                ndk_sys::AMediaCodec_getInputBuffer(direct.raw, idx, &mut cap)
            };
            if buf.is_null() {
                return Err("getInputBuffer(direct) returned null".into());
            }
            let len = annex_b.len().min(cap);
            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), buf, len);
            let st = {
                let _l = direct.call_lock.lock().unwrap();
                ndk_sys::AMediaCodec_queueInputBuffer(direct.raw, idx, 0, len, pts_us as u64, 0)
            };
            if st != ndk_sys::media_status_t::AMEDIA_OK {
                return Err(format!("queueInputBuffer(direct): {:?}", st).into());
            }
        }
        Ok(())
    }

    /// Direct-mode try_recv: dequeue an output buffer and wrap its index in
    /// a deferred-release frame.
    fn try_recv_direct(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let direct = Arc::clone(self.direct.as_ref().unwrap());
        unsafe {
            let mut info: ndk_sys::AMediaCodecBufferInfo = std::mem::zeroed();
            let idx = {
                let _l = direct.call_lock.lock().unwrap();
                ndk_sys::AMediaCodec_dequeueOutputBuffer(direct.raw, &mut info, 0)
            };
            if idx >= 0 {
                let pts_us = info.presentationTimeUs;
                let frame_idx = self.decoded_frame_idx;
                if self.last_decoded_pts_us >= 0 && pts_us < self.last_decoded_pts_us {
                    log::warn!(
                        "[mc-direct] BACKWARD #{} pts={}ms last={}ms",
                        frame_idx,
                        pts_us / 1000,
                        self.last_decoded_pts_us / 1000
                    );
                }
                self.last_decoded_pts_us = pts_us;
                self.decoded_frame_idx += 1;
                let hdr_meta = self
                    .pending_hdr_meta
                    .remove(&pts_us)
                    .or(self.static_hdr_meta);
                return Ok(Some(DecodedVideoFrame {
                    pts_us,
                    width: self.width,
                    height: self.height,
                    native: PlatformFrame::MediaCodecDirect(DirectVideoFrame {
                        codec: direct,
                        index: idx as usize,
                        released: std::sync::atomic::AtomicBool::new(false),
                    }),
                    desired_present_ns: 0,
                    color: self.color,
                    hdr_meta,
                }));
            }
            // Negative status codes: ndk_sys exposes them as i32 constants.
            const TRY_AGAIN: isize = -1; // AMEDIACODEC_INFO_TRY_AGAIN_LATER
            const FORMAT_CHANGED: isize = -2; // AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED
            const BUFFERS_CHANGED: isize = -3; // AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED
            match idx {
                TRY_AGAIN | BUFFERS_CHANGED => Ok(None),
                FORMAT_CHANGED => {
                    let fmt = {
                        let _l = direct.call_lock.lock().unwrap();
                        ndk_sys::AMediaCodec_getOutputFormat(direct.raw)
                    };
                    if !fmt.is_null() {
                        // Prefer the crop rect (true content size; NDK key
                        // "crop" as an AMediaFormat Rect — the Java-style
                        // crop-left/right int keys are absent here). The
                        // codec scales the crop to the surface, so this is
                        // what the aspect logic needs.
                        let crop_key = std::ffi::CString::new("crop").unwrap();
                        let (mut l, mut t, mut r, mut b) = (0i32, 0i32, 0i32, 0i32);
                        let (w, h) = if ndk_sys::AMediaFormat_getRect(
                            fmt,
                            crop_key.as_ptr(),
                            &mut l,
                            &mut t,
                            &mut r,
                            &mut b,
                        ) && r > l
                            && b > t
                        {
                            ((r - l + 1) as u32, (b - t + 1) as u32)
                        } else {
                            let get = |name: &str| -> Option<i32> {
                                let key = std::ffi::CString::new(name).unwrap();
                                let mut v: i32 = 0;
                                if ndk_sys::AMediaFormat_getInt32(fmt, key.as_ptr(), &mut v) {
                                    Some(v)
                                } else {
                                    None
                                }
                            };
                            (
                                get("width").unwrap_or(self.width as i32) as u32,
                                get("height").unwrap_or(self.height as i32) as u32,
                            )
                        };
                        self.width = w;
                        self.height = h;
                        log::info!("[mc-direct] output format: {}x{} (content)", w, h);
                        ndk_sys::AMediaFormat_delete(fmt);
                    }
                    Ok(None)
                }
                other => Err(format!("dequeueOutputBuffer(direct): {}", other).into()),
            }
        }
    }
}

impl HwVideoDecoder for MediaCodecDecoder {
    fn name(&self) -> &'static str {
        "MediaCodec"
    }

    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError> {
        let mime = match params.codec {
            VideoCodec::Hevc => "video/hevc",
            VideoCodec::H264 => "video/avc",
        };

        // Direct mode: the codec renders straight into the host's video
        // Surface (HW plane carries HDR/HDR10+/DV to the display) — no
        // ImageReader, no GL involvement for video pixels.
        self.direct_window = params.direct_window;
        if self.direct_window != 0 {
            self.width = params.width;
            self.height = params.height;
            self.color = params.color;
            // Dolby Vision: prefer the platform DV decoder — it consumes
            // the RPU NALs and reconstructs full DV in the OS pipeline.
            // Falls back to the HEVC base layer (profiles 7/8 only;
            // profile 5 has no compatible BL and must error out).
            if let Some(p) = params.dovi_profile {
                self.keep_dv_nalus = true;
                match self.configure_direct(&params, "video/dolby-vision") {
                    Ok(()) => {
                        log::info!("MediaCodecDecoder: platform Dolby Vision decoder (profile {})", p);
                        return Ok(());
                    }
                    Err(e) => {
                        self.keep_dv_nalus = false;
                        if matches!(p, 7 | 8) {
                            log::warn!(
                                "MediaCodecDecoder: video/dolby-vision unavailable ({}) — \
                                 falling back to the HEVC base layer",
                                e
                            );
                        } else {
                            return Err(format!(
                                "Dolby Vision profile {} needs the platform DV decoder, \
                                 which failed: {}",
                                p, e
                            )
                            .into());
                        }
                    }
                }
            }
            return self.configure_direct(&params, mime);
        }

        // 32 physical NV12 surfaces ≈ 1.33 s of buffer at 24 fps.
        // Segment boundaries cause a ~1.5 s decoder stall (MediaCodec IDR
        // warmup + segment parse). With 16 images the 667 ms buffer drained
        // before the stall ended, triggering LATE + an aggressive drain that
        // skipped 2.7 s of content — visible freeze-then-jump every 6 s.
        // 32 images nearly covers the stall; combined with proportional
        // frame drain in player.rs the recovery is smooth.
        // 64 caused a device-level SIGABRT on the MT8696 (PowerVR driver
        // has a hard limit on AImageReader max_images for YUV_420_888).
        // Memory cost: 32 × ~1.4 MB (720p NV12) ≈ 45 MB.
        // 10-bit P010 is 2× that per pixel; above 1440p the 32-deep pool
        // would cost >0.5 GB of gralloc memory, so cap it at 16 there
        // (4K stalls recover via the LATE drain instead of the deep pool).
        let max_images = if params.color.bit_depth > 8
            && params.width as u64 * params.height as u64 > 2560 * 1440
        {
            16
        } else {
            32
        };

        // Surface pixel format:
        //   8-bit  → YUV_420_888: defined NV12-ish layout (Y + interleaved
        //            CbCr) that both EGL and Vulkan import without
        //            VkSamplerYcbcrConversion.
        //   10-bit → PRIVATE first: the HAL picks the decoder's own native
        //            10-bit layout, so the codec can always write it and
        //            EGL can always import it (only opaque to CPU/Vulkan).
        //            YCBCR_P010 second — it allocates fine on devices
        //            whose decoder can't actually fill it (MT8696: the
        //            codec accepted the surface and then no image ever
        //            arrived in the reader), so it's the riskier option.
        //            YUV_420_888 last (8-bit downconvert — PQ tonemapping
        //            still applies, just with banding).
        // AIMAGE_FORMAT_YCBCR_P010 is missing from ndk 0.9's enum;
        // construct it through the num_enum catch-all.
        const AIMAGE_FORMAT_YCBCR_P010: i32 = 0x36;
        let gpu_only = HardwareBufferUsage::GPU_SAMPLED_IMAGE | HardwareBufferUsage::CPU_READ_NEVER;
        let mut reader: Option<ImageReader> = None;
        if params.color.bit_depth > 8 {
            for (fmt, name) in [
                (ImageFormat::PRIVATE, "PRIVATE"),
                (ImageFormat::from(AIMAGE_FORMAT_YCBCR_P010), "YCBCR_P010"),
            ] {
                match ImageReader::new_with_usage(
                    params.width as i32,
                    params.height as i32,
                    fmt,
                    gpu_only,
                    max_images,
                ) {
                    Ok(r) => {
                        log::info!("MediaCodecDecoder: 10-bit surface pool = {}", name);
                        reader = Some(r);
                        break;
                    }
                    Err(e) => {
                        log::warn!("MediaCodecDecoder: {} ImageReader failed ({:?})", name, e);
                    }
                }
            }
        }
        let reader = match reader {
            Some(r) => r,
            None => ImageReader::new(
                params.width as i32,
                params.height as i32,
                ImageFormat::YUV_420_888,
                max_images,
            )
            .map_err(|e| -> DecoderError { format!("ImageReader::new: {:?}", e).into() })?,
        };

        let window = reader
            .window()
            .map_err(|e| -> DecoderError { format!("ImageReader::window: {:?}", e).into() })?;

        let codec = MediaCodec::from_decoder_type(mime).ok_or_else(|| -> DecoderError {
            format!("MediaCodec::from_decoder_type({}) returned None", mime).into()
        })?;

        let mut format = MediaFormat::new();
        format.set_str("mime", mime);
        format.set_i32("width", params.width as i32);
        format.set_i32("height", params.height as i32);
        if !params.hvcc_nalus.is_empty() {
            // Build Annex-B csd-0: start-code prefix + raw NALU body for each NALU.
            let mut csd = Vec::new();
            for n in &params.hvcc_nalus {
                csd.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                csd.extend_from_slice(n);
            }
            format.set_buffer("csd-0", &csd);
        }

        codec
            .configure(&format, Some(&window), MediaCodecDirection::Decoder)
            .map_err(|e| -> DecoderError { format!("MediaCodec::configure: {:?}", e).into() })?;
        codec
            .start()
            .map_err(|e| -> DecoderError { format!("MediaCodec::start: {:?}", e).into() })?;

        self.reader = Some(Arc::new(reader));
        self.codec = Some(codec);
        self.width = params.width;
        self.height = params.height;
        self.color = params.color;
        log::info!(
            "MediaCodecDecoder: configured {}, {}x{}, surface output",
            mime,
            params.width,
            params.height
        );
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        if self.codec.is_none() && self.direct.is_none() {
            return Err("submit before configure".into());
        }

        // `sample` is length-prefixed NALU (raw mdat). Convert to Annex-B
        // (start-code prefixed) — MediaCodec expects this for HEVC/H.264.
        let nalus = parse_hevc_nalu(sample)
            .map_err(|e| -> DecoderError { format!("NALU parse: {}", e).into() })?;
        let mut annex_b = Vec::with_capacity(sample.len() + nalus.len() * 4);
        for n in nalus {
            // Harvest HDR metadata from prefix SEIs on the way in —
            // MediaCodec's Surface path doesn't surface SEI on output, so
            // pair it with the frame by pts (one access unit per submit).
            // Only bother for HDR streams; the parse is cheap but pointless
            // on the SDR ladder.
            // NALUs here carry the 4-byte start code prefix.
            let body = n.strip_prefix(&[0, 0, 0, 1][..]).unwrap_or(&n);
            let nal_type = hevc::nal_unit_type(body);
            // Dolby Vision RPU / enhancement-layer NALs: the platform
            // video/dolby-vision decoder NEEDS them (keep_dv_nalus), but a
            // plain video/hevc decoder doesn't know them — most ignore
            // unspecified NAL types, some vendor decoders throw. On the
            // base-layer fallback drop them at the door.
            if !self.keep_dv_nalus
                && matches!(nal_type, Some(hevc::NAL_DV_RPU) | Some(hevc::NAL_DV_EL))
            {
                continue;
            }
            if self.color.is_hdr() && nal_type == Some(hevc::NAL_SEI_PREFIX) {
                {
                    let meta = hevc::parse_sei_hdr_metadata(body);
                    if let Some(hp) = meta.hdr10plus {
                        if self.pending_hdr_meta.is_empty() && self.decoded_frame_idx == 0 {
                            log::info!(
                                "[mc] stream carries HDR10+ dynamic metadata (ST 2094-40) — \
                                 direct mode forwards it to the display"
                            );
                        }
                        self.pending_hdr_meta.insert(
                            pts_us,
                            HdrFrameMeta {
                                peak_nits: hp.max_scl_nits,
                                avg_nits: Some(hp.avg_maxrgb_nits),
                            },
                        );
                        // Bound the map: stale entries pile up when frames
                        // get dropped inside the codec (flush, errors).
                        // 128 ≈ 5 s at 24 fps, far beyond codec latency.
                        while self.pending_hdr_meta.len() > 128 {
                            let oldest = *self.pending_hdr_meta.keys().next().unwrap();
                            self.pending_hdr_meta.remove(&oldest);
                        }
                    }
                    if let Some(cll) = meta.static_info.max_cll_nits {
                        self.seen_max_cll_nits = Some(cll);
                    }
                    // MaxCLL once seen, else the mastering-display peak. Sticky
                    // so alternating-SEI streams don't flip the peak each frame.
                    let peak = self
                        .seen_max_cll_nits
                        .or(meta.static_info.mastering_peak_nits);
                    if let Some(peak_nits) = peak {
                        let new = HdrFrameMeta { peak_nits, avg_nits: None };
                        if self.static_hdr_meta != Some(new) {
                            log::info!(
                                "[mc] static HDR metadata: peak {} nits (MaxCLL {:?}, mastering {:?})",
                                peak_nits,
                                meta.static_info.max_cll_nits,
                                meta.static_info.mastering_peak_nits
                            );
                            self.static_hdr_meta = Some(new);
                        }
                    }
                }
            }
            annex_b.extend_from_slice(&n);
        }

        if self.direct.is_some() {
            return self.submit_direct(&annex_b, pts_us);
        }
        let codec = self.codec.as_ref().unwrap();

        // Retry until an input slot is free. Each wait is 5 ms; the codec
        // typically frees a slot within one frame interval (~33ms at 24fps).
        // If we stall here for a long time it means all output surfaces are
        // occupied (AHBs in the video channel), starving MediaCodec of buffers.
        let mut input_buf = {
            let mut retries = 0u32;
            loop {
                match codec
                    .dequeue_input_buffer(Duration::from_millis(5))
                    .map_err(|e| -> DecoderError { format!("dequeue_input_buffer: {:?}", e).into() })?
                {
                    DequeuedInputBufferResult::Buffer(b) => break b,
                    DequeuedInputBufferResult::TryAgainLater => {
                        retries += 1;
                        if retries % 20 == 0 {
                            log::warn!("[mc] dequeue_input stall {}x5ms={} ms pts={}", retries, retries * 5, pts_us / 1000);
                        }
                        std::thread::yield_now();
                    }
                }
            }
        };

        let dst = input_buf.buffer_mut();
        let copy_len = annex_b.len().min(dst.len());
        unsafe {
            std::ptr::copy_nonoverlapping(
                annex_b.as_ptr(),
                dst.as_mut_ptr() as *mut u8,
                copy_len,
            );
        }

        codec
            .queue_input_buffer(input_buf, 0, copy_len, pts_us as u64, 0)
            .map_err(|e| -> DecoderError { format!("queue_input_buffer: {:?}", e).into() })?;

        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        if self.direct.is_some() {
            return self.try_recv_direct();
        }
        let codec = self
            .codec
            .as_ref()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;
        let reader = self
            .reader
            .as_ref()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;

        // Drain the codec: dequeue one output buffer and release it to the
        // surface (render = true). This pushes the decoded frame into the
        // ImageReader's queue, where acquire_latest_image() can pick it up.
        let dequeued = codec
            .dequeue_output_buffer(Duration::ZERO)
            .map_err(|e| -> DecoderError { format!("dequeue_output_buffer: {:?}", e).into() })?;

        let pts_us = match dequeued {
            DequeuedOutputBufferInfoResult::Buffer(out) => {
                let pts = out.info().presentation_time_us();
                let idx = self.decoded_frame_idx;
                let delta = if self.last_decoded_pts_us >= 0 { pts - self.last_decoded_pts_us } else { 0 };
                if self.last_decoded_pts_us >= 0 && pts < self.last_decoded_pts_us {
                    log::warn!("[mc] BACKWARD #{} pts={}ms last={}ms Δ={}ms",
                        idx, pts / 1000, self.last_decoded_pts_us / 1000, delta / 1000);
                } else {
                    log::trace!("[mc] decoded #{} pts={}ms Δ={}ms",
                        idx, pts / 1000, delta / 1000);
                }
                self.last_decoded_pts_us = pts;
                self.decoded_frame_idx += 1;
                codec
                    .release_output_buffer(out, /* render = */ true)
                    .map_err(|e| -> DecoderError {
                        format!("release_output_buffer: {:?}", e).into()
                    })?;
                pts
            }
            DequeuedOutputBufferInfoResult::TryAgainLater => return Ok(None),
            DequeuedOutputBufferInfoResult::OutputFormatChanged => {
                let fmt = codec.output_format();
                if let (Some(w), Some(h)) = (fmt.i32("width"), fmt.i32("height")) {
                    self.width = w as u32;
                    self.height = h as u32;
                    log::info!("MediaCodec output format: {}x{}", w, h);
                }
                return Ok(None);
            }
            DequeuedOutputBufferInfoResult::OutputBuffersChanged => return Ok(None),
        };

        // Pull the freshly-rendered image off the surface in FIFO order.
        // release_output_buffer(render=true) is nearly synchronous for
        // ImageReader but may need a few retries. We MUST NOT return None
        // here after having consumed a MediaCodec output slot — doing so
        // leaves a frame stranded in ImageReader, and the next try_recv
        // would pair the next MediaCodec PTS with the stale image, causing
        // visible content to be displayed at the wrong timestamp.
        //
        // The wait is BOUNDED: on some devices a surface format that
        // allocated fine can still never be fed by the decoder (MT8696 +
        // YCBCR_P010: codec configures, reports the output format, and
        // then no image ever arrives) — an unbounded spin here hangs the
        // whole decoder task silently. 2 s is two orders of magnitude
        // above the normal latch latency; bail with a clear error so the
        // failure is visible in the log instead of a frozen black screen.
        let acquire_started = std::time::Instant::now();
        let mut acquire_retries = 0u32;
        let image = loop {
            match reader
                .acquire_next_image()
                .map_err(|e| -> DecoderError { format!("acquire_next_image: {:?}", e).into() })?
            {
                AcquireResult::Image(img) => break img,
                AcquireResult::MaxImagesAcquired => {
                    // All surfaces are held by AHB refs in the video channel.
                    // The sync producer (on another thread) must render+drop frames
                    // to free slots. Yield and retry — do NOT return None, which
                    // would strand this rendered frame and cause PTS/content mismatch.
                    acquire_retries += 1;
                    if acquire_retries % 100 == 0 {
                        log::warn!("[mc] MAX_IMAGES_ACQUIRED spin {}x pts={}", acquire_retries, pts_us / 1000);
                    }
                    std::thread::yield_now();
                }
                AcquireResult::NoBufferAvailable => {
                    if acquire_started.elapsed() > Duration::from_secs(2) {
                        return Err(format!(
                            "ImageReader produced no image for 2s after \
                             release_output_buffer (pts={}ms) — surface format \
                             unsupported by this decoder?",
                            pts_us / 1000
                        )
                        .into());
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        };

        // Get the unowned HardwareBuffer reference from the Image and acquire
        // a strong ref, but keep the Image itself inside the frame: deleting
        // the AImage returns the slot to the ImageReader's free pool and lets
        // MediaCodec overwrite the pixels with a future frame while this one
        // is still queued for render (the scene-cut flicker). The slot is
        // held until the renderer's keepalive ring drops the last Arc clone.
        let hb_unowned = image
            .hardware_buffer()
            .map_err(|e| -> DecoderError { format!("Image::hardware_buffer: {:?}", e).into() })?;
        let buffer = Arc::new(SendableAhb::new(hb_unowned.acquire(), image, Arc::clone(reader)));

        // Re-attach HDR metadata harvested at submit time: the dynamic
        // (HDR10+) entry for exactly this pts, else the static fallback.
        let hdr_meta = self
            .pending_hdr_meta
            .remove(&pts_us)
            .or(self.static_hdr_meta);

        Ok(Some(DecodedVideoFrame {
            pts_us,
            width: self.width,
            height: self.height,
            native: PlatformFrame::HardwareBuffer(AndroidHardwareBufferFrame {
                buffer,
                width: self.width,
                height: self.height,
            }),
            desired_present_ns: 0,
            color: self.color,
            hdr_meta,
        }))
    }

    fn flush(&mut self) -> Result<(), DecoderError> {
        if let Some(codec) = self.codec.as_ref() {
            codec
                .flush()
                .map_err(|e| -> DecoderError { format!("MediaCodec::flush: {:?}", e).into() })?;
        }
        if let Some(direct) = self.direct.as_ref() {
            // Flush invalidates outstanding output indices; in-flight
            // DirectVideoFrames then no-op their releases (the codec
            // tolerates a release error, logged at trace).
            let _l = direct.call_lock.lock().unwrap();
            unsafe {
                ndk_sys::AMediaCodec_flush(direct.raw);
            }
        }
        self.last_decoded_pts_us = -1;
        self.decoded_frame_idx = 0;
        self.pending_hdr_meta.clear();
        Ok(())
    }

    fn is_direct(&self) -> bool {
        self.direct_window != 0
    }

    fn set_stop_signal(&mut self, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.stop_signal = Some(stop);
    }
}

impl Drop for MediaCodecDecoder {
    fn drop(&mut self) {
        // Direct mode: stop the codec NOW (disconnects the video Surface so
        // the next decoder can attach; invalidates in-flight frame indices,
        // which their release handles tolerate via the stopped flag). The
        // AMediaCodec object itself is deleted when the last Arc — possibly
        // held by a frame still queued in the channel — drops.
        if let Some(direct) = self.direct.take() {
            direct.stop();
        }
    }
}
