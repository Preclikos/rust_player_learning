"""Switch-quality analyser for the Android trace (see MainActivity.traceEvent).

Reads the `rustplayer_trace` JSONL a scripted run leaves in logcat and decides,
per switch, whether the player honoured its two contracts:

  SOFT / auto (ABR) switch  - the viewer must see NOTHING. No buffering event,
      no stall, no dropped frames, no skipped presentation slot, and the A/V
      clock must not lurch. A resolution change is the only thing allowed to
      show, and it must actually happen (otherwise the switch was a no-op and
      the run proved nothing).

  MANUAL switch (quality / audio / subtitles) - loading is EXPECTED: exactly
      one `buffering`, promptly, then `playing` inside a budget, and from there
      a settle window with no further hitching and no backwards jump.

Usage:
    adb logcat -c
    adb shell am start -n cz.preclikos.rust_player/cz.preclikos.rustplayer.MainActivity \
        --es scenario abr_soft --ei iterations 5
    adb logcat -d -s rustplayer_trace > run.log
    python scripts/switchcheck.py run.log

    --device-slow   doubles every latency budget (emulators pace badly)
    --json          machine-readable summary on stdout

Exit code 0 = every switch held its contract, 1 = at least one FAIL.

NOTE on what this can and cannot see: the engine's own judder counters
(`judder_frames`, `int_gt58`) are gated on `delta_pts < 100` in
`video_sync_loop`, so a hole ACROSS a splice - exactly what a bad switch
produces - is never counted. A clean verdict here therefore means "the engine
reported nothing wrong", not "the viewer saw nothing". The pixel check
(block D: barcode + screenrecord) is what closes that gap.
"""

import argparse
import json
import sys

# ---- thresholds ----------------------------------------------------------
# Windows are in ms around the MARK that caused the switch.
SOFT_WINDOW_MS = 12_000        # a soft swap waits for a segment boundary
MANUAL_BUFFER_DEADLINE_MS = 250    # loading must appear promptly, or the UI lies
MANUAL_PLAYING_BUDGET_MS = 1_500   # ...and clear inside this
SETTLE_SKIP_MS = 500           # ignore the first moments after `playing`
SETTLE_MS = 5_000
MAX_DRIFT_MS = 40
MAX_POSITION_JUMP_MS = 800
MAX_JUDDER_PCT = 2.0
MIN_AUDIO_PEAK_DB = -40.0


def load(path):
    """Parse the trace, tolerating the noise logcat puts around our lines."""
    marks, events = [], []
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            brace = line.find("{")
            if brace < 0:
                continue
            try:
                obj = json.loads(line[brace:].strip())
            except json.JSONDecodeError:
                continue
            if not isinstance(obj, dict) or "t" not in obj:
                continue
            if "mark" in obj:
                marks.append(obj)
            elif "e" in obj and isinstance(obj["e"], dict):
                ev = dict(obj["e"])
                ev["t"] = obj["t"]
                events.append(ev)
    marks.sort(key=lambda m: m["t"])
    events.sort(key=lambda e: e["t"])
    return marks, events


def between(events, t0, t1, kind=None):
    return [
        e for e in events
        if t0 <= e["t"] <= t1 and (kind is None or e.get("type") == kind)
    ]


def stats_delta(stats, field):
    """How much a cumulative counter moved across a list of stats events."""
    vals = [s.get(field) for s in stats if isinstance(s.get(field), (int, float))]
    if len(vals) < 2:
        return 0
    return vals[-1] - vals[0]


def check_soft(mark, events, slow):
    """The seamless contract: the viewer must see nothing."""
    t0, t1 = mark["t"], mark["t"] + SOFT_WINDOW_MS * (2 if slow else 1)
    fails, notes = [], []

    buffering = between(events, t0, t1, "buffering")
    if buffering:
        reasons = ", ".join(b.get("reason", "?") for b in buffering)
        fails.append(f"{len(buffering)} buffering event(s) [{reasons}] - the viewer saw loading")

    stats = between(events, t0, t1, "stats")
    if len(stats) < 2:
        notes.append("not enough stats samples to judge; window too short or run cut off")
    else:
        for field, label in (
            ("stall_events", "stall"),
            ("frames_dropped", "dropped frame"),
            ("int_gt58", "skipped presentation slot"),
            ("pipeline_retries", "pipeline rebuild"),
        ):
            d = stats_delta(stats, field)
            if d > 0:
                fails.append(f"{d} {label}(s) during the swap")

        drifts = [s["av_drift_ms"] for s in stats if isinstance(s.get("av_drift_ms"), (int, float))]
        if len(drifts) >= 2 and max(drifts) - min(drifts) > MAX_DRIFT_MS:
            fails.append(
                f"A/V drift moved {max(drifts) - min(drifts)}ms across the swap "
                f"(limit {MAX_DRIFT_MS}ms)"
            )

        # The swap must actually have happened, or a clean result is meaningless.
        sizes = {(s.get("width"), s.get("height")) for s in stats if s.get("width")}
        if len(sizes) < 2:
            fails.append(f"resolution never changed ({sizes or 'unknown'}) - the swap was a no-op")

    changed = between(events, t0, t1, "track_changed")
    if not changed:
        notes.append("no track_changed event in the window")
    return fails, notes


def check_manual(mark, events, slow):
    """The deliberate contract: loading, then clean playback."""
    scale = 2 if slow else 1
    t0 = mark["t"]
    search_end = t0 + MANUAL_PLAYING_BUDGET_MS * scale + 2_000
    fails, notes = [], []

    buffering = between(events, t0, search_end, "buffering")
    if not buffering:
        fails.append("no buffering - the user got no feedback that the switch is happening")
        first_buf = None
    else:
        first_buf = buffering[0]
        delay = first_buf["t"] - t0
        if delay > MANUAL_BUFFER_DEADLINE_MS * scale:
            fails.append(f"loading appeared only after {delay}ms (limit {MANUAL_BUFFER_DEADLINE_MS * scale}ms)")

    playing = [e for e in between(events, t0, search_end, "playing")
               if first_buf is None or e["t"] >= first_buf["t"]]
    if not playing:
        fails.append(f"never returned to playing within {MANUAL_PLAYING_BUDGET_MS * scale + 2000}ms")
        return fails, notes
    resumed = playing[0]
    took = resumed["t"] - t0
    if took > MANUAL_PLAYING_BUDGET_MS * scale:
        fails.append(f"loading took {took}ms (budget {MANUAL_PLAYING_BUDGET_MS * scale}ms)")

    # Settle: after the spinner clears, playback must be clean.
    s0 = resumed["t"] + SETTLE_SKIP_MS
    s1 = s0 + SETTLE_MS * scale
    if between(events, s0, s1, "buffering"):
        fails.append("hitched again after the switch had supposedly finished")

    stats = between(events, s0, s1, "stats")
    if len(stats) < 2:
        notes.append("settle window has too few stats samples")
    else:
        for field, label in (("stall_events", "stall"), ("int_gt58", "skipped presentation slot")):
            d = stats_delta(stats, field)
            if d > 0:
                fails.append(f"{d} {label}(s) in the settle window")
        decoded = stats_delta(stats, "frames_decoded")
        judder = stats_delta(stats, "judder_frames")
        if decoded > 0:
            pct = 100.0 * judder / decoded
            if pct > MAX_JUDDER_PCT:
                fails.append(f"judder {pct:.1f}% of frames (limit {MAX_JUDDER_PCT}%)")
        drifts = [s["av_drift_ms"] for s in stats if isinstance(s.get("av_drift_ms"), (int, float))]
        if drifts and max(abs(d) for d in drifts) > MAX_DRIFT_MS:
            fails.append(f"A/V drift {max(drifts, key=abs)}ms after the switch (limit {MAX_DRIFT_MS}ms)")
        # audio_peak_db is [left, right] dB, or null before the first mixed
        # frame. The loudest channel over the settle window is what proves the
        # new track is audible - a clean event sequence does not.
        peaks = [max(s["audio_peak_db"]) for s in stats
                 if isinstance(s.get("audio_peak_db"), list) and s["audio_peak_db"]]
        if peaks and max(peaks) < MIN_AUDIO_PEAK_DB:
            fails.append(f"audio silent after the switch (loudest peak {max(peaks):.0f} dB)")
        elif not peaks:
            notes.append("no audio_peak_db in stats - cannot tell whether sound came back")

    # Position must not lurch backwards.
    if isinstance(mark.get("pos"), (int, float)) and mark["pos"] >= 0:
        after = between(events, resumed["t"], s1, "position")
        if after:
            jump = after[0].get("position_ms", 0) - mark["pos"]
            if jump < -MAX_POSITION_JUMP_MS or jump > MAX_POSITION_JUMP_MS:
                fails.append(f"position jumped {jump:+}ms across the switch (limit +/-{MAX_POSITION_JUMP_MS}ms)")
    return fails, notes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("logfile")
    ap.add_argument("--device-slow", action="store_true",
                    help="double every latency budget (emulators pace badly)")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    marks, events = load(args.logfile)
    if not marks:
        print("no MARK lines found - was the app started with tracing on?", file=sys.stderr)
        return 1
    for m in marks:
        if m.get("mark") == "scenario_abort":
            print(f"scenario aborted: {m.get('why')}", file=sys.stderr)
            return 1

    results = []
    for m in marks:
        if m.get("mark") != "select":
            continue
        mode = m.get("mode", "manual")
        if mode == "soft":
            fails, notes = check_soft(m, events, args.device_slow)
            contract = "seamless"
        elif mode == "auto":
            continue  # arming ABR is not itself a switch
        else:
            fails, notes = check_manual(m, events, args.device_slow)
            contract = "deliberate"
        results.append({
            "t": m["t"], "kind": m.get("kind"), "mode": mode, "contract": contract,
            "step": m.get("step"), "label": m.get("label"),
            "pass": not fails, "failures": fails, "notes": notes,
        })

    if args.json:
        print(json.dumps({"results": results}, indent=2))
    else:
        for r in results:
            head = f"[{'PASS' if r['pass'] else 'FAIL'}] {r['kind']}/{r['mode']} ({r['contract']})"
            step = "" if r["step"] is None else f" step {r['step']}"
            print(f"{head}{step} @{r['t']}ms  {r['label'] or ''}")
            for f in r["failures"]:
                print(f"         ! {f}")
            for n in r["notes"]:
                print(f"         . {n}")
        bad = sum(1 for r in results if not r["pass"])
        print(f"\n{len(results) - bad}/{len(results)} switches held their contract")

    if not results:
        print("no switches found in the trace", file=sys.stderr)
        return 1
    return 1 if any(not r["pass"] for r in results) else 0


if __name__ == "__main__":
    sys.exit(main())
