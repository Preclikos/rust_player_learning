//! Debug HUD data: counters updated at segment / event granularity (never per
//! frame) and a bounded event log. The snapshot a HUD shows is only built
//! when a host asks for it (`Player::debug_snapshot`), so a HUD that is off
//! costs nothing and one that is on costs one read of these values per poll.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;

use crate::rt::Instant;

/// Which media pipeline a counter belongs to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Track {
    Video = 0,
    Audio = 1,
}

/// Download-side counters of one track. Written once per segment.
#[derive(Default)]
pub(crate) struct TrackCounters {
    pub segments: AtomicU64,
    pub last_index: AtomicU64,
    pub last_kib: AtomicU64,
    pub last_download_ms: AtomicU64,
    pub last_media_ms: AtomicU64,
    /// Segments waiting in the download -> decode channel after the last send.
    pub queued: AtomicU64,
    pub capacity: AtomicU64,
    pub retries: AtomicU64,
    /// Last segment's decrypt + parse on the prepare thread, ms (video).
    pub last_prepare_ms: AtomicU64,
}

/// Longest the event log gets; older entries fall off.
const LOG_ENTRIES: usize = 80;

#[derive(Clone, Serialize)]
pub struct DebugLogEntry {
    /// Seconds since the player was created.
    pub t_s: f64,
    /// Playback position when it happened, ms.
    pub position_ms: u64,
    /// Short category: `event`, `abr`, `pipeline`, `surface`, `audio`, `net`, `decode`, `seek`.
    pub kind: &'static str,
    pub text: String,
}

pub(crate) struct DebugCounters {
    started: Instant,
    pub tracks: [TrackCounters; 2],
    pub boundary_stalls: AtomicU64,
    pub boundary_stall_last_ms: AtomicU64,
    /// Buffer depths and network stall as the last Stats event reported them.
    pub video_ahead_ms: std::sync::atomic::AtomicI64,
    pub audio_ahead_ms: std::sync::atomic::AtomicI64,
    pub net_stall_last_ms: AtomicU64,
    /// Set by the player once its position counter exists.
    position_ms: std::sync::OnceLock<std::sync::Arc<AtomicU64>>,
    log: Mutex<VecDeque<DebugLogEntry>>,
}

impl Default for DebugCounters {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            tracks: Default::default(),
            boundary_stalls: AtomicU64::new(0),
            boundary_stall_last_ms: AtomicU64::new(0),
            video_ahead_ms: std::sync::atomic::AtomicI64::new(0),
            audio_ahead_ms: std::sync::atomic::AtomicI64::new(0),
            net_stall_last_ms: AtomicU64::new(0),
            position_ms: std::sync::OnceLock::new(),
            log: Mutex::new(VecDeque::with_capacity(LOG_ENTRIES)),
        }
    }
}

impl DebugCounters {
    pub fn track(&self, track: Track) -> &TrackCounters {
        &self.tracks[track as usize]
    }

    pub fn attach_position(&self, position_ms: std::sync::Arc<AtomicU64>) {
        let _ = self.position_ms.set(position_ms);
    }

    pub fn uptime_s(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Record a notable event. Only call from places that fire at most a few
    /// times a second (state changes, switches, failures), never per frame.
    pub fn log(&self, kind: &'static str, text: impl Into<String>) {
        let entry = DebugLogEntry {
            t_s: (self.uptime_s() * 10.0).round() / 10.0,
            position_ms: self.position_ms.get().map(|p| p.load(Ordering::Relaxed)).unwrap_or(0),
            kind,
            text: text.into(),
        };
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        if log.len() == LOG_ENTRIES {
            log.pop_front();
        }
        log.push_back(entry);
    }

    /// The newest `n` entries, oldest first.
    pub fn recent(&self, n: usize) -> Vec<DebugLogEntry> {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.iter().skip(log.len().saturating_sub(n)).cloned().collect()
    }
}

/// The player's event channel. Same interface as the broadcast sender it
/// wraps; every event except the periodic `Position` / `Stats` is also written
/// to the debug log, so the log shows the lifecycle without a hook at each of
/// the ~30 send sites.
pub(crate) struct EventBus {
    tx: tokio::sync::broadcast::Sender<crate::PlayerEvent>,
    debug: std::sync::Arc<crate::player::StatsState>,
}

impl EventBus {
    pub fn new(capacity: usize, stats: std::sync::Arc<crate::player::StatsState>) -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(capacity);
        Self { tx, debug: stats }
    }

    pub fn send(
        &self,
        ev: crate::PlayerEvent,
    ) -> Result<usize, tokio::sync::broadcast::error::SendError<crate::PlayerEvent>> {
        if let Some(text) = describe_event(&ev) {
            self.debug.debug.log("event", text);
        }
        self.tx.send(ev)
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::PlayerEvent> {
        self.tx.subscribe()
    }
}

/// One line for the debug log, `None` for the periodic events.
fn describe_event(ev: &crate::PlayerEvent) -> Option<String> {
    use crate::PlayerEvent as E;
    Some(match ev {
        E::Position { .. } | E::Stats { .. } => return None,
        E::Idle => "idle".into(),
        E::ManifestLoaded { duration, video_tracks, audio_tracks, subtitle_tracks } => format!(
            "manifest {} min, {video_tracks} video / {audio_tracks} audio / {subtitle_tracks} text",
            duration.as_secs() / 60
        ),
        E::Prepared => "prepared".into(),
        E::Buffering { reason } => format!("buffering ({reason:?})"),
        E::Playing => "playing".into(),
        E::Paused => "paused".into(),
        E::TrackChanged { kind, info } => {
            let size = match (info.width, info.height) {
                (Some(w), Some(h)) => format!(" {w}x{h}"),
                _ => String::new(),
            };
            format!(
                "{kind:?} -> repr {}{size} {} {:.1} Mb/s{}",
                info.representation_id,
                info.codec,
                info.bitrate_bps as f64 / 1_000_000.0,
                info.language.as_deref().map(|l| format!(" [{l}]")).unwrap_or_default()
            )
        }
        E::GlitchRecovered { detail } => format!("recovered: {detail}"),
        E::EndOfStream => "end of stream".into(),
        E::Error { kind, detail } => format!("ERROR {kind:?}: {detail}"),
    })
}

/// Everything a debug HUD shows, as one serialisable value. Built by
/// `Player::debug_snapshot`; `lines()` gives the same content as text so
/// every platform's HUD shows exactly the same thing.
#[derive(Clone, Serialize)]
pub struct DebugSnapshot {
    pub state: String,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub uptime_s: f64,
    pub video: DebugVideo,
    pub audio: DebugAudio,
    pub sync: DebugSync,
    pub network: DebugNetwork,
    pub abr: DebugAbr,
    pub session: DebugSession,
    pub events: Vec<DebugLogEntry>,
}

#[derive(Clone, Serialize, Default)]
pub struct DebugPipeline {
    /// Media ms downloaded ahead of the rendered position.
    pub buffer_ahead_ms: i64,
    /// Index of the segment most recently downloaded.
    pub segment: u64,
    pub segments_downloaded: u64,
    /// Segments waiting for the decoder / capacity of that queue.
    pub queued: u64,
    pub capacity: u64,
    pub last_segment_kib: u64,
    pub last_segment_download_ms: u64,
    /// Media duration of that segment, ms (download faster than this = keeping up).
    pub last_segment_media_ms: u64,
    pub retries: u64,
}

#[derive(Clone, Serialize, Default)]
pub struct DebugVideo {
    pub decoder: String,
    /// How frames reach the screen (direct video plane, GLES, wgpu backend…).
    pub output: String,
    pub representation: Option<u32>,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub bandwidth_bps: u64,
    pub frames_decoded: u64,
    pub frames_dropped: u64,
    pub frames_late: u64,
    pub last_prepare_ms: u64,
    pub boundary_stalls: u64,
    pub boundary_stall_last_ms: u64,
    pub render_gap_max_ms: u64,
    pub judder_frames: u64,
    /// Render intervals, ms: [<25, 25–41, 42–58, >58].
    pub interval_hist: [u64; 4],
    pub pipeline: DebugPipeline,
    /// Android direct mode: release lead and frames released vs shown.
    pub present: Option<DebugPresent>,
}

/// How early frames are released to the display, and whether it shows them
/// (Android direct mode, see `crate::present_lead`).
#[derive(Clone, Serialize, Default)]
pub struct DebugPresent {
    /// Lead applied to the last frame, ms.
    pub lead_ms: u64,
    /// The display's lower bound (deadline + jitter + one vsync, ≥ 50 ms).
    pub display_floor_ms: u64,
    /// Cap after decoder starvation, ms; `None` = no cap.
    pub cap_ms: Option<u64>,
    pub deadline_ms: f64,
    pub vsync_ms: f64,
    pub app_vsync_offset_ms: f64,
    pub decoder_starved: u64,
    /// Frames released to the display in the last second.
    pub released_per_s: u64,
    /// Frames the codec reported shown in the last second (`None` below API 33).
    pub shown_per_s: Option<u64>,
    /// Release stamps snapped to the display's vsync grid.
    pub vsync_snap: bool,
}

#[derive(Clone, Serialize, Default)]
pub struct DebugAudio {
    pub representation: Option<u32>,
    pub codec: String,
    pub channels: Option<u32>,
    pub sample_rate: u32,
    pub passthrough: bool,
    /// The output as the sink describes itself (backend, format, written /
    /// played, what is queued in the device).
    pub output: String,
    pub underruns: u64,
    pub peak_db: Option<[f32; 2]>,
    pub pipeline: DebugPipeline,
}

#[derive(Clone, Serialize, Default)]
pub struct DebugSync {
    pub av_drift_ms: Option<i64>,
    pub av_drift_max_ms: i64,
    /// `audio` while the audio device drives the clock, `wall` on fallback.
    pub clock: String,
    pub clock_wall_fallbacks: u64,
    pub output_latency_ms: u64,
}

#[derive(Clone, Serialize, Default)]
pub struct DebugNetwork {
    pub bandwidth_bps: u64,
    pub bytes_total: u64,
    pub net_stall_ms: u64,
}

#[derive(Clone, Serialize, Default)]
pub struct DebugAbr {
    pub strategy: String,
    pub profile: String,
    pub last_switch_s_ago: Option<f64>,
    pub buffer_target_s: u32,
}

#[derive(Clone, Serialize, Default)]
pub struct DebugSession {
    pub stall_events: u64,
    pub stall_ms_total: u64,
    pub pipeline_retries: u64,
    pub audio_output_rebuilds: u32,
}

fn mmss(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn secs(ms: i64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

fn mbps(bps: u64) -> String {
    format!("{:.1} Mb/s", bps as f64 / 1_000_000.0)
}

fn pipeline_line(p: &DebugPipeline) -> String {
    format!(
        "buf {}  seg #{} ({} done)  queue {}/{}  last {} KiB in {} ms / {} ms media{}",
        secs(p.buffer_ahead_ms),
        p.segment,
        p.segments_downloaded,
        p.queued,
        p.capacity,
        p.last_segment_kib,
        p.last_segment_download_ms,
        p.last_segment_media_ms,
        if p.retries > 0 { format!("  retries {}", p.retries) } else { String::new() }
    )
}

impl DebugSnapshot {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// The HUD as text lines, identical on every platform. `events` recent
    /// log lines are appended at the end.
    pub fn lines(&self, events: usize) -> Vec<String> {
        let v = &self.video;
        let a = &self.audio;
        let mut out = vec![
            format!(
                "{}  {} / {}  up {:.0}s",
                self.state,
                mmss(self.position_ms),
                mmss(self.duration_ms),
                self.uptime_s
            ),
            format!(
                "V {}x{} {} {}  repr {}  {}",
                v.width,
                v.height,
                v.codec,
                mbps(v.bandwidth_bps),
                v.representation.map(|r| r.to_string()).unwrap_or_else(|| "-".into()),
                v.decoder
            ),
            format!("V out {}", if v.output.is_empty() { "-" } else { v.output.as_str() }),
            format!("V {}", pipeline_line(&v.pipeline)),
            format!(
                "V frames {}  drop {}  late {}  prepare {} ms  boundary stalls {} (last {} ms)",
                v.frames_decoded,
                v.frames_dropped,
                v.frames_late,
                v.last_prepare_ms,
                v.boundary_stalls,
                v.boundary_stall_last_ms
            ),
            format!(
                "V render gap max {} ms  judder {}  int <25 {}  25-41 {}  42-58 {}  >58 {}",
                v.render_gap_max_ms,
                v.judder_frames,
                v.interval_hist[0],
                v.interval_hist[1],
                v.interval_hist[2],
                v.interval_hist[3]
            ),
        ];
        if let Some(p) = &v.present {
            out.push(format!(
                "V present lead {} ms (display {} ms{})  deadline {:.1} ms  vsync {:.1} ms{}  starved {}  shown/released {}/{} per s",
                p.lead_ms,
                p.display_floor_ms,
                p.cap_ms.map(|c| format!(", cap {c} ms")).unwrap_or_default(),
                p.deadline_ms,
                p.vsync_ms,
                if p.vsync_snap { " snapped" } else { "" },
                p.decoder_starved,
                p.shown_per_s.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
                p.released_per_s
            ));
        }
        out.extend([
            format!(
                "A {} {}ch {} Hz{}  repr {}  underruns {}",
                a.codec,
                a.channels.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                a.sample_rate,
                if a.passthrough { "  PASSTHROUGH" } else { "" },
                a.representation.map(|r| r.to_string()).unwrap_or_else(|| "-".into()),
                a.underruns
            ),
            format!("A out {}", if a.output.is_empty() { "-" } else { a.output.as_str() }),
            format!("A {}", pipeline_line(&a.pipeline)),
            format!(
                "sync drift {} (max {} ms)  clock {} (fallbacks {})  out latency {} ms",
                self.sync.av_drift_ms.map(|d| format!("{d} ms")).unwrap_or_else(|| "-".into()),
                self.sync.av_drift_max_ms,
                self.sync.clock,
                self.sync.clock_wall_fallbacks,
                self.sync.output_latency_ms
            ),
            format!(
                "net {}  total {:.1} MiB  stall {} ms   abr {} / {}  target {}s{}",
                mbps(self.network.bandwidth_bps),
                self.network.bytes_total as f64 / 1_048_576.0,
                self.network.net_stall_ms,
                self.abr.strategy,
                self.abr.profile,
                self.abr.buffer_target_s,
                self.abr
                    .last_switch_s_ago
                    .map(|s| format!("  last switch {s:.0}s ago"))
                    .unwrap_or_default()
            ),
            format!(
                "session stalls {} ({} ms)  pipeline retries {}  audio output rebuilds {}",
                self.session.stall_events,
                self.session.stall_ms_total,
                self.session.pipeline_retries,
                self.session.audio_output_rebuilds
            ),
        ]);
        let skip = self.events.len().saturating_sub(events);
        for e in &self.events[skip..] {
            out.push(format!("{:>6.1}s {:>5} {:<8} {}", e.t_s, mmss(e.position_ms), e.kind, e.text));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_the_newest_entries() {
        let c = DebugCounters::default();
        for i in 0..(LOG_ENTRIES + 5) {
            c.log("event", format!("e{i}"));
        }
        let recent = c.recent(3);
        let texts: Vec<_> = recent.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec![format!("e{}", LOG_ENTRIES + 2), format!("e{}", LOG_ENTRIES + 3), format!("e{}", LOG_ENTRIES + 4)]);
        assert_eq!(c.recent(1000).len(), LOG_ENTRIES);
    }

    #[test]
    fn snapshot_serialises_and_formats() {
        let snap = DebugSnapshot {
            state: "playing".into(),
            position_ms: 61_000,
            duration_ms: 3_600_000,
            uptime_s: 12.0,
            video: DebugVideo { width: 3840, height: 2160, ..Default::default() },
            audio: DebugAudio { passthrough: true, ..Default::default() },
            sync: DebugSync::default(),
            network: DebugNetwork::default(),
            abr: DebugAbr::default(),
            session: DebugSession::default(),
            events: vec![DebugLogEntry { t_s: 1.5, position_ms: 0, kind: "event", text: "playing".into() }],
        };
        let json = snap.to_json();
        assert!(json.contains("\"passthrough\":true") && json.contains("\"width\":3840"));
        let lines = snap.lines(10);
        assert!(lines[0].starts_with("playing  1:01 / 60:00"));
        assert!(lines.iter().any(|l| l.contains("PASSTHROUGH")));
        assert!(lines.last().unwrap().ends_with("playing"));
    }
}
