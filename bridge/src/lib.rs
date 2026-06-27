// Shared playback fixture used by the desktop, Android, and (eventually)
// iOS shells. Each shell only owns the window + event-loop wiring; the
// boring `open_url → set_clearkey → prepare → pick tracks → play()` dance
// lives here so the two implementations stay in lock-step.

use std::collections::HashMap;

use player::Player;

// The bridge core. Re-exported at the crate root so consumers write
// `bridge::BridgeHost` / `bridge::start` (not `bridge::bridge::…`).
pub mod bridge;
pub use self::bridge::*;

/// Encrypted DASH stream used by every shell for smoke-testing the
/// pipeline end-to-end (manifest fetch, CENC decryption, A/V sync).
pub const TEST_MANIFEST_URL: &str = "https://preclikos.cz/examples/encrypted/manifest.mpd";

/// ClearKey KID → key pairs the test stream is encrypted with. Hardcoded
/// here so the shells don't ship two near-identical literal blocks.
pub fn test_clearkeys() -> HashMap<String, String> {
    let mut keys = HashMap::new();
    keys.insert(
        "0fd37dac41c0e987e68d43b801b1210c".to_string(),
        "fd8d9f408c2bd702970afcd3b219e791".to_string(),
    );
    keys.insert(
        "519af81ab2d284f52aa8257d96b5e4bd".to_string(),
        "627ef72b42d98770dec20ecab46cd1f4".to_string(),
    );
    keys
}

/// Test-shell video representation override. Values: `hdr` (first HDR10 /
/// highest 10-bit rep), `dv` (first Dolby Vision rep), a numeric rep
/// index, or unset for the historical default (index 5 = 720p SDR).
///
/// Desktop: `RUST_PLAYER_VIDEO` env var. Android: the app's external
/// files dir is the only place both `adb push` (no root) and the app can
/// touch, so a one-line `video_pref.txt` there acts as the env var:
///   adb shell "mkdir -p /sdcard/Android/data/cz.preclikos.rust_player/files"
///   adb shell "echo hdr > /sdcard/Android/data/cz.preclikos.rust_player/files/video_pref.txt"
pub(crate) fn video_pref() -> Option<String> {
    if let Ok(v) = std::env::var("RUST_PLAYER_VIDEO") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    #[cfg(target_os = "android")]
    for path in [
        "/storage/emulated/0/Android/data/cz.preclikos.rust_player/files/video_pref.txt",
        "/sdcard/Android/data/cz.preclikos.rust_player/files/video_pref.txt",
    ] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                log::info!("video_pref: {} (from {})", s, path);
                return Some(s);
            }
        }
    }
    None
}

/// Subtitle-styling preference, same dual mechanism as [`video_pref`]:
/// the `env` var on desktop, or a one-line `<file>` in the app's external
/// files dir on Android (`adb push`-able without root). Returns the
/// trimmed value or `None` when unset. `file` is only read on Android.
pub(crate) fn sub_pref(env: &str, file: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    #[cfg(target_os = "android")]
    for dir in [
        "/storage/emulated/0/Android/data/cz.preclikos.rust_player/files",
        "/sdcard/Android/data/cz.preclikos.rust_player/files",
    ] {
        let path = format!("{}/{}", dir, file);
        if let Ok(s) = std::fs::read_to_string(&path) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                log::info!("sub_pref {}: {} (from {})", env, s, path);
                return Some(s);
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = file;
    None
}

/// Build a [`SubtitleStyle`] from the `RUST_PLAYER_SUB_COLOR` /
/// `RUST_PLAYER_SUB_SIZE` preferences (env or Android `*.txt`). Colour
/// accepts names (`yellow`, `white`, …) or hex (`#FFCC00`); size accepts a
/// numeric scale (`1.2`) or a name (`small`/`medium`/`large`/`xlarge`).
/// Unset or unparseable values fall back to the default.
pub(crate) fn subtitle_style() -> player::SubtitleStyle {
    let mut style = player::SubtitleStyle::DEFAULT;
    if let Some(c) = sub_pref("RUST_PLAYER_SUB_COLOR", "sub_color.txt") {
        match player::SubtitleStyle::parse_color(&c) {
            Some(rgba) => style.text_color = rgba,
            None => log::warn!("RUST_PLAYER_SUB_COLOR: unrecognised colour {:?}", c),
        }
    }
    if let Some(sz) = sub_pref("RUST_PLAYER_SUB_SIZE", "sub_size.txt") {
        let scale = match sz.to_ascii_lowercase().as_str() {
            "small" => Some(0.8),
            "medium" | "normal" => Some(1.0),
            "large" => Some(1.3),
            "xlarge" | "huge" => Some(1.6),
            _ => sz.parse::<f32>().ok(),
        };
        match scale {
            Some(s) => style.size_scale = s,
            None => log::warn!("RUST_PLAYER_SUB_SIZE: not a number or size name: {:?}", sz),
        }
    }
    style
}

/// Opens the encrypted test stream, installs ClearKey keys, prepares the
/// pipeline, and picks a default video + audio track. Returns once the
/// first `play()` handle has been spawned, with the play-supervisor task
/// running in the background.
///
/// The Android MediaCodec audio backend currently understands only
/// `mp4a` (AAC); other codecs in the manifest's audio adaptations would
/// fail to configure. The desktop FFmpeg backend handles everything, so
/// the helper prefers an `mp4a` representation when one is available and
/// falls back to the last/first one otherwise — matching what each shell
/// did individually before the unification.
pub async fn run_test_playback(mut player: Player) {
    if let Err(e) = player.open_url(TEST_MANIFEST_URL).await {
        log::error!("open_url: {}", e);
        return;
    }

    if let Err(e) = player.set_clearkey(test_clearkeys()) {
        log::error!("set_clearkey: {}", e);
        return;
    }

    if let Err(e) = player.prepare().await {
        log::error!("prepare: {}", e);
        return;
    }

    let tracks = match player.get_tracks() {
        Ok(t) => t,
        Err(e) => {
            log::error!("get_tracks: {}", e);
            return;
        }
    };

    // Shell behaviour: file-flag passthrough, auto-select the first subtitle.
    apply_default_tracks(&player, &tracks, None, true);

    // Re-spawn the play() task on natural exit so the stream loops
    // continuously — useful for soak testing the pipeline.
    let player_for_loop = player.clone();
    tokio::spawn(async move {
        loop {
            let handle = match player_for_loop.play() {
                Ok(h) => h,
                Err(e) => {
                    log::error!("play(): {}", e);
                    break;
                }
            };
            let _ = handle.await;
        }
    });
}

/// Pick a sensible default video + audio (+ subtitle) track on `player`,
/// honouring the same `video_pref` / audio / subtitle / passthrough
/// preferences across every shell. Shared by [`run_test_playback`] and the
/// [`bridge`] core so the two paths can't drift apart.
///
/// The Android MediaCodec audio backend currently understands only
/// `mp4a` (AAC); other codecs in the manifest's audio adaptations would
/// fail to configure. The desktop FFmpeg backend handles everything, so
/// the helper prefers an `mp4a` representation when one is available and
/// falls back to the last/first one otherwise.
/// `passthrough_override`: `Some(b)` forces audio passthrough on/off (a product
/// host has already made the sink-gated decision); `None` keeps the shell's
/// env / `audio_passthrough.txt` file-flag behaviour. `auto_select_subtitle`:
/// when `false`, no subtitle track is auto-selected (the host applies its own
/// language/forced policy after play).
pub(crate) fn apply_default_tracks(
    player: &Player,
    tracks: &player::Tracks,
    passthrough_override: Option<bool>,
    auto_select_subtitle: bool,
) {
    // Video: representation picked by `video_pref()` (default: first
    // adaptation, index 5 = 720p HEVC in the preclikos.cz fixture,
    // matching what both shells used to hardcode). The hdr/dv preferences
    // scan EVERY video adaptation set — DV reps commonly live in their
    // own set, not the first one.
    if tracks.video.is_empty() {
        log::error!("no video adaptations in manifest");
        return;
    }
    for (ai, a) in tracks.video.iter().enumerate() {
        for (i, r) in a.representations.iter().enumerate() {
            log::info!(
                "video adapt[{}] rep[{}]: {}x{} {} {}bps hdr10={} dv={}",
                ai, i, r.width, r.height, r.codecs, r.bandwidth, r.hdr10, r.dolby_vision
            );
        }
    }
    let pref = video_pref();
    let all = || {
        tracks
            .video
            .iter()
            .flat_map(|a| a.representations.iter().map(move |r| (a, r)))
    };
    let first_adapt = tracks.video.first().unwrap();
    let picked = match pref.as_deref() {
        // First rep flagged HDR10; the MPD often mis-signals colorimetry,
        // so fall back to the highest-resolution 10-bit rep (the SPS VUI
        // decides the actual render path either way).
        Some("hdr") => all()
            .find(|(_, r)| r.hdr10 && !r.dolby_vision)
            .or_else(|| {
                all()
                    .filter(|(_, r)| r.is_10bit() && !r.dolby_vision)
                    .max_by_key(|(_, r)| r.height)
            })
            .or_else(|| all().max_by_key(|(_, r)| r.height)),
        Some("dv") => all()
            .filter(|(_, r)| r.dolby_vision)
            .max_by_key(|(_, r)| r.height),
        Some(s) => s
            .parse::<usize>()
            .ok()
            .and_then(|i| first_adapt.representations.get(i).map(|r| (first_adapt, r))),
        // Product-safe default: highest rung at or below 1080p (ABR adapts up
        // from there), falling back to the first rep. The old fixed index 5
        // was the 720p rung of the preclikos test fixture and is `None` on
        // product manifests with fewer reps — which failed play() outright.
        None => all()
            .filter(|(_, r)| r.height <= 1080)
            .max_by_key(|(_, r)| r.height)
            .or_else(|| first_adapt.representations.first().map(|r| (first_adapt, r))),
    };
    let (video_adapt, video_repr) = match picked {
        Some(p) => p,
        None => {
            log::error!(
                "no video representation matches preference {:?} (and default index 5 is missing)",
                pref
            );
            return;
        }
    };
    player.set_video_track(video_adapt, video_repr);
    log::info!(
        "selected video {}x{} {} (pref={:?}, hdr10={}, dv={})",
        video_repr.width, video_repr.height, video_repr.codecs, pref,
        video_repr.hdr10, video_repr.dolby_vision
    );

    // Audio passthrough opt-in (Android, bitstream to HDMI/AVR). Same dual
    // mechanism as direct.txt / video_pref.txt: an env var on desktop, or a
    // one-line `audio_passthrough.txt` (== "1") in the app's external files
    // dir on Android. When on, we force an `ec-3` track and tell the player
    // to feed the raw bitstream to an AudioTrack instead of decoding to PCM:
    //   adb shell "echo 1 > /sdcard/Android/data/cz.preclikos.rust_player/files/audio_passthrough.txt"
    let passthrough = passthrough_override.unwrap_or_else(|| {
        sub_pref("RUST_PLAYER_AUDIO_PASSTHROUGH", "audio_passthrough.txt").as_deref() == Some("1")
    });

    // Audio: prefer an AAC (mp4a*) representation since the Android
    // MediaCodec backend can't configure EC-3 / AC-3 without an esds box.
    // Desktop accepts either, so this is a safe lowest-common-denominator.
    // Override via `RUST_PLAYER_AUDIO=ec-3` (or `ac-3`, `mp4a`) for runtime
    // testing of the AudioToolbox / FFmpeg / MediaCodec AC-3 paths; on Android
    // the same value can come from an `audio_pref.txt` file. Passthrough forces
    // `ec-3` (the bitstream sink only handles E-AC-3 / AC-3).
    let codec_prefix: String = sub_pref("RUST_PLAYER_AUDIO", "audio_pref.txt")
        .unwrap_or_else(|| if passthrough { "ec-3".to_string() } else { "mp4a".to_string() });
    log::info!(
        "audio preference: codec prefix = {} (passthrough={})",
        codec_prefix, passthrough
    );
    let (audio_adapt, audio_repr) = match tracks
        .audio
        .iter()
        .find_map(|a| {
            a.representations
                .iter()
                .find(|r| r.codecs.starts_with(codec_prefix.as_str()))
                .map(|r| (a, r))
        })
        .or_else(|| {
            tracks
                .audio
                .last()
                .and_then(|a| a.representations.last().map(|r| (a, r)))
        }) {
        Some(pair) => pair,
        None => {
            log::error!("no audio adaptations in manifest");
            return;
        }
    };
    player.set_audio_track(audio_adapt, audio_repr);
    log::info!(
        "selected audio {} {}Hz",
        audio_repr.codecs, audio_repr.bandwidth
    );

    // Engage bitstream passthrough once a codec the sink understands is
    // selected. The player still self-gates: it only takes the bitstream
    // path when the selected codec is ec-3/ac-3 AND the AudioTrack sink
    // builds, falling back to PCM otherwise.
    if passthrough {
        if matches!(audio_repr.codecs.as_str(), "ec-3" | "ac-3") {
            player.set_audio_passthrough(true);
            log::info!("audio passthrough enabled (opt-in, codec={})", audio_repr.codecs);
        } else {
            log::warn!(
                "audio passthrough requested but selected codec is {} (no ec-3/ac-3 rep) — staying PCM",
                audio_repr.codecs
            );
        }
    }

    // Product hosts pick their own subtitle (language + forced policy) after
    // play, so they disable auto-selection.
    if !auto_select_subtitle {
        return;
    }

    // Subtitles: pick the first text track when the manifest has one and a
    // font is available. Android always has Roboto; desktop honours
    // RUST_PLAYER_FONT. Opt out by deleting the track from the manifest —
    // this is a smoke-test shell, visibility beats configurability.
    if let Some(text_repr) = tracks
        .text
        .first()
        .and_then(|a| a.representations.first())
    {
        // The overlay ships with an embedded DejaVu Sans default (wide
        // glyph coverage — music notes, dashes, full diacritics), so a cue
        // renders without any host font. `RUST_PLAYER_FONT` (env, or a
        // `subtitle_font.txt` path on Android) only *overrides* that.
        if let Some(path) = sub_pref("RUST_PLAYER_FONT", "subtitle_font.txt") {
            match std::fs::read(&path) {
                Ok(bytes) => match player.set_subtitle_font(bytes) {
                    Ok(()) => log::info!("subtitle font override: {}", path),
                    Err(e) => log::warn!("subtitle font rejected ({}): {}", path, e),
                },
                Err(e) => log::warn!("subtitle font unreadable ({}): {}", path, e),
            }
        }

        player.set_subtitle_style(subtitle_style());
        player.set_subtitle_track(text_repr);
        log::info!("selected subtitle track {}", text_repr.id);
    }
}
