# Audio passthrough (E-AC-3 over HDMI) — SHIPPED + start-deadlock VYŘEŠENO (2026-06-23)

> **Stav:** passthrough **na masteru, funkční, ověřené na zařízení.**
> **Zařízení:** Google TV Streamer (`kirkwood`, armeabi-v7a), TV→HDMI(ARC)→soundbar.
> **Poslední fix:** `b030db8` (start-deadlock direct AudioTracku).

---

## 1) SHIPPED — na masteru `rust_player_learning`

| commit | co |
|---|---|
| `b030db8` | **start-deadlock fix** (dvoufázový feed, viz §3). Ověřeno: video 24-25 fps, A/V drift stabilní ~112 ms, write_ahead drží AHEAD_MS, head sleduje médium 1:1. |
| `b4eb4f7` | **MediaClock**: audio-disciplinované hodiny. **Seam pro passthrough** — clock čte `AudioSink::played_ms`, takže passthrough sink ho převezme beze změny videa. |
| `2367ea1` | `set_audio_passthrough(bool)` toggle API (default off). |
| `912ca0a` | `AudioTrackSink` primitiva (JNI, jni 0.22). |

PCM/AAC cesta funguje normálně. Frame-accurate seek, #23 stally, resume — viz `RESUME_SEEK_STILL_BROKEN.md`.

---

## 2) Architektura

- **Capability** (bridge `eac3_passthrough_supported`, JNI `AudioTrack.isDirectPlaybackSupported(E_AC3)`) → na zařízení vrací `true`.
- **Výběr stopy** (bridge `run_playback`): když `passthrough_supported && audio_passthrough_pref` → vybrat **EC-3** rep + `player.set_audio_passthrough(true)`; jinak AAC (PCM). Player se ještě sám gatuje: bitstream cestu vezme jen když je kodek `ec-3`/`ac-3` A sink se postaví, jinak PCM fallback.
- **`AudioTrackSink`** (`renderers/audio_passthrough.rs`, android): `AudioTrack(ENCODING_E_AC3, 48k, 5.1)` přes Builder; `write` = syrové AU (blocking, STREAM mode → back-pressure na bufferu tracku); `played_ms` = `getTimestamp().framePosition` interpolováno na CLOCK_MONOTONIC „teď"; impl traitu `AudioPassthrough`.
- **`AudioRenderer`** (`renderers/audio.rs`): drží `Option<Arc<dyn AudioPassthrough>>`; `played_ms`/`output_latency_ms`/`flush`/`set_paused`/`is_passthrough` delegují na passthrough když je set; `set_passthrough(Some)` zapauzuje cpal (ať se neperou o HDMI).
- **Feed** (`player.rs::audio_passthrough_play` + `audio_passthrough_task`): download+decrypt audio (reuse `download_task`), parse mp4 AU, **trim na seek cíl**, dvoufázový zápis do sinku (§3). Spawnuto v play loopu místo `audio_play` když passthrough; PCM sample-channel se zahodí → `audio_sync_loop` no-opne.
- **MediaClock** čte `audio_sink.played_ms()` → v passthrough = AudioTrack getTimestamp. `audio_base=0` pro passthrough (played_ms je už 0-based od startu tracku).

### Klíčová rozhodnutí
- **Líný `play()`** AudioTracku až při PRVNÍM zápisu AU (ne v `new()`): volání na nerozjetém direct tracku → `dead IAudioTrack` recyklace.
- **Gate prvního zápisu na `pipeline_live`** (video ready): jinak audio hraje ~1-2 s před videem (startup dekódu) a video to nedožene.
- **`stopped` guard** v sinku: `write`/`played_ms`/lifecycle no-op po Drop → seek teardown nesáhne na released track (crash safety).
- **cpal pauza** při passthrough: souběžný PCM stream odpojoval bitstream (`Error -32` EPIPE).

---

## 3) Start-deadlock — root cause + fix (`b030db8`)

**Symptom (původní):** EC-3 passthrough = **žádný zvuk + „zabrzděné" video** (~1-2 fps). Líný `play()` z předchozí iterace tohle NEvyřešil.

**Příčina:** direct/compressed `AudioTrack` (ENCODING_E_AC3, STREAM) **nezačne hrát** — `getTimestamp()` vrací false, `getPlaybackHeadPosition()`/`played_ms()` jsou 0 — dokud se nenabufferuje dost bitstreamu k překročení start-thresholdu (**empiricky ~2,5 s média** na tomhle boxu + HDMI AVR, hluboko nad starým 200 ms oknem). Feed head-pacoval zápisy na `AHEAD_MS=200` podle playback headu → nabufferoval jen ~224 ms → **head se nikdy nehnul → feed čekal věčně → video clock (MediaClock čte `played_ms`) zamrzl → MediaCodec back-pressuroval na 1-2 fps.** Klasický chicken-and-egg: feed čeká na rozjetí playbacku, playback se nerozjede bez dat, feed je přestal posílat.

**Fix — dvoufázový feed** (`audio_passthrough_task`):
- **PRIME:** dokud `played_ms()==0`, krmit BEZ head-gatingu — blocking `write()` back-pressuruje na bufferu tracku. Buffer zvětšen 64 KiB→**256 KiB** (≈2,7 s @ 768 kbps), aby prime překročil threshold dřív než `write` zablokuje.
- **STEADY:** jakmile se head rozjede, pacovat na `AHEAD_MS=750` ms ahead reálné pozice → head zůstane použitelné hodiny a seek teardown neuvázne na sekundách bufferu.

**Důkaz na zařízení** (720p SDR, ec-3, kirkwood): `decoded` 24-25 fps, A/V drift stabilní ~112 ms (dřív neomezeně rostl), `write_ahead` drží ~745 ms, playback head roste 1:1 s médiem, 0 dropů, přes 30 s čisté.

---

## 4) Otevřená rizika / další kroky

- **Slyšitelný zvuk ze soundbaru NEpotvrzen** (zařízení neslyším). Playback head běží v realtime s validním getTimestamp → HAL bitstream reálně konzumuje, takže DD+ z HDMI velmi pravděpodobně jde — ale **potvrdit poslechem.**
- **Lip-sync ~112 ms** (audio za videem, konstantní): doladit přes `output_latency_ms` na passthrough sinku (AVR/decode latence). Malý, ne ten kritický bug.
- **Seek lifecycle:** dva direct AudioTracky na HDMI naráz když se starý neuvolní před novým (analogie video Surface gate v `08a0f0a`). `stopped`-guard + `set_passthrough(None)` na začátku play loopu mají starý uvolnit — pozor na 2 tracky naráz / EPIPE. NEOVĚŘENO pod seekem.
- **EC-3 PCM fallback** je flaky — když `AudioTrackSink::new` selže, spadne to do MediaCodec EC-3 dekódu (`queue_input ErrorUnknown`). Lepší fallback řešit, ne ignorovat.
- **Genuine no-AVR:** když by capability check pustil passthrough ale head se NIKDY nerozjel, prime fáze dokrmí buffer a `write` zablokuje + video zamrzne. Bridge capability check to má pokrýt; watchdog v prime fázi by to zatvrdil.
- **Cross-platform:** passthrough je android-only; cpal/iOS/desktop netknuté.

---

## 5) Repro / build / deploy

**Zkušební apka = `cz.preclikos.rust_player`** (= `app-android/` v tomhle repu; fixture `app-shared/src/lib.rs`). Opt-in soubory (jako `direct.txt`/`video_pref.txt`):
```
# zapnout passthrough (vynutí ec-3 + set_audio_passthrough(true)):
adb shell "echo 1 > /sdcard/Android/data/cz.preclikos.rust_player/files/audio_passthrough.txt"
# POZOR na confound: stale video_pref=hdr vynutí 4K HDR10+, které tenhle box dekóduje 1-2 fps.
# Pro izolaci audia použij 720p SDR:
adb shell "echo 5 > /sdcard/Android/data/cz.preclikos.rust_player/files/video_pref.txt"
```
Build (jen v7a, co zařízení potřebuje):
```
export LIBCLANG_PATH=C:\msys64\mingw64\bin          # ffmpeg-sys bindgen na Windows
export ANDROID_NDK_HOME=…/Sdk/ndk/29.0.13113456     # gradle libs.versions.toml
cargo ndk -t armeabi-v7a --platform 26 -o app-android/android/app/src/main/jniLibs build --release -p app-android
cp …/ndk/29…/toolchains/llvm/prebuilt/windows-x86_64/sysroot/usr/lib/arm-linux-androideabi/libc++_shared.so \
   app-android/android/app/src/main/jniLibs/armeabi-v7a/
(cd app-android/android && ./gradlew :app:assembleDebug -x buildRustDebug -x copyLibCxxShared)
adb install -r app-android/android/app/build/outputs/apk/debug/app-debug.apk
adb shell monkey -p cz.preclikos.rust_player -c android.intent.category.LAUNCHER 1
```
Log: tag je **`player`** (ne `bz_rust_player` — to je produkční BlackZone app). Odchyt čistě podle pid:
```
adb logcat --pid=$(adb shell pidof cz.preclikos.rust_player) -v time | grep -E "audio-pt|\[audio\]|HEALTH|drift"
```
Diag řádky: `[audio] passthrough engaged`, `[audio-pt] au=…head=…write_ahead=…` (Info), `[audio-pt] played_ms probe` (debug), `[vsync] … HEALTH … decoded=…/s drift=…ms`.

Pozn.: produkční bridge (`BlackZoneAndroidRust`) staví všechny ABI a podepisuje release; tady stačí debug v7a.
