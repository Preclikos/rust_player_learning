# Handoffs

Working notes / handoffs between sessions and the BlackZone integration. Source
of truth is always the code + git history; these capture the *why* and the
device-verified state at a point in time.

| Doc | Téma | Stav |
|---|---|---|
| [PREBUILT_DISTRIBUTION_AAR_XCFRAMEWORK.md](PREBUILT_DISTRIBUTION_AAR_XCFRAMEWORK.md) | Ship player as prebuilt AAR (GitHub Packages) + XCFramework (SwiftPM) — consumers compile no Rust | ✅ Android AAR verified; ⚠️ iOS + CI written, Mac/CI-verify pending |
| [UNIFIED_BRIDGE_FOR_PRODUCT_APPS.md](UNIFIED_BRIDGE_FOR_PRODUCT_APPS.md) | `app_shared::bridge` as the product consumer surface (StartConfig, `forced`, `video_size`) | ✅ 3 core gaps resolved; BlackZone app-side migration pending |
| [AUDIO_PASSTHROUGH_HANDOFF.md](AUDIO_PASSTHROUGH_HANDOFF.md) | E-AC-3 HDMI passthrough (sink, feed, MediaClock) | ✅ shipped + start-deadlock fixed |
| [CRASH_b030db8_AFR_SURFACE_UAF_AND_PT_LEAK.md](CRASH_b030db8_AFR_SURFACE_UAF_AND_PT_LEAK.md) | AFR setFrameRate UAF + passthrough-task runaway | ✅ fixed (`5afa722`, `ef8e42e`); duplicate-spawn + host surface contract still open |
| [ABR_REBUILD_ORPHANED_DOWNLOADER.md](ABR_REBUILD_ORPHANED_DOWNLOADER.md) | ABR rebuild orphans downloader → SendError wedge | ✅ downloader fixed (`99854b4`); orphaned decode+vsync teardown waits on `player.stop()` (see below) |
| [PLAYER_STOP_TEARDOWN.md](PLAYER_STOP_TEARDOWN.md) | No public `stop()` → playback/audio survives host teardown; pipeline tasks linger | ✅ `Player::stop()` fixed (`b798f59`) + bridge rewired (pending pin bump + device verify) |
| [RESUME_SEEK_STILL_BROKEN.md](RESUME_SEEK_STILL_BROKEN.md) | Resume + position-after-seek | ✅ resolved |
| [SUBTITLE_STYLE_HOST_API.md](SUBTITLE_STYLE_HOST_API.md) | Re-export `SubtitleStyle` for the host | ✅ done (host-side wiring remains) |
| [RESUME_START_POSITION.md](RESUME_START_POSITION.md) | API request: deterministic start position | 📦 historical (shipped) |
| [SUBTITLE_OVERSCAN_FIX.md](SUBTITLE_OVERSCAN_FIX.md) | Subtitle bottom safe-area / TV overscan | 📦 historical (wired) |
| [OUTCOME.md](OUTCOME.md) | Integration summary for BlackZone TV (2026-06-14) | 📦 historical |
