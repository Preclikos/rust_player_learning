//! Platform-agnostic **bridge core**.
//!
//! The Android (`app-android`) and iOS (`app-ios`) shells used to each carry
//! their own copy of the open_url → prepare → pick-tracks → play() dance plus
//! event forwarding. This module hoists all of that platform-agnostic logic
//! into one place so the two thin shells stay in lock-step and so the shape is
//! a clean precursor to a generated Kotlin/Swift binding library.
//!
//! A shell does three things:
//!   1. construct a [`player::Player`] for its surface,
//!   2. implement [`BridgeHost`] (push events to the host UI; provide the
//!      provider policy — auth/URL-rewrite + DRM key resolution),
//!   3. call [`start`] and drive the returned [`BridgeHandle`].
//!
//! The provider hooks (`intercept` / `resolve_key`) carry NO BlackZone-specific
//! logic here — the test shells implement them trivially (passthrough + baked
//! ClearKeys). A product app implements real auth/license there. The player
//! crate stays provider-agnostic exactly as before.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc, Notify};

// Provider-facing types re-exported so a shell implements `BridgeHost` against
// a single import path (`app_shared::bridge::{BoxError, PreparedRequest, …}`).
pub use player::{BoxError, PreparedRequest, RequestKind};

use player::{
    AbrStrategy, LicenseResolver, Player, PlayerEvent, RequestInterceptor, Tracks,
};

/// Implemented by each platform shell. The bridge core calls these to (a) push
/// player events to the host UI as unified JSON, and (b) delegate provider
/// policy back to the host.
///
/// `intercept` defaults to passthrough (the right behaviour for the
/// self-contained test stream); `resolve_key` is required because the encrypted
/// fixture needs a key per KID.
#[async_trait]
pub trait BridgeHost: Send + Sync + 'static {
    /// Fire-and-forget: one player event serialized to the unified JSON schema
    /// (see [`event_to_json`]). Called from a Tokio worker — the shell is
    /// responsible for hopping to its UI thread if needed.
    fn on_event(&self, json: String);

    /// Provider hook: rewrite the URL / add headers before a request is sent.
    async fn intercept(
        &self,
        url: String,
        kind: RequestKind,
    ) -> Result<PreparedRequest, BoxError> {
        let _ = kind;
        Ok(PreparedRequest {
            url,
            ..Default::default()
        })
    }

    /// Provider hook: resolve a CENC Key ID to its 16-byte ClearKey.
    async fn resolve_key(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError>;
}

// --- player-trait adapters: forward the canonical player callbacks to the host

struct HostInterceptor(Arc<dyn BridgeHost>);

#[async_trait]
impl RequestInterceptor for HostInterceptor {
    async fn intercept(
        &self,
        url: String,
        kind: RequestKind,
    ) -> Result<PreparedRequest, BoxError> {
        self.0.intercept(url, kind).await
    }
}

struct HostResolver(Arc<dyn BridgeHost>);

#[async_trait]
impl LicenseResolver for HostResolver {
    async fn resolve(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        self.0.resolve_key(kid).await
    }
}

// --- commands that need the (private) Tracks type for an index→repr lookup ---

enum Cmd {
    Video { adapt: usize, repr: usize, soft: bool },
    VideoAuto,
    Audio { adapt: usize, repr: usize },
    Subtitle { adapt: usize, repr: usize },
    ClearSubs,
}

/// Pre-`play()` configuration for [`start`]. `Default` reproduces the
/// self-contained test-shell behaviour (file-flag audio passthrough,
/// auto-select the first subtitle, no resume), so `app-android` / `app-ios`
/// pass `StartConfig::default()` and are unchanged. A product host overrides
/// these to drive resume, a real sink-gated passthrough decision, and its own
/// (post-play) subtitle selection.
pub struct StartConfig {
    /// Absolute resume position, applied before the first `play()`. `None`
    /// starts at 0 (unless [`start_fraction`](Self::start_fraction) is set).
    pub start_position: Option<Duration>,
    /// Resume as a fraction of the real media duration (`0.0..=1.0`), resolved
    /// against the duration discovered in `prepare()` — for product apps that
    /// store resume as a percent and can't compute the exact `Duration` before
    /// play. Used only when `start_position` is `None`.
    pub start_fraction: Option<f32>,
    /// `Some(true/false)` forces passthrough on/off (the host already made the
    /// `isDirectPlaybackSupported` + codec decision); `None` keeps the shell's
    /// env / `audio_passthrough.txt` file-flag behaviour.
    pub audio_passthrough: Option<bool>,
    /// Auto-select the first subtitle track during default selection. Product
    /// hosts set `false` and apply their own language/forced policy after play.
    pub auto_select_subtitle: bool,
    /// Preferred audio language (BCP-47, e.g. `"cs"`, `"en"`) applied DURING
    /// default selection — i.e. before the first `play()`, while the pipeline
    /// is not yet live. This is the rebuild-free way to honour a saved
    /// audio-language preference: picking it here means `play()` starts on the
    /// right track, with NO post-start `selectAudio()` (which would
    /// `seek(position())`-rebuild on top of a resume — the BlackZone startup
    /// stall). `None` keeps the codec-default pick. No match → codec default.
    pub preferred_audio_language: Option<String>,
    /// Preferred subtitle language (BCP-47) applied during default selection.
    /// When `Some`, the matching text track is selected before first frame
    /// regardless of [`auto_select_subtitle`](Self::auto_select_subtitle); the
    /// host no longer needs a post-start `selectSubtitle()`. No match → falls
    /// back to the `auto_select_subtitle` policy.
    pub preferred_subtitle_language: Option<String>,
}

impl Default for StartConfig {
    fn default() -> Self {
        Self {
            start_position: None,
            start_fraction: None,
            audio_passthrough: None,
            auto_select_subtitle: true,
            preferred_audio_language: None,
            preferred_subtitle_language: None,
        }
    }
}

/// Opaque handle the shell keeps for the player's lifetime. Cheap to hold; all
/// state lives behind `Arc`s shared with the background orchestrator + event
/// pump. Track switches are dispatched through a command channel because the
/// index→representation lookup needs the orchestrator-owned [`Tracks`].
pub struct BridgeHandle {
    player: Player,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    shutdown: Arc<Notify>,
    tracks_json: Arc<Mutex<String>>,
    duration_ms: Arc<AtomicU64>,
}

/// Wire the host's provider hooks into `player`, spawn the event pump and the
/// open_url → prepare → tracks → play() orchestrator, and return a handle. The
/// caller's `player` clone must keep its surface alive until the handle (and
/// every clone) is dropped.
pub fn start(
    player: Player,
    manifest_url: String,
    host: Arc<dyn BridgeHost>,
    config: StartConfig,
) -> BridgeHandle {
    player.set_request_interceptor(Arc::new(HostInterceptor(host.clone())));
    player.set_license_resolver(Arc::new(HostResolver(host.clone())));

    let shutdown = Arc::new(Notify::new());
    let tracks_json = Arc::new(Mutex::new(String::from("{}")));
    let duration_ms = Arc::new(AtomicU64::new(0));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();

    spawn_event_pump(player.events(), host.clone(), duration_ms.clone());

    tokio::spawn(orchestrate(
        player.clone(),
        manifest_url,
        host,
        tracks_json.clone(),
        duration_ms.clone(),
        cmd_rx,
        shutdown.clone(),
        config,
    ));

    BridgeHandle {
        player,
        cmd_tx,
        shutdown,
        tracks_json,
        duration_ms,
    }
}

impl BridgeHandle {
    /// Resume after a [`pause`](Self::pause). (The orchestrator issues the
    /// initial `play()`; this is the UI play/pause toggle's resume side.)
    pub fn play(&self) {
        self.player.resume();
    }
    pub fn pause(&self) {
        self.player.pause();
    }
    pub fn is_paused(&self) -> bool {
        self.player.is_paused()
    }
    pub fn seek_ms(&self, position_ms: i64) {
        self.player
            .seek(Duration::from_millis(position_ms.max(0) as u64));
    }
    /// Absolute volume, 0.0..=1.0.
    pub fn set_volume(&self, volume: f32) {
        self.player.set_volume(volume);
    }
    pub fn position_ms(&self) -> i64 {
        self.player.position().as_millis() as i64
    }
    /// Total duration in ms, tracked from `ManifestLoaded` / `Position` events
    /// (the player exposes no direct duration getter). 0 until known.
    pub fn duration_ms(&self) -> i64 {
        self.duration_ms.load(Ordering::Relaxed) as i64
    }
    /// Unified tracks snapshot JSON (see [`tracks_to_json`]). `"{}"` until
    /// `prepare()` completes.
    pub fn tracks_json(&self) -> String {
        self.tracks_json.lock().unwrap().clone()
    }

    /// Manual hard switch (resets ABR to Manual). Indices are into the
    /// `video` array of [`tracks_json`](Self::tracks_json): `adapt` =
    /// adaptation index, `repr` = representation index within it.
    pub fn set_video_track(&self, adapt: usize, repr: usize) {
        let _ = self.cmd_tx.send(Cmd::Video { adapt, repr, soft: false });
    }
    /// Soft switch via the ABR supervisor swap path (no pipeline restart).
    pub fn set_video_track_soft(&self, adapt: usize, repr: usize) {
        let _ = self.cmd_tx.send(Cmd::Video { adapt, repr, soft: true });
    }
    /// Re-arm bandwidth-EWMA ABR (auto quality).
    pub fn set_video_auto(&self) {
        let _ = self.cmd_tx.send(Cmd::VideoAuto);
    }
    pub fn set_audio_track(&self, adapt: usize, repr: usize) {
        let _ = self.cmd_tx.send(Cmd::Audio { adapt, repr });
    }
    pub fn set_subtitle_track(&self, adapt: usize, repr: usize) {
        let _ = self.cmd_tx.send(Cmd::Subtitle { adapt, repr });
    }
    pub fn clear_subtitles(&self) {
        let _ = self.cmd_tx.send(Cmd::ClearSubs);
    }

    /// Forward a surface size change to the renderer.
    pub fn resize(&self, width: u32, height: u32) {
        self.player
            .resize(player::PhysicalSize::new(width.max(1), height.max(1)));
    }

    /// The underlying player, for platform-specific wiring the unified surface
    /// doesn't cover (`set_video_output_window`, `set_display_hdr_types`, …).
    pub fn player(&self) -> &Player {
        &self.player
    }

    /// Signal the orchestrator to stop playback and tear the pipeline down.
    /// The shell drops the handle and releases its surfaces afterwards.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }
}

async fn orchestrate(
    mut player: Player,
    manifest_url: String,
    host: Arc<dyn BridgeHost>,
    tracks_json: Arc<Mutex<String>>,
    duration_ms: Arc<AtomicU64>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    shutdown: Arc<Notify>,
    config: StartConfig,
) {
    if let Err(e) = player.open_url(&manifest_url).await {
        host.on_event(error_json("other", &format!("open_url: {e}")));
        return;
    }
    if let Err(e) = player.prepare().await {
        host.on_event(error_json("other", &format!("prepare: {e}")));
        return;
    }
    let tracks = match player.get_tracks() {
        Ok(t) => t,
        Err(e) => {
            host.on_event(error_json("other", &format!("get_tracks: {e}")));
            return;
        }
    };
    duration_ms.store(tracks.duration.as_millis() as u64, Ordering::Relaxed);
    *tracks_json.lock().unwrap() = tracks_to_json(&tracks);
    host.on_event(obj("tracks_ready"));

    // Resume must be set before the first play(): an absolute position if the
    // host gave one, otherwise a fraction of the now-known real duration
    // (product apps store resume as a percent and can't compute the exact
    // Duration until prepare() reveals it).
    let resume = config.start_position.or_else(|| {
        config
            .start_fraction
            .map(|f| tracks.duration.mul_f32(f.clamp(0.0, 1.0)))
    });
    if resume.is_some() {
        player.set_start_position(resume);
    }

    // Default selection (video_pref / audio codec / subtitle font+style),
    // honouring the host's passthrough + auto-subtitle policy.
    crate::apply_default_tracks(
        &player,
        &tracks,
        config.audio_passthrough,
        config.auto_select_subtitle,
        config.preferred_audio_language.as_deref(),
        config.preferred_subtitle_language.as_deref(),
    );

    // Initial playback. play() resolves on EndOfStream / stop / exhausted
    // retries; the event pump reports those to the host. We don't auto-loop —
    // the host drives replay.
    let play_player = player.clone();
    let mut play_task = tokio::spawn(async move {
        // Consume the Result (its `Box<dyn Error>` is not Send) BEFORE the
        // await, so the spawned future stays Send.
        let handle = match play_player.play() {
            Ok(h) => h,
            Err(e) => {
                log::error!("play(): {e}");
                return;
            }
        };
        let _ = handle.await;
    });

    loop {
        tokio::select! {
            _ = shutdown.notified() => break,
            cmd = cmd_rx.recv() => match cmd {
                None => break,
                Some(c) => apply_cmd(&player, &tracks, c),
            },
        }
    }

    player.stop().await;
    play_task.abort();
    let _ = (&mut play_task).await;
}

fn apply_cmd(player: &Player, tracks: &Tracks, cmd: Cmd) {
    match cmd {
        Cmd::Video { adapt, repr, soft } => {
            if let Some(r) = tracks
                .video
                .get(adapt)
                .and_then(|a| a.representations.get(repr))
            {
                if soft {
                    player.change_video_track_soft(r);
                } else {
                    player.change_video_track(r);
                }
            } else {
                log::warn!("set_video_track: no rep at adapt={adapt} repr={repr}");
            }
        }
        Cmd::VideoAuto => {
            player.set_abr_strategy(AbrStrategy::BandwidthEwma { safety_factor: 1.25 });
        }
        Cmd::Audio { adapt, repr } => {
            if let Some(a) = tracks.audio.get(adapt) {
                if let Some(r) = a.representations.get(repr) {
                    player.change_audio_track(a, r);
                    return;
                }
            }
            log::warn!("set_audio_track: no rep at adapt={adapt} repr={repr}");
        }
        Cmd::Subtitle { adapt, repr } => {
            if let Some(r) = tracks
                .text
                .get(adapt)
                .and_then(|a| a.representations.get(repr))
            {
                player.set_subtitle_track(r);
            } else {
                log::warn!("set_subtitle_track: no rep at adapt={adapt} repr={repr}");
            }
        }
        Cmd::ClearSubs => player.clear_subtitle_track(),
    }
}

fn spawn_event_pump(
    mut rx: broadcast::Receiver<PlayerEvent>,
    host: Arc<dyn BridgeHost>,
    duration_ms: Arc<AtomicU64>,
) {
    tokio::spawn(async move {
        let mut last_size = (0u32, 0u32);
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    match &ev {
                        PlayerEvent::ManifestLoaded { duration, .. }
                        | PlayerEvent::Position { duration, .. } => {
                            duration_ms.store(duration.as_millis() as u64, Ordering::Relaxed);
                        }
                        // Synthesize a dedicated video-size event the first time
                        // (and whenever) the rendered resolution changes, so a
                        // consumer can shape its video plane without parsing the
                        // periodic `stats` event. (current_resolution is the only
                        // place the player reports post-ABR size.)
                        PlayerEvent::Stats {
                            current_resolution: Some((w, h)),
                            ..
                        } if (*w, *h) != last_size && *w > 0 && *h > 0 => {
                            last_size = (*w, *h);
                            host.on_event(format!(
                                r#"{{"type":"video_size","width":{},"height":{}}}"#,
                                w, h
                            ));
                        }
                        _ => {}
                    }
                    host.on_event(event_to_json(&ev));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

// --- unified JSON contract (single source of truth for both platforms) ------

/// Serialize one [`PlayerEvent`] to the unified event JSON. Schema:
/// `{"type": "...", <fields>}` where `type` is one of `idle`,
/// `manifest_loaded`, `prepared`, `buffering`, `playing`, `paused`,
/// `position`, `track_changed`, `glitch_recovered`, `stats`, `end_of_stream`,
/// `error`. (The pump additionally synthesizes a `video_size` event —
/// `{"type":"video_size","width","height"}` — when the rendered resolution
/// first appears / changes; it is not produced here.)
pub fn event_to_json(ev: &PlayerEvent) -> String {
    match ev {
        PlayerEvent::Idle => obj("idle"),
        PlayerEvent::ManifestLoaded {
            duration,
            video_tracks,
            audio_tracks,
            subtitle_tracks,
        } => format!(
            r#"{{"type":"manifest_loaded","duration_ms":{},"video_tracks":{},"audio_tracks":{},"subtitle_tracks":{}}}"#,
            duration.as_millis(),
            video_tracks,
            audio_tracks,
            subtitle_tracks
        ),
        PlayerEvent::Prepared => obj("prepared"),
        PlayerEvent::Buffering { reason } => format!(
            r#"{{"type":"buffering","reason":{}}}"#,
            jstr(buffering_reason(reason))
        ),
        PlayerEvent::Playing => obj("playing"),
        PlayerEvent::Paused => obj("paused"),
        PlayerEvent::Position {
            position,
            duration,
            buffered_ahead_secs,
            bandwidth_bps,
        } => format!(
            r#"{{"type":"position","position_ms":{},"duration_ms":{},"buffered_ahead_secs":{:.3},"bandwidth_bps":{}}}"#,
            position.as_millis(),
            duration.as_millis(),
            buffered_ahead_secs,
            bandwidth_bps
        ),
        PlayerEvent::TrackChanged { kind, info } => format!(
            r#"{{"type":"track_changed","kind":{},"representation_id":{},"label":{}}}"#,
            jstr(track_kind(kind)),
            info.representation_id,
            jstr(&info.label)
        ),
        PlayerEvent::GlitchRecovered { detail } => {
            format!(r#"{{"type":"glitch_recovered","detail":{}}}"#, jstr(detail))
        }
        PlayerEvent::Stats {
            video_frames_decoded,
            video_frames_dropped,
            audio_underruns,
            net_stall_ms,
            decoder_name,
            current_resolution,
            av_drift_ms,
            video_buffer_ahead_ms,
            audio_buffer_ahead_ms,
            video_segment,
            stall_events,
            pipeline_retries,
            render_gap_max_ms,
            judder_frames,
            interval_hist,
            bandwidth_bps,
            ..
        } => {
            let (w, h) = current_resolution.unwrap_or((0, 0));
            format!(
                r#"{{"type":"stats","frames_decoded":{},"frames_dropped":{},"audio_underruns":{},"net_stall_ms":{},"decoder":{},"width":{},"height":{},"av_drift_ms":{},"video_buffer_ahead_ms":{},"audio_buffer_ahead_ms":{},"video_segment":{},"stall_events":{},"pipeline_retries":{},"render_gap_max_ms":{},"judder_frames":{},"int_lt25":{},"int_25_41":{},"int_42_58":{},"int_gt58":{},"bandwidth_bps":{}}}"#,
                video_frames_decoded,
                video_frames_dropped,
                audio_underruns,
                net_stall_ms,
                jstr(decoder_name),
                w,
                h,
                av_drift_ms.unwrap_or(0),
                video_buffer_ahead_ms,
                audio_buffer_ahead_ms,
                video_segment,
                stall_events,
                pipeline_retries,
                render_gap_max_ms,
                judder_frames,
                interval_hist[0],
                interval_hist[1],
                interval_hist[2],
                interval_hist[3],
                bandwidth_bps
            )
        }
        PlayerEvent::EndOfStream => obj("end_of_stream"),
        PlayerEvent::Error { kind, detail } => format!(
            r#"{{"type":"error","kind":{},"detail":{}}}"#,
            jstr(&error_kind(kind)),
            jstr(detail)
        ),
    }
}

/// Serialize the track list to the unified tracks JSON. Each entry carries
/// `adapt`/`repr` indices (the args [`BridgeHandle::set_video_track`] et al.
/// expect) plus display metadata.
pub fn tracks_to_json(t: &Tracks) -> String {
    let video: Vec<String> = t
        .video
        .iter()
        .enumerate()
        .flat_map(|(ai, a)| {
            a.representations
                .iter()
                .enumerate()
                .map(move |(ri, r)| {
                    format!(
                        r#"{{"adapt":{},"repr":{},"id":{},"width":{},"height":{},"codecs":{},"bandwidth":{},"hdr10":{},"dolbyVision":{},"label":{}}}"#,
                        ai, ri, r.id, r.width, r.height, jstr(&r.codecs), r.bandwidth,
                        r.hdr10, r.dolby_vision, jstr(&r.label())
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let audio: Vec<String> = t
        .audio
        .iter()
        .enumerate()
        .flat_map(|(ai, a)| {
            let lang = a.language().unwrap_or("").to_string();
            a.representations
                .iter()
                .enumerate()
                .map(move |(ri, r)| {
                    format!(
                        r#"{{"adapt":{},"repr":{},"id":{},"lang":{},"codecs":{},"bandwidth":{},"channels":{},"label":{}}}"#,
                        ai, ri, r.id, jstr(&lang), jstr(&r.codecs), r.bandwidth,
                        r.channels.unwrap_or(0), jstr(&r.label())
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let text: Vec<String> = t
        .text
        .iter()
        .enumerate()
        .flat_map(|(ai, a)| {
            let lang = a.language().unwrap_or("").to_string();
            let forced = a.is_forced();
            a.representations
                .iter()
                .enumerate()
                .map(move |(ri, r)| {
                    format!(
                        r#"{{"adapt":{},"repr":{},"id":{},"lang":{},"forced":{},"codecs":{},"bandwidth":{},"label":{}}}"#,
                        ai, ri, r.id, jstr(&lang), forced, jstr(&r.codecs), r.bandwidth, jstr(&r.label())
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    format!(
        r#"{{"durationMs":{},"video":[{}],"audio":[{}],"text":[{}]}}"#,
        t.duration.as_millis(),
        video.join(","),
        audio.join(","),
        text.join(",")
    )
}

fn obj(ty: &str) -> String {
    format!(r#"{{"type":{}}}"#, jstr(ty))
}

fn error_json(kind: &str, detail: &str) -> String {
    format!(
        r#"{{"type":"error","kind":{},"detail":{}}}"#,
        jstr(kind),
        jstr(detail)
    )
}

fn buffering_reason(r: &player::BufferingReason) -> &'static str {
    match r {
        player::BufferingReason::Initial => "initial",
        player::BufferingReason::Stall => "stall",
        player::BufferingReason::Seek => "seek",
        player::BufferingReason::TrackSwitch => "track_switch",
    }
}

fn track_kind(k: &player::TrackKind) -> &'static str {
    match k {
        player::TrackKind::Video => "video",
        player::TrackKind::Audio => "audio",
        player::TrackKind::Subtitle => "subtitle",
    }
}

fn error_kind(k: &player::PlayerErrorKind) -> String {
    match k {
        player::PlayerErrorKind::Network => "network".to_string(),
        player::PlayerErrorKind::Http { status } => format!("http_{status}"),
        player::PlayerErrorKind::Interceptor => "interceptor".to_string(),
        player::PlayerErrorKind::LicenseResolver => "license_resolver".to_string(),
        player::PlayerErrorKind::ManifestParse => "manifest_parse".to_string(),
        player::PlayerErrorKind::Decoder => "decoder".to_string(),
        player::PlayerErrorKind::Other => "other".to_string(),
    }
}

/// Minimal JSON string escaper — wraps `s` in quotes and escapes the control
/// set. Avoids a `serde` dependency for the handful of strings we emit.
fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
