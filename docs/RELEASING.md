# Releasing

One version line covers Android, iOS and web: a release of any platform is
`max(all android-v*/ios-v*/web-v* tags on the remote) + 1`, and the three are
tagged together at the same commit. Tags trigger the self-hosted publish
workflows; nothing else needs bumping in the tree.

## The script

`scripts/release.sh` is bash and runs the same way in Git Bash on Windows,
on macOS and on Linux (needs `git`, `gh` logged in, `python3` for the gate);
`scripts/release.ps1` is only a wrapper that calls it.

```bash
scripts/release.sh --wait-and-bump-ios          # next version, all platforms
scripts/release.sh --version 0.2.0              # explicit version
scripts/release.sh --platforms android,web      # subset
scripts/release.sh --dry-run                    # every check, no tag
```

It refuses a dirty tree or an unpushed HEAD, reads the version from the
**remote** tags (never from memory — two sessions releasing in parallel once
produced web-v0.1.34 on one commit and 0.1.35 on another), requires a green
conformance run covering HEAD, tags, pushes, and lists the runs. With
`--wait-and-bump-ios` it also waits for the iOS xcframework, reads its checksum
from the GitHub release and commits the `Package.swift` pin — the step that
was forgotten for ios-v0.1.9, 0.1.12, 0.1.25 and 0.1.29.

## Conformance gate

Every publish workflow starts with `scripts/conformance/require-green.sh`.
A tag publishes only when conformance covers its commit:

- a completed, successful `conformance` run on master for that exact
  commit; or
- for a commit that did not touch `player/**` (and so never triggered a
  run), the newest green run on an ancestor with no `player/**` change in
  between.

A run still in progress, a failed run, or player changes since the last
green run refuse the publish — re-run the job once conformance is green, or
dispatch the workflow by hand with `skip_conformance: true` when you have
decided the risk is yours (`--skip-conformance-check` on the script only
skips the local pre-check; the workflow gate still applies).

## What conformance checks

`player/examples/conformance.rs` on the `conformance-asset-v1` release
(local range server, `--switches 3 --seeks 2`): stalls, pipeline retries,
render gaps/bursts, judder, A/V drift, late frames, lip-sync per mark pair
(median and worst), throughput — and, since the desktop seek regression of
2026-09-26, the mute-pipeline family none of those could see:

| check | fails when |
|---|---|
| `clock-fallbacks` | the master clock ever handed over to the wall clock (audio position stood still while playing) |
| `audio-rebuilds` | the audio-output watchdog rebuilt a pipeline |
| `audio-after-rebuild` | a seek/switch rebuild never advanced the audio position within 6 s |
| `lipsync-per-rebuild` | a rebuild window long enough for a mark pair produced none |

Run it locally the same way CI does (Python 3, `gh`):

```powershell
gh release download conformance-asset-v1 -D asset
py -3 scripts/conformance/rangesrv.py 8123 asset
cargo run --release --example conformance -p player -- `
  http://127.0.0.1:8123/manifest.mpd `
  --key 00112233445566778899aabbccddeeff:0123456789abcdef0123456789abcdef `
  --secs 90 --switches 3 --seeks 2
```

For an interactive desktop check on the same asset, `example-desktop` takes
`RUST_PLAYER_URL` / `RUST_PLAYER_CLEARKEY` and `seek <ms>` / `seek ±<ms>` on
stdin.
