// Shared playback fixture used by the desktop, Android, and (eventually)
// iOS shells. Each shell only owns the window + event-loop wiring; the
// boring `open_url → set_clearkey → prepare → pick tracks → play()` dance
// lives here so the two implementations stay in lock-step.

use std::collections::HashMap;

use player::Player;

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
fn video_pref() -> Option<String> {
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

    // Video: first adaptation; representation picked by `video_pref()`
    // (default: index 5 = 720p HEVC in the preclikos.cz fixture, matching
    // what both shells used to hardcode).
    let video_adapt = match tracks.video.first() {
        Some(a) => a,
        None => {
            log::error!("no video adaptations in manifest");
            return;
        }
    };
    for (i, r) in video_adapt.representations.iter().enumerate() {
        log::info!(
            "video rep[{}]: {}x{} {} {}bps hdr10={} dv={}",
            i, r.width, r.height, r.codecs, r.bandwidth, r.hdr10, r.dolby_vision
        );
    }
    let pref = video_pref();
    let reps = &video_adapt.representations;
    let video_repr = match pref.as_deref() {
        // First rep flagged HDR10; the MPD often mis-signals colorimetry,
        // so fall back to the highest-resolution 10-bit rep (the SPS VUI
        // decides the actual render path either way).
        Some("hdr") => reps
            .iter()
            .find(|r| r.hdr10 && !r.dolby_vision)
            .or_else(|| {
                reps.iter()
                    .filter(|r| r.is_10bit() && !r.dolby_vision)
                    .max_by_key(|r| r.height)
            })
            .or_else(|| reps.iter().max_by_key(|r| r.height)),
        Some("dv") => reps.iter().find(|r| r.dolby_vision),
        Some(s) => s.parse::<usize>().ok().and_then(|i| reps.get(i)),
        None => reps.get(5),
    };
    let video_repr = match video_repr {
        Some(r) => r,
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

    // Audio: prefer an AAC (mp4a*) representation since the Android
    // MediaCodec backend can't configure EC-3 / AC-3 without an esds box.
    // Desktop accepts either, so this is a safe lowest-common-denominator.
    // Override via `RUST_PLAYER_AUDIO=ec-3` (or `ac-3`, `mp4a`) for runtime
    // testing of the AudioToolbox / FFmpeg / MediaCodec AC-3 paths.
    let codec_prefix: String =
        std::env::var("RUST_PLAYER_AUDIO").unwrap_or_else(|_| "mp4a".to_string());
    log::info!("audio preference: codec prefix = {}", codec_prefix);
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
