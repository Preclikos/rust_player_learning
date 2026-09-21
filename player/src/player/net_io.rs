//! Download plumbing and the segment-level crypto glue: the download task,
//! CENC decryption (serial and parallel) and the small shared helpers.

use super::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Callback fired by `download_task` after each successful segment
/// download, passed the segment's end-PTS in milliseconds. Both
/// `video_play` and `audio_play` plug this in to push the
/// `Position.buffered_ahead_secs` gauge forward as media lands in the
/// local download buffer — rather than only when the decoder finally
/// gets around to producing a frame.
pub(super) type SegmentDoneCallback = Arc<dyn Fn(i64) + Send + Sync>;

pub(super) async fn download_task(
    segments: Vec<Segment>,
    start_index: usize,
    segment_sender: Sender<DataSegment>,
    stop: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    http: Arc<HttpClient>,
    stats: Option<Arc<StatsState>>,
    on_segment_done: Option<SegmentDoneCallback>,
    // Upper-exclusive segment index. Default `usize::MAX` means "no soft
    // limit, run to natural EOF". The supervisor lowers this on ABR
    // swaps so the OLD pipeline drains its already-downloaded tail
    // cleanly (segments < new_start) while the NEW pipeline downloads
    // from new_start in parallel — no PTS overlap because the two
    // ranges are disjoint, and av_sync sees a continuous frame stream.
    soft_end_exclusive: Arc<AtomicUsize>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    /// How long `download_task` keeps retrying a single failing
    /// segment before giving up and ending the pipeline. The inner
    /// `HttpClient::get` already does ~3 short retries (~2 s); this
    /// outer cap covers extended outages, e.g. a Wi-Fi drop while a
    /// movie is playing. Long enough for a typical reconnect, short
    /// enough that the player doesn't sit silently forever when the
    /// network is genuinely gone.
    const SEGMENT_RETRY_TOTAL: Duration = Duration::from_secs(30);

    let segment_slice = &segments[..];
    // Set when a segment was abandoned after SEGMENT_RETRY_TOTAL — turned
    // into this task's Err below (natural completion / stop / soft-end /
    // receiver-drop all stay Ok).
    let mut gave_up: Option<Box<dyn Error + Send + Sync>> = None;
    for i in start_index..segment_slice.len() {
        // A dropped receiver means the consumer (this pipeline's decode side) is
        // gone — a rebuild/ABR swap tore it down. Stop immediately; this is not
        // a network failure and must NOT hit the retry path, or an orphaned
        // downloader spams "downstream receiver dropped" for 30 s and wedges the
        // rebuild (see ABR_REBUILD_ORPHANED_DOWNLOADER handoff).
        if stop_flag.load(Ordering::Relaxed) || segment_sender.is_closed() {
            break;
        }
        // Soft end: supervisor sets this to the NEW pipeline's start
        // index on an ABR swap so the OLD pipeline exits naturally at
        // the swap boundary instead of needing a hard stop_flag.
        // Re-read every iteration so an in-flight swap takes effect
        // promptly.
        if i >= soft_end_exclusive.load(Ordering::Relaxed) {
            log::debug!(
                "[dl] soft end at segment {} reached (limit={}); pipeline draining for swap",
                i,
                soft_end_exclusive.load(Ordering::Relaxed),
            );
            break;
        }
        let seg = &segment_slice[i];
        let mut backoff = Duration::from_millis(500);
        // Outer retry loop: keep trying the same segment until it
        // succeeds, the user stops playback, the seek target changes,
        // or `SEGMENT_RETRY_TOTAL` elapses. Previously download_task
        // broke out on the first error — so a brief network blip tore
        // the whole pipeline down. Now a Wi-Fi blip rides through
        // transparently, and only a genuine extended outage ends the
        // pipeline.
        let retry_started = Instant::now();
        let mut should_break = false;
        let mut last_err: Option<Box<dyn Error + Send + Sync>> = None;
        loop {
            // stop, or the receiver vanished (pipeline torn down) → terminate,
            // never retry a dropped channel.
            if stop_flag.load(Ordering::Relaxed) || segment_sender.is_closed() {
                should_break = true;
                break;
            }
            if retry_started.elapsed() > SEGMENT_RETRY_TOTAL {
                log::error!(
                    "[dl] segment {} gave up after {:?} of retries: {}",
                    i,
                    SEGMENT_RETRY_TOTAL,
                    last_err
                        .as_ref()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "(no error captured)".to_string())
                );
                // Surface the give-up as an error. Returning Ok here made an
                // extended outage indistinguishable from natural EOF: the
                // decoder drained, the pipeline exited cleanly, the video
                // supervisor saw "natural EOF" (no retry, no Error event)
                // and av_sync emitted a fake EndOfStream — the consumer
                // kicked the user out of playback and marked the title
                // watched. An Err instead routes through the supervisor's
                // bounded retry and, on exhaustion, PlayerEvent::Error.
                gave_up = Some(
                    last_err
                        .take()
                        .unwrap_or_else(|| "segment retries exhausted".into()),
                );
                should_break = true;
                break;
            }
            let sender = segment_sender.clone();
            let outcome = tokio::select! {
                res = download_and_queue(i, seg, sender, &http, stats.as_ref()) => Some(res),
                _ = stop.notified() => None,
            };
            match outcome {
                Some(Ok(())) => {
                    log::debug!("[dl] produced segment {}", i);
                    if let Some(cb) = &on_segment_done {
                        cb(seg.end_time().as_millis() as i64);
                    }
                    break;
                }
                Some(Err(e)) => {
                    // Receiver dropped mid-send = consumer gone, not a transport
                    // error. Stop now instead of retrying (the SendError-loop wedge).
                    if segment_sender.is_closed() {
                        log::debug!("[dl] segment {} receiver gone — stopping (pipeline torn down)", i);
                        should_break = true;
                        break;
                    }
                    log::warn!(
                        "[dl] segment {} failed, retrying in {:?}: {}",
                        i, backoff, e
                    );
                    last_err = Some(e);
                    tokio::select! {
                        _ = crate::rt::sleep(backoff) => {}
                        _ = stop.notified() => {
                            should_break = true;
                            break;
                        }
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(8));
                }
                None => {
                    should_break = true;
                    break;
                }
            }
        }
        // net_stall is accumulated inside download_and_queue from the actual
        // network time vs the segment's media duration (see there). It must NOT
        // be measured from `started.elapsed()` here: that wall span includes the
        // time `send()` blocks on a FULL channel — i.e. it spikes precisely when
        // the buffer is HEALTHY/full (backpressure), which flashed the host's
        // loading spinner periodically during otherwise-fine playback.
        if should_break {
            break;
        }
    }
    if let Some(e) = gave_up {
        return Err(format!(
            "segment download gave up after {:?}: {}",
            SEGMENT_RETRY_TOTAL, e
        )
        .into());
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(super) struct DataSegment {
    pub(super) id: usize,
    pub(super) data: Vec<u8>,
}

pub(super) fn log_task_result<T, E: std::fmt::Display>(
    name: &str,
    result: Result<Result<T, E>, crate::rt::JoinError>,
) {
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => log::error!("{}: {}", name, e),
        Err(e) => log::error!("{}: join error: {}", name, e),
    }
}

/// Parse the `tenc` box from a track's init segment and, if the track is
/// CENC-encrypted, resolve its content key up front (async — possibly via a
/// `LicenseResolver`) so the per-sample `decrypt_sample` stays sync on the hot
/// path. Returns `None` for a clear track. `label` ("video"/"audio") only
/// flavours the log lines and the error text.
pub(super) async fn setup_track_crypto(
    init_data: &[u8],
    decryptor: Option<Arc<dyn Decryptor>>,
    label: &str,
) -> Result<Option<TrackCrypto>, Box<dyn Error + Send + Sync>> {
    let tenc = match parse_tenc(init_data) {
        Some(t) => t,
        None => {
            log::info!("{}: clear (no tenc box)", label);
            return Ok(None);
        }
    };

    log::info!(
        "{}: CENC encrypted, KID={} iv_size={}",
        label,
        kid_short(&tenc.default_kid),
        tenc.default_iv_size
    );
    let dec = decryptor.ok_or_else(|| -> Box<dyn Error + Send + Sync> {
        format!(
            "{} track is CENC-encrypted but no decryptor configured \
             (call Player::set_clearkey or set_license_resolver)",
            label
        )
        .into()
    })?;
    dec.ensure_key_for(tenc.default_kid)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> {
            format!("license resolve ({} kid={}): {}", label, kid_short(&tenc.default_kid), e).into()
        })?;
    Ok(Some(TrackCrypto {
        decryptor: dec,
        kid: tenc.default_kid,
        iv_size: tenc.default_iv_size as usize,
    }))
}

/// Below this, the thread hand-off costs more than the decryption saves.
pub(super) const PARALLEL_DECRYPT_MIN_BYTES: usize = 2 * 1024 * 1024;

/// Decrypt one sample, honouring the "clear sample inside an encrypted senc"
/// cases that CENC allows. Shared by the serial and parallel paths so they can
/// never disagree about what counts as clear.
pub(super) fn decrypt_one_sample(
    tc: &TrackCrypto,
    entry: &crate::crypto::SencEntry,
    sample: &mut [u8],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // IV all-zeros with no subsamples, or subsamples that encrypt nothing,
    // both mean the sample is in the clear. Applying a keystream anyway would
    // corrupt it — CTR with IV=0 still XORs against a real keystream.
    let iv_is_zero = entry.iv.iter().all(|&b| b == 0);
    let no_encrypted_bytes =
        !entry.subsamples.is_empty() && entry.subsamples.iter().all(|&(_, enc)| enc == 0);
    if iv_is_zero || no_encrypted_bytes {
        return Ok(());
    }
    tc.decryptor
        .decrypt_sample(&tc.kid, &entry.iv, sample, &entry.subsamples)
}

/// Split the segment into `workers` contiguous groups of samples and decrypt
/// them concurrently.
///
/// The split is by BYTES, not by sample count: sample sizes within a GOP vary
/// by an order of magnitude (an IDR against a B-frame), so an even count would
/// leave one worker holding most of the work.
pub(super) fn decrypt_samples_parallel(
    data_vec: &mut [u8],
    tc: &TrackCrypto,
    sample_ranges: &[(usize, usize)],
    senc_entries: &[crate::crypto::SencEntry],
    workers: usize,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let pairs: Vec<(&(usize, usize), &crate::crypto::SencEntry)> =
        sample_ranges.iter().zip(senc_entries.iter()).collect();
    let total: usize = pairs.iter().map(|(r, _)| r.1).sum();
    let per_worker = total / workers + 1;

    // Group boundaries, then turn them into disjoint &mut slices.
    let mut groups: Vec<&[(&(usize, usize), &crate::crypto::SencEntry)]> = Vec::new();
    let mut start = 0usize;
    let mut acc = 0usize;
    for i in 0..pairs.len() {
        acc += pairs[i].0 .1;
        let last = i + 1 == pairs.len();
        if acc >= per_worker || last {
            groups.push(&pairs[start..i + 1]);
            start = i + 1;
            acc = 0;
        }
    }

    // Validate the assumption the split rests on BEFORE splitting anything:
    // samples ascending, non-overlapping, inside the buffer. If a segment ever
    // breaks it, say so and let the caller stay serial rather than guess.
    let mut watermark = 0usize;
    for (r, _) in &pairs {
        if r.0 < watermark || r.0 + r.1 > data_vec.len() {
            return Err("sample offsets are not ascending; cannot split safely".into());
        }
        watermark = r.0 + r.1;
    }

    let mut rest: &mut [u8] = data_vec;
    let mut consumed = 0usize;
    let mut chunks: Vec<(&mut [u8], usize, &[(&(usize, usize), &crate::crypto::SencEntry)])> =
        Vec::with_capacity(groups.len());
    for (gi, group) in groups.iter().enumerate() {
        if group.is_empty() {
            continue;
        }
        let group_end = group.last().map(|(r, _)| r.0 + r.1).unwrap_or(consumed);
        let take_to = if gi + 1 == groups.len() {
            rest.len()
        } else {
            group_end - consumed
        };
        let (mine, tail) = rest.split_at_mut(take_to);
        chunks.push((mine, consumed, group));
        consumed += take_to;
        rest = tail;
    }

    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|(buf, base, group)| {
                scope.spawn(move || -> Result<(), String> {
                    for ((offset, size), entry) in group.iter().copied() {
                        let a = offset - base;
                        let b = a + size;
                        if b > buf.len() {
                            return Err(format!("sample {a}..{b} outside its chunk"));
                        }
                        decrypt_one_sample(tc, entry, &mut buf[a..b]).map_err(|e| e.to_string())?;
                    }
                    Ok(())
                })
            })
            .collect();
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err::<(), Box<dyn Error + Send + Sync>>(e.into()),
                Err(_) => return Err("decrypt worker panicked".into()),
            }
        }
        Ok(())
    })
}

pub(super) fn decrypt_segment_in_place(
    data_vec: &mut [u8],
    track_crypto: Option<&TrackCrypto>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let tc = match track_crypto {
        Some(t) => t,
        None => return Ok(()),
    };
    crate::crypto::log_aes_capability();

    // No senc box = this segment is clear, even though the track's `tenc`
    // box advertises encryption. Common Encryption explicitly supports
    // mixed-protection tracks — some samples / segments in the clear,
    // others encrypted with the cached KID. Skipping decryption is the
    // correct behaviour here; treating it as fatal stalled playback the
    // first time a clear segment landed.
    let senc_entries = match parse_senc(data_vec, tc.iv_size) {
        Some(e) => e,
        None => {
            log::debug!(
                "[crypto] no senc in segment — treating as clear (track kid={})",
                kid_short(&tc.kid)
            );
            return Ok(());
        }
    };

    let t_ranges = Instant::now();
    let sample_ranges: Vec<(usize, usize)> = mp4_sample_table(&data_vec[..])?
        .into_iter()
        .map(|(offset, size, _pts, _timescale)| (offset, size))
        .collect();

    let ranges_ms = t_ranges.elapsed().as_millis();
    let t_aes = Instant::now();

    // Spread the samples across cores when there is enough work to pay for the
    // threads. Samples are disjoint and in ascending offset order, so the
    // buffer can be split into per-worker slices with `split_at_mut` — no
    // unsafe aliasing, each worker owns its bytes outright.
    //
    // Worth it mainly where AES has no hardware backend (32-bit ARM, ~10 MiB/s)
    // and a 4K segment costs ~0.7 s single-threaded; with hardware AES the
    // whole segment is a few ms and the split simply never triggers.
    let total_bytes = sample_ranges.iter().map(|&(_, sz)| sz).sum::<usize>();
    let workers = if total_bytes >= PARALLEL_DECRYPT_MIN_BYTES && sample_ranges.len() > 1 {
        std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(1)
    } else {
        1
    };
    if workers > 1 {
        match decrypt_samples_parallel(data_vec, tc, &sample_ranges, &senc_entries, workers) {
            Ok(()) => {
                log::debug!(
                    "[crypto] {} samples over {} threads, senc+ranges {}ms, aes {}ms ({} KiB)",
                    sample_ranges.len(),
                    workers,
                    ranges_ms,
                    t_aes.elapsed().as_millis(),
                    total_bytes / 1024
                );
                return Ok(());
            }
            Err(e) => {
                // Only the layout check refuses; a real cipher failure would
                // have failed serially too. Fall through rather than fail.
                log::debug!("[crypto] parallel decrypt declined ({e}); doing it serially");
            }
        }
    }

    let mut enc_bytes = 0usize;
    let mut spans = 0usize;
    for ((offset, size), entry) in sample_ranges.iter().zip(senc_entries.iter()) {
        let end = offset + size;
        if end > data_vec.len() {
            continue;
        }
        // Per-sample "clear" entries also exist within an encrypted senc:
        //   - IV all-zeros AND no subsamples → sample is clear
        //   - subsamples list present but every entry has encrypted=0
        // In both cases applying the keystream is a no-op anyway (CTR with
        // IV=0 still XORs against a real keystream, breaking the data), so
        // we must detect and skip.
        let iv_is_zero = entry.iv.iter().all(|&b| b == 0);
        let no_encrypted_bytes = !entry.subsamples.is_empty()
            && entry.subsamples.iter().all(|&(_, enc)| enc == 0);
        if iv_is_zero || no_encrypted_bytes {
            continue;
        }
        if entry.subsamples.is_empty() {
            enc_bytes += end - offset;
            spans += 1;
        } else {
            enc_bytes += entry.subsamples.iter().map(|&(_, e)| e as usize).sum::<usize>();
            spans += entry.subsamples.len();
        }
        tc.decryptor
            .decrypt_sample(&tc.kid, &entry.iv, &mut data_vec[*offset..end], &entry.subsamples)?;
    }
    let aes_ms = t_aes.elapsed().as_millis();
    // DIAG (verbose only): separates "the cipher is slow" from "we call it badly" — bytes
    // actually fed to AES, and how many spans they arrived in.
    if aes_ms + ranges_ms > 30 {
        log::debug!(
            "[crypto] {} samples, senc+ranges {}ms, aes {}ms over {} spans / {} KiB encrypted \
             ({:.0} MiB/s)",
            sample_ranges.len(),
            ranges_ms,
            aes_ms,
            spans,
            enc_bytes / 1024,
            if aes_ms > 0 {
                enc_bytes as f64 / (aes_ms as f64 / 1000.0) / (1024.0 * 1024.0)
            } else {
                0.0
            }
        );
    }
    Ok(())
}

/// Build a `TrackInfo` snapshot from a video representation. Used by
/// `TrackChanged` events on both user-driven and ABR-driven switches.
pub(super) fn video_track_info(repr: &VideoRepresenation) -> TrackInfo {
    TrackInfo {
        representation_id: repr.id,
        codec: repr.codec_short().to_string(),
        bitrate_bps: repr.bandwidth,
        width: Some(repr.width),
        height: Some(repr.height),
        fps: None,
        channels: None,
        sample_rate_hz: None,
        language: None,
        label: repr.label(),
        hdr10: repr.is_hdr10(),
        dolby_vision: repr.is_dolby_vision(),
    }
}

pub(super) fn find_segment_index(segments: &[Segment], target: Duration) -> usize {
    if segments.is_empty() {
        return 0;
    }
    for (i, seg) in segments.iter().enumerate() {
        if seg.end_time() > target {
            return i;
        }
    }
    segments.len() - 1
}

pub(super) async fn download_and_queue(
    index: usize,
    segment: &Segment,
    sender: Sender<DataSegment>,
    http: &HttpClient,
    stats: Option<&Arc<StatsState>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let dl = segment
        .download(http, RequestKind::Segment)
        .await
        .map_err(|e| -> Box<dyn Error + Send + Sync> {
            format!("segment download: {}", e).into()
        })?;
    if let Some(s) = stats {
        update_bandwidth_ewma(&s.bandwidth_bps_ewma, dl.data.len(), dl.elapsed);
        // net_stall = how much SLOWER than realtime this segment downloaded.
        // A large segment that arrives in ~its own media duration is keeping
        // pace (no stall); only download time BEYOND that means the link can't
        // sustain the bitrate and the buffer is draining toward a real rebuffer.
        // Measured from the network time (`dl.elapsed`) only — NOT the wall span
        // that includes `send()` blocking on a full channel (healthy buffer
        // backpressure), which is what made this spike during fine playback.
        let dl_ms = dl.elapsed.as_millis() as u64;
        let seg_ms = segment
            .end_time()
            .saturating_sub(segment.start_time())
            .as_millis() as u64;
        if dl_ms > seg_ms {
            s.net_stall_ms.fetch_add(dl_ms - seg_ms, Ordering::Relaxed);
        }
    }
    let data_segment = DataSegment {
        id: index,
        data: dl.data,
    };
    if let Err(e) = sender.send(data_segment).await {
        return Err(format!("downstream receiver dropped: {:?}", e).into());
    }
    Ok(())
}

/// EWMA with smoothing factor ~1/8 (last 8 samples weight equivalent). The
/// instantaneous rate per segment is `bytes * 8 / elapsed_secs`; we fold it
/// into the running estimate so single fast/slow segments don't whipsaw
/// ABR. `Relaxed` is fine — readers tolerate stale-by-one values.
pub(super) fn update_bandwidth_ewma(ewma: &AtomicU64, bytes: usize, elapsed: Duration) {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 || bytes == 0 {
        return;
    }
    let instant_bps = (bytes as f64 * 8.0 / secs) as u64;
    let prev = ewma.load(Ordering::Relaxed);
    let next = if prev == 0 {
        instant_bps
    } else {
        // alpha = 1/8
        ((prev as u128 * 7 + instant_bps as u128) / 8) as u64
    };
    ewma.store(next, Ordering::Relaxed);
}

