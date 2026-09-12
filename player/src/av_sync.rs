//! Platform-independent A/V sync primitives shared by every audio sink and
//! by the sync loops in `player.rs`. Everything in here is pure bookkeeping
//! with no device access, so it is unit-tested directly (see the bottom of
//! the file) — the device-specific sinks (cpal, Android `AudioTrack`, the
//! null sink) only plug their consumption/presentation counters into it.
//!
//! # The two invariants this module enforces
//!
//! 1. **One media timeline.** Video frames, audio samples and the master
//!    clock all live on the same 0-based axis: `media = raw_pts − origin`,
//!    where `origin` is the first segment's presentation time. Neither side
//!    is anchored to "when the first frame happened to arrive" any more —
//!    a frame with media pts P is shown when the clock reads P, and the
//!    clock reads P exactly when the audio sample for P is audible.
//!
//! 2. **The clock counts only THIS pipeline's audio.** A seek / track switch
//!    flushes the sink, but every real device still holds a tail of the OLD
//!    content in its buffer (cpal: one callback buffer; Android `AudioTrack`:
//!    the whole track buffer, 100–300 ms). The new pipeline's first sample is
//!    not audible until that tail has drained. `FlushState` records the
//!    device position at which the post-flush content begins (the
//!    *boundary*), and `played_since_flush` is measured from there — so the
//!    clock reads "0 ms of new audio played" while the tail drains instead of
//!    running ahead by the tail. Running ahead by the tail on every rebuild
//!    was the "video leads audio after switching on Android" bug.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// A block of interleaved-stereo f32 PCM queued to an audio sink, tagged with
/// the flush generation current when it was queued. The device consumer
/// drops chunks whose generation is stale (queued before the last flush) —
/// this is what makes `flush()` race-free: content decoded for the OLD
/// pipeline can never slip into the new one, and content queued after the
/// flush can never be discarded by a late flush.
pub struct AudioChunk {
    pub gen: u64,
    pub samples: Vec<f32>,
}

/// Flush-generation + playback-boundary bookkeeping shared between an audio
/// sink's producer side (`put_samples` / `flush`) and its device consumer
/// (cpal callback / writer thread).
///
/// Positions are in whatever unit the consumer counts in (samples for cpal,
/// device frames for `AudioTrack`); the sink converts to ms.
pub struct FlushState {
    /// Bumped by every `flush()`. Chunks carry the generation current at the
    /// time they were queued.
    gen: AtomicU64,
    /// `(generation, device position at which that generation's first
    /// sample was handed to the device)`. Only meaningful while
    /// `generation == gen` — otherwise the current generation's content has
    /// not reached the device yet. Starts as `NO_BOUNDARY` so even the very
    /// first generation is marked by the consumer, not assumed.
    boundary: Mutex<(u64, u64)>,
}

/// Sentinel generation meaning "no boundary marked yet".
const NO_BOUNDARY: u64 = u64::MAX;

impl Default for FlushState {
    fn default() -> Self {
        Self {
            gen: AtomicU64::new(0),
            boundary: Mutex::new((NO_BOUNDARY, 0)),
        }
    }
}

impl FlushState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generation to tag newly queued chunks with.
    pub fn current_gen(&self) -> u64 {
        self.gen.load(Ordering::Acquire)
    }

    /// Start a new generation: everything queued before this call is stale.
    /// Returns the new generation.
    pub fn flush(&self) -> u64 {
        self.gen.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The consumer reached the first chunk of `gen`; `position` is the
    /// device position (cumulative) at that instant — everything below it is
    /// old content still draining, everything from it on is this generation.
    pub fn mark_boundary(&self, gen: u64, position: u64) {
        *self.boundary.lock().unwrap() = (gen, position);
    }

    /// True when the consumer has already marked a boundary for `gen`.
    pub fn has_boundary(&self, gen: u64) -> bool {
        self.boundary.lock().unwrap().0 == gen
    }

    /// Device position at which the CURRENT generation's content begins, or
    /// `None` if it hasn't reached the device yet.
    pub fn boundary(&self) -> Option<u64> {
        let (g, pos) = *self.boundary.lock().unwrap();
        (g == self.current_gen()).then_some(pos)
    }

    /// How much of the current generation's content the device has presented,
    /// given its cumulative presented position. 0 while the previous
    /// generation's tail is still draining (or nothing new was queued yet).
    pub fn played_since_flush(&self, presented: u64) -> u64 {
        match self.boundary() {
            Some(b) => presented.saturating_sub(b),
            None => 0,
        }
    }
}

/// Result of pulling the next chunk off a sink's queue.
pub enum Pulled {
    /// A live chunk of the current generation. `starts_gen` is true for the
    /// first chunk of a generation — the consumer must mark the boundary
    /// with its own device position BEFORE handing any of it to the device.
    Chunk { samples: Vec<f32>, gen: u64, starts_gen: bool },
    /// Queue empty right now.
    Empty,
    /// Producer gone (sink torn down).
    Closed,
}

/// Consumer-side cursor over a chunk queue: filters stale generations and
/// flags generation starts. Device-agnostic — the cpal callback drives it
/// sample-by-sample (`next_sample`), the Android writer chunk-by-chunk
/// (`next_chunk` / `blocking_next_chunk`).
pub struct ChunkCursor {
    rx: tokio::sync::mpsc::Receiver<AudioChunk>,
    /// Current chunk + read offset (sample-level API only).
    cur: Option<(Vec<f32>, usize)>,
    cur_gen: u64,
    state: std::sync::Arc<FlushState>,
    /// Samples handed to the device by the sample-level API (this cursor is
    /// the sole writer; `commit` publishes it).
    consumed: u64,
    consumed_shared: std::sync::Arc<AtomicU64>,
    closed: bool,
}

impl ChunkCursor {
    pub fn new(
        rx: tokio::sync::mpsc::Receiver<AudioChunk>,
        state: std::sync::Arc<FlushState>,
        consumed_shared: std::sync::Arc<AtomicU64>,
    ) -> Self {
        Self {
            rx,
            cur: None,
            cur_gen: 0,
            state,
            consumed: 0,
            consumed_shared,
            closed: false,
        }
    }

    fn classify(&mut self, chunk: AudioChunk) -> Option<Pulled> {
        let live = self.state.current_gen();
        if chunk.gen < live {
            return None; // stale — queued before the last flush
        }
        let starts_gen = !self.state.has_boundary(chunk.gen);
        self.cur_gen = chunk.gen;
        Some(Pulled::Chunk {
            samples: chunk.samples,
            gen: chunk.gen,
            starts_gen,
        })
    }

    /// Non-blocking pull (realtime callbacks).
    pub fn next_chunk(&mut self) -> Pulled {
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    if let Some(p) = self.classify(chunk) {
                        return p;
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Pulled::Empty,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.closed = true;
                    return Pulled::Closed;
                }
            }
        }
    }

    /// Blocking pull (plain writer threads). Never returns `Empty`.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn blocking_next_chunk(&mut self) -> Pulled {
        loop {
            match self.rx.blocking_recv() {
                Some(chunk) => {
                    if let Some(p) = self.classify(chunk) {
                        return p;
                    }
                }
                None => {
                    self.closed = true;
                    return Pulled::Closed;
                }
            }
        }
    }

    /// Sample-level pull for the cpal callback. `None` = nothing available
    /// (emit silence). Marks the boundary itself with the consumed-sample
    /// position, and drops the rest of the current chunk the moment a flush
    /// supersedes its generation.
    pub fn next_sample(&mut self) -> Option<f32> {
        loop {
            if let Some((buf, off)) = self.cur.as_mut() {
                if self.cur_gen == self.state.current_gen() {
                    if *off < buf.len() {
                        let s = buf[*off];
                        *off += 1;
                        self.consumed += 1;
                        return Some(s);
                    }
                }
                self.cur = None;
            }
            match self.next_chunk() {
                Pulled::Chunk { samples, gen, starts_gen } => {
                    if starts_gen {
                        self.state.mark_boundary(gen, self.consumed);
                    }
                    self.cur = Some((samples, 0));
                }
                Pulled::Empty | Pulled::Closed => return None,
            }
        }
    }

    /// Publish the consumed-sample count (call once per callback).
    pub fn commit(&self) {
        self.consumed_shared.store(self.consumed, Ordering::Release);
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

/// 0-based media time of a frame/sample in ms (`raw_pts − origin`, clamped).
pub fn media_pts_ms(raw_pts_us: i64, origin_us: i64) -> u64 {
    ((raw_pts_us - origin_us).max(0) / 1000) as u64
}

/// What `AudioAligner` decided for one decoded audio frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlignAction {
    /// The whole frame lies before the target / inside an overlap — skip it.
    Drop,
    /// Emit the frame after skipping `skip_frames` leading per-channel frames
    /// and prepending `pad_frames` per-channel frames of silence.
    Emit { skip_frames: usize, pad_frames: usize },
}

/// Keeps the PCM handed to the sink continuous on the media axis.
///
/// * **Start alignment.** The first audible sample must be the one at the
///   seek target: DASH audio segments rarely share boundaries with video, so
///   the segment containing the target starts up to ~1 s earlier. Leading
///   samples are trimmed (or silence is padded when the audio starts AFTER
///   the target). Without it every subsequent sample is late by that gap —
///   perceived as "audio lags after seeking".
/// * **Continuity.** After alignment each frame is expected exactly where the
///   previous one ended. A gap (a decoder swallowed a corrupt AU, an edit
///   list / priming discontinuity at a segment boundary) is padded with
///   silence and an overlap is trimmed, so a dropped 32 ms E-AC-3 frame does
///   not shift ALL later audio 32 ms early — that shift is permanent and
///   accumulates across a long playback.
///
/// Positions are tracked in per-channel sample frames at the sink rate so
/// integer-ms pts rounding (an AAC frame is 21.333 ms) cannot accumulate.
pub struct AudioAligner {
    rate: i64,
    /// Absolute media ms the first audible sample must correspond to.
    target_abs_ms: i64,
    /// Media position (in sample frames) where the next frame is expected,
    /// once aligned.
    expected_frames: Option<i64>,
    /// Deviation below which a frame is considered contiguous.
    tolerance_frames: i64,
    /// Longest gap we fill with silence; beyond it the stream is treated as
    /// a genuine discontinuity and re-anchored (logged by the caller).
    max_pad_frames: i64,
}

impl AudioAligner {
    /// Tolerance for calling consecutive frames contiguous. Above the pts
    /// rounding jitter (≤ 1 ms) and the few-ms output jitter of a rate
    /// converter (44.1 → 48 kHz content), but BELOW one decoded frame
    /// (AAC 21.3 ms, (E-)AC-3 32 ms) so a single lost or duplicated frame is
    /// still caught and corrected.
    pub const TOLERANCE_MS: i64 = 10;
    /// Cap on silence insertion for a single gap.
    pub const MAX_PAD_MS: i64 = 5_000;

    pub fn new(sample_rate: u32, target_abs_ms: i64) -> Self {
        let rate = sample_rate.max(1) as i64;
        Self {
            rate,
            target_abs_ms,
            expected_frames: None,
            tolerance_frames: Self::TOLERANCE_MS * rate / 1000,
            max_pad_frames: Self::MAX_PAD_MS * rate / 1000,
        }
    }

    fn ms_to_frames(&self, ms: i64) -> i64 {
        ms * self.rate / 1000
    }

    pub fn is_aligned(&self) -> bool {
        self.expected_frames.is_some()
    }

    /// Decide what to do with a frame that starts at absolute media
    /// `pts_ms` and holds `frames` per-channel sample frames.
    pub fn plan(&mut self, pts_ms: i64, frames: usize) -> AlignAction {
        let frames_i = frames as i64;
        let pts_frames = self.ms_to_frames(pts_ms);
        match self.expected_frames {
            None => {
                let target = self.ms_to_frames(self.target_abs_ms);
                let end = pts_frames + frames_i;
                if end <= target {
                    return AlignAction::Drop;
                }
                if pts_frames < target {
                    let skip = (target - pts_frames) as usize;
                    self.expected_frames = Some(end);
                    return AlignAction::Emit { skip_frames: skip, pad_frames: 0 };
                }
                let pad = (pts_frames - target) as usize;
                self.expected_frames = Some(end);
                AlignAction::Emit { skip_frames: 0, pad_frames: pad }
            }
            Some(expected) => {
                let dev = pts_frames - expected;
                if dev.abs() <= self.tolerance_frames {
                    self.expected_frames = Some(expected + frames_i);
                    return AlignAction::Emit { skip_frames: 0, pad_frames: 0 };
                }
                if dev > 0 {
                    // Gap: fill with silence (bounded), then the frame.
                    let pad = dev.min(self.max_pad_frames);
                    // Past the cap this is a discontinuity: re-anchor on the
                    // frame's own pts rather than fall permanently behind.
                    self.expected_frames = Some(pts_frames + frames_i);
                    return AlignAction::Emit { skip_frames: 0, pad_frames: pad as usize };
                }
                // Overlap: trim what was already emitted.
                let overlap = -dev;
                if overlap >= frames_i {
                    // Entirely inside already-emitted content.
                    return AlignAction::Drop;
                }
                self.expected_frames = Some(expected + frames_i - overlap);
                AlignAction::Emit { skip_frames: overlap as usize, pad_frames: 0 }
            }
        }
    }

    /// Absolute media ms of the next expected sample, once aligned.
    #[cfg(test)]
    pub fn expected_ms(&self) -> Option<i64> {
        self.expected_frames.map(|f| f * 1000 / self.rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ---------------- FlushState / played_since_flush ----------------

    #[test]
    fn played_since_flush_is_zero_until_new_content_reaches_device() {
        let st = FlushState::new();
        // Session start: gen 0 content begins at position 0.
        st.mark_boundary(0, 0);
        assert_eq!(st.played_since_flush(48_000), 48_000);
        // Seek: flush. The device has presented 48_000 and still holds a
        // tail of old content up to 52_000 (written but not presented).
        let g = st.flush();
        assert_eq!(g, 1);
        // Until the consumer reaches gen-1 content the clock must not move
        // — this was the Android "video leads by the AudioTrack tail" bug.
        assert_eq!(st.played_since_flush(50_000), 0);
        assert_eq!(st.boundary(), None);
        // Consumer hands the first gen-1 chunk to the device at 52_000.
        st.mark_boundary(1, 52_000);
        assert_eq!(st.played_since_flush(50_000), 0); // tail still draining
        assert_eq!(st.played_since_flush(52_000), 0); // first new sample due now
        assert_eq!(st.played_since_flush(52_960), 960); // 20 ms of NEW audio
    }

    #[test]
    fn double_flush_invalidates_intermediate_boundary() {
        let st = FlushState::new();
        st.mark_boundary(0, 0);
        st.flush(); // gen 1
        st.mark_boundary(1, 1000);
        st.flush(); // gen 2 before any gen-2 content arrived
        assert_eq!(st.boundary(), None);
        assert_eq!(st.played_since_flush(5000), 0);
        st.mark_boundary(2, 3000);
        assert_eq!(st.played_since_flush(5000), 2000);
    }

    // ---------------- ChunkCursor ----------------

    fn chunk(gen: u64, n: usize, v: f32) -> AudioChunk {
        AudioChunk { gen, samples: vec![v; n] }
    }

    #[test]
    fn cursor_drops_stale_generation_and_marks_boundary() {
        let st = Arc::new(FlushState::new());
        let consumed = Arc::new(AtomicU64::new(0));
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut cur = ChunkCursor::new(rx, st.clone(), consumed.clone());

        // Gen 0: two chunks, consume 6 of 8 samples.
        tx.try_send(chunk(0, 4, 1.0)).unwrap();
        tx.try_send(chunk(0, 4, 1.0)).unwrap();
        for _ in 0..6 {
            assert_eq!(cur.next_sample(), Some(1.0));
        }
        cur.commit();
        assert_eq!(consumed.load(Ordering::Relaxed), 6);
        assert_eq!(st.boundary(), Some(0));

        // Producer of the OLD pipeline queues one more chunk, then a seek
        // flushes, then the NEW pipeline queues content.
        tx.try_send(chunk(0, 4, 1.0)).unwrap();
        st.flush();
        tx.try_send(chunk(1, 4, 2.0)).unwrap();

        // The remainder of the current gen-0 chunk and the late gen-0 chunk
        // are all dropped; the next sample is NEW content, and the boundary
        // is the consumed position at that moment (6).
        assert_eq!(cur.next_sample(), Some(2.0));
        assert_eq!(st.boundary(), Some(6));
        assert_eq!(st.played_since_flush(6), 0);
        for _ in 0..3 {
            assert_eq!(cur.next_sample(), Some(2.0));
        }
        assert_eq!(cur.next_sample(), None); // empty, not closed
        assert!(!cur.is_closed());
        cur.commit();
        assert_eq!(consumed.load(Ordering::Relaxed), 10);
        assert_eq!(st.played_since_flush(10), 4);
    }

    #[test]
    fn cursor_chunk_api_flags_generation_start_once() {
        let st = Arc::new(FlushState::new());
        let consumed = Arc::new(AtomicU64::new(0));
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut cur = ChunkCursor::new(rx, st.clone(), consumed);
        tx.try_send(chunk(0, 2, 0.0)).unwrap();
        tx.try_send(chunk(0, 2, 0.0)).unwrap();
        match cur.next_chunk() {
            Pulled::Chunk { starts_gen, gen, .. } => {
                assert!(starts_gen);
                assert_eq!(gen, 0);
                // Writer-style consumers mark with their own position.
                st.mark_boundary(gen, 1234);
            }
            _ => panic!("expected chunk"),
        }
        match cur.next_chunk() {
            Pulled::Chunk { starts_gen, .. } => assert!(!starts_gen),
            _ => panic!("expected chunk"),
        }
        assert!(matches!(cur.next_chunk(), Pulled::Empty));
        drop(tx);
        assert!(matches!(cur.next_chunk(), Pulled::Closed));
        assert!(cur.is_closed());
    }

    // ---------------- AudioAligner ----------------

    const RATE: u32 = 48_000;
    const AAC: usize = 1024; // per-channel frames per AAC frame (21.333 ms)

    #[test]
    fn aligner_drops_frames_wholly_before_target_and_trims_the_straddler() {
        // Target 10.000 s; audio segment starts at 9.500 s.
        let mut al = AudioAligner::new(RATE, 10_000);
        let mut pts_frames = 9_500i64 * 48;
        let mut n = 0;
        loop {
            let pts_ms = pts_frames * 1000 / 48_000;
            let a = al.plan(pts_ms, AAC);
            if a != AlignAction::Drop {
                // The straddling frame: skip up to the target sample (the
                // aligner works from the integer-ms pts it was given).
                let expected_skip = (10_000 * 48 - pts_ms * 48) as usize;
                assert_eq!(a, AlignAction::Emit { skip_frames: expected_skip, pad_frames: 0 });
                assert!(expected_skip < AAC);
                break;
            }
            pts_frames += AAC as i64;
            n += 1;
            assert!(n < 30, "never reached the target");
        }
        assert!(al.is_aligned());
    }

    #[test]
    fn aligner_pads_silence_when_audio_starts_after_target() {
        let mut al = AudioAligner::new(RATE, 10_000);
        // First audio frame at 10.100 s → 100 ms of silence first.
        assert_eq!(
            al.plan(10_100, AAC),
            AlignAction::Emit { skip_frames: 0, pad_frames: 4_800 }
        );
    }

    #[test]
    fn aligner_target_on_absolute_axis_with_origin() {
        // Content origin 7 979 ms (non-zero BMDT), seek target 0 → the first
        // audible sample is the one at absolute 7 979 ms, NOT 8 s of
        // silence followed by late audio. (Regression: the old trim compared
        // absolute audio pts against the 0-based target.)
        let mut al = AudioAligner::new(RATE, 7_979);
        assert_eq!(
            al.plan(7_979, AAC),
            AlignAction::Emit { skip_frames: 0, pad_frames: 0 }
        );
    }

    #[test]
    fn aligner_passes_contiguous_frames_despite_ms_rounding() {
        let mut al = AudioAligner::new(RATE, 0);
        // 21.333 ms frames stamped with integer-ms pts, 200 frames = 4.27 s.
        for i in 0..200i64 {
            let pts_ms = i * AAC as i64 * 1000 / 48_000; // floor → 0,21,42,64,…
            assert_eq!(
                al.plan(pts_ms, AAC),
                AlignAction::Emit { skip_frames: 0, pad_frames: 0 },
                "frame {i} flagged discontinuous by rounding jitter"
            );
        }
    }

    #[test]
    fn aligner_fills_a_swallowed_frame_with_silence() {
        let mut al = AudioAligner::new(RATE, 0);
        // E-AC-3: 1536-frame (32 ms) AUs. Frames 0,1 fine, frame 2 lost to a
        // decode error, frame 3 arrives at 96 ms.
        assert_eq!(al.plan(0, 1536), AlignAction::Emit { skip_frames: 0, pad_frames: 0 });
        assert_eq!(al.plan(32, 1536), AlignAction::Emit { skip_frames: 0, pad_frames: 0 });
        let a = al.plan(96, 1536);
        match a {
            AlignAction::Emit { skip_frames: 0, pad_frames } => {
                // ~32 ms of silence (1536 frames ± ms rounding).
                assert!((1500..=1560).contains(&pad_frames), "pad={pad_frames}");
            }
            other => panic!("expected pad, got {other:?}"),
        }
        // And the stream is contiguous again from there.
        assert_eq!(al.plan(128, 1536), AlignAction::Emit { skip_frames: 0, pad_frames: 0 });
    }

    #[test]
    fn aligner_trims_an_overlap_and_drops_fully_repeated_frames() {
        let mut al = AudioAligner::new(RATE, 0);
        assert_eq!(al.plan(0, AAC), AlignAction::Emit { skip_frames: 0, pad_frames: 0 });
        // Repeat of frame 0 (segment-boundary duplicate): entirely emitted.
        assert_eq!(al.plan(0, AAC), AlignAction::Drop);
        // A frame that overlaps the last 10 ms already emitted.
        let a = al.plan(11, AAC); // expected at 21.333 ms, arrives at 11 ms
        match a {
            AlignAction::Emit { skip_frames, pad_frames: 0 } => {
                assert!((480..=520).contains(&skip_frames), "skip={skip_frames}");
            }
            other => panic!("expected trim, got {other:?}"),
        }
    }

    #[test]
    fn aligner_caps_pad_and_reanchors_on_a_true_discontinuity() {
        let mut al = AudioAligner::new(RATE, 0);
        assert_eq!(al.plan(0, AAC), AlignAction::Emit { skip_frames: 0, pad_frames: 0 });
        // A 60 s jump (new period / broken timeline): pad at most MAX_PAD_MS
        // and continue from the frame's own pts.
        match al.plan(60_000, AAC) {
            AlignAction::Emit { pad_frames, .. } => {
                assert_eq!(pad_frames as i64, AudioAligner::MAX_PAD_MS * 48);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(al.expected_ms(), Some(60_000 + 21));
    }

    // ---------------- media_pts_ms ----------------

    #[test]
    fn media_pts_subtracts_origin_and_clamps() {
        assert_eq!(media_pts_ms(7_979_000 + 41_708, 7_979_000), 41);
        assert_eq!(media_pts_ms(7_900_000, 7_979_000), 0);
        assert_eq!(media_pts_ms(1_000_000, 0), 1000);
    }
}
