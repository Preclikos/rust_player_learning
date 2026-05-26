// iOS AudioToolbox-based AudioDecoder.
//
// Status: skeleton. Will use `AudioConverter` (or `AudioFileStream` for ADTS)
// to decode AAC, AC-3 and Enhanced AC-3 raw frames to interleaved f32 PCM,
// then resample to the output device's preferred rate. Mirrors the structure
// of `videotoolbox.rs` — the iOS playback pipeline isn't wired up yet, but
// the codec dispatch is in place so the AudioCodec enum stays exhaustive on
// every platform.

#![cfg(target_os = "ios")]

use super::{AudioCodec, AudioDecoder, AudioDecoderParams, DecodedAudioFrame, DecoderError};

pub struct AudioToolboxDecoder {
    codec: Option<AudioCodec>,
}

impl AudioToolboxDecoder {
    pub fn new() -> Self {
        Self { codec: None }
    }
}

impl AudioDecoder for AudioToolboxDecoder {
    fn configure(&mut self, params: AudioDecoderParams) -> Result<(), DecoderError> {
        // AudioToolbox expects a four-char-code format ID; map our enum.
        // kAudioFormatMPEG4AAC = 'aac '  (0x61616320)
        // kAudioFormatAC3      = 'ac-3'  (0x61632d33)
        // kAudioFormatEnhancedAC3 = 'ec-3' (0x65632d33)
        let _fourcc: u32 = match params.codec {
            AudioCodec::Aac => 0x6161_6320,
            AudioCodec::Ac3 => 0x6163_2d33,
            AudioCodec::Eac3 => 0x6563_2d33,
        };
        self.codec = Some(params.codec);
        Err("AudioToolboxDecoder::configure not yet implemented".into())
    }

    fn submit(&mut self, _sample: &[u8], _pts_us: i64) -> Result<(), DecoderError> {
        Err("AudioToolboxDecoder::submit not yet implemented".into())
    }

    fn try_recv(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        Err("AudioToolboxDecoder::try_recv not yet implemented".into())
    }
}
