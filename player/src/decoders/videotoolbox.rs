//! Apple (macOS + iOS) HEVC decoder backed by VTDecompressionSession.
//!
//! Pipeline:
//!   `configure(params)` builds a CMVideoFormatDescription from the hvcC
//!   VPS/SPS/PPS NALUs and creates a VTDecompressionSession with a
//!   destination-image-buffer attribute dict that requests NV12
//!   (kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange) on a
//!   Metal-compatible IOSurface.
//!
//!   `submit(sample, pts)` wraps the length-prefixed (AVCC) sample bytes
//!   in a CMBlockBuffer + CMSampleBuffer and asynchronously decodes via
//!   VTDecompressionSessionDecodeFrame. The output callback pushes the
//!   resulting CVPixelBufferRef into a shared Mutex<VecDeque>.
//!
//!   `try_recv()` pops the queue and wraps the CVPixelBuffer in a
//!   `PlatformFrame::CvPixelBuffer(CvPixelBufferOwned)` so the renderer
//!   can zero-copy import it via CVMetalTextureCache (shared with macOS).

#![cfg(any(target_os = "ios", target_os = "macos"))]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex};

use super::{
    CvPixelBufferOwned, DecodedVideoFrame, DecoderError, HwVideoDecoder, PlatformFrame, VideoCodec,
    VideoDecoderParams,
};

// -------------------------------------------------------------------------
// CoreFoundation / CoreMedia / VideoToolbox raw FFI
// -------------------------------------------------------------------------

type CFTypeRef = *const c_void;
type CFAllocatorRef = CFTypeRef;
type CFDictionaryRef = CFTypeRef;
type CFMutableDictionaryRef = *mut c_void;
type CFStringRef = CFTypeRef;
type CFNumberRef = CFTypeRef;
type CFBooleanRef = CFTypeRef;
type CFIndex = isize;
type OSStatus = i32;

type CMBlockBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMVideoFormatDescriptionRef = *mut c_void;
type CVImageBufferRef = *mut c_void;
type VTDecompressionSessionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const K_CM_TIME_FLAGS_VALID: u32 = 1;

#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_timestamp: CMTime,
    decode_timestamp: CMTime,
}

type VTDecompressionOutputCallback = extern "C" fn(
    decompression_output_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image_buffer: CVImageBufferRef,
    presentation_timestamp: CMTime,
    presentation_duration: CMTime,
);

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    decompression_output_callback: VTDecompressionOutputCallback,
    decompression_output_refcon: *mut c_void,
}

// kCFNumberSInt32Type for CFNumberCreate
const K_CF_NUMBER_SINT32_TYPE: CFIndex = 3;

// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange = '420v' = 0x34323076
// (FullRange = '420f' = 0x34323066). Pick VideoRange to match what
// VTDecompressionSession yields by default with the matching key in the
// destination image buffer dict.
const K_CV_PIXEL_FORMAT_TYPE_420_YPCBCR8_BIPLANAR_VIDEO_RANGE: i32 = 0x34323076;

#[link(name = "CoreFoundation", kind = "framework")]
#[link(name = "CoreMedia", kind = "framework")]
#[link(name = "CoreVideo", kind = "framework")]
#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    // -- CoreFoundation --
    fn CFNumberCreate(alloc: CFAllocatorRef, ty: CFIndex, value_ptr: *const c_void) -> CFNumberRef;
    fn CFDictionaryCreateMutable(
        alloc: CFAllocatorRef,
        capacity: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    fn CFDictionaryAddValue(dict: CFMutableDictionaryRef, key: CFTypeRef, value: CFTypeRef);
    fn CFRelease(cf: CFTypeRef);

    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFBooleanTrue: CFBooleanRef;

    // -- CoreVideo keys (extern C globals, dereference to read the CFStringRef) --
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferMetalCompatibilityKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;

    // -- CoreVideo buffer geometry (used by try_recv to report real dimensions
    //    in the DecodedVideoFrame so downstream Stats can render the resolution
    //    instead of "0×0"). --
    fn CVPixelBufferGetWidth(buf: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetHeight(buf: CVImageBufferRef) -> usize;

    // -- CoreMedia --
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        extensions: CFDictionaryRef,
        format_description_out: *mut CMVideoFormatDescriptionRef,
    ) -> OSStatus;

    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;

    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CMBlockBufferRef,
        offset_into_destination: usize,
        num_bytes: usize,
    ) -> OSStatus;

    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMVideoFormatDescriptionRef,
        num_samples: i64,
        num_sample_timing_entries: i64,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: i64,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;

    // -- VideoToolbox --
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMVideoFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;

    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;

    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

// -------------------------------------------------------------------------
// Decoder implementation
// -------------------------------------------------------------------------

/// Bag the queue + the shared session pointer behind an Arc so the
/// async output callback can outlive the decoder for a few µs (callback
/// is invoked from a VT-internal worker thread).
struct SharedState {
    /// (pts_us, CvPixelBufferOwned) — drained by try_recv() in PTS order
    /// of arrival; VT may reorder but the play loop tolerates non-monotonic
    /// PTS as long as each frame carries its true PTS.
    queue: Mutex<VecDeque<(i64, CvPixelBufferOwned)>>,
}

pub struct VideoToolboxDecoder {
    state: Arc<SharedState>,
    format_desc: CMVideoFormatDescriptionRef,
    session: VTDecompressionSessionRef,
    /// Stamped onto every decoded frame (from configure params).
    color: crate::decoders::VideoColorInfo,
}

unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    pub fn new() -> Result<Self, DecoderError> {
        Ok(Self {
            state: Arc::new(SharedState {
                queue: Mutex::new(VecDeque::new()),
            }),
            format_desc: ptr::null_mut(),
            session: ptr::null_mut(),
            color: Default::default(),
        })
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session as CFTypeRef);
            }
            if !self.format_desc.is_null() {
                CFRelease(self.format_desc as CFTypeRef);
            }
        }
    }
}

// VT calls this on its internal decode worker. We marshal the
// CVPixelBuffer + PTS into the shared queue; everything else (status,
// info_flags, duration) is ignored by the play loop.
extern "C" fn decode_output_callback(
    decompression_output_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: CVImageBufferRef,
    _presentation_timestamp: CMTime,
    _presentation_duration: CMTime,
) {
    if status != 0 || image_buffer.is_null() {
        // status != 0 → VT decode error for this frame; drop silently.
        // image_buffer null → frame dropped (e.g. reference frame only).
        return;
    }

    // SAFETY: refcon is set to `Arc::into_raw(state.clone())` in
    // VTDecompressionSessionCreate's outputCallback record. We *don't*
    // consume the Arc here — clone it back without taking ownership.
    let state: &SharedState = unsafe { &*(decompression_output_refcon as *const SharedState) };

    // source_frame_refcon is the (i64)-encoded PTS we passed to DecodeFrame.
    let pts_us = source_frame_refcon as i64;

    let buf = unsafe { CvPixelBufferOwned::from_retained(image_buffer as *mut c_void) };
    if let Ok(mut q) = state.queue.lock() {
        q.push_back((pts_us, buf));
    }
}

impl HwVideoDecoder for VideoToolboxDecoder {
    fn name(&self) -> &'static str {
        "VideoToolbox"
    }

    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError> {
        if !matches!(params.codec, VideoCodec::Hevc) {
            return Err("VideoToolboxDecoder currently supports HEVC only".into());
        }
        if params.hvcc_nalus.is_empty() {
            return Err("hvcc_nalus is empty — need VPS/SPS/PPS".into());
        }

        // CMVideoFormatDescription wants C arrays of (ptr, size). Build
        // them from the Vec<Vec<u8>> input; the NALUs themselves are kept
        // alive by params for the duration of this call.
        let ptrs: Vec<*const u8> = params.hvcc_nalus.iter().map(|n| n.as_ptr()).collect();
        let sizes: Vec<usize> = params.hvcc_nalus.iter().map(|n| n.len()).collect();

        let mut format_desc: CMVideoFormatDescriptionRef = ptr::null_mut();
        let st = unsafe {
            CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                ptr::null(),
                ptrs.len(),
                ptrs.as_ptr(),
                sizes.as_ptr(),
                4, // 4-byte length prefix (AVCC)
                ptr::null(),
                &mut format_desc,
            )
        };
        if st != 0 || format_desc.is_null() {
            return Err(format!("CMVideoFormatDescriptionCreateFromHEVCParameterSets: {}", st).into());
        }
        self.format_desc = format_desc;

        // Destination image buffer attributes:
        //   PixelFormatType = NV12 (420v, video range)
        //   MetalCompatibility = true (request IOSurface usable from Metal)
        //   IOSurfaceProperties = empty dict (signals "use IOSurface backing")
        let dest_attrs = unsafe {
            let d = CFDictionaryCreateMutable(
                ptr::null(),
                3,
                &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
                &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
            );

            let fmt: i32 = K_CV_PIXEL_FORMAT_TYPE_420_YPCBCR8_BIPLANAR_VIDEO_RANGE;
            let fmt_num = CFNumberCreate(
                ptr::null(),
                K_CF_NUMBER_SINT32_TYPE,
                &fmt as *const _ as *const c_void,
            );
            CFDictionaryAddValue(d, kCVPixelBufferPixelFormatTypeKey as CFTypeRef, fmt_num);
            CFRelease(fmt_num);

            CFDictionaryAddValue(
                d,
                kCVPixelBufferMetalCompatibilityKey as CFTypeRef,
                kCFBooleanTrue as CFTypeRef,
            );

            // Empty IOSurface properties dict — its mere presence is the signal.
            let io_props = CFDictionaryCreateMutable(
                ptr::null(),
                0,
                &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
                &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
            );
            CFDictionaryAddValue(
                d,
                kCVPixelBufferIOSurfacePropertiesKey as CFTypeRef,
                io_props as CFTypeRef,
            );
            CFRelease(io_props as CFTypeRef);

            d as CFDictionaryRef
        };

        // Output callback: refcon = Arc<SharedState> pointer. We keep our
        // own Arc clone alive for the session's lifetime; the callback
        // dereferences via `&*ptr` without taking ownership, so as long
        // as `self.state` lives, the callback is safe.
        let refcon = Arc::as_ptr(&self.state) as *mut c_void;
        let cb_record = VTDecompressionOutputCallbackRecord {
            decompression_output_callback: decode_output_callback,
            decompression_output_refcon: refcon,
        };

        let mut session: VTDecompressionSessionRef = ptr::null_mut();
        let st = unsafe {
            VTDecompressionSessionCreate(
                ptr::null(),
                self.format_desc,
                ptr::null(),
                dest_attrs,
                &cb_record,
                &mut session,
            )
        };
        unsafe { CFRelease(dest_attrs) };
        if st != 0 || session.is_null() {
            return Err(format!("VTDecompressionSessionCreate: {}", st).into());
        }
        self.session = session;
        self.color = params.color;
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        if self.session.is_null() {
            return Err("submit before configure".into());
        }

        // CMBlockBuffer wraps a copy of the sample bytes. CM allocates the
        // memory itself (memory_block null + non-null length); we then
        // memcpy via CMBlockBufferReplaceDataBytes. Lifetime: CMBlockBuffer
        // is owned by the CMSampleBuffer which is owned by VT once we hand
        // it off; both are CFType so CFRelease balances out.
        let mut block_buffer: CMBlockBufferRef = ptr::null_mut();
        let st = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                ptr::null_mut(), // CM allocates
                sample.len(),
                ptr::null(), // default block allocator
                ptr::null(),
                0,
                sample.len(),
                0,
                &mut block_buffer,
            )
        };
        if st != 0 {
            return Err(format!("CMBlockBufferCreateWithMemoryBlock: {}", st).into());
        }
        let st = unsafe {
            CMBlockBufferReplaceDataBytes(
                sample.as_ptr() as *const c_void,
                block_buffer,
                0,
                sample.len(),
            )
        };
        if st != 0 {
            unsafe { CFRelease(block_buffer as CFTypeRef) };
            return Err(format!("CMBlockBufferReplaceDataBytes: {}", st).into());
        }

        // CMSampleBuffer: single sample of `sample.len()` bytes, timing
        // info carrying the original microsecond PTS in a 1µs timescale.
        let timing = CMSampleTimingInfo {
            duration: CMTime {
                value: 0,
                timescale: 0,
                flags: 0,
                epoch: 0,
            },
            presentation_timestamp: CMTime {
                value: pts_us,
                timescale: 1_000_000,
                flags: K_CM_TIME_FLAGS_VALID,
                epoch: 0,
            },
            decode_timestamp: CMTime {
                value: 0,
                timescale: 0,
                flags: 0,
                epoch: 0,
            },
        };
        let size = sample.len();
        let mut sample_buf: CMSampleBufferRef = ptr::null_mut();
        let st = unsafe {
            CMSampleBufferCreateReady(
                ptr::null(),
                block_buffer,
                self.format_desc,
                1,
                1,
                &timing,
                1,
                &size,
                &mut sample_buf,
            )
        };
        // CMSampleBufferCreateReady retains the block buffer on success;
        // we always release our reference.
        unsafe { CFRelease(block_buffer as CFTypeRef) };
        if st != 0 || sample_buf.is_null() {
            return Err(format!("CMSampleBufferCreateReady: {}", st).into());
        }

        // Source frame refcon: stash pts_us so the output callback can
        // recover it. (Decode reorders frames; we can't rely on FIFO.)
        let pts_ref = pts_us as *mut c_void;
        let mut info_flags: u32 = 0;
        let st = unsafe {
            VTDecompressionSessionDecodeFrame(
                self.session,
                sample_buf,
                0, // no special flags — synchronous-ish, but VT may still buffer
                pts_ref,
                &mut info_flags,
            )
        };
        unsafe { CFRelease(sample_buf as CFTypeRef) };
        if st != 0 {
            return Err(format!("VTDecompressionSessionDecodeFrame: {}", st).into());
        }
        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let mut q = self
            .state
            .queue
            .lock()
            .map_err(|_| -> DecoderError { "queue poisoned".into() })?;
        let Some((pts_us, buf)) = q.pop_front() else {
            return Ok(None);
        };
        drop(q);

        // Pull real dimensions off the CVPixelBuffer so downstream consumers
        // (Stats event, renderer change_frame_size notifications) get correct
        // values instead of 0 — the renderer recomputes its own texture size
        // either way, but the player's Stats path surfaces this directly.
        let raw = buf.as_ptr() as CVImageBufferRef;
        let width = unsafe { CVPixelBufferGetWidth(raw) } as u32;
        let height = unsafe { CVPixelBufferGetHeight(raw) } as u32;
        Ok(Some(DecodedVideoFrame {
            pts_us,
            width,
            height,
            native: PlatformFrame::CvPixelBuffer(buf),
            desired_present_ns: 0,
            color: self.color,
            hdr_meta: None,
        }))
    }
}

// Suppress unused-import warning when VideoCodec::H264 isn't constructed.
const _: VideoCodec = VideoCodec::Hevc;
