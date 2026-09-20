//! One representation's pipeline: the download-only prefetch half, the
//! decode half, and the per-track `*_play` tasks that join them.

use super::*;

// ---------------------------------------------------------------------------
// Download + decode pipeline builders (renderer-agnostic)
// ---------------------------------------------------------------------------

/// The download-side product of one representation's pipeline, ready to be
/// handed to [`run_decode`]. Produced by [`video_prefetch`].
///
/// Splitting the pipeline into a download half (this) and a decode half lets
/// the ABR supervisor fetch the NEW representation's first segments *while the
/// OLD representation's decoder is still running* — the OLD HW decoder slot is
/// untouched, so there's no second-instance allocation conflict, and av_sync
/// keeps getting OLD frames the whole time. Only once NEW is buffered locally
/// does OLD tear down and NEW's decode start, so the gap that used to trip the
/// 300 ms starvation pause (the visible buffering freeze) collapses to a fast
/// local configure + first-GOP decode.
pub(super) struct VideoPrefetch {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) init_data: Vec<u8>,
    pub(super) hvcc_nalus: Vec<Vec<u8>>,
    pub(super) color: VideoColorInfo,
    pub(super) dovi_profile: Option<u8>,
    pub(super) track_crypto: Option<TrackCrypto>,
    pub(super) download_rx: mpsc::Receiver<DataSegment>,
    pub(super) download_handle: JoinHandle<Result<(), Box<dyn Error + Send + Sync>>>,
    /// Fired (once) when `prime_target` segments have been buffered into
    /// `download_rx`. The supervisor awaits this before tearing OLD down.
    pub(super) primed: Arc<Notify>,
    /// First segment already decrypted + parsed, started by
    /// [`VideoPrefetch::start_first_prepare`] while the OLD rung still played.
    pub(super) first_prepared: Option<PrepareHandle>,
    /// THIS prefetch's own download high-water (absolute pts ms of the last
    /// segment it finished fetching). Always tracked, whether or not this
    /// pipeline is the one publishing the shared gauge.
    pub(super) dl_pts_ms: Arc<std::sync::atomic::AtomicI64>,
    /// Whether this pipeline's downloads advance the SHARED buffer gauge
    /// (`stats.last_decoded_pts_ms`). False for a swap prefetch until it
    /// takes over as the live pipeline — see [`VideoPrefetch::take_over_buffer_gauge`].
    pub(super) track_dl: Arc<AtomicBool>,
}

impl VideoPrefetch {
    /// Become the pipeline that publishes the buffer-ahead gauge.
    ///
    /// Called once the OLD rung is gone and this prefetch's download is the
    /// only one feeding the picture. The gauge is RESET to this pipeline's own
    /// high-water rather than left at the maximum: after a swap the content
    /// that will actually play comes from here, and OLD may have fetched
    /// seconds further ahead in a representation nobody will see again.
    ///
    /// Without this the gauge froze at the swap and then decayed with real
    /// time - reading ~0.2 s (just the decoded-frame cushion) about half a
    /// minute later. That is not cosmetic: the ABR engine gates up-switches on
    /// it (`MIN_UPSWITCH_BUFFER_MS`), so quality got stuck down after the
    /// first automatic switch, and consumers' buffer indicators read empty.
    pub(super) fn take_over_buffer_gauge(&self, stats: &StatsState) {
        let mine = self.dl_pts_ms.load(Ordering::Relaxed);
        if mine > 0 {
            stats.last_decoded_pts_ms.store(mine, Ordering::Relaxed);
        }
        self.track_dl.store(true, Ordering::Relaxed);
    }

    /// Take the first buffered segment out of the download channel and start
    /// its decrypt + mp4 parse NOW, on a blocking thread.
    ///
    /// The decode loop hides that work in steady state by preparing segment
    /// N+1 while feeding N, but the FIRST segment of a pipeline has nothing to
    /// hide behind, and the cost scales with bytes: ~0.7 s for a 14 Mbps 4K
    /// segment on a TV SoC. On an ABR swap that landed squarely on the
    /// critical path — teardown, then 0.7 s of AES before a single frame
    /// existed — which is why switching UP dropped ~10 frames while switching
    /// down dropped one. Started here instead, it overlaps the seconds the
    /// supervisor spends waiting for the segment boundary.
    ///
    /// No-op when nothing is buffered yet or one is already in flight.
    pub(super) fn start_first_prepare(&mut self) {
        if self.first_prepared.is_some() {
            return;
        }
        let Ok(segment) = self.download_rx.try_recv() else {
            return;
        };
        log::debug!(
            "[abr] preparing NEW segment {} ahead of the swap ({} KiB)",
            segment.id,
            segment.data.len() / 1024
        );
        self.first_prepared = Some(prepare_segment(
            Arc::new(self.init_data.clone()),
            self.track_crypto.clone(),
            segment,
        ));
    }
}

/// Download half of a video pipeline: fetch + parse the init segment, resolve
/// the CENC key, and spawn [`download_task`] streaming media segments into a
/// bounded channel. Touches the network only — never the HW decoder — so it is
/// safe to run concurrently with another representation's live decoder.
#[allow(clippy::too_many_arguments)]
pub(super) async fn video_prefetch(
    repr: &VideoRepresenation,
    start_index: usize,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    segments_in_flight: usize,
    soft_end_exclusive: Arc<AtomicUsize>,
    // Notify `primed` once this many segments are buffered. `usize::MAX` means
    // "never signal" — used for the initial pipeline, which has no OLD to
    // overlap and so decodes immediately.
    prime_target: usize,
) -> Result<VideoPrefetch, Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(segments_in_flight);

    let init_dl = repr
        .segment_init
        .download(&http, RequestKind::InitSegment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("init download: {}", e).into() })?;
    let init_data = init_dl.data;

    let hvcc_nalus = parse_hvcc_nalus(&init_data)
        .ok_or_else(|| -> Box<dyn Error + Send + Sync> { "no hvcC in init segment".into() })?;

    // Dolby Vision policy: profiles 7/8 carry a decodable HEVC base layer
    // (HDR10/SDR/HLG-compatible, correctly signalled in the SPS VUI), so
    // they can always play through the normal Main10 + tonemap path with
    // the RPU NAL dropped. On Android the decoder additionally tries the
    // platform `video/dolby-vision` codec (full DV, RPU kept) and only
    // falls back to the base layer. Profile 5 (IPTPQc2) has NO compatible
    // base layer — it is only playable where a real DV decoder exists
    // (Android direct mode); elsewhere refuse up front with an actionable
    // error instead of rendering green/purple garbage.
    let dovi_profile = crate::crypto::parse_dovi_config(&init_data).map(|dovi| {
        log::info!(
            "video: Dolby Vision profile {}.{} (rpu={} el={} bl={} compat_id={})",
            dovi.profile,
            dovi.level,
            dovi.rpu_present,
            dovi.el_present,
            dovi.bl_present,
            dovi.bl_signal_compatibility_id
        );
        if dovi.el_present {
            log::warn!(
                "video: DV enhancement layer present — ignored unless the platform DV decoder picks it up"
            );
        }
        dovi.profile
    });
    if let Some(p) = dovi_profile {
        if !matches!(p, 7 | 8) && !cfg!(target_os = "android") {
            return Err(format!(
                "Dolby Vision profile {} has no backward-compatible base layer \
                 (needs a platform DV decoder) — unsupported on this target",
                p
            )
            .into());
        }
    }

    // Colour info comes from the SPS VUI — the MPD is not trustworthy here
    // (our test stream signals BT.709 on PQ representations). Fall back to
    // the hvcC bit depth when the SPS doesn't parse.
    let sps_color = crate::parsers::hevc::parse_sps_color_info(&hvcc_nalus);
    let color = VideoColorInfo::from_sps(sps_color, parse_hvcc_bit_depth(&init_data));
    if color.bit_depth != 8 || color.is_hdr() {
        log::info!(
            "video: {}-bit, transfer={:?}, bt2020={}, full_range={} (SPS VUI {})",
            color.bit_depth,
            color.transfer,
            color.bt2020,
            color.full_range,
            if sps_color.is_some() { "parsed" } else { "missing — hvcC fallback" },
        );
    }

    let track_crypto = setup_track_crypto(&init_data, decryptor, "video").await?;

    let segments = repr.segments.clone();
    let primed = Arc::new(Notify::new());
    let dl_stats = Arc::clone(&stats);
    let cb_primed = Arc::clone(&primed);
    let downloaded = Arc::new(AtomicUsize::new(0));
    // Only the initial / OLD pipeline (prime_target == MAX) advances the
    // download-time decoded-PTS high-water. A swap-prefetch must NOT touch it:
    // the supervisor reads last_decoded_pts_ms at OLD teardown as the splice
    // point, and NEW's downloaded-ahead segments would otherwise inflate it.
    // The INITIAL pipeline publishes the shared gauge from the start; a swap
    // prefetch must not, or its downloaded-ahead segments would inflate the
    // figure while OLD is still the one playing. It takes over at teardown
    // (`take_over_buffer_gauge`) instead of being excluded for good.
    let track_dl = Arc::new(AtomicBool::new(prime_target == usize::MAX));
    let dl_pts_ms = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let cb_track_dl = Arc::clone(&track_dl);
    let cb_dl_pts = Arc::clone(&dl_pts_ms);
    let on_video_dl: SegmentDoneCallback = Arc::new(move |pts_ms| {
        // Own high-water first: it is what this pipeline hands over.
        if pts_ms > cb_dl_pts.load(Ordering::Relaxed) {
            cb_dl_pts.store(pts_ms, Ordering::Relaxed);
        }
        if cb_track_dl.load(Ordering::Relaxed) {
            let prev = dl_stats.last_decoded_pts_ms.load(Ordering::Relaxed);
            if pts_ms > prev {
                dl_stats.last_decoded_pts_ms.store(pts_ms, Ordering::Relaxed);
            }
        }
        // `notify_one` (not `notify_waiters`) so the permit survives even if
        // the supervisor hasn't reached its `.notified()` await yet — avoids a
        // lost-wakeup race when a small segment finishes downloading fast.
        let n = downloaded.fetch_add(1, Ordering::Relaxed) + 1;
        if n == prime_target {
            cb_primed.notify_one();
        }
    });
    let download_handle = task::spawn(download_task(
        segments,
        start_index,
        download_tx,
        stop,
        stop_flag,
        http,
        Some(Arc::clone(&stats)),
        Some(on_video_dl),
        soft_end_exclusive,
    ));

    Ok(VideoPrefetch {
        width: repr.width,
        height: repr.height,
        init_data,
        hvcc_nalus,
        color,
        dovi_profile,
        track_crypto,
        download_rx,
        download_handle,
        primed,
        first_prepared: None,
        dl_pts_ms,
        track_dl,
    })
}

/// Carried into the decode half on an ABR swap so NEW splices cleanly onto
/// OLD's tail. NEW is started on the segment *containing* the current playback
/// position (its keyframe is therefore at/before the splice), then the decoder
/// drops every frame at/below `skip_below_pts_us` — the absolute PTS OLD last
/// rendered — so NEW's first *emitted* frame is the next one forward. That
/// avoids both a backward rewind AND a future-PTS frame that av_sync would
/// sit and wait for (the multi-second freeze seen on a 1080→4K upswitch, where
/// NEW used to start a whole segment ahead). `started` is for the timing log.
/// Switch the video plane's display refresh rate to match the content fps
/// (adaptive frame rate). Prefers `ANativeWindow_setFrameRateWithChangeStrategy`
/// (API 31) with the ALWAYS strategy so the panel *actually* changes mode
/// (e.g. 60 -> 24 Hz) — plain `ANativeWindow_setFrameRate` is seamless-only,
/// and on most TVs a 60->24 switch is NOT a seamless transition, so it would
/// silently never change. ALWAYS costs a brief blink at start/stop, which is
/// the expected "match content frame rate" behaviour. Falls back to the
/// seamless `setFrameRate` on API 30. Both symbols live in libnativewindow.so
/// and are dlsym'd (ndk-sys doesn't link it; a build-time extern broke the
/// 32-bit Streamer .so load — same lesson as `ANativeWindow_setBuffersDataSpace`).
/// No-op below API 30.
#[cfg(target_os = "android")]
pub(super) fn set_window_frame_rate(window: usize, fps: f32) {
    use std::sync::OnceLock;
    if window == 0 || !(fps > 0.0) {
        return;
    }
    unsafe fn resolve(name: &[u8]) -> *mut libc::c_void {
        // libEGL usually pulled libnativewindow into the process already, so
        // RTLD_DEFAULT finds the symbol without bumping refcounts; else dlopen.
        let mut s = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const _);
        if s.is_null() {
            let lib = libc::dlopen(
                b"libnativewindow.so\0".as_ptr() as *const _,
                libc::RTLD_NOW | libc::RTLD_GLOBAL,
            );
            if !lib.is_null() {
                s = libc::dlsym(lib, name.as_ptr() as *const _);
            }
        }
        s
    }
    // int32_t (ANativeWindow*, float rate, int8_t compatibility, int8_t strategy)
    type WithStrategyFn = unsafe extern "C" fn(*mut std::ffi::c_void, f32, i8, i8) -> i32;
    // int32_t (ANativeWindow*, float rate, int8_t compatibility)
    type BasicFn = unsafe extern "C" fn(*mut std::ffi::c_void, f32, i8) -> i32;
    enum FrameRateFn {
        WithStrategy(WithStrategyFn),
        Basic(BasicFn),
    }
    static SYM: OnceLock<Option<FrameRateFn>> = OnceLock::new();
    let f = SYM.get_or_init(|| unsafe {
        let with_strategy = resolve(b"ANativeWindow_setFrameRateWithChangeStrategy\0");
        if !with_strategy.is_null() {
            return Some(FrameRateFn::WithStrategy(
                std::mem::transmute::<*mut libc::c_void, WithStrategyFn>(with_strategy),
            ));
        }
        let basic = resolve(b"ANativeWindow_setFrameRate\0");
        if basic.is_null() {
            log::warn!("[afr] ANativeWindow_setFrameRate* unavailable (API < 30) — adaptive frame rate off");
            None
        } else {
            Some(FrameRateFn::Basic(
                std::mem::transmute::<*mut libc::c_void, BasicFn>(basic),
            ))
        }
    });
    // compatibility = 1 (FIXED_SOURCE): the source has a fixed inherent rate.
    // strategy = 1 (ALWAYS): switch even when not seamless.
    let win = window as *mut std::ffi::c_void;
    match f {
        Some(FrameRateFn::WithStrategy(f)) => {
            let rc = unsafe { f(win, fps, 1, 1) };
            if rc == 0 {
                log::info!("[afr] switching display to {:.3} fps (fixed-source, always)", fps);
            } else {
                log::warn!("[afr] setFrameRateWithChangeStrategy({:.3}) returned {}", fps, rc);
            }
        }
        Some(FrameRateFn::Basic(f)) => {
            let rc = unsafe { f(win, fps, 1) };
            if rc == 0 {
                log::info!("[afr] hinted {:.3} fps (seamless-only, API 30)", fps);
            } else {
                log::warn!("[afr] setFrameRate({:.3}) returned {}", fps, rc);
            }
        }
        None => {}
    }
}

/// Owns the direct-mode video-plane `ANativeWindow` for the player's lifetime.
///
/// The host hands us a raw `ANativeWindow*` via `set_video_output_window`. We
/// take an `ANativeWindow_acquire` reference on it and only release when the
/// last `Player` clone drops (or the host installs a different window). Without
/// that owned ref the host's `SurfaceView` teardown could free the window — and
/// its internal mutex — while a pipeline rebuild is still asserting AFR on it,
/// crashing in `ANativeWindow_setFrameRate*` → `Surface::hook_query` with
/// "pthread_mutex_lock on a destroyed mutex" (SIGABRT). Holding the ref keeps
/// the window object alive, so a stale `setFrameRate` is at worst a no-op, not a
/// use-after-free. The pointer is still stored as a `usize` for the lock-free
/// reads on the build path; this type just bolts lifetime onto it.
pub(super) struct DirectWindow(AtomicUsize);

impl DirectWindow {
    pub(super) fn new() -> Self {
        DirectWindow(AtomicUsize::new(0))
    }

    /// Current window pointer (0 = none / classic renderer path).
    pub(super) fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Install `window` (0 to clear), acquiring it and releasing the previous.
    /// Balanced per call: each `set` releases exactly the ref the prior `set`
    /// acquired, so repeated identical sets don't leak or over-release.
    pub(super) fn set(&self, window: usize) {
        #[cfg(target_os = "android")]
        unsafe {
            if window != 0 {
                ndk_sys::ANativeWindow_acquire(window as *mut ndk_sys::ANativeWindow);
            }
            let old = self.0.swap(window, Ordering::Relaxed);
            if old != 0 {
                ndk_sys::ANativeWindow_release(old as *mut ndk_sys::ANativeWindow);
            }
        }
        #[cfg(not(target_os = "android"))]
        self.0.store(window, Ordering::Relaxed);
    }
}

impl Drop for DirectWindow {
    fn drop(&mut self) {
        #[cfg(target_os = "android")]
        unsafe {
            let w = self.0.load(Ordering::Relaxed);
            if w != 0 {
                ndk_sys::ANativeWindow_release(w as *mut ndk_sys::ANativeWindow);
            }
        }
    }
}

pub(super) struct SwapSplice {
    pub(super) started: Instant,
    pub(super) skip_below_pts_us: i64,
}

/// Decode half: configure the HW decoder and run [`video_decoder_task`] against
/// the segments [`video_prefetch`] is already streaming, joining both halves to
/// completion. `decoder` (the scarce HW slot) must be created by the caller
/// only *after* any previous representation's decoder has been dropped.
pub(super) async fn run_decode(
    pf: VideoPrefetch,
    sender: Sender<DecodedVideoFrame>,
    video_ready: Arc<Notify>,
    mut decoder: Box<dyn HwVideoDecoder>,
    stats: Arc<StatsState>,
    decoder_stop_flag: Arc<AtomicBool>,
    // `Some(..)` on an ABR swap (splice trim + timing), `None` initially.
    splice: Option<SwapSplice>,
    // Android direct mode video window (0 = renderer path).
    direct_window: usize,
    // Player-level HDR-to-8-bit decode switch, sampled here at configure
    // time so ABR swaps / retries pick up a changed value.
    hdr_decode_8bit: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    decoder.configure(VideoDecoderParams {
        codec: VideoCodec::Hevc,
        width: pf.width,
        height: pf.height,
        hvcc_nalus: pf.hvcc_nalus,
        color: pf.color,
        direct_window,
        dovi_profile: pf.dovi_profile,
        force_8bit_hdr: hdr_decode_8bit.load(Ordering::Relaxed),
    })?;
    // Direct mode: let the input-buffer spin observe teardown so a seek /
    // track-switch can't strand the decode task in the spin (see [B] in
    // mediacodec submit_direct).
    decoder.set_stop_signal(decoder_stop_flag.clone());

    let decoder_task = task::spawn(video_decoder_task(
        pf.download_rx,
        sender,
        decoder,
        pf.init_data,
        video_ready,
        pf.track_crypto,
        stats,
        decoder_stop_flag,
        splice,
        pf.first_prepared,
    ));

    let (dl_res, dec_res) = join!(pf.download_handle, decoder_task);
    // Propagate failures so the supervisor's bounded retry actually fires —
    // logging alone turned real download/decode deaths into "natural EOF".
    // Download first: a decoder error is usually just the consequence of the
    // feed dying.
    let dl_err = flatten_task_result("video download_task", dl_res);
    let dec_err = flatten_task_result("video decoder_task", dec_res);
    match dl_err.or(dec_err) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Log and collapse a joined task result into `Some(error)` when either the
/// task itself failed (panic/abort) or it returned `Err`.
pub(super) fn flatten_task_result<T>(
    name: &str,
    result: Result<Result<T, Box<dyn Error + Send + Sync>>, tokio::task::JoinError>,
) -> Option<Box<dyn Error + Send + Sync>> {
    match result {
        Ok(Ok(_)) => None,
        Ok(Err(e)) => {
            log::error!("{}: {}", name, e);
            Some(e)
        }
        Err(e) => {
            log::error!("{}: join error: {}", name, e);
            Some(format!("{name} panicked: {e}").into())
        }
    }
}

pub(super) async fn video_play(
    video_representation: VideoRepresenation,
    start_index: usize,
    video_ready: Arc<Notify>,
    sender: Sender<DecodedVideoFrame>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    decoder: Box<dyn HwVideoDecoder>,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    segments_in_flight: usize,
    // See `download_task::soft_end_exclusive`. Plumbed through so the
    // supervisor can softly cap an old pipeline mid-flight without
    // discarding its already-decoded tail.
    soft_end_exclusive: Arc<AtomicUsize>,
    // Android direct mode video window (0 = renderer path).
    direct_window: usize,
    hdr_decode_8bit: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Initial pipeline: nothing to overlap with, so download and decode run
    // back to back. `prime_target = MAX` → the readiness signal never fires
    // (only the supervisor's swap path consumes it).
    let pf = video_prefetch(
        &video_representation,
        start_index,
        stop,
        Arc::clone(&stop_flag),
        decryptor,
        http,
        Arc::clone(&stats),
        segments_in_flight,
        soft_end_exclusive,
        usize::MAX,
    )
    .await?;
    run_decode(
        pf,
        sender,
        video_ready,
        decoder,
        stats,
        stop_flag,
        None,
        direct_window,
        hdr_decode_8bit,
    )
    .await
}

pub(super) async fn audio_play(
    audio_representation: AudioRepresentation,
    start_index: usize,
    audio_ready: Arc<Notify>,
    sender: Sender<DecodedAudioFrame>,
    output_sample_rate: u32,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    mut decoder: Box<dyn AudioDecoder>,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    segments_in_flight: usize,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(segments_in_flight);

    let init_dl = audio_representation
        .segment_init
        .download(&http, RequestKind::InitSegment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("audio init download: {}", e).into() })?;
    let init_data = init_dl.data;

    let track_crypto = setup_track_crypto(&init_data, decryptor, "audio").await?;

    let codecs_str = audio_representation.codecs.as_str();
    let codec = if codecs_str.starts_with("mp4a") {
        AudioCodec::Aac
    } else if codecs_str == "ec-3" {
        AudioCodec::Eac3
    } else if codecs_str == "ac-3" {
        AudioCodec::Ac3
    } else {
        return Err(format!("Unsupported audio codec: {}", codecs_str).into());
    };

    // AAC carries its codec params in `esds` (AudioSpecificConfig), and both
    // FFmpeg and MediaCodec want those 2 bytes as extradata / csd-0 to open
    // the decoder. AC-3 and EAC-3 are self-describing (each frame begins with
    // a syncinfo header), so the decoder just needs the MIME plus the
    // sample-rate/channel hints from the DASH manifest.
    let (input_sample_rate, input_channels, codec_specific_data) = match codec {
        AudioCodec::Aac => {
            let aac_config = parse_aac_config(&init_data)
                .ok_or("Audio codec not supported (no AAC config in init segment)")?;
            let rate = aac_sampling_frequency_index_to_u32(aac_config.freq_index);
            let ch = aac_config.chan_conf as u16;
            let dsi: [u8; 2] = [
                (aac_config.profile << 3) | (aac_config.freq_index >> 1),
                ((aac_config.freq_index & 0x01) << 7) | (aac_config.chan_conf << 3),
            ];
            log::info!(
                "audio: AAC profile={} freq_index={} (={}Hz) chan_conf={}",
                aac_config.profile, aac_config.freq_index, rate, aac_config.chan_conf
            );
            (rate, ch, dsi.to_vec())
        }
        AudioCodec::Ac3 | AudioCodec::Eac3 => {
            let rate = audio_representation.audio_sampling_rate;
            let ch = audio_representation.channels.unwrap_or(2) as u16;
            log::info!("audio: {:?} {}Hz {}ch", codec, rate, ch);
            (rate, ch, Vec::new())
        }
    };

    decoder.configure(AudioDecoderParams {
        codec,
        input_sample_rate,
        input_channels,
        output_sample_rate,
        codec_specific_data,
    })?;

    let segments = audio_representation.segments.clone();
    let dl_stats = Arc::clone(&stats);
    let on_audio_dl: SegmentDoneCallback = Arc::new(move |pts_ms| {
        let prev = dl_stats.audio_last_decoded_pts_ms.load(Ordering::Relaxed);
        if pts_ms > prev {
            dl_stats
                .audio_last_decoded_pts_ms
                .store(pts_ms, Ordering::Relaxed);
        }
    });
    let download_task = task::spawn(download_task(
        segments,
        start_index,
        download_tx,
        stop,
        stop_flag,
        http,
        Some(Arc::clone(&stats)),
        Some(on_audio_dl),
        // Audio doesn't ABR-switch in this player (single audio rep per
        // session), so the soft-end mechanism is unused — usize::MAX
        // disables the early break.
        Arc::new(AtomicUsize::new(usize::MAX)),
    ));
    let decoder_task = task::spawn(audio_decoder_task(
        download_rx,
        sender,
        decoder,
        init_data,
        audio_ready,
        track_crypto,
        stats,
    ));

    let (dl_res, dec_res) = join!(download_task, decoder_task);
    let dl_err = flatten_task_result("audio download_task", dl_res);
    let dec_err = flatten_task_result("audio decoder_task", dec_res);
    match dl_err.or(dec_err) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Audio passthrough feed: download + decrypt the audio segments, slice the
/// compressed access units out of the mp4 samples and write them straight to
/// the bitstream sink (no decode, no PCM channel). Mirrors `audio_play`'s
/// download path; the decoder + `audio_sync_loop` are bypassed. Pre-target AUs
/// are dropped so audio begins at the seek target, matching video's
/// frame-accurate discard, so A/V line up under the passthrough clock.
#[cfg(target_os = "android")]
pub(super) async fn audio_passthrough_play(
    audio_representation: AudioRepresentation,
    start_index: usize,
    sink: Arc<dyn crate::renderers::AudioPassthrough>,
    audio_ready: Arc<Notify>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    segments_in_flight: usize,
    discard_below_us: i64,
    pipeline_live: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (download_tx, download_rx) = mpsc::channel::<DataSegment>(segments_in_flight);

    let init_dl = audio_representation
        .segment_init
        .download(&http, RequestKind::InitSegment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> {
            format!("audio init download: {}", e).into()
        })?;
    let init_data = init_dl.data;
    let track_crypto = setup_track_crypto(&init_data, decryptor, "audio").await?;

    let segments = audio_representation.segments.clone();
    let download = task::spawn(download_task(
        segments,
        start_index,
        download_tx,
        stop,
        stop_flag.clone(),
        http,
        Some(Arc::clone(&stats)),
        None,
        Arc::new(AtomicUsize::new(usize::MAX)),
    ));
    let feed = task::spawn(audio_passthrough_task(
        download_rx,
        sink,
        init_data,
        audio_ready,
        track_crypto,
        stop_flag,
        discard_below_us,
        pipeline_live,
    ));
    let (dl_res, feed_res) = join!(download, feed);
    log_task_result("audio download_task (passthrough)", dl_res);
    log_task_result("audio passthrough_task", feed_res);
    Ok(())
}

#[cfg(target_os = "android")]
pub(super) async fn audio_passthrough_task(
    mut receiver: Receiver<DataSegment>,
    sink: Arc<dyn crate::renderers::AudioPassthrough>,
    init_data: Vec<u8>,
    audio_ready: Arc<Notify>,
    track_crypto: Option<TrackCrypto>,
    stop_flag: Arc<AtomicBool>,
    discard_below_us: i64,
    pipeline_live: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut first_au_written = false;
    let mut au_count = 0u64;
    let mut base_pts_ms: i64 = 0;
    while let Some(segment) = receiver.recv().await {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let mut data_vec = init_data.clone();
        data_vec.extend_from_slice(&segment.data[..]);
        decrypt_segment_in_place(&mut data_vec, track_crypto.as_ref())?;

        let sample_info: Vec<(usize, usize, i64, u64)> = {
            let mp4 = Mp4::read_bytes(&data_vec)
                .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("mp4: {}", e).into() })?;
            let (_id, track) = mp4
                .tracks()
                .first_key_value()
                .ok_or_else(|| -> Box<dyn Error + Send + Sync> { "no track".into() })?;
            track
                .samples
                .iter()
                .map(|s| (s.offset as usize, s.size as usize, s.composition_timestamp, s.timescale))
                .collect()
        };

        for (offset, size, ts, ts_scale) in sample_info {
            if stop_flag.load(Ordering::Relaxed) {
                return Ok(());
            }
            if offset + size > data_vec.len() {
                continue;
            }
            let pts_us = if ts_scale > 0 { ts * 1_000_000 / ts_scale as i64 } else { 0 };
            // Frame-accurate seek: drop AUs before the target.
            if pts_us < discard_below_us {
                continue;
            }
            let au_ms = pts_us / 1000;
            if !first_au_written {
                // Start audio only once video is live (pipeline_live), so the
                // two begin together. Otherwise the feed plays the first AU
                // immediately while video is still ~1-2s from its first frame,
                // and audio runs that far ahead for the whole pipeline.
                while !pipeline_live.load(Ordering::Relaxed) {
                    if stop_flag.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                base_pts_ms = au_ms;
            }
            // Two-phase feed. PRIME: an E-AC-3 *direct* AudioTrack does not
            // begin output — `getTimestamp` stays false / `played_ms` reads 0 —
            // until enough compressed data is buffered to cross its start
            // threshold (empirically well over the old 200 ms window; ~1–2 s of
            // media on the Google TV Streamer + HDMI AVR). So until the playback
            // head first moves we DON'T gate on it: we just keep writing, and
            // `AudioTrack.write` (blocking, STREAM mode) back-pressures on the
            // track's own buffer once it fills. Gating on the head before it has
            // started is the deadlock we hit: 200 ms never reached the threshold
            // → head never moved → feed waited forever → the video clock (which
            // reads `played_ms`) froze and the decoder back-pressured to ~1 fps.
            //
            // STEADY: once the head is moving, pace to keep the write at most
            // AHEAD_MS of media ahead of the real playback position, so the head
            // stays a usable clock and seek teardown doesn't strand seconds of
            // queued audio. (Wall-pacing instead buffered the whole startup gap
            // ahead — write_ahead≈1970 ms — which is why we pace on the head.)
            const AHEAD_MS: i64 = 750;
            // Upper bound on PRIME (NOT while paused — see below). Catches the
            // runaway: a stale duplicate feed left over from a pipeline rebuild
            // whose sink never becomes the active output, so its head stays 0
            // and its write() never blocks → it buffers forever (write_ahead →
            // minutes, the observed leak after an audio-track switch). Generous
            // because a *live* but slowly-locking output (an HDMI AVR waking
            // from standby) also shows head 0 with non-blocking writes for a
            // while — seen >10 s on a cold soundbar — and abandoning a live
            // pipeline strands it with no recovery. 30 s clears realistic AVR
            // wake yet still bounds a dead sink to seconds, not minutes.
            const PRIME_MAX_AHEAD_MS: i64 = 30_000;
            // While paused before the head starts, buffer only this far then
            // idle: a paused/not-yet-started track's write() does NOT block, so
            // without a cap the feed would run the buffer away (and a head still
            // at 0 *because we're paused* would trip the dead-sink abandon
            // below). This is enough to cross the start threshold so resume
            // plays immediately.
            const PRIME_PAUSED_TARGET_MS: i64 = 3_000;
            // Stall watchdog: in steady playback the head should drain the
            // buffer at ~real time. If over STALL_WINDOW_MS it advances less
            // than STALL_MIN_ADVANCE_MS (well under 25% of real time) while we
            // hold data and aren't paused, the output has wedged — the head, and
            // the video clock paced to it, would crawl until the next seek
            // rebuilds the track. (Observed on 4K/ec-3: head ~2% of real time,
            // video at 1 fps, only a seek recovered it.)
            const STALL_WINDOW_MS: u64 = 3_000;
            const STALL_MIN_ADVANCE_MS: i64 = 750;
            let mut chk_played = sink.played_ms().unwrap_or(0) as i64;
            let mut chk_wall = Instant::now();
            loop {
                if stop_flag.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let played = sink.played_ms().unwrap_or(0) as i64;
                // Prime phase: head not started yet → write now (write() blocks
                // on the full track buffer for backpressure; the buffer is sized
                // above the start threshold so we can reach it without blocking).
                if played == 0 {
                    // Paused before the head started: head reads 0 because we're
                    // paused, not because the sink is dead. Buffer a little for a
                    // prompt resume, then wait — do NOT abandon.
                    if sink.is_paused() {
                        if au_ms - base_pts_ms >= PRIME_PAUSED_TARGET_MS {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            continue;
                        }
                        break;
                    }
                    if au_ms - base_pts_ms > PRIME_MAX_AHEAD_MS {
                        log::warn!(
                            "[audio-pt] playback head never started after {}ms buffered — \
                             abandoning passthrough feed (stale pipeline or unsupported output)",
                            au_ms - base_pts_ms
                        );
                        return Ok(());
                    }
                    break;
                }
                // Steady phase: within the ahead budget → write; else wait for
                // playback to drain it.
                if au_ms - (base_pts_ms + played) <= AHEAD_MS {
                    break;
                }
                // We have data buffered but the head isn't taking it. Measure the
                // head rate over STALL_WINDOW_MS; if it's crawling (and we aren't
                // paused) the track wedged — nudge it and log the ground truth.
                if chk_wall.elapsed() >= Duration::from_millis(STALL_WINDOW_MS) {
                    let advanced = played - chk_played;
                    let paused = sink.is_paused();
                    if !paused && advanced < STALL_MIN_ADVANCE_MS {
                        let (have_ts, frame_pos) = sink.head_debug();
                        log::warn!(
                            "[audio-pt] STALL: head +{}ms in {}ms (played={}ms write_ahead={}ms \
                             paused={} have_ts={} frame_pos={}) — nudging (pause→play)",
                            advanced, chk_wall.elapsed().as_millis(), played,
                            au_ms - (base_pts_ms + played), paused, have_ts, frame_pos
                        );
                        sink.recover_stall();
                    }
                    chk_played = played;
                    chk_wall = Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // `block_in_place`: hand this worker's other tasks (the video decode
            // pipeline!) to a sibling worker while the JNI write runs.
            let au = &data_vec[offset..offset + size];
            tokio::task::block_in_place(|| sink.write(au));
            au_count += 1;
            if !first_au_written {
                log::debug!("[audio-pt] first AU written (au_ms={}), play() armed", au_ms);
                audio_ready.notify_one();
                first_au_written = true;
            }
            if au_count % 48 == 0 {
                let head = base_pts_ms + sink.played_ms().unwrap_or(0) as i64;
                log::info!(
                    "[audio-pt] au={}ms head={}ms write_ahead={}ms",
                    au_ms, head, au_ms - head
                );
            }
        }
    }
    Ok(())
}

