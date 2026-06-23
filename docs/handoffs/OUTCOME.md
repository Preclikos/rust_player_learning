# Outcome — player změny pro integraci (2026-06-14)

> **Komu:** BlackZone TV integrace (a další konzumenti playeru).
> **Od:** vlastník playeru.
> **Větev:** vše na `master` (po merge `feature/hdr-stack`). Path-dep → po
> rebuildu si to integrace vezme automaticky.
> **Co dělat:** přidat pár volání, **smazat host-side hacky** (níže), a ověřit
> na zařízení body označené ⚠️.

---

## TL;DR — co teď player poskytuje (a co díky tomu smazat)

| Téma | Nové v playeru | Co na straně integrace |
|---|---|---|
| Resume | `set_start_position(Option<Duration>)` | **smazat** seek-před-play, seek-po-Playing+400ms, odložené ABR |
| Titulky / overscan | `set_subtitle_safe_insets(bottom_px)` | nechat (už zapojeno); + host **musí centrovat** video plane |
| AFR | `set_adaptive_frame_rate(bool)` | nechat (už zapojeno přes bridge toggle) |
| A/V sync | video mastrované audio clockem | nic — interní; **jen ověřit** ⚠️ |
| Direct seek stall | diagnostika `produced=` | **poslat 1 log** (níže) |

---

## 1) Resume — deterministická start pozice (commit `94deb63`)

Player má nově:
```rust
/// Synchronní (žádný spawn → žádný race s play()). play() ji použije jako
/// iniciální offset (priorita nad 0, pod explicitním pending seek()).
/// One-shot: play() ji spotřebuje. None = od začátku.
pub fn set_start_position(&self, pos: Option<Duration>);
```
A **úvodní ABR auto-switch je držený, dokud pipeline nevyprodukuje první
snímek** (event-based brána `pipeline_live`, ne timer) — takže nekoliduje s
resume startem.

> **Doplněk 2026-06-16 (po device testu):** `set_start_position` samo o sobě
> ještě resume nedotáhlo — `change_audio_track`/`change_video_track` volaly
> `seek(position())`, a host aplikuje jazykovou preferenci po `prepare()` (před
> 1. snímkem, kdy `position()==0`) → `seek_target=Some(0)` přebil
> `pending_resume`. Navíc pozice po seeku vracela `played_ms` místo absolutní.
> Obojí opraveno v `player.rs` (track-switch seek gatovaný na `pipeline_live`;
> audio clock rebasovaný na seek offset). Ověřeno na zařízení. Detail:
> `RESUME_SEEK_STILL_BROKEN.md`.

**Integrace se smrskne na:**
```rust
if let Some(ms) = resume_ms { player.set_start_position(Some(Duration::from_millis(ms))); }
player.set_abr_strategy(AbrStrategy::BandwidthEwma { safety_factor: 1.25 });
player.play();
```
**Smazat** z `run_playback` (dle `RESUME_START_POSITION.md`):
1. NEseekovat před `play()`,
2. seek až po prvním `Playing` + ~400ms,
3. držet `Manual` a odkládat `set_abr_strategy(BandwidthEwma)` ~1,5 s.

Všechny tři timing hacky už nejsou potřeba.

## 2) Titulky — safe-area + z-order (commit `a1adba9`; host-side hotovo)

- Player: `set_subtitle_safe_insets(bottom_px)` — kotví spodní hranu titulků na
  reálný inset; `0` ⇒ fallback 10 % TV title-safe. **Už zapojeno**
  (`nativeSetSubtitleSafeInsetBottom` ← `RustPlaybackFragment.surfaceChanged`:
  `max(WindowInsets systemBars|displayCutout bottom, 5 % výšky)`).
- **Host kontrakt (nechat, je to nutné):**
  - **overlay = `setZOrderOnTop(true)`** (ne `MediaOverlay`) — jinak ho černý
    pruh okna překryje a titulky se na pruh nedostanou.
  - **video plane vycentrovat** (`Gravity.CENTER` v `onVideoSizeChanged`) —
    player kreslí titulky na střed-dole celé plochy a neví, kam host video dal.
- Doladění výšky je čistě konstanta `actionSafe` (teď 5 %) v `surfaceChanged` —
  když TV overscan ořízne, zvedni.

## 3) AFR — adaptive frame rate (commity `60912eb`, `c45f101`; hotovo)

- Player: `set_adaptive_frame_rate(bool)` (default **on**). V direct režimu
  napoví fps displeji přes `ANativeWindow_setFrameRateWithChangeStrategy` se
  strategií **ALWAYS** (panel reálně přepne na 24 Hz, krátké bliknutí — záměr).
- **Už zapojeno** (`nativeSetAdaptiveFrameRate` ← `configurationStorage.getAfr()`).
  V logcatu: `[afr] switching display to 24.000 fps (fixed-source, always)`.

## 4) A/V sync — video mastrované audio clockem (commit `f5c3e85`)

Občasný desync za běhu (seek to spravil) byl tím, že video se pacovalo
wall-clockem a audio device-clockem, bez serva → rozejití zůstalo. Teď se video
pacuje podle skutečné audio pozice (`played_ms`) → nemůžou se rozejít.
**Integrace nic nemění** — jen ⚠️ **ověřit delší přehrávání**: žádný plíživý
drift, plynulé tempo bez judderu. (Kdyby tempo zlobilo, je to revert 1 commitu.)

---

## ⚠️ Co ověřit na zařízení (nešlo z build-time)

1. **A/V sync** — delší přehrávání, žádný drift, plynulé tempo (commit `f5c3e85`).
2. **reqwest 0.13 TLS** (commit `03bf53e`) — HTTPS handshake se build-time
   neověří. Otestovat: manifest fetch + segmenty + ClearKey licence na reálném
   Androidu. (macOS/iOS sdílí rustls cestu, ale z Windows nejde přeložit —
   compile-unverified.) Kdyby TLS padal, je to lokalizované v `net.rs::build_client`.
3. **AFR** — panel reálně skočí na 24 Hz (viz `[afr]` log).
4. **Resume** — `set_start_position` + `play()` dosedne na správnou pozici bez hacků.
5. **Titulky** — sedí v černém pruhu mimo obraz, neořízne.

## 🔴 Otevřené — potřebuju 1 log

**Direct-mode seek stall** (úkol #23, commit `bb3bf61` přidal diagnostiku): po
seeku/startu na offsetu se občas MediaCodec zasekne na `dequeue_input`. Až
narazíš, pošli ten řádek z logcatu (`bz_rust_player`):
```
[mc-direct] dequeue_input stall ...x5ms pts=... produced=N
```
- `produced=0` ⇒ kodek nevyprodukoval výstup = lifecycle/Surface (starý kodek na
  sdílené Surface) → fix = zaručit teardown starého kodeku před konfigurací nového.
- `produced>0` ⇒ frame tekly a zaseklo se až pak = backpressure → fix jinde.

S tím číslem dodám přesný fix bez hádání. (Možné, že to změna ready-wait
v `bb3bf61` — video_ready tvrdé, audio_ready jen 3s — už zmírnila; tak rovnou zkus.)

---

## Pozn. k závislostem (path-dep, jen rebuild)

Refresh proběhl: tokio 1.52, aes 0.9 / ctr 0.10, quick-xml 0.40, re_mp4 0.5,
jni 0.22, reqwest 0.13. **ffmpeg zůstává na 8.1** (LLVM 22 — nebumpovat). wgpu
fork rebase odložen. Bridge se přeložil proti všemu; jediné s runtime rizikem je
reqwest 0.13 TLS (viz výše).
