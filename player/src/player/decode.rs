//! Segment preparation (init concat, CENC decrypt, mp4 parse) and the
//! decoder-feeding tasks that turn prepared segments into frames.

use super::*;

// ---------------------------------------------------------------------------
// Decoder tasks (platform-generic, communicate via channels)
// ---------------------------------------------------------------------------

/// A segment with its init header prepended, CENC-decrypted and mp4-parsed —
/// everything that has to happen before bytes can be fed to a decoder.
pub(super) struct PreparedSegment {
    id: usize,
    data_vec: Vec<u8>,
    sample_info: Vec<(usize, usize, i64, u64)>,
}

pub(super) type PrepareHandle =
    crate::rt::JoinHandle<Result<PreparedSegment, Box<dyn Error + Send + Sync>>>;

/// Sample table of a prepared segment: `(offset, size, composition pts,
/// timescale)` for every sample of its first — and, in a DASH media segment,
/// only — track.
///
/// Three call sites built this identically and a fourth built a subset of it;
/// one place to get the "no track" and parse errors right is worth more than
/// the four lines it saves each time.
pub(super) fn mp4_sample_table(
    data: &[u8],
) -> Result<Vec<(usize, usize, i64, u64)>, Box<dyn Error + Send + Sync>> {
    let mp4 = Mp4::read_bytes(data)
        .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("mp4: {}", e).into() })?;
    let (_id, track) = mp4
        .tracks()
        .first_key_value()
        .ok_or_else(|| -> Box<dyn Error + Send + Sync> { "no track".into() })?;
    Ok(track
        .samples
        .iter()
        .map(|s| {
            (
                s.offset as usize,
                s.size as usize,
                s.composition_timestamp,
                s.timescale,
            )
        })
        .collect())
}

/// Init concat + CENC decrypt + mp4 parse, on a BLOCKING thread.
///
/// Cost scales with segment SIZE, not frame count: ~17 MB/s of software AES on
/// a TV SoC, so a 14 Mbps 4K segment is ~0.7 s of pure CPU. In steady state the
/// decode loop hides that by preparing segment N+1 while it feeds N. The first
/// segment of a pipeline has nothing to hide behind — which is why an ABR swap
/// starts it during the prefetch instead, while the OLD rung is still playing.
pub(super) fn prepare_segment(
    init_data: Arc<Vec<u8>>,
    crypto: Option<TrackCrypto>,
    segment: DataSegment,
) -> PrepareHandle {
    #[cfg(not(target_arch = "wasm32"))]
    {
        crate::rt::spawn_blocking(move || prepare_blocking(&init_data, crypto.as_ref(), segment))
    }
    // Browser: no blocking pool, one thread. Same steps as a task that yields
    // to the event loop between AES slices (see
    // `decrypt_segment_in_place_cooperative`), so audio callbacks and decoder
    // output keep flowing while a segment is prepared.
    #[cfg(target_arch = "wasm32")]
    {
        crate::rt::spawn(async move {
            let t_copy = Instant::now();
            let mut data_vec = Vec::with_capacity(init_data.len() + segment.data.len());
            data_vec.extend_from_slice(&init_data);
            data_vec.extend_from_slice(&segment.data[..]);
            let copy_ms = t_copy.elapsed().as_millis();
            crate::prof::SEGMENT_PREP.add(t_copy.elapsed().as_micros() as u64);
            let t_dec = Instant::now();
            decrypt_segment_in_place_cooperative(&mut data_vec, crypto.as_ref()).await?;
            let dec_ms = t_dec.elapsed().as_millis();
            let t_parse = Instant::now();
            let sample_info = mp4_sample_table(&data_vec)?;
            let parse_ms = t_parse.elapsed().as_millis();
            crate::prof::SEGMENT_PREP.add(t_parse.elapsed().as_micros() as u64);
            crate::prof::DECRYPT.add((dec_ms * 1000) as u64);
            if copy_ms + dec_ms + parse_ms > 30 {
                log::debug!(
                    "[prep] segment {} {} KiB: copy {}ms decrypt {}ms (cooperative) parse {}ms",
                    segment.id,
                    data_vec.len() / 1024,
                    copy_ms,
                    dec_ms,
                    parse_ms
                );
            }
            Ok(PreparedSegment {
                id: segment.id,
                data_vec,
                sample_info,
            })
        })
    }
}

/// The blocking-thread body of [`prepare_segment`] (native).
#[cfg(not(target_arch = "wasm32"))]
fn prepare_blocking(
    init_data: &[u8],
    crypto: Option<&TrackCrypto>,
    segment: DataSegment,
) -> Result<PreparedSegment, Box<dyn Error + Send + Sync>> {
    let t_copy = Instant::now();
    let mut data_vec = Vec::with_capacity(init_data.len() + segment.data.len());
    data_vec.extend_from_slice(init_data);
    data_vec.extend_from_slice(&segment.data[..]);
    let copy_ms = t_copy.elapsed().as_millis();
    let t_dec = Instant::now();
    decrypt_segment_in_place(&mut data_vec, crypto)?;
    let dec_ms = t_dec.elapsed().as_millis();
    let t_parse = Instant::now();
    let sample_info = mp4_sample_table(&data_vec)?;
    let parse_ms = t_parse.elapsed().as_millis();
    // DIAG (verbose only): which third of prepare() actually costs,
    // per segment size. Answers "is this the AES or not" without
    // guessing - it was the AES, at ~16 MiB/s.
    if copy_ms + dec_ms + parse_ms > 30 {
        log::debug!(
            "[prep] segment {} {} KiB: copy {}ms decrypt {}ms parse {}ms",
            segment.id,
            data_vec.len() / 1024,
            copy_ms,
            dec_ms,
            parse_ms
        );
    }
    Ok(PreparedSegment {
        id: segment.id,
        data_vec,
        sample_info,
    })
}

/// Upper bound on the time the decode loops spend waiting for a
/// callback-driven decoder to catch up (see `breathe!`), so a decoder that
/// produces nothing for a stretch of input (codec priming, a dropped frame)
/// cannot wedge the loop. Wall time, not turns: an idle event loop turns in
/// well under a millisecond, and a WebCodecs decoder's first output after
/// configure takes longer than a few hundred of those.
const MAX_BREATHE: Duration = Duration::from_millis(1500);

/// Cap on the end-of-stream drain wait. Kept under the sync loops' 300 ms
/// "no frame" starvation heuristic: a codec flush normally completes in a
/// few ms, and a tail that takes longer than this is not worth a spurious
/// Buffering(Stall) (which also parks the audio sink) right before EndOfStream.
const MAX_DRAIN: Duration = Duration::from_millis(250);

/// Backpressure for callback-driven decoders (WebCodecs): while the decoder
/// reports more input in flight than it wants, give the host event loop
/// turns — that is when it delivers output. Native decoders never ask
/// (output is pulled from the codec synchronously), so this costs them one
/// boolean per sample.
///
/// Only a real wait fixes the browser: a single turn per sample let the loop
/// run ~1000 AUs/s ahead of the decoder, and a WebCodecs decoder flooded like
/// that stops delivering output. A macro (not an async fn taking `&decoder`)
/// so no borrow of the `dyn` decoder is held across the await — the
/// decoder traits are `Send`, not `Sync`.
macro_rules! breathe {
    ($decoder:expr, $stop_flag:expr) => {
        breathe!($decoder, $stop_flag, MAX_BREATHE)
    };
    ($decoder:expr, $stop_flag:expr, $cap:expr) => {{
        let started = Instant::now();
        let mut turns = 0u32;
        while $decoder.wants_event_loop() && !$stop_flag.load(Ordering::Relaxed) {
            if started.elapsed() >= $cap {
                log::debug!(
                    "[dec] decoder still busy after {} event-loop turns / {}ms; feeding anyway",
                    turns,
                    started.elapsed().as_millis()
                );
                break;
            }
            // Wait for the decoder to actually deliver something, rather
            // than bouncing the event loop and asking again. The bounce has no
            // delay by design, so on a decoder that stays full — which is the
            // steady state while the pipeline feeds ahead — this loop ran flat
            // out and cost a whole core. Measured on a 720p stream: one
            // renderer thread at 0.99 of a core, of which the engine's own
            // timed work was 1.2%; the rest was this.
            //
            // The timeout keeps the old behaviour's safety: a decoder that
            // silently stops delivering still gets re-checked rather than
            // parking here until MAX_BREATHE.
            match $decoder.output_ready() {
                Some(ready) => {
                    let _ = crate::rt::timeout(
                        std::time::Duration::from_millis(50),
                        ready.notified(),
                    )
                    .await;
                }
                None => crate::rt::cooperative_yield().await,
            }
            turns += 1;
        }
    }};
}

pub(super) async fn video_decoder_task(
    mut receiver: Receiver<DataSegment>,
    sender: Sender<DecodedVideoFrame>,
    mut decoder: Box<dyn HwVideoDecoder>,
    init_data: Vec<u8>,
    video_ready: Arc<Notify>,
    track_crypto: Option<TrackCrypto>,
    stats: Arc<StatsState>,
    // Local stop signal so the supervisor can abort the decoder MID-QUEUE
    // on an ABR swap. Without this, decoder_task drains every segment
    // already in download_rx before exiting — at 1× consumption that's
    // up to `segments_in_flight` × segment_duration ≈ 24 s of OLD content
    // playing after the swap request, then a PTS jump back to NEW's
    // start. Reacting to stop here cuts the swap-to-NEW-frame latency
    // to one in-flight segment + the few frames already buffered in
    // `sender`.
    stop_flag: Arc<AtomicBool>,
    // `Some(..)` on an ABR-swap pipeline: trims NEW's overlap with OLD's tail
    // (frames at/below `skip_below_pts_us`) so the splice is forward-contiguous,
    // and stamps the first-frame-after-teardown timing log. `None` initially.
    splice: Option<SwapSplice>,
    // First segment whose decrypt+parse was started ahead of time (see
    // `prepare_segment`). `None` outside an ABR swap.
    first_prepared: Option<PrepareHandle>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut first_frame_signaled = false;

    // Reorder buffer to fix two sources of non-monotonic PTS:
    //   1. HEVC B-frames: hardware decoders may output in decode order, not
    //      display order, so composition timestamps are non-monotonic.
    //   2. Segment boundaries: the last N frames of segment K may arrive from
    //      the MediaCodec pipeline *after* the first frames of segment K+1,
    //      causing PTS to jump backward every ~4 seconds.
    // We always emit the lowest-PTS frame from the buffer. video_ready fires
    // on the first send so start_time is calibrated to when frames are
    // actually available (avoids a timing hole at startup).
    // Direct mode holds codec output buffers captive in this window — keep
    // it shallow there (see the frame-channel capacity comment in play()).
    let reorder_depth: usize = if decoder.is_direct() { 2 } else { 4 };
    let mut reorder_buf: Vec<DecodedVideoFrame> = Vec::with_capacity(reorder_depth + 1);

    // Segment preparation (init concat + CENC decrypt + mp4 parse) runs on
    // a BLOCKING thread, overlapped with feeding the previous segment.
    // Software AES on 32-bit devices costs ~0.5 s per 4K segment — done
    // inline (the old shape) that was a guaranteed codec starvation +
    // LATE drain at every segment boundary; overlapped it disappears into
    // the ~6 s feed window of the segment before it.
    let init_data = Arc::new(init_data);
    let prepare = {
        let init_data = Arc::clone(&init_data);
        let crypto = track_crypto.clone();
        move |segment: DataSegment| prepare_segment(Arc::clone(&init_data), crypto.clone(), segment)
    };

    // Normally None — but an ABR swap hands over the first segment already
    // being prepared (started while the OLD rung still played), which is the
    // difference between the new rung's first frame landing inside OLD's frame
    // cushion and landing ~0.7 s after it ran dry.
    let mut pending_prepare: Option<PrepareHandle> = first_prepared;
    loop {
        let boundary_t0 = Instant::now();
        let prepared = match pending_prepare.take() {
            // Steady state: the next segment was prepared while the
            // previous one fed — this await is ~0.
            Some(handle) => handle
                .await
                .map_err(|e| -> Box<dyn Error + Send + Sync> {
                    format!("prepare task: {}", e).into()
                })??,
            // Startup / buffer-dry case: wait for a download, prepare it
            // (nothing to overlap with).
            None => {
                let Some(segment) = receiver.recv().await else {
                    break;
                };
                if stop_flag.load(Ordering::Relaxed) {
                    log::debug!("[dec] stop signal received between segments; aborting drain");
                    break;
                }
                prepare(segment)
                    .await
                    .map_err(|e| -> Box<dyn Error + Send + Sync> {
                        format!("prepare task: {}", e).into()
                    })??
            }
        };
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        // Kick off the NEXT segment's preparation before feeding this one
        // — downloads run ~4 segments ahead, so it's normally buffered.
        if let Ok(next) = receiver.try_recv() {
            pending_prepare = Some(prepare(next));
        }
        let boundary_ms = boundary_t0.elapsed().as_millis();
        if boundary_ms > 50 {
            log::info!(
                "[dec] segment {} boundary stall {}ms ({} samples, {} KiB)",
                prepared.id, boundary_ms,
                prepared.sample_info.len(), prepared.data_vec.len() / 1024
            );
        }
        log::debug!("[dec] consuming video segment: {}", prepared.id);
        stats.diag_video_seg.fetch_add(1, Ordering::Relaxed);
        stats.video_segment_id.store(prepared.id as u64, Ordering::Relaxed);
        let data_vec = prepared.data_vec;
        let sample_info = prepared.sample_info;

        let mut first_pts_us: Option<i64> = None;
        let mut last_pts_us: i64 = 0;
        for (offset, size, ts, ts_scale) in sample_info {
            // Check stop_flag INSIDE the segment-processing loop too.
            // Without this, an ABR swap fired mid-segment would wait for
            // the current segment's frames to finish pacing through the
            // 8-slot frame_sender at 1× (~6 s for a 144-frame segment),
            // and only then notice the supervisor's stop signal. That's
            // the "60 s before ABR takes effect" the user reported.
            // Bailing mid-segment loses the rest of this segment but
            // keeps the swap responsive.
            if stop_flag.load(Ordering::Relaxed) {
                log::debug!("[dec] stop signal received mid-segment; aborting drain");
                return Ok(());
            }
            if offset + size > data_vec.len() {
                continue;
            }
            let sample_data = &data_vec[offset..offset + size];
            let pts_us = if ts_scale > 0 { ts * 1_000_000 / ts_scale as i64 } else { 0 };
            if first_pts_us.is_none() { first_pts_us = Some(pts_us); }
            last_pts_us = pts_us;

            // Drain the frames the codec ALREADY has ready BEFORE feeding it
            // more input. A direct MediaCodec whose output buffers are all
            // dequeued-but-undrained refuses new input — submit() then spins on
            // a full codec while the very frames that would free it sit waiting
            // to be pulled, so the old feed-then-drain order deadlocked once the
            // pool filled (#23 backpressure variant: produced>0, dequeue_input
            // stall forever, video starves). Draining first keeps the codec's
            // output pool flowing so submit doesn't wedge.
            if !drain_video_decoder(
                &mut decoder,
                &mut reorder_buf,
                reorder_depth,
                &splice,
                &mut first_frame_signaled,
                &video_ready,
                &stats,
                &sender,
                &stop_flag,
            )
            .await?
            {
                return Ok(());
            }

            {
                let _p = crate::prof::Timer::new(&crate::prof::VIDEO_SUBMIT);
                decoder.submit(sample_data, pts_us)?;
            }
            breathe!(decoder, stop_flag);
        }
        if let Some(first) = first_pts_us {
            log::info!("[dec] seg done: pts {}..{}ms", first / 1000, last_pts_us / 1000);
        }
    }

    // End of input: let the codec emit what it still holds. A callback-driven
    // decoder needs event-loop turns for that (native ones are pulled below).
    decoder.signal_end_of_stream();
    breathe!(decoder, stop_flag, MAX_DRAIN);

    // Final drain: with drain-before-submit, the last submitted sample's output
    // is still inside the codec — pull it before the reorder flush so the tail
    // frames aren't lost.
    if !drain_video_decoder(
        &mut decoder,
        &mut reorder_buf,
        reorder_depth,
        &splice,
        &mut first_frame_signaled,
        &video_ready,
        &stats,
        &sender,
        &stop_flag,
    )
    .await?
    {
        return Ok(());
    }

    // Flush remaining frames in PTS order.
    reorder_buf.sort_by_key(|f| f.pts_us);
    for frame in reorder_buf.drain(..) {
        if let Some(sp) = &splice {
            if frame.pts_us <= sp.skip_below_pts_us {
                continue;
            }
        }
        if !first_frame_signaled {
            video_ready.notify_one();
            first_frame_signaled = true;
        }
        if sender.send(frame).await.is_err() {
            return Ok(());
        }
    }

    Ok(())
}

/// Pull every frame the video decoder currently has ready into the reorder
/// buffer and forward the lowest-PTS ones downstream. MUST run before each
/// `submit()` in the decode loop (direct mode): a MediaCodec whose output
/// buffers are all dequeued-but-undrained refuses new input, so feeding before
/// draining deadlocks. Returns `false` when the pipeline should stop (the
/// frame channel closed, or a teardown was signalled mid-send).
pub(super) async fn drain_video_decoder(
    decoder: &mut Box<dyn HwVideoDecoder>,
    reorder_buf: &mut Vec<DecodedVideoFrame>,
    reorder_depth: usize,
    splice: &Option<SwapSplice>,
    first_frame_signaled: &mut bool,
    video_ready: &Arc<Notify>,
    stats: &Arc<StatsState>,
    sender: &Sender<DecodedVideoFrame>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let _p = crate::prof::Timer::new(&crate::prof::VIDEO_DRAIN);
    loop {
        match decoder.try_recv()? {
            Some(frame) => {
                // Track the high-water-mark of decoded PTS so av_sync can
                // publish `buffered_ahead_secs` (reorder-buffered frames are
                // already decoded and render shortly).
                let pts_ms = frame.pts_us / 1000;
                stats.last_decoded_pts_ms.fetch_max(pts_ms, Ordering::Relaxed);
                reorder_buf.push(frame);
                // Startup fast-path: emit the very first frame as soon as it
                // is decoded instead of waiting for the reorder buffer to fill
                // (reorder_depth + 1 frames). At a cold start / post-seek the
                // first decoded frame is the segment-start IDR keyframe — the
                // lowest PTS in its GOP — so emitting it early can't reorder
                // ahead of an earlier frame. This is exactly the wait that
                // gates time-to-first-frame (video_ready → av_sync). Restricted
                // to `splice.is_none()`: an ABR swap mid-play needs the full
                // reorder discipline (its first kept frame is mid-GOP, not an
                // IDR) and doesn't show startup latency anyway.
                let ready = if !*first_frame_signaled && splice.is_none() {
                    !reorder_buf.is_empty()
                } else {
                    reorder_buf.len() > reorder_depth
                };
                if ready {
                    let min_idx = reorder_buf
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, f)| f.pts_us)
                        .map(|(i, _)| i)
                        .unwrap();
                    let to_send = reorder_buf.swap_remove(min_idx);
                    // ABR splice trim: drop NEW frames at/below the PTS OLD last
                    // rendered so NEW joins forward-contiguous (no rewind, no
                    // future-PTS frame av_sync would sit waiting for).
                    if let Some(sp) = splice {
                        if to_send.pts_us <= sp.skip_below_pts_us {
                            continue;
                        }
                    }
                    if !*first_frame_signaled {
                        video_ready.notify_one();
                        *first_frame_signaled = true;
                        if let Some(sp) = splice {
                            log::info!(
                                "[abr] NEW first frame {}ms after OLD teardown (pts={}ms)",
                                sp.started.elapsed().as_millis(),
                                to_send.pts_us / 1000
                            );
                        }
                    }
                    if sender.send(to_send).await.is_err() {
                        return Ok(false);
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        log::debug!("[dec] stop signal received after send; aborting");
                        return Ok(false);
                    }
                }
            }
            None => break,
        }
    }
    Ok(true)
}

pub(super) async fn audio_decoder_task(
    mut receiver: Receiver<DataSegment>,
    sender: Sender<DecodedAudioFrame>,
    mut decoder: Box<dyn AudioDecoder>,
    init_data: Vec<u8>,
    audio_ready: Arc<Notify>,
    track_crypto: Option<TrackCrypto>,
    stats: Arc<StatsState>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Mirror the video pattern: fire audio_ready only once we've
    // actually produced + queued a real PCM frame, not on the first
    // submit. Firing too early made av_sync_handler set start_time
    // before any audio was in cpal's queue, so the speaker only
    // started hearing real samples 20-200 ms after video began
    // rendering — perceived as constant audio lag.
    let mut first_audio_signaled = false;
    // The audio task has no local stop signal (it ends when its input
    // channel closes); the breathing wait only needs something to poll.
    let stop_flag = Arc::new(AtomicBool::new(false));
    while let Some(segment) = receiver.recv().await {
        log::debug!("[dec] consuming audio segment: {}", segment.id);
        stats.diag_audio_seg.fetch_add(1, Ordering::Relaxed);

        // block_in_place: CENC decrypt of a whole segment is heavy CPU work
        // (~100+ ms of software AES on 32-bit TV SoCs) — run inline on a
        // runtime worker it stalls every other task scheduled there. The same
        // rule as the video path (which offloads via spawn_blocking) and the
        // MediaCodec calls below: BLOCKING WORK MUST NOT OCCUPY A RUNTIME
        // WORKER, or the reactive tasks (vsync pacing, audio feed, timers)
        // starve and playback degrades to a ~1 fps convoy.
        #[cfg(not(target_arch = "wasm32"))]
        let (data_vec, sample_info) = crate::rt::block_in_place(
            || -> Result<(Vec<u8>, Vec<(usize, usize, i64, u64)>), Box<dyn Error + Send + Sync>> {
                let mut data_vec = init_data.clone();
                data_vec.extend_from_slice(&segment.data[..]);
                decrypt_segment_in_place(&mut data_vec, track_crypto.as_ref())?;

                let sample_info = mp4_sample_table(&data_vec)?;
                Ok((data_vec, sample_info))
            },
        )?;
        // Browser: the same cooperative path as video — WebCrypto when the
        // key is platform-held (wrapped licence) or the segment is large,
        // sliced software AES otherwise. There is no blocking pool here.
        #[cfg(target_arch = "wasm32")]
        let (data_vec, sample_info) = {
            let mut data_vec = init_data.clone();
            data_vec.extend_from_slice(&segment.data[..]);
            decrypt_segment_in_place_cooperative(&mut data_vec, track_crypto.as_ref()).await?;
            let sample_info = mp4_sample_table(&data_vec)?;
            (data_vec, sample_info)
        };

        for (offset, size, ts, ts_scale) in sample_info {
            if offset + size > data_vec.len() {
                continue;
            }
            let sample_data = &data_vec[offset..offset + size];
            let pts_us = if ts_scale > 0 { ts * 1_000_000 / ts_scale as i64 } else { 0 };

            {
                let _p = crate::prof::Timer::new(&crate::prof::AUDIO_SUBMIT);
                decoder.submit(sample_data, pts_us)?;
            }
            breathe!(decoder, stop_flag);

            loop {
                match decoder.try_recv()? {
                    Some(frame) => {
                        let pts_ms = frame.pts_ms;
                        stats
                            .audio_last_decoded_pts_ms
                            .fetch_max(pts_ms, Ordering::Relaxed);
                        if sender.send(frame).await.is_err() {
                            return Ok(());
                        }
                        stats.diag_audio_dec.fetch_add(1, Ordering::Relaxed);
                        if !first_audio_signaled {
                            audio_ready.notify_one();
                            first_audio_signaled = true;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    // End of input: emit the tail the decoder still holds (see the video task).
    decoder.signal_end_of_stream();
    breathe!(decoder, stop_flag, MAX_DRAIN);
    while let Some(frame) = decoder.try_recv()? {
        stats
            .audio_last_decoded_pts_ms
            .fetch_max(frame.pts_ms, Ordering::Relaxed);
        if sender.send(frame).await.is_err() {
            return Ok(());
        }
        stats.diag_audio_dec.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}
