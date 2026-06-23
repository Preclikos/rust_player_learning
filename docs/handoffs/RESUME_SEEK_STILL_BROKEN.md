# Resume + position-after-seek — VYŘEŠENO (2026-06-16)

> **Stav:** ✅ opraveno v playeru, ověřeno na zařízení.
> **Commit:** `fix/resume-startpos-clobber-and-clock-rebase` (`player/src/player.rs`).
> **Zařízení:** Google TV Streamer (`kirkwood`, armeabi-v7a), direct MediaCodec cesta.
> **Pro integraci:** jen přebuildit `.so` (path-dep) — host se nemění, volání
> byla správná. Navíc lze odstranit jeden zbytečný host hack (níže).

Tahle nóta dřív tvrdila „dvě živé chyby v playeru". Po instrumentaci na
zařízení se ukázalo, že **obě jsou reálné a nezávislé** (ne, jedna není
důsledek druhé) — a obě jsou teď opravené. Níže příčina + fix + důkaz.

---

## #1 — `set_start_position` ignorováno (přehrávání od segmentu 0)

**Příčina (interakce host↔player, ne čistý player bug):**
`change_audio_track` i `change_video_track` volaly `self.seek(self.position())`.
Konzument aplikuje uloženou jazykovou preferenci hned po `prepare()`
(BlackZone `applyLanguagePreference` v `onPlayerPrepared`) → `change_audio_track`
→ `seek(position())`. Jenže **před prvním snímkem je `position()` == 0**, takže
to zaparkuje `seek_target = Some(0)`. `play()` smyčka pak vezme
`seek_target.take() = Some(0)` a přes `or_else(pending_resume)` se uložená
resume pozice (`set_start_position`) **vůbec nepoužije** → start od segmentu 0.

Diagnostický log to ukázal jednoznačně:
```
resume: start at 4512002ms (48.09%)
[play] loop start: seek_target=Some(0ns) pending_resume=Some(4512.002s)   ← nula přebíjí resume
[play] start: seek_offset=0ms vidx=0 ...
[dec] segment 0 boundary stall ...                                        ← hraje od začátku
```

**Fix (player):** restart-seek v `change_audio_track`/`change_video_track`
je gatovaný na `pipeline_live`. Dokud pipeline nevyprodukuje první snímek,
track-switch **neseekuje** — `play()` smyčka si novou stopu přečte z
representation cell při startu a `pending_resume` se spotřebuje normálně.
Po náběhu (live) se chová beze změny (seek na aktuální pozici s novou stopou).

## #2 — pozice po seeku = `played_ms` (ne absolutní)

**Příčina:** `video_sync_loop` kalibroval `pts_base` z kumulativního audio
`played_ms` (audio-master clock, commit `f5c3e85`), které se nikdy
nerebasovalo na seek/start offset. `start_time` seek-aware byl, ale krmil jen
wall-clock fallback. `flush()` navíc neresetuje `samples_consumed` → `played_ms`
běží přes celou session. Důsledek: špatný seekbar/čas, rozbitý relativní seek
(špatná báze) a ABR soft-switch vybírající restart segment z nesmyslné pozice.

**Fix (player):** v anchor okamžiku pipeline se sejme `played_ms` (`audio_base`)
a audio clock se rebasuje na 0-based media timeline téhle pipeline:
`media = seek_offset + (played − audio_base)`. Pacing beze změny (konstanta se
vyruší v present-time delta), absolutní je jen reportovaná pozice.

---

## Důkaz po opravě (zařízení, resume na ~75 min)
```
seekTo target=4560166ms (nativePos before=4550166ms)   ← pozice už je na ~75 min ⇒ resume dosedl
[play] start: seek_offset=4560166ms vidx=760 snapped=4560000ms segs=1564
[dec] segment 761 boundary stall ...                   ← dekóduje seg 761 (~76 min), ne 0
```
`nativePos` před seekem = 4550166 → 4560000 → 4566000 → 4572000 — absolutní,
sedí na snapped targety, ne na `played_ms`.

## Cleanup na straně hosta (volitelné)
`maybeApplyMovieResume` (seek-po-`Playing` + 450 ms) v `PlaybackController.kt`
je teď **redundantní** — `set_start_position` posadí obraz sám. Lze vyhodit
(včetně komentáře „není honored on this device", který už neplatí). Bez něj
zmizí i ten zbytečný re-seek hned po startu.
