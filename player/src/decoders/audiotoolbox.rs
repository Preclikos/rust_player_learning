//! Apple (macOS + iOS) AAC / AC-3 / EAC-3 decoder backed by AudioConverter.
//!
//! Input  ASBD: compressed source (`'aac '` / `'ac-3'` / `'ec-3'`) at the
//!              codec's native sample rate, with the AudioSpecificConfig
//!              (`csd-0` from esds) installed as the AudioConverter
//!              decompression magic cookie for AAC.
//! Output ASBD: 32-bit float, interleaved stereo PCM at the device's
//!              preferred rate. AudioConverter does codec decode +
//!              resample in a single pass.
//!
//! Each submitted compressed packet is decoded synchronously inside
//! `submit`; `try_recv` pops from a small FIFO. AudioConverter is
//! stateful but cheap to drive packet-by-packet.

#![cfg(any(target_os = "ios", target_os = "macos"))]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;

use super::{
    AudioCodec, AudioDecoder, AudioDecoderParams, DecodedAudioFrame, DecoderError,
};

// -------------------------------------------------------------------------
// CoreAudio / AudioToolbox raw FFI
// -------------------------------------------------------------------------

type OSStatus = i32;
type AudioConverterRef = *mut c_void;
type AudioConverterPropertyID = u32;

// fourcc helpers
const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const K_AUDIO_FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
const K_AUDIO_FORMAT_MPEG4_AAC: u32 = fourcc(b"aac ");
const K_AUDIO_FORMAT_AC3: u32 = fourcc(b"ac-3");
const K_AUDIO_FORMAT_ENHANCED_AC3: u32 = fourcc(b"ec-3");

const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;

// kAudioFormatMPEG4ObjectID_AAC_LC = 2; placed in mFormatFlags of the
// input ASBD so AudioConverter knows which AAC profile to expect when
// no magic cookie is supplied. With a cookie the field is ignored.
const K_MPEG4_OBJECT_ID_AAC_LC: u32 = 2;

const K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE: AudioConverterPropertyID = fourcc(b"dmgc");

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamPacketDescription {
    start_offset: i64,
    variable_frames_in_packet: u32,
    data_byte_size: u32,
}

type AudioConverterComplexInputDataProc = extern "C" fn(
    audio_converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList,
    out_data_packet_description: *mut *mut AudioStreamPacketDescription,
    in_user_data: *mut c_void,
) -> OSStatus;

#[link(name = "AudioToolbox", kind = "framework")]
#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioConverterNew(
        source_format: *const AudioStreamBasicDescription,
        destination_format: *const AudioStreamBasicDescription,
        out_audio_converter: *mut AudioConverterRef,
    ) -> OSStatus;

    fn AudioConverterDispose(audio_converter: AudioConverterRef) -> OSStatus;

    fn AudioConverterSetProperty(
        audio_converter: AudioConverterRef,
        property_id: AudioConverterPropertyID,
        property_data_size: u32,
        property_data: *const c_void,
    ) -> OSStatus;

    fn AudioConverterFillComplexBuffer(
        audio_converter: AudioConverterRef,
        input_data_proc: AudioConverterComplexInputDataProc,
        input_data_proc_user_data: *mut c_void,
        io_output_data_packet_size: *mut u32,
        out_output_data: *mut AudioBufferList,
        out_packet_description: *mut AudioStreamPacketDescription,
    ) -> OSStatus;
}

// -------------------------------------------------------------------------
// Decoder
// -------------------------------------------------------------------------

/// Holds the AudioConverter plus pending input/output FIFOs.
pub struct AudioToolboxDecoder {
    converter: AudioConverterRef,
    /// Active input pixel format: '420v' etc — for audio it's the codec
    /// fourcc; only used for logging.
    codec_fourcc: u32,
    input_sample_rate: u32,
    output_sample_rate: u32,
    /// Decoded PCM frames awaiting `try_recv`.
    output_queue: VecDeque<DecodedAudioFrame>,
    /// Scratch buffer reused for every decoded packet so we don't allocate
    /// the output Vec on every frame. Cleared, written, then drained into
    /// a Vec<f32> on push to `output_queue`.
    scratch_out: Vec<f32>,
}

// AudioConverterRef is documented as **NOT** thread-safe — but we only
// touch it from `submit`/`try_recv`, which the play loop serialises.
unsafe impl Send for AudioToolboxDecoder {}

impl AudioToolboxDecoder {
    pub fn new() -> Self {
        Self {
            converter: ptr::null_mut(),
            codec_fourcc: 0,
            input_sample_rate: 0,
            output_sample_rate: 0,
            output_queue: VecDeque::new(),
            scratch_out: Vec::new(),
        }
    }
}

impl Drop for AudioToolboxDecoder {
    fn drop(&mut self) {
        if !self.converter.is_null() {
            unsafe { AudioConverterDispose(self.converter) };
            self.converter = ptr::null_mut();
        }
    }
}

// Per-call state for the input-data callback. The decoder fills this with
// the one packet to feed, then invokes FillComplexBuffer. The callback
// hands the packet over once and on the second call reports zero packets,
// which tells the converter "drain the rest of the output for this call".
struct InputCtx {
    /// Borrowed packet bytes — must outlive the FillComplexBuffer call.
    packet: *const u8,
    packet_len: u32,
    desc: AudioStreamPacketDescription,
    consumed: bool,
}

extern "C" fn input_proc(
    _converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList,
    out_data_packet_description: *mut *mut AudioStreamPacketDescription,
    in_user_data: *mut c_void,
) -> OSStatus {
    // SAFETY: in_user_data is an exclusive `&mut InputCtx` for the
    // duration of FillComplexBuffer; nothing else touches it.
    let ctx = unsafe { &mut *(in_user_data as *mut InputCtx) };

    if ctx.consumed {
        // No more input → signal end-of-this-batch; the converter will
        // flush whatever it has and return.
        unsafe { *io_number_data_packets = 0 };
        return 0;
    }

    // Single-packet hand-off.
    unsafe {
        let buf = &mut (*io_data).buffers[0];
        buf.number_channels = 0; // compressed input — channels live in the ASBD
        buf.data_byte_size = ctx.packet_len;
        buf.data = ctx.packet as *mut c_void;

        *io_number_data_packets = 1;
        if !out_data_packet_description.is_null() {
            *out_data_packet_description = &mut ctx.desc as *mut _;
        }
    }
    ctx.consumed = true;
    0
}

impl AudioDecoder for AudioToolboxDecoder {
    fn configure(&mut self, params: AudioDecoderParams) -> Result<(), DecoderError> {
        let (format_id, format_flags) = match params.codec {
            AudioCodec::Aac => (K_AUDIO_FORMAT_MPEG4_AAC, K_MPEG4_OBJECT_ID_AAC_LC),
            AudioCodec::Ac3 => (K_AUDIO_FORMAT_AC3, 0),
            AudioCodec::Eac3 => (K_AUDIO_FORMAT_ENHANCED_AC3, 0),
        };

        let frames_per_packet: u32 = match params.codec {
            // AAC LC: 1024 PCM samples per access unit
            AudioCodec::Aac => 1024,
            // AC-3 / EAC-3: 1536 samples per frame
            AudioCodec::Ac3 | AudioCodec::Eac3 => 1536,
        };

        let in_asbd = AudioStreamBasicDescription {
            sample_rate: params.input_sample_rate as f64,
            format_id,
            format_flags,
            bytes_per_packet: 0, // variable
            frames_per_packet,
            bytes_per_frame: 0, // VBR-ish
            channels_per_frame: params.input_channels as u32,
            bits_per_channel: 0,
            reserved: 0,
        };

        let out_asbd = AudioStreamBasicDescription {
            sample_rate: params.output_sample_rate as f64,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            // Stereo f32 interleaved: 2 ch × 4 bytes/sample = 8 bytes/frame/packet.
            bytes_per_packet: 8,
            frames_per_packet: 1,
            bytes_per_frame: 8,
            channels_per_frame: 2,
            bits_per_channel: 32,
            reserved: 0,
        };

        let mut converter: AudioConverterRef = ptr::null_mut();
        let st = unsafe { AudioConverterNew(&in_asbd, &out_asbd, &mut converter) };
        if st != 0 || converter.is_null() {
            return Err(format!("AudioConverterNew: {}", st).into());
        }

        // AAC: install AudioSpecificConfig as the magic cookie so the decoder
        // doesn't expect ADTS headers on each input packet.
        if matches!(params.codec, AudioCodec::Aac) && !params.codec_specific_data.is_empty() {
            let st = unsafe {
                AudioConverterSetProperty(
                    converter,
                    K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE,
                    params.codec_specific_data.len() as u32,
                    params.codec_specific_data.as_ptr() as *const c_void,
                )
            };
            if st != 0 {
                log::warn!(
                    "AudioConverterSetProperty(MagicCookie) status {} — proceeding anyway",
                    st
                );
            }
        }

        self.converter = converter;
        self.codec_fourcc = format_id;
        self.input_sample_rate = params.input_sample_rate;
        self.output_sample_rate = params.output_sample_rate;

        log::info!(
            "AudioToolboxDecoder: configured {:?} ({}Hz {}ch) -> 48k?stereo (out={}Hz)",
            params.codec,
            params.input_sample_rate,
            params.input_channels,
            params.output_sample_rate,
        );
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        if self.converter.is_null() {
            return Err("submit before configure".into());
        }

        // Upper bound for output frames produced by ONE input packet
        // (frames_per_packet input × output_rate/input_rate × small safety).
        let in_fpp = match self.codec_fourcc {
            x if x == K_AUDIO_FORMAT_MPEG4_AAC => 1024.0_f64,
            _ => 1536.0_f64,
        };
        let cap_frames = ((in_fpp * self.output_sample_rate as f64
            / self.input_sample_rate.max(1) as f64)
            .ceil() as u32)
            + 64; // headroom for the converter's internal buffering
        self.scratch_out
            .resize((cap_frames as usize) * 2 /* stereo */, 0.0_f32);

        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (self.scratch_out.len() * std::mem::size_of::<f32>()) as u32,
                data: self.scratch_out.as_mut_ptr() as *mut c_void,
            }],
        };

        let mut input_ctx = InputCtx {
            packet: sample.as_ptr(),
            packet_len: sample.len() as u32,
            desc: AudioStreamPacketDescription {
                start_offset: 0,
                variable_frames_in_packet: 0,
                data_byte_size: sample.len() as u32,
            },
            consumed: false,
        };

        let mut io_packets = cap_frames; // output packets requested = output frames (1 frame/packet)
        let st = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter,
                input_proc,
                &mut input_ctx as *mut _ as *mut c_void,
                &mut io_packets,
                &mut buffer_list,
                ptr::null_mut(),
            )
        };
        if st != 0 {
            return Err(format!("AudioConverterFillComplexBuffer: {}", st).into());
        }

        let produced_frames = io_packets as usize;
        if produced_frames == 0 {
            // Decoder buffered input without producing output (typical
            // for the very first call). Caller will resubmit; we already
            // marked input consumed.
            return Ok(());
        }

        let samples = self.scratch_out[..produced_frames * 2].to_vec();
        let pts_ms = pts_us / 1000;
        self.output_queue.push_back(DecodedAudioFrame { pts_ms, samples });
        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        Ok(self.output_queue.pop_front())
    }
}
