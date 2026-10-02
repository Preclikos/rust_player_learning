//! CoreMedia / CoreFoundation bindings for direct mode, wrapped so the
//! rest of the module never touches a raw `CFTypeRef` lifetime:
//! [`FormatDescription`], [`SampleBuffer`], [`Timebase`], and the few
//! Objective-C helpers used on the `AVSampleBufferDisplayLayer`.

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

use objc2::encode::{Encoding, RefEncode};
use objc2::runtime::{AnyObject, Bool};
use objc2::msg_send;

use crate::decoders::{DecoderError, VideoDecoderParams};

type CFTypeRef = *const c_void;
type CFStringRef = CFTypeRef;
type CFMutableDictionaryRef = *mut c_void;
type OSStatus = i32;
type CMBlockBufferRef = *mut c_void;
type CMFormatDescriptionRef = *mut c_void;
type CMClockRef = *mut c_void;

/// `struct opaqueCMSampleBuffer` — encoded for objc2's signature check of
/// `-[AVSampleBufferDisplayLayer enqueueSampleBuffer:]`.
#[repr(C)]
pub(super) struct OpaqueSampleBuffer {
    _p: [u8; 0],
}
unsafe impl RefEncode for OpaqueSampleBuffer {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("opaqueCMSampleBuffer", &[]));
}

/// `struct OpaqueCMTimebase` — for `-setControlTimebase:`.
#[repr(C)]
pub(super) struct OpaqueTimebase {
    _p: [u8; 0],
}
unsafe impl RefEncode for OpaqueTimebase {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("OpaqueCMTimebase", &[]));
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const K_CM_TIME_FLAGS_VALID: u32 = 1;

impl CMTime {
    const INVALID: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };

    pub(super) fn from_us(us: i64) -> Self {
        CMTime { value: us, timescale: 1_000_000, flags: K_CM_TIME_FLAGS_VALID, epoch: 0 }
    }

    pub(super) fn from_ns(ns: i64) -> Self {
        CMTime { value: ns, timescale: 1_000_000_000, flags: K_CM_TIME_FLAGS_VALID, epoch: 0 }
    }

    /// Nanoseconds; 0 for an invalid time.
    pub(super) fn as_ns(&self) -> i64 {
        if self.flags & K_CM_TIME_FLAGS_VALID == 0 || self.timescale <= 0 {
            return 0;
        }
        (self.value as i128 * 1_000_000_000 / self.timescale as i128) as i64
    }
}

#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_timestamp: CMTime,
    decode_timestamp: CMTime,
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const FOURCC_HVC1: u32 = u32::from_be_bytes(*b"hvc1");
const FOURCC_DVH1: u32 = u32::from_be_bytes(*b"dvh1");

// AVFoundation is linked so `AVSampleBufferDisplayLayer` resolves at
// runtime (the Swift / ObjC hosts link it anyway; the unit tests don't).
#[link(name = "AVFoundation", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: isize) -> CFTypeRef;
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFDictionaryCreateMutable(
        alloc: CFTypeRef,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(dict: CFMutableDictionaryRef, key: CFTypeRef, value: CFTypeRef);
    fn CFArrayGetCount(arr: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(arr: CFTypeRef, idx: isize) -> CFTypeRef;

    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFBooleanTrue: CFTypeRef;
    static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFStringRef;
    static kCMSampleAttachmentKey_DoNotDisplay: CFStringRef;

    fn CMVideoFormatDescriptionCreate(
        alloc: CFTypeRef,
        codec_type: u32,
        width: i32,
        height: i32,
        extensions: CFTypeRef,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFTypeRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFTypeRef,
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
        data_length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFTypeRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        // CMItemCount = signed long; declared exactly as in videotoolbox.rs
        // so the two extern blocks agree (clashing_extern_declarations).
        num_samples: i64,
        num_sample_timing_entries: i64,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: i64,
        sample_size_array: *const usize,
        sample_buffer_out: *mut *mut c_void,
    ) -> OSStatus;
    fn CMSampleBufferGetSampleAttachmentsArray(sbuf: *mut OpaqueSampleBuffer, create: u8) -> CFTypeRef;

    fn CMClockGetHostTimeClock() -> CMClockRef;
    fn CMClockGetTime(clock: CMClockRef) -> CMTime;
    fn CMTimebaseCreateWithSourceClock(
        alloc: CFTypeRef,
        source_clock: CMClockRef,
        timebase_out: *mut *mut OpaqueTimebase,
    ) -> OSStatus;
    fn CMTimebaseGetTime(timebase: *mut OpaqueTimebase) -> CMTime;
    fn CMTimebaseGetRate(timebase: *mut OpaqueTimebase) -> f64;
    fn CMTimebaseSetRate(timebase: *mut OpaqueTimebase, rate: f64) -> OSStatus;
    fn CMTimebaseSetRateAndAnchorTime(
        timebase: *mut OpaqueTimebase,
        rate: f64,
        timebase_time: CMTime,
        immediate_source_time: CMTime,
    ) -> OSStatus;
}

/// Owned CoreFoundation object, released on drop.
struct CfOwned(CFTypeRef);

impl Drop for CfOwned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn cf_string(s: &CStr) -> CfOwned {
    CfOwned(unsafe { CFStringCreateWithCString(ptr::null(), s.as_ptr(), K_CF_STRING_ENCODING_UTF8) })
}

fn cf_data(bytes: &[u8]) -> CfOwned {
    CfOwned(unsafe { CFDataCreate(ptr::null(), bytes.as_ptr(), bytes.len() as isize) })
}

fn cf_dictionary() -> CfOwned {
    CfOwned(unsafe {
        CFDictionaryCreateMutable(
            ptr::null(),
            0,
            &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
        ) as CFTypeRef
    })
}

/// `CMVideoFormatDescription` built from the sample-entry atoms the init
/// segment carried: `hvcC`, plus the DV configuration box for `dvh1`. The
/// OS parses both itself.
pub(super) struct FormatDescription(CMFormatDescriptionRef);

unsafe impl Send for FormatDescription {}

impl FormatDescription {
    /// `dv` = the `dvcC`/`dvvC` box (type, payload) to describe the stream
    /// as Dolby Vision; `None` describes its HEVC (base) layer.
    pub(super) fn new(params: &VideoDecoderParams, dv: Option<&([u8; 4], Vec<u8>)>) -> Result<Self, DecoderError> {
        if params.decoder_config_record.is_empty() {
            return Err("direct mode: no hvcC record".into());
        }
        let atoms = cf_dictionary();
        let put = |name: &CStr, bytes: &[u8]| unsafe {
            CFDictionarySetValue(atoms.0 as CFMutableDictionaryRef, cf_string(name).0, cf_data(bytes).0);
        };
        put(c"hvcC", &params.decoder_config_record);
        if let Some((kind, record)) = dv {
            put(&CString::new(kind.to_vec()).unwrap_or_default(), record);
        }
        let ext = cf_dictionary();
        let mut fd: CMFormatDescriptionRef = ptr::null_mut();
        let st = unsafe {
            CFDictionarySetValue(
                ext.0 as CFMutableDictionaryRef,
                kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
                atoms.0,
            );
            CMVideoFormatDescriptionCreate(
                ptr::null(),
                if dv.is_some() { FOURCC_DVH1 } else { FOURCC_HVC1 },
                params.width.max(1) as i32,
                params.height.max(1) as i32,
                ext.0,
                &mut fd,
            )
        };
        if st != 0 || fd.is_null() {
            return Err(format!("CMVideoFormatDescriptionCreate: {st}").into());
        }
        Ok(Self(fd))
    }
}

impl Drop for FormatDescription {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

/// One compressed access unit as a `CMSampleBuffer` (a copy of the bytes,
/// PTS only — decode order is enqueue order, so no DTS).
pub(super) struct SampleBuffer(*mut OpaqueSampleBuffer);

impl SampleBuffer {
    pub(super) fn new(format: &FormatDescription, sample: &[u8], pts_us: i64) -> Result<Self, DecoderError> {
        let mut block: CMBlockBufferRef = ptr::null_mut();
        let st = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                ptr::null_mut(), // CoreMedia allocates
                sample.len(),
                ptr::null(),
                ptr::null(),
                0,
                sample.len(),
                0,
                &mut block,
            )
        };
        if st != 0 {
            return Err(format!("CMBlockBufferCreateWithMemoryBlock: {st}").into());
        }
        let block = CfOwned(block as CFTypeRef);
        let st = unsafe {
            CMBlockBufferReplaceDataBytes(sample.as_ptr() as *const c_void, block.0 as CMBlockBufferRef, 0, sample.len())
        };
        if st != 0 {
            return Err(format!("CMBlockBufferReplaceDataBytes: {st}").into());
        }
        let timing = CMSampleTimingInfo {
            duration: CMTime::INVALID,
            presentation_timestamp: CMTime::from_us(pts_us),
            decode_timestamp: CMTime::INVALID,
        };
        let size = sample.len();
        let mut sbuf: *mut c_void = ptr::null_mut();
        // Retains the block buffer on success; `block` drops our reference.
        let st = unsafe {
            CMSampleBufferCreateReady(ptr::null(), block.0 as CMBlockBufferRef, format.0, 1, 1, &timing, 1, &size, &mut sbuf)
        };
        if st != 0 || sbuf.is_null() {
            return Err(format!("CMSampleBufferCreateReady: {st}").into());
        }
        Ok(Self(sbuf as *mut OpaqueSampleBuffer))
    }

    /// Decode but never show (ABR splice overlap: already shown by the
    /// previous representation, still needed as a reference).
    pub(super) fn mark_do_not_display(&self) {
        unsafe {
            let attachments = CMSampleBufferGetSampleAttachmentsArray(self.0, 1);
            if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
                let dict = CFArrayGetValueAtIndex(attachments, 0) as CFMutableDictionaryRef;
                CFDictionarySetValue(dict, kCMSampleAttachmentKey_DoNotDisplay, kCFBooleanTrue);
            }
        }
    }

    pub(super) fn as_ptr(&self) -> *mut OpaqueSampleBuffer {
        self.0
    }
}

impl Drop for SampleBuffer {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

/// A `CMTimebase` on the host clock — the layer's `controlTimebase`.
pub(super) struct Timebase(*mut OpaqueTimebase);

unsafe impl Send for Timebase {}

impl Timebase {
    pub(super) fn new() -> Option<Self> {
        let mut tb: *mut OpaqueTimebase = ptr::null_mut();
        let st = unsafe { CMTimebaseCreateWithSourceClock(ptr::null(), CMClockGetHostTimeClock(), &mut tb) };
        if st != 0 || tb.is_null() {
            log::warn!("[direct] CMTimebaseCreateWithSourceClock failed ({st})");
            return None;
        }
        Some(Self(tb))
    }

    pub(super) fn raw(&self) -> *mut OpaqueTimebase {
        self.0
    }

    pub(super) fn time_ns(&self) -> i64 {
        unsafe { CMTimebaseGetTime(self.0) }.as_ns()
    }

    pub(super) fn rate(&self) -> f64 {
        unsafe { CMTimebaseGetRate(self.0) }
    }

    pub(super) fn stop(&self) {
        unsafe { CMTimebaseSetRate(self.0, 0.0) };
    }

    /// Run at rate 1 with media time `pts_us` landing `lead_ns` from now on
    /// the host clock.
    pub(super) fn run_from(&self, pts_us: i64, lead_ns: i64) {
        let host_now = unsafe { CMClockGetTime(CMClockGetHostTimeClock()) }.as_ns();
        unsafe {
            CMTimebaseSetRateAndAnchorTime(self.0, 1.0, CMTime::from_us(pts_us), CMTime::from_ns(host_now + lead_ns))
        };
    }
}

impl Drop for Timebase {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

// ---------------------------------------------------------------------
// AVSampleBufferDisplayLayer messaging
// ---------------------------------------------------------------------

pub(super) fn responds_to(obj: &AnyObject, sel: objc2::runtime::Sel) -> bool {
    let r: Bool = unsafe { msg_send![obj, respondsToSelector: sel] };
    r.as_bool()
}

/// `AVQueuedSampleBufferRenderingStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LayerStatus {
    Unknown,
    Rendering,
    Failed,
}

pub(super) fn layer_status(layer: &AnyObject) -> LayerStatus {
    let status: isize = unsafe { msg_send![layer, status] };
    match status {
        1 => LayerStatus::Rendering,
        2 => LayerStatus::Failed,
        _ => LayerStatus::Unknown,
    }
}

/// `(code, localizedDescription)` of the layer's `error`, or `(0, "")`.
pub(super) fn layer_error(layer: &AnyObject) -> (isize, String) {
    unsafe {
        let err: *mut AnyObject = msg_send![layer, error];
        let Some(err) = err.as_ref() else { return (0, String::new()) };
        let code: isize = msg_send![err, code];
        let desc: *mut AnyObject = msg_send![err, localizedDescription];
        let text = match desc.as_ref() {
            Some(d) => {
                let utf8: *const c_char = msg_send![d, UTF8String];
                if utf8.is_null() { String::new() } else { CStr::from_ptr(utf8).to_string_lossy().into_owned() }
            }
            None => String::new(),
        };
        (code, text)
    }
}

/// `requiresFlushToResumeDecoding` (iOS 14 / macOS 11): the OS interrupted
/// the layer and wants a flush, not a failover.
pub(super) fn layer_wants_flush(layer: &AnyObject) -> bool {
    responds_to(layer, objc2::sel!(requiresFlushToResumeDecoding))
        && unsafe { msg_send![layer, requiresFlushToResumeDecoding] }
}

pub(super) fn layer_flush(layer: &AnyObject) {
    unsafe { let _: () = msg_send![layer, flush]; }
}

pub(super) fn layer_enqueue(layer: &AnyObject, sample: &SampleBuffer) {
    unsafe { let _: () = msg_send![layer, enqueueSampleBuffer: sample.as_ptr()]; }
}

pub(super) fn layer_set_control_timebase(layer: &AnyObject, timebase: &Timebase) {
    unsafe { let _: () = msg_send![layer, setControlTimebase: timebase.raw()]; }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoders::{VideoCodec, VideoColorInfo};

    #[test]
    fn cmtime_round_trips_to_ns() {
        assert_eq!(CMTime::from_us(1_500_000).as_ns(), 1_500_000_000);
        assert_eq!(CMTime::from_ns(42).as_ns(), 42);
        assert_eq!(CMTime::INVALID.as_ns(), 0);
    }

    fn params(dovi: bool) -> VideoDecoderParams {
        // A real Main10 hvcC header (23 bytes, zero parameter-set arrays):
        // the description only has to parse the record, not decode.
        let hvcc = vec![1u8, 0x22, 0x20, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 120, 0xf0, 0, 0xfc, 0xfd, 0xfa, 0xfa, 0, 0, 0x0f, 0];
        VideoDecoderParams {
            codec: VideoCodec::Hevc,
            width: 1920,
            height: 1080,
            hvcc_nalus: vec![],
            decoder_config_record: hvcc,
            color: VideoColorInfo::default(),
            direct_window: 0,
            force_8bit_hdr: false,
            dovi_profile: dovi.then_some(8),
            dovi_record: dovi.then(|| (*b"dvvC", vec![1, 0, 0x10, 0x35, 0x10, 0, 0, 0])),
            max_width: 1920,
            max_height: 1080,
        }
    }

    #[test]
    fn hevc_and_dolby_vision_format_descriptions_build() {
        let p = params(true);
        FormatDescription::new(&p, None).expect("hvc1");
        FormatDescription::new(&p, p.dovi_record.as_ref()).expect("dvh1");
    }

    #[test]
    fn format_description_needs_an_hvcc_record() {
        let mut p = params(false);
        p.decoder_config_record.clear();
        assert!(FormatDescription::new(&p, None).is_err());
    }

    #[test]
    fn sample_buffer_wraps_bytes_and_takes_attachments() {
        let p = params(false);
        let fd = FormatDescription::new(&p, None).unwrap();
        let sb = SampleBuffer::new(&fd, &[0, 0, 0, 2, 0x26, 0x01], 40_000).expect("sample buffer");
        sb.mark_do_not_display();
        assert!(!sb.as_ptr().is_null());
    }

    #[test]
    fn timebase_runs_and_stops() {
        let tb = Timebase::new().expect("timebase");
        assert_eq!(tb.rate(), 0.0);
        tb.run_from(5_000_000, 0);
        assert_eq!(tb.rate(), 1.0);
        assert!((tb.time_ns() - 5_000_000_000).abs() < 50_000_000, "{}", tb.time_ns());
        tb.stop();
        assert_eq!(tb.rate(), 0.0);
    }
}
