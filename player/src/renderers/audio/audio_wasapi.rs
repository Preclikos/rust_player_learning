//! Windows 7.1 PCM output for the spatial-sound path, on our own WASAPI
//! shared-mode client (child of `audio_cpal`, which owns the queue + clock).
//!
//! With Windows Sonic / Dolby Atmos for Headphones / DTS Headphone:X enabled,
//! the endpoint still reports a stereo mix format, yet it accepts a 7.1 stream
//! and renders it binaurally (verified by ear on a JBL Live 770NC with Dolby
//! Atmos, 2026-10-05: all seven speaker positions distinct). cpal can't use
//! that — it opens multichannel streams with `KSAUDIO_SPEAKER_DIRECTOUT`, i.e.
//! channels without speaker positions — so this writer opens the stream with
//! a real 7.1 mask (FL FR FC LFE BL BR SL SR, the order `ChannelLayout::
//! default(8)` produces) and `AUTOCONVERTPCM`: if the user switches to a
//! device without spatial sound, Windows downmixes instead of refusing.
//!
//! Same contract as the cpal stream: `fill` pulls from the shared cursor (the
//! consumed-sample clock), `output_latency_ms` is the queued-ahead padding
//! plus the stream latency, and a dead stream (device unplugged / switched,
//! `AUDCLNT_E_DEVICE_INVALIDATED`) is rebuilt on the current default device.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::{fill, OutputShared, STREAM_RETRY};

const CHANNELS: u16 = 8;
/// FL FR FC LFE BL BR SL SR (KSAUDIO_SPEAKER_7POINT1_SURROUND).
const MASK_7POINT1_SURROUND: u32 = 0x63F;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT.
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
/// Requested endpoint buffer, in 100 ns units (100 ms).
const BUFFER_HNS: i64 = 1_000_000;

/// True when the default render endpoint has spatial sound switched on (the
/// spatial audio platform offers it a render stream). Errors and devices
/// without the feature read as false.
pub(super) fn spatial_sound_enabled() -> bool {
    // A/B kill switch: RUST_PLAYER_SPATIAL=0 keeps the plain cpal output.
    if std::env::var("RUST_PLAYER_SPATIAL").is_ok_and(|v| v == "0") {
        log::info!("[audio] RUST_PLAYER_SPATIAL=0 — Windows spatial output disabled");
        return false;
    }
    let res = unsafe {
        (|| -> windows::core::Result<bool> {
            let _com = ComGuard::init();
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let sac: ISpatialAudioClient = dev.Activate(CLSCTX_ALL, None)?;
            Ok(sac
                .IsSpatialAudioStreamAvailable(&ISpatialAudioObjectRenderStream::IID, None)
                .is_ok())
        })()
    };
    match res {
        Ok(on) => {
            log::info!("[audio] Windows spatial sound on the default device: {on}");
            on
        }
        Err(e) => {
            log::info!("[audio] Windows spatial sound query failed ({e}); treating as off");
            false
        }
    }
}

/// Run the 7.1 writer until `stopped`, rebuilding the stream whenever it dies.
pub(super) fn run(shared: &OutputShared, rate: u32, stopped: &AtomicBool) {
    let _com = unsafe { ComGuard::init() };
    let mut failures = 0u32;
    while !stopped.load(Ordering::Relaxed) {
        match unsafe { Stream::open(rate) } {
            Ok(stream) => {
                if failures > 0 {
                    log::info!("[audio] 7.1 WASAPI stream reopened after {failures} failed attempt(s)");
                }
                failures = 0;
                shared.last_callback_ms.store(shared.now_ms(), Ordering::Relaxed);
                if let Err(e) = unsafe { stream.pump(shared, rate, stopped) } {
                    log::warn!("[audio] 7.1 WASAPI stream dead ({e}) — rebuilding it on the default device");
                }
            }
            Err(e) => {
                if failures == 0 {
                    log::warn!("[audio] cannot open the 7.1 WASAPI stream, retrying: {e}");
                }
                failures += 1;
                std::thread::park_timeout(STREAM_RETRY);
            }
        }
    }
}

struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    buffer_frames: u32,
    /// Endpoint stream latency, in frames.
    latency_frames: u64,
}

impl Stream {
    unsafe fn open(rate: u32) -> windows::core::Result<Self> {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let mut fmt = WAVEFORMATEXTENSIBLE::default();
        fmt.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE;
        fmt.Format.nChannels = CHANNELS;
        fmt.Format.nSamplesPerSec = rate;
        fmt.Format.wBitsPerSample = 32;
        fmt.Format.nBlockAlign = CHANNELS * 4;
        fmt.Format.nAvgBytesPerSec = rate * CHANNELS as u32 * 4;
        fmt.Format.cbSize = 22;
        fmt.Samples.wValidBitsPerSample = 32;
        fmt.dwChannelMask = MASK_7POINT1_SURROUND;
        fmt.SubFormat = SUBTYPE_IEEE_FLOAT;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_HNS,
            0,
            &fmt.Format,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        let stream = (|| -> windows::core::Result<Self> {
            client.SetEventHandle(event)?;
            let render: IAudioRenderClient = client.GetService()?;
            let buffer_frames = client.GetBufferSize()?;
            let latency_hns = client.GetStreamLatency().unwrap_or(0).max(0) as u64;
            Ok(Self {
                client: client.clone(),
                render,
                event,
                buffer_frames,
                latency_frames: latency_hns * rate as u64 / 10_000_000,
            })
        })();
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                let _ = CloseHandle(event);
                return Err(e);
            }
        };
        stream.client.Start()?;
        Ok(stream)
    }

    /// Feed the stream until `stopped` (Ok) or a WASAPI error (Err: rebuild).
    unsafe fn pump(
        &self,
        shared: &OutputShared,
        rate: u32,
        stopped: &AtomicBool,
    ) -> windows::core::Result<()> {
        loop {
            if stopped.load(Ordering::Relaxed) {
                return Ok(());
            }
            // Signalled once per device period; the timeout keeps `stopped`
            // responsive and catches a device that stopped signalling.
            if WaitForSingleObject(self.event, 200) != WAIT_OBJECT_0 {
                let quiet = shared
                    .now_ms()
                    .saturating_sub(shared.last_callback_ms.load(Ordering::Relaxed));
                if quiet > super::STREAM_DEAD_MS {
                    return Err(windows::core::Error::new(
                        AUDCLNT_E_DEVICE_INVALIDATED,
                        format!("no buffer event for {quiet} ms"),
                    ));
                }
                continue;
            }
            let padding = self.client.GetCurrentPadding()?;
            let avail = self.buffer_frames.saturating_sub(padding);
            if avail == 0 {
                continue;
            }
            let ptr = self.render.GetBuffer(avail)? as *mut f32;
            let data = std::slice::from_raw_parts_mut(ptr, avail as usize * CHANNELS as usize);
            fill(shared, data);
            self.render.ReleaseBuffer(avail, 0)?;
            shared.last_callback_ms.store(shared.now_ms(), Ordering::Relaxed);
            // The first sample just written is audible after what was already
            // queued plus the endpoint latency (cpal's playback − callback).
            let ms = (padding as u64 + self.latency_frames) * 1000 / rate.max(1) as u64;
            if ms > 0 && ms <= 1000 {
                shared.output_latency_ms.store(ms, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// CoInitializeEx(MTA) for this thread, balanced on drop when it succeeded.
struct ComGuard(bool);

impl ComGuard {
    unsafe fn init() -> Self {
        Self(CoInitializeEx(None, COINIT_MULTITHREADED).is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}
