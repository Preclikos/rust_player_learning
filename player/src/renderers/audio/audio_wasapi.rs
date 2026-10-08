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
//! plus the stream latency, and a dead stream (device unplugged,
//! `AUDCLNT_E_DEVICE_INVALIDATED`) is rebuilt on the current default device.
//! A stream does NOT die when another device becomes the default (headphones
//! reconnected), so the writer polls the default endpoint and moves to it.
//! After any rebuild the clock is re-aligned with the wall so lip-sync
//! survives the switch: audio the gap left behind is skipped (`skip_ms`); if
//! the old stream's queue held more than the gap lasted (a quick, controlled
//! switch), the new stream opens on that much silence instead. A controlled
//! switch (host toggle, new default device) lets the old stream play out
//! first, so no content is lost.
//!
//! `Player::set_spatial_audio(false)` (read live) moves the writer to a plain
//! stereo stream: the queue stays 7.1 (the renderer's layout is fixed), the
//! writer folds it down itself (ITU-R BS.775) — what Windows gets from any
//! other app.
//!
//! The spatial renderer plays a 7.1 stream ~13.4 dB quieter than a stereo one
//! (headroom for its binaural mix; measured 2026-10-08 by loopback, the same
//! for FL+FR and FC), so the 7.1 stream is written with `SPATIAL_MAKEUP_GAIN`
//! to land at the cpal/stereo level. Float samples above 1.0 survive the
//! shared-mode mix (verified: ×4.2 in → 0.89 peak out, unclipped). Only on a
//! device that really spatializes: AUTOCONVERTPCM's plain downmix would play
//! it 13 dB too loud.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::{fill, skip_ms, OutputShared, STREAM_RETRY};

/// Channels in the queue (what the decoders mix to).
const CHANNELS: u16 = 8;
/// FL FR FC LFE BL BR SL SR (KSAUDIO_SPEAKER_7POINT1_SURROUND).
const MASK_7POINT1_SURROUND: u32 = 0x63F;
/// FL FR (KSAUDIO_SPEAKER_STEREO), the spatial-audio-off stream.
const MASK_STEREO: u32 = 0x3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT.
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
/// Requested endpoint buffer, in 100 ns units (100 ms).
const BUFFER_HNS: i64 = 1_000_000;
/// How often the writer checks whether the default output device changed.
const DEFAULT_POLL_MS: u64 = 1_000;
/// Clock offsets below this after a rebuild are left alone. Kept tiny on
/// purpose: a switch's offset is biased (gap a little over the queue every
/// time), so a coarser threshold let ~20 ms per switch pile up.
const MIN_SKIP_MS: u64 = 2;
/// Upper bound on letting the old stream play out before a controlled switch.
const DRAIN_MAX_MS: u64 = 300;
/// +13.4 dB on the 7.1 stream: undoes the spatial renderer's attenuation.
const SPATIAL_MAKEUP_GAIN: f32 = 4.68;

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
            Ok(spatializes(&dev))
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

/// True when the spatial audio platform offers `dev` a render stream (spatial
/// sound switched on for it).
unsafe fn spatializes(dev: &IMMDevice) -> bool {
    dev.Activate::<ISpatialAudioClient>(CLSCTX_ALL, None)
        .and_then(|sac| sac.IsSpatialAudioStreamAvailable(&ISpatialAudioObjectRenderStream::IID, None))
        .is_ok()
}

/// Run the 7.1 writer until `stopped`, rebuilding the stream whenever it dies
/// or the default device changes.
pub(super) fn run(shared: &OutputShared, rate: u32, stopped: &AtomicBool, enabled: &AtomicBool) {
    let _com = unsafe { ComGuard::init() };
    let mut failures = 0u32;
    // `epoch` ms of the last buffer the previous stream took, once one died,
    // and how much of what it was handed was still queued (counted as played
    // by the clock, lost with the stream).
    let mut gap_from: Option<(u64, u64, String)> = None;
    while !stopped.load(Ordering::Relaxed) {
        let spatial = enabled.load(Ordering::Relaxed);
        match unsafe { Stream::open(rate, spatial) } {
            Ok(stream) => {
                // The HUD prints the stream's channel count next to it.
                let backend = "WASAPI";
                log::info!(
                    "[audio] WASAPI {} on \"{}\" (gain {})",
                    if spatial { "7.1 (spatial sound)" } else { "stereo" },
                    stream.device_name,
                    stream.gain
                );
                {
                    let mut st = shared.status.lock().unwrap();
                    st.backend = backend.into();
                    st.device = stream.device_name.clone();
                    st.stream_channels = stream.out_channels;
                }
                if let Some((from, lost_ms, why)) = gap_from.take() {
                    // The clock counted the old stream's queue as played at the
                    // last write: what the gap lasted beyond it is audio to
                    // skip, a gap shorter than it is silence to hold.
                    let gap = shared.now_ms().saturating_sub(from);
                    let offset = gap as i64 - lost_ms as i64;
                    let mut skipped = 0;
                    if offset >= MIN_SKIP_MS as i64 {
                        skipped = skip_ms(shared, offset as u64, rate, CHANNELS);
                    } else if -offset >= MIN_SKIP_MS as i64 && !shared.paused_flag.load(Ordering::Relaxed) {
                        stream.hold_frames.set((-offset) as u64 * rate as u64 / 1000);
                    }
                    let held = stream.hold_frames.get() * 1000 / rate.max(1) as u64;
                    let what = format!(
                        "{why} -> \"{}\": gap {gap} ms, {lost_ms} ms queued, skipped {skipped} ms, held {held} ms{}",
                        stream.device_name,
                        if failures > 0 { format!(", {failures} failed open(s)") } else { String::new() }
                    );
                    log::info!("[audio] output switch: {what}");
                    shared.note_switch(what);
                }
                failures = 0;
                shared.last_callback_ms.store(shared.now_ms(), Ordering::Relaxed);
                let why = match unsafe { stream.pump(shared, rate, stopped, enabled) } {
                    Ok(Exit::Stopped) => break,
                    Ok(Exit::ModeChanged) => {
                        let why = if enabled.load(Ordering::Relaxed) { "host switched spatial on" } else { "host switched spatial off" };
                        log::info!("[audio] {why} — rebuilding the WASAPI stream");
                        why.to_string()
                    }
                    Ok(Exit::DefaultChanged) => {
                        log::info!("[audio] default output device changed — moving the WASAPI stream");
                        "default device changed".to_string()
                    }
                    Err(e) => {
                        log::warn!("[audio] WASAPI stream dead ({e}) — rebuilding it on the default device");
                        format!("device lost ({})", e.code())
                    }
                };
                gap_from = Some((
                    shared.last_callback_ms.load(Ordering::Relaxed),
                    stream.queued_ms.get(),
                    why,
                ));
            }
            Err(e) => {
                if failures == 0 {
                    log::warn!("[audio] cannot open the WASAPI stream, retrying: {e}");
                }
                failures += 1;
                std::thread::park_timeout(STREAM_RETRY);
            }
        }
    }
}

/// The device's display name ("Headphones (JBL Live 770NC)"), or its id.
unsafe fn friendly_name(dev: &IMMDevice, id: &str) -> String {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    dev.OpenPropertyStore(STGM_READ)
        .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
        .map(|v| v.to_string())
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| id.to_string())
}

enum Exit {
    Stopped,
    DefaultChanged,
    ModeChanged,
}

/// Endpoint id of the current default render device.
unsafe fn default_device_id(en: &IMMDeviceEnumerator) -> windows::core::Result<String> {
    let id = en.GetDefaultAudioEndpoint(eRender, eConsole)?.GetId()?;
    let out = id.to_string().unwrap_or_default();
    CoTaskMemFree(Some(id.0 as *const _));
    Ok(out)
}

struct Stream {
    enumerator: IMMDeviceEnumerator,
    device_id: String,
    device_name: String,
    client: IAudioClient,
    render: IAudioRenderClient,
    /// Channels of THIS stream: 8, or 2 when the host switched spatial off.
    out_channels: u16,
    spatial: bool,
    /// Applied to the 7.1 samples (`SPATIAL_MAKEUP_GAIN` on a spatializing
    /// device, else 1.0).
    gain: f32,
    /// PCM handed to the endpoint and not yet played, as of the last write.
    /// The clock counted it as played already, so after a rebuild the clock
    /// is `gap − queued_ms` behind the wall (whether the queue played out in
    /// a drain or died with the device).
    queued_ms: std::cell::Cell<u64>,
    /// Silence still to emit before pulling from the queue (frames), when the
    /// rebuild left the clock ahead of the wall.
    hold_frames: std::cell::Cell<u64>,
    event: HANDLE,
    buffer_frames: u32,
    /// Endpoint stream latency, in frames.
    latency_frames: u64,
}

impl Stream {
    unsafe fn open(rate: u32, spatial: bool) -> windows::core::Result<Self> {
        let (ch, mask) = if spatial { (CHANNELS, MASK_7POINT1_SURROUND) } else { (2, MASK_STEREO) };
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let device_id = {
            let id = dev.GetId()?;
            let out = id.to_string().unwrap_or_default();
            CoTaskMemFree(Some(id.0 as *const _));
            out
        };
        let device_name = friendly_name(&dev, &device_id);
        let gain = if spatial && spatializes(&dev) { SPATIAL_MAKEUP_GAIN } else { 1.0 };
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let mut fmt = WAVEFORMATEXTENSIBLE::default();
        fmt.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE;
        fmt.Format.nChannels = ch;
        fmt.Format.nSamplesPerSec = rate;
        fmt.Format.wBitsPerSample = 32;
        fmt.Format.nBlockAlign = ch * 4;
        fmt.Format.nAvgBytesPerSec = rate * ch as u32 * 4;
        fmt.Format.cbSize = 22;
        fmt.Samples.wValidBitsPerSample = 32;
        fmt.dwChannelMask = mask;
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
                enumerator: en.clone(),
                device_id: device_id.clone(),
                device_name: device_name.clone(),
                client: client.clone(),
                render,
                out_channels: ch,
                spatial,
                gain,
                queued_ms: std::cell::Cell::new(0),
                hold_frames: std::cell::Cell::new(0),
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

    /// Feed the stream until `stopped`, the default device changes, or a
    /// WASAPI error (Err: rebuild).
    unsafe fn pump(
        &self,
        shared: &OutputShared,
        rate: u32,
        stopped: &AtomicBool,
        enabled: &AtomicBool,
    ) -> windows::core::Result<Exit> {
        let mut last_default_check = shared.now_ms();
        // Stereo stream: the 7.1 queue is pulled into this and folded down.
        let mut scratch: Vec<f32> = Vec::new();
        loop {
            if stopped.load(Ordering::Relaxed) {
                return Ok(Exit::Stopped);
            }
            if enabled.load(Ordering::Relaxed) != self.spatial {
                self.drain(rate);
                return Ok(Exit::ModeChanged);
            }
            if shared.now_ms().saturating_sub(last_default_check) >= DEFAULT_POLL_MS {
                last_default_check = shared.now_ms();
                // A failed lookup (no device right now) is not a change; the
                // stream's own errors cover a device that went away.
                if default_device_id(&self.enumerator).is_ok_and(|id| id != self.device_id) {
                    self.drain(rate);
                    return Ok(Exit::DefaultChanged);
                }
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
            let hold = self.hold_frames.get();
            if hold > 0 {
                let n = (hold as u32).min(avail);
                self.render.GetBuffer(n)?;
                self.render.ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                self.hold_frames.set(hold - n as u64);
                shared.last_callback_ms.store(shared.now_ms(), Ordering::Relaxed);
                continue;
            }
            let ptr = self.render.GetBuffer(avail)? as *mut f32;
            let data = std::slice::from_raw_parts_mut(ptr, avail as usize * self.out_channels as usize);
            if self.out_channels == CHANNELS {
                fill(shared, data);
                if self.gain != 1.0 {
                    data.iter_mut().for_each(|s| *s *= self.gain);
                }
            } else {
                scratch.resize(avail as usize * CHANNELS as usize, 0.0);
                fill(shared, &mut scratch[..]);
                data.copy_from_slice(&crate::decoders::pcm::downmix_to_stereo(&scratch, CHANNELS as usize));
            }
            self.render.ReleaseBuffer(avail, 0)?;
            shared.last_callback_ms.store(shared.now_ms(), Ordering::Relaxed);
            // The first sample just written is audible after what was already
            // queued plus the endpoint latency (cpal's playback − callback).
            let rate_ms = rate.max(1) as u64;
            self.queued_ms.set((padding + avail) as u64 * 1000 / rate_ms);
            let ms = (padding as u64 + self.latency_frames) * 1000 / rate_ms;
            if ms > 0 && ms <= 1000 {
                shared.output_latency_ms.store(ms, Ordering::Relaxed);
            }
        }
    }
}

impl Stream {
    /// Let the queued PCM play out (bounded) before the stream is dropped, so
    /// a controlled switch loses no content. `queued_ms` keeps the queue as of
    /// the last write: the drain time is part of the measured gap either way.
    unsafe fn drain(&self, _rate: u32) {
        let start = std::time::Instant::now();
        let mut left = self.client.GetCurrentPadding().unwrap_or(0);
        while left > 0 && start.elapsed().as_millis() < DRAIN_MAX_MS as u128 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            left = self.client.GetCurrentPadding().unwrap_or(0);
        }
        if left > 0 {
            log::info!("[audio] old WASAPI stream not drained within {DRAIN_MAX_MS} ms ({left} frames left)");
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
