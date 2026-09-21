//! ABR video supervisor: owns the video pipeline inside one `play()` call and
//! performs make-before-break representation swaps.

use super::*;

// ---------------------------------------------------------------------------
// Video supervisor — owns the video pipeline lifecycle within one play()
// ---------------------------------------------------------------------------

/// Factory closure that produces a fresh `HwVideoDecoder`. The supervisor
/// invokes it once per spawned `video_play` (i.e. once per representation),
/// so the platform-specific decoder type stays out of this module.
pub(super) type VideoDecoderFactory = Arc<dyn Fn() -> Box<dyn HwVideoDecoder> + Send + Sync>;

/// Signal one half of a pipeline to stop: raise its flag and wake whatever is
/// parked on its notify.
///
/// The two always travel together — raising the flag alone leaves a task
/// asleep until its next timeout, and notifying alone lets it loop straight
/// back in. Nine call sites did both by hand.
fn signal_stop(flag: &AtomicBool, stop: &Notify) {
    flag.store(true, Ordering::Relaxed);
    stop.notify_waiters();
}

/// Long-lived task that owns the video pipeline for a single `play()` call.
/// It runs one representation's decode at a time and switches on ABR request.
///
/// Make-before-break soft-switch flow on receiving a new representation:
///   1. Compute the next segment in the NEW representation that starts after
///      the current playback PTS, so the first new frame can't land before the
///      last old frame on the timeline.
///   2. Prefetch NEW via [`video_prefetch`] — download its init + first
///      segment(s) into a buffer — WHILE the OLD pipeline keeps decoding and
///      feeding av_sync. Prefetch touches only the network, never the HW
///      decoder, so it can't collide with OLD's live decoder slot.
///   3. Once NEW has buffered `PRIME_TARGET` segments (or a timeout elapses),
///      tear OLD down: drop `soft_end`/stop_flag so its download + decode exit
///      and the single HW decoder slot frees.
///   4. Start NEW's decode ([`run_decode`]) from the prefetched buffer. Decode
///      is still sequential w.r.t. OLD — some HW decoder paths (Intel D3D11VA,
///      certain MediaCodec drivers) won't allocate a second instance while the
///      first is live — but because the segments are already local, NEW's first
///      frame is just a configure + first-GOP decode away, well inside OLD's
///      ~8-frame (~333 ms) buffer drain. So av_sync never starves and the swap
///      no longer shows the buffering freeze it did when the whole NEW startup
///      (download included) happened only after OLD stopped.
///
/// Audio keeps playing throughout — only the video pipeline is touched.
pub(super) async fn video_supervisor(
    // DIAG: pipeline generation id (see video_sync_loop).
    gen: u64,
    initial_repr: VideoRepresenation,
    initial_start_index: usize,
    frame_sender: Sender<DecodedVideoFrame>,
    video_ready: Arc<Notify>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    decryptor: Option<Arc<dyn Decryptor>>,
    decoder_factory: VideoDecoderFactory,
    http: Arc<HttpClient>,
    stats: Arc<StatsState>,
    mut switch_rx: tokio::sync::watch::Receiver<Option<VideoRepresenation>>,
    position_ms: Arc<AtomicU64>,
    events: Arc<broadcast::Sender<PlayerEvent>>,
    segments_in_flight: usize,
    // Content origin (first segment's absolute presentation time). position_ms
    // is 0-based, so we add this back to locate segments on a soft swap.
    origin: Duration,
    // Android direct mode video window (0 = renderer path).
    direct_window: usize,
    hdr_decode_8bit: Arc<AtomicBool>,
    // Resume slot written when retries are exhausted: the NEXT play() call
    // starts from this position instead of zero ("continue where we
    // stopped" semantics for the consumer's manual retry).
    pending_resume: Arc<StdMutex<Option<Duration>>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Pipeline failures (network death mid-stream, decoder errors) are
    // retried from the current playback position with backoff. The
    // counter resets once playback makes real progress, so a long movie
    // surviving three separate hiccups hours apart keeps recovering, while
    // a hard failure (dead URL, broken stream) exhausts quickly and
    // surfaces as PlayerEvent::Error instead of a fake EndOfStream.
    const MAX_PIPELINE_RETRIES: u32 = 3;
    const PROGRESS_RESET_MS: u64 = 10_000;
    let mut retry_attempt: u32 = 0;
    let mut last_fail_pos_ms: u64 = 0;
    let spawn_pipeline = |repr: VideoRepresenation,
                          start_index: usize,
                          local_stop: Arc<Notify>,
                          local_stop_flag: Arc<AtomicBool>|
     -> (
        crate::rt::JoinHandle<Result<(), Box<dyn Error + Send + Sync>>>,
        Arc<AtomicUsize>,
    ) {
        let soft_end = Arc::new(AtomicUsize::new(usize::MAX));
        let handle = crate::rt::spawn(video_play(
            repr,
            start_index,
            video_ready.clone(),
            frame_sender.clone(),
            local_stop,
            local_stop_flag,
            decryptor.clone(),
            decoder_factory(),
            Arc::clone(&http),
            Arc::clone(&stats),
            segments_in_flight,
            soft_end.clone(),
            direct_window,
            Arc::clone(&hdr_decode_8bit),
        ));
        (handle, soft_end)
    };

    // The pipeline currently feeding av_sync. `cur_*` are reassigned on each
    // swap. An ABR switch prefetches the NEW representation (network only — no
    // HW decoder) WHILE this one keeps decoding, then tears OLD down and brings
    // NEW's decode up from the prefetched buffer. That keeps av_sync fed across
    // the swap so it never hits its 300 ms starvation pause (the visible
    // "buffering" freeze): the gap shrinks from a full download + decode to a
    // local configure + first-GOP decode.
    let mut current_repr = initial_repr;
    let mut cur_stop = Arc::new(Notify::new());
    let mut cur_flag = Arc::new(AtomicBool::new(false));
    log::info!("[video gen {}] supervisor start (repr={} idx={})", gen, current_repr.id, initial_start_index);
    let (mut cur_handle, mut cur_soft_end) = spawn_pipeline(
        current_repr.clone(),
        initial_start_index,
        cur_stop.clone(),
        cur_flag.clone(),
    );

    // Segments to buffer in NEW before tearing OLD down. One segment is
    // seconds of frames, which a HW decoder chews through far faster than
    // real-time, so NEW's first frame reaches av_sync inside OLD's ~8-frame
    // (~333 ms) buffer drain.
    const PRIME_TARGET: usize = 1;
    // Cap on how long to wait for that prefetch. Past this we swap anyway and
    // accept (for this one swap) the old freeze rather than stalling forever on
    // a dead/slow segment.
    const PRIME_TIMEOUT: Duration = Duration::from_secs(8);
    // How long av_sync tolerates the teardown→first-NEW-frame hole before its
    // starvation detector may flip into Buffering. Configure + first-GOP is
    // ~0.3–0.8 s (worst on MediaCodec direct); comfortably inside this. A swap
    // that's genuinely wedged outlives the grace and buffers honestly.
    const SWAP_GRACE: Duration = Duration::from_secs(3);

/// Measured CENC decrypt throughput on the slowest hardware we target (a
/// Google TV Streamer does ~16 MB/s: 11 MiB in ~680 ms). Used only to decide
/// how much lead a swap needs before a segment boundary, so erring low just
/// makes the swap pick a later boundary.
const DECRYPT_BYTES_PER_SEC: f64 = 16.0 * 1024.0 * 1024.0;

/// Download throughput the ABR estimator is currently seeing, in BYTES/s.
fn measured_download_bps(stats: &StatsState) -> f64 {
    stats.bandwidth_bps_ewma.load(Ordering::Relaxed) as f64 / 8.0
}

/// How long the swap may hold the OLD rung at the segment boundary waiting for
/// NEW's first segment to finish decrypting. Covers a 14 Mbps 4K segment
/// (~0.65 s at the ~17 MB/s software-AES rate) with headroom; past that
/// something is wrong and dropping a few frames beats stalling the swap.
const PREPARE_READY_BUDGET: Duration = Duration::from_millis(1_200);

    loop {
        // Race: play-level stop, the current pipeline finishing on its own
        // (natural EOF — must propagate so the keepalive frame_sender drops,
        // the channel closes, and av_sync fires EndOfStream), or an ABR switch.
        let new_repr: VideoRepresenation = loop {
            tokio::select! {
                _ = stop.notified() => {
                    signal_stop(&cur_flag, &cur_stop);
                    let _ = cur_handle.await;
                    return Ok(());
                }
                res = &mut cur_handle => {
                    let detail = match res {
                        Ok(Ok(())) => {
                            // Natural EOF — propagate so the keepalive
                            // frame_sender drops, the channel closes, and
                            // av_sync fires EndOfStream.
                            log::info!("[video] supervisor: pipeline exited naturally; closing");
                            return Ok(());
                        }
                        Ok(Err(e)) => e.to_string(),
                        Err(e) => format!("pipeline task panicked: {}", e),
                    };
                    if stop_flag.load(Ordering::Relaxed) {
                        // Teardown raced the failure — not an error.
                        return Ok(());
                    }
                    log::error!("[video] supervisor: pipeline failed: {}", detail);

                    // Bounded retry-with-resume. Progress since the last
                    // failure resets the budget.
                    let pos_now = position_ms.load(Ordering::Relaxed);
                    if pos_now.saturating_sub(last_fail_pos_ms) > PROGRESS_RESET_MS {
                        retry_attempt = 0;
                    }
                    last_fail_pos_ms = pos_now;
                    retry_attempt += 1;
                    stats.pipeline_retries.fetch_add(1, Ordering::Relaxed);
                    if retry_attempt > MAX_PIPELINE_RETRIES {
                        log::error!(
                            "[video] supervisor: {} consecutive pipeline failures — giving up at {}ms",
                            retry_attempt - 1,
                            pos_now
                        );
                        // Park the position for the consumer's next play()
                        // ("continue where we stopped"), surface the error,
                        // and stop WITHOUT the fake EndOfStream (av_sync
                        // checks stop_flag before emitting EOS).
                        *pending_resume.lock().unwrap() =
                            Some(Duration::from_millis(pos_now));
                        let _ = events.send(PlayerEvent::Error {
                            kind: PlayerErrorKind::Decoder,
                            detail,
                        });
                        signal_stop(&stop_flag, &stop);
                        return Err("video pipeline retries exhausted".into());
                    }
                    log::warn!(
                        "[video] supervisor: retrying pipeline ({}/{}) from {}ms",
                        retry_attempt,
                        MAX_PIPELINE_RETRIES,
                        pos_now
                    );
                    // Backoff, abortable by stop. av_sync's starvation
                    // detection keeps the consumer in Buffering meanwhile.
                    tokio::select! {
                        _ = crate::rt::sleep(Duration::from_secs(retry_attempt as u64)) => {}
                        _ = stop.notified() => return Ok(()),
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        return Ok(());
                    }

                    // Respawn from the segment containing the current
                    // position; the splice trims re-decoded frames at/below
                    // the last rendered PTS so playback continues forward
                    // (no rewind). The retry task includes the prefetch, so
                    // a network failure during recovery lands back in this
                    // arm and consumes another attempt.
                    let pos_abs = Duration::from_millis(pos_now) + origin;
                    let resume_idx = find_segment_index(&current_repr.segments, pos_abs);
                    cur_stop = Arc::new(Notify::new());
                    cur_flag = Arc::new(AtomicBool::new(false));
                    cur_soft_end = Arc::new(AtomicUsize::new(usize::MAX));
                    cur_handle = crate::rt::spawn({
                        let repr = current_repr.clone();
                        let stop = cur_stop.clone();
                        let flag = cur_flag.clone();
                        let soft_end = cur_soft_end.clone();
                        let decryptor = decryptor.clone();
                        let http = Arc::clone(&http);
                        let stats = Arc::clone(&stats);
                        let sender = frame_sender.clone();
                        let video_ready = video_ready.clone();
                        let decoder_factory = decoder_factory.clone();
                        let hdr_decode_8bit = Arc::clone(&hdr_decode_8bit);
                        let splice_pts_us = pos_abs.as_micros() as i64;
                        async move {
                            let pf = video_prefetch(
                                &repr,
                                resume_idx,
                                stop,
                                Arc::clone(&flag),
                                decryptor,
                                http,
                                Arc::clone(&stats),
                                segments_in_flight,
                                soft_end,
                                usize::MAX,
                            )
                            .await?;
                            run_decode(
                                pf,
                                sender,
                                video_ready,
                                decoder_factory(),
                                stats,
                                flag,
                                Some(SwapSplice {
                                    started: Instant::now(),
                                    skip_below_pts_us: splice_pts_us,
                                }),
                                direct_window,
                                hdr_decode_8bit,
                            )
                            .await
                        }
                    });
                    continue;
                }
                _ = async {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let _ = switch_rx.changed().await;
                } => {
                    if stop_flag.load(Ordering::Relaxed) {
                        signal_stop(&cur_flag, &cur_stop);
                        let _ = cur_handle.await;
                        return Ok(());
                    }
                    if let Some(new) = switch_rx.borrow_and_update().clone() {
                        break new;
                    }
                    // Spurious None — keep waiting.
                }
            }
        };

        // Avoid swapping to the same representation (the ABR engine guards
        // this too, but explicit is cheap and idempotent).
        if new_repr.id == current_repr.id {
            continue;
        }

        // Switch at the NEXT segment boundary after the current position
        // (position_ms is 0-based; segment start_times are absolute → add
        // origin). NEW starts on that keyframe-aligned boundary and OLD plays
        // up to it (step 2.5), so the two are contiguous: NEW's first frame
        // isn't in the future (no forward-PTS wait) and there's only a tiny
        // ~buffer-sized overlap for the decoder to trim. Starting on the
        // *current* segment instead made NEW decode seconds of throwaway frames
        // to reach the splice and starved av_sync into buffering.
        let pos = Duration::from_millis(position_ms.load(Ordering::Relaxed)) + origin;
        let mut new_start = find_segment_index(&new_repr.segments, pos);
        if new_start + 1 < new_repr.segments.len() {
            new_start += 1;
        }

        // The boundary is whatever the MANIFEST says, so we can also CHOOSE
        // WHICH one. The next boundary can be milliseconds away — the switch
        // request lands at an arbitrary phase — and getting NEW downloaded and
        // decrypted in time is not free: a 14 Mbps segment is ~1.5 MB/s of
        // download plus ~0.7 s of CENC decrypt on a TV SoC. Arriving at the
        // boundary unprepared is exactly what made an upswitch drop ~10 frames.
        //
        // So require real lead, and if the next boundary does not offer it,
        // take the one after: OLD keeps playing at its current quality, which
        // is what make-before-break is for. Bounded to one extra segment.
        //
        // Only when moving UP. A downswitch is usually the buffer asking for
        // help, and delaying it by a whole segment is how you turn a quality
        // drop into a rebuffer.
        let moving_up = new_repr.bandwidth > current_repr.bandwidth;
        if moving_up {
            let boundary_at = new_repr
                .segments
                .get(new_start)
                .map(|seg| seg.start_time())
                .unwrap_or(pos);
            let lead = boundary_at.saturating_sub(pos);
            // Bytes from the manifest (bandwidth x segment duration), turned
            // into time by the two rates that actually bound us.
            let seg_secs = new_repr
                .segments
                .get(new_start)
                .map(|seg| (seg.end_time().saturating_sub(seg.start_time())).as_secs_f64())
                .unwrap_or(0.0);
            let bytes = new_repr.bandwidth as f64 / 8.0 * seg_secs;
            let dl_secs = bytes / measured_download_bps(&stats).max(1.0);
            let prep_secs = bytes / DECRYPT_BYTES_PER_SEC;
            let needed = Duration::from_secs_f64((dl_secs + prep_secs) * 1.3);
            if lead < needed && new_start + 1 < new_repr.segments.len() {
                log::info!(
                    "[abr] next boundary is only {}ms away, need ~{}ms to have                      seg {} ready ({:.1} MiB) — switching one segment later",
                    lead.as_millis(),
                    needed.as_millis(),
                    new_start,
                    bytes / (1024.0 * 1024.0)
                );
                new_start += 1;
            }
        }

        log::info!(
            "[abr] soft switch: repr {} -> {} from seg {} (pos {}ms)",
            current_repr.id, new_repr.id, new_start, pos.as_millis()
        );
        let swap_t0 = Instant::now();

        // --- Make-before-break, step 1: prefetch NEW while OLD keeps playing.
        // `video_prefetch` only downloads — it never allocates the HW decoder,
        // so it can't collide with OLD's live decoder slot.
        let new_stop = Arc::new(Notify::new());
        let new_flag = Arc::new(AtomicBool::new(false));
        let new_soft_end = Arc::new(AtomicUsize::new(usize::MAX));
        let mut new_pf = tokio::select! {
            r = video_prefetch(
                &new_repr,
                new_start,
                new_stop.clone(),
                new_flag.clone(),
                decryptor.clone(),
                Arc::clone(&http),
                Arc::clone(&stats),
                segments_in_flight,
                new_soft_end.clone(),
                PRIME_TARGET,
            ) => match r {
                Ok(pf) => pf,
                Err(e) => {
                    // NEW couldn't even begin downloading — keep OLD playing.
                    log::error!(
                        "[abr] prefetch of repr {} failed; staying on {}: {}",
                        new_repr.id, current_repr.id, e
                    );
                    continue;
                }
            },
            _ = stop.notified() => {
                signal_stop(&cur_flag, &cur_stop);
                let _ = cur_handle.await;
                return Ok(());
            }
            res = &mut cur_handle => {
                // OLD ended before NEW even started; nothing to swap into.
                if let Ok(Err(e)) = res {
                    log::error!("[video] supervisor: pipeline failed during prefetch: {}", e);
                }
                return Ok(());
            }
        };

        // --- step 2: wait until NEW has buffered enough to decode without a
        // network wait. OLD keeps feeding av_sync throughout.
        let primed = Arc::clone(&new_pf.primed);
        let mut old_done = false;
        tokio::select! {
            _ = primed.notified() => {
                log::info!("[abr] NEW primed {}ms after switch", swap_t0.elapsed().as_millis());
                // Get the decrypt of NEW's first segment off the critical path
                // while OLD still has seconds to play (see start_first_prepare).
                new_pf.start_first_prepare();
            }
            _ = crate::rt::sleep(PRIME_TIMEOUT) => {
                log::warn!(
                    "[abr] prefetch prime timed out after {}ms; swapping anyway",
                    swap_t0.elapsed().as_millis()
                );
            }
            _ = stop.notified() => {
                // Tear NEW's prefetch down (the flag makes download_task exit;
                // dropping new_pf closes its channel too) and OLD, then exit.
                signal_stop(&new_flag, &new_stop);
                signal_stop(&cur_flag, &cur_stop);
                let _ = cur_handle.await;
                return Ok(());
            }
            res = &mut cur_handle => {
                // OLD reached EOF while NEW was priming — bring NEW up anyway.
                if let Ok(Err(e)) = res {
                    log::error!("[video] supervisor: pipeline failed during prime: {}", e);
                }
                old_done = true;
            }
        }

        // --- step 2.5: let OLD play up to the chosen boundary so NEW (which
        // starts there) isn't in the future. OLD keeps rendering at its current
        // quality the whole time — no freeze; the switch just lands on a
        // segment boundary, like every production DASH player. Cap OLD's
        // download at the boundary so it can't run on while we wait.
        cur_soft_end.store(new_start, Ordering::Relaxed);
        let boundary_ms = new_repr
            .segments
            .get(new_start)
            .map(|s| s.start_time().as_millis() as u64)
            .unwrap_or(0);

        // --- step 2.6, renderer-path platforms only: warm handoff. Desktop
        // hw decoders (D3D11VA/VAAPI/VideoToolbox) can coexist, unlike
        // Android's single direct-mode MediaCodec slot — so configure NEW and
        // decode its first GOP NOW, while OLD is still rendering toward the
        // boundary, parking the decoded frames behind a gate. After OLD's
        // teardown the gate opens and NEW's frames follow OLD's tail with no
        // decoder spin-up in between. This *removes* the teardown→first-frame
        // hole (~0.5 s of frozen picture on every ABR switch) instead of
        // merely masking it (see swap_grace_deadline, which still covers the
        // direct-mode path and any warm-up that outlives the boundary wait).
        let mut new_pf = Some(new_pf);
        // Only where two concurrent HW decoder instances are verified safe:
        // FFmpeg D3D11VA/VAAPI (Windows/Linux). Android is out — even the GL
        // path decodes via MediaCodec and a second concurrent 4K instance is
        // not guaranteed on TV/mobile SoCs. Apple (VideoToolbox) is out after
        // a field report: the ios-v0.1.7 build with warm handoff enabled
        // flickered — two VT sessions + parked CVPixelBuffers need their own
        // validation before this can be re-enabled there. Everywhere else the
        // swap grace window still hides the switch.
        // Android direct mode is excluded for a reason that is NOT the SoC
        // instance limit (kirkwood advertises four): both decoders would have
        // to produce into the SAME output Surface, and a BufferQueue accepts
        // one producer. The instance count only becomes the question on the
        // Android GL path, where each decoder owns its own ImageReader — and
        // that needs a device on that path to verify before it is turned on,
        // so it stays off rather than assumed.
        let warm_capable =
            cfg!(any(target_os = "windows", target_os = "linux")) && direct_window == 0;
        let warm = if warm_capable && boundary_ms != 0 {
            // Tiny gate on purpose: each held frame pins a surface from the
            // decoder's fixed hw frame pool (D3D11VA/VAAPI), and holding a
            // dozen while nothing drains exhausts the pool — send_packet then
            // fails with ENOMEM and the whole pipeline falls into retry. Two
            // held frames prove the decoder is configured and producing
            // (that's all the warm-up needs); the decode task simply blocks on
            // the gate until release, keeping the pool healthy.
            let (gate_tx, mut gate_rx) =
                tokio::sync::mpsc::channel::<DecodedVideoFrame>(2);
            let release = Arc::new(Notify::new());
            // Buffer-then-forward pump: holds whatever NEW decodes until the
            // supervisor opens the gate (post-teardown), then becomes a plain
            // forwarder into the main frame channel. If NEW's decode dies
            // before release (stop/error), the gate closes and the pump just
            // flushes and exits — the supervisor's retry handles the rest.
            crate::rt::spawn({
                let sender = frame_sender.clone();
                let release = Arc::clone(&release);
                async move {
                    // Do NOT read the gate before release: draining it here
                    // would defeat its backpressure and let NEW decode ahead
                    // unboundedly — every decoded frame pins a surface in the
                    // decoder's FIXED hw frame pool, which exhausts after ~19
                    // frames and kills the pipeline with send_packet ENOMEM.
                    // Parked like this, NEW blocks on the full gate with just
                    // gate+reorder frames outstanding: configured, first GOP
                    // decoded, pool healthy.
                    release.notified().await;
                    while let Some(f) = gate_rx.recv().await {
                        if sender.send(f).await.is_err() {
                            return;
                        }
                    }
                }
            });
            // Splice exactly at the boundary: OLD's download is soft-capped to
            // this segment index, so everything below the boundary is OLD's
            // and NEW keeps the boundary frame up (the trim is `<=`, hence -1).
            let splice_pts_us = boundary_ms as i64 * 1000 - 1;
            let handle = crate::rt::spawn(run_decode(
                new_pf.take().unwrap(),
                gate_tx,
                video_ready.clone(),
                decoder_factory(),
                Arc::clone(&stats),
                new_flag.clone(),
                Some(SwapSplice {
                    started: Instant::now(),
                    skip_below_pts_us: splice_pts_us,
                }),
                direct_window,
                Arc::clone(&hdr_decode_8bit),
            ));
            Some((handle, release))
        } else {
            None
        };
        // Begin the swap a touch before the boundary so NEW's first GOP is
        // decoded by the time OLD's buffered tail drains.
        const BOUNDARY_LEAD_MS: u64 = 150;
        // Never wait forever (e.g. OLD stalls) — cap and switch anyway.
        const MAX_BOUNDARY_WAIT: Duration = Duration::from_secs(15);
        let wait_start = Instant::now();
        while !old_done && boundary_ms != 0 {
            let rendered_abs =
                position_ms.load(Ordering::Relaxed) + origin.as_millis() as u64;
            if rendered_abs + BOUNDARY_LEAD_MS >= boundary_ms {
                break;
            }
            if wait_start.elapsed() >= MAX_BOUNDARY_WAIT {
                log::warn!("[abr] boundary wait hit cap; switching mid-segment");
                break;
            }
            let remaining = boundary_ms - (rendered_abs + BOUNDARY_LEAD_MS);
            tokio::select! {
                _ = crate::rt::sleep(Duration::from_millis(remaining.min(120))) => {}
                _ = stop.notified() => {
                    signal_stop(&new_flag, &new_stop);
                    // Unblock a warm NEW decode parked on its full gate so it
                    // can observe the flag and drop the HW decoder — otherwise
                    // it (and the decoder slot) would leak past this stop.
                    if let Some((_, release)) = warm.as_ref() {
                        release.notify_one();
                    }
                    signal_stop(&cur_flag, &cur_stop);
                    let _ = cur_handle.await;
                    return Ok(());
                }
                res = &mut cur_handle => {
                    if let Ok(Err(e)) = res {
                        log::error!("[video] supervisor: pipeline failed during boundary wait: {}", e);
                    }
                    old_done = true;
                }
            }
        }
        log::info!(
            "[abr] reached boundary {}ms after switch (pos≈{}ms boundary={}ms)",
            swap_t0.elapsed().as_millis(),
            position_ms.load(Ordering::Relaxed) + origin.as_millis() as u64,
            boundary_ms
        );

        // Reaching the boundary is not the same as being READY at it. The
        // decrypt of NEW's first segment was started back when the prefetch
        // primed, but it costs ~0.65 s for a 14 Mbps segment and the switch
        // request lands at an arbitrary phase — ask for one 1.1 s before the
        // boundary and it is still running when we get here. Tearing OLD down
        // on top of that is what turned an upswitch into ~10 dropped frames.
        //
        // So wait for it. OLD is still decoding and still has downloaded
        // content past the boundary (its soft cap is only applied below), so
        // this costs the viewer nothing — the splice simply lands a few
        // hundred ms later inside NEW's first segment, which is 6 s long.
        // Bounded, because a wedged prepare must not hold the swap forever.
        if let Some(pf) = new_pf.as_mut() {
            if let Some(h) = pf.first_prepared.as_ref() {
                let wait_t0 = Instant::now();
                while !h.is_finished() && wait_t0.elapsed() < PREPARE_READY_BUDGET {
                    crate::rt::sleep(Duration::from_millis(10)).await;
                }
                let waited = wait_t0.elapsed();
                // Only worth the operator's attention when the budget ran
                // out - that means the swap went ahead unprepared and the
                // viewer may have seen it. A short, successful hold is detail.
                if !h.is_finished() {
                    log::warn!(
                        "[abr] NEW's decrypt did not finish within {}ms at the boundary;                          swapping anyway",
                        waited.as_millis()
                    );
                } else if waited > Duration::from_millis(20) {
                    log::debug!(
                        "[abr] held OLD {}ms at the boundary for NEW's decrypt",
                        waited.as_millis()
                    );
                }
            }
        }

        let _ = events.send(PlayerEvent::TrackChanged {
            kind: TrackKind::Video,
            info: video_track_info(&new_repr),
        });

        // --- step 3: tear OLD down (frees the single HW decoder slot), then
        // start NEW's decode from the already-downloaded buffer. soft_end is
        // belt-and-braces alongside the flag + notify. `cur_handle` is only
        // awaited if it didn't already finish above (a JoinHandle must not be
        // polled twice).
        // From here until NEW's first frame reaches av_sync there is a planned
        // frame hole; arm the grace window so the starvation detector doesn't
        // flash Buffering + pause audio over it (the "spinner blink on every
        // ABR switch").
        *stats.swap_grace_deadline.lock().unwrap() = Some(Instant::now() + SWAP_GRACE);
        if !old_done {
            cur_soft_end.store(new_start, Ordering::Relaxed);
            signal_stop(&cur_flag, &cur_stop);
            log::info!("[video gen {}] soft-swap: awaiting OLD repr {} decode teardown", gen, current_repr.id);
            let _ = cur_handle.await;
            log::info!("[video gen {}] soft-swap: OLD repr {} decode joined", gen, current_repr.id);
        } else {
            log::info!("[video gen {}] soft-swap: OLD repr {} already done", gen, current_repr.id);
        }
        // OLD is gone: NEW's download is now the only thing feeding the
        // picture, so it becomes the pipeline that publishes buffer-ahead.
        if let Some(pf) = new_pf.as_ref() {
            pf.take_over_buffer_gauge(&stats);
        }

        cur_handle = match warm {
            // Warm handoff: NEW is already configured with its first GOP
            // decoded and parked — just open the gate. Its frames land in the
            // main channel right behind OLD's tail.
            Some((handle, release)) => {
                release.notify_one();
                log::info!(
                    "[abr] OLD torn down {}ms after switch; warm handoff gate opened",
                    swap_t0.elapsed().as_millis()
                );
                handle
            }
            // Direct-mode path (single MediaCodec slot): NEW's decoder may
            // only exist now that OLD's is dropped.
            //
            // Splice NEW onto OLD's tail at the last RENDERED pts so NEW
            // resumes on the very next frame — forward-contiguous, no rewind
            // and no future-PTS frame to wait on.
            //
            // This used to read `stats.last_decoded_pts_ms`, but that is the
            // DOWNLOAD high-water (set in video_prefetch's segment-done
            // callback), not the last frame OLD actually showed. On a
            // low-bitrate OLD the downloader races many seconds ahead of the
            // picture, so trimming NEW to it discarded every frame between the
            // boundary and the download head — the renderer then had no frame
            // at the current position and stalled ("[vsync] no frame →
            // buffering") for seconds on every swap while audio (own clock)
            // and the downloader kept running (the "everything plays but no
            // frames reach the screen" freeze, on every platform). The render
            // position (position_ms + origin, same expression the boundary
            // wait above uses) is where OLD's picture actually is, so NEW
            // splices there cleanly.
            None => {
                let rendered_abs_ms =
                    position_ms.load(Ordering::Relaxed) + origin.as_millis() as u64;
                let splice_pts_us = rendered_abs_ms as i64 * 1000;
                log::info!(
                    "[abr] OLD torn down {}ms after switch; NEW trimmed to pts>{}ms",
                    swap_t0.elapsed().as_millis(),
                    splice_pts_us / 1000
                );
                let decode_t0 = Instant::now();
                let decoder = decoder_factory();
                crate::rt::spawn(run_decode(
                    new_pf.take().expect("new_pf unconsumed on non-warm path"),
                    frame_sender.clone(),
                    video_ready.clone(),
                    decoder,
                    Arc::clone(&stats),
                    new_flag.clone(),
                    Some(SwapSplice {
                        started: decode_t0,
                        skip_below_pts_us: splice_pts_us,
                    }),
                    direct_window,
                    Arc::clone(&hdr_decode_8bit),
                ))
            }
        };
        cur_stop = new_stop;
        cur_flag = new_flag;
        cur_soft_end = new_soft_end;
        current_repr = new_repr;
    }
}

