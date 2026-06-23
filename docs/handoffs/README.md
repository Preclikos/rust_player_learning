# Handoffs

Working notes / handoffs between sessions and the BlackZone integration. Source
of truth is always the code + git history; these capture the *why* and the
device-verified state at a point in time.

| Doc | Téma | Stav |
|---|---|---|
| [AUDIO_PASSTHROUGH_HANDOFF.md](AUDIO_PASSTHROUGH_HANDOFF.md) | E-AC-3 HDMI passthrough (sink, feed, MediaClock) | ✅ shipped + start-deadlock fixed |
| [CRASH_b030db8_AFR_SURFACE_UAF_AND_PT_LEAK.md](CRASH_b030db8_AFR_SURFACE_UAF_AND_PT_LEAK.md) | AFR setFrameRate UAF + passthrough-task runaway | ✅ fixed (`5afa722`, `ef8e42e`); duplicate-spawn + host surface contract still open |
| [ABR_REBUILD_ORPHANED_DOWNLOADER.md](ABR_REBUILD_ORPHANED_DOWNLOADER.md) | ABR rebuild orphans downloader → SendError wedge | ✅ primary fix (`99854b4`); confirm new pipeline doesn't starve |
| [RESUME_SEEK_STILL_BROKEN.md](RESUME_SEEK_STILL_BROKEN.md) | Resume + position-after-seek | ✅ resolved |
| [SUBTITLE_STYLE_HOST_API.md](SUBTITLE_STYLE_HOST_API.md) | Re-export `SubtitleStyle` for the host | ✅ done (host-side wiring remains) |
| [RESUME_START_POSITION.md](RESUME_START_POSITION.md) | API request: deterministic start position | 📦 historical (shipped) |
| [SUBTITLE_OVERSCAN_FIX.md](SUBTITLE_OVERSCAN_FIX.md) | Subtitle bottom safe-area / TV overscan | 📦 historical (wired) |
| [OUTCOME.md](OUTCOME.md) | Integration summary for BlackZone TV (2026-06-14) | 📦 historical |
