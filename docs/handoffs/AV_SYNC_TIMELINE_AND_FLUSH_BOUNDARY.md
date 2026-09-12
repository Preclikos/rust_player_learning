# A/V sync: jedna časová osa + flush boundary (2026-09-12)

> **Stav:** ✅ opraveno v enginu, ověřeno desktop soak harnessem (Windows, D3D12/WASAPI)
> vč. nového nezávislého měření lip-syncu. ⚠️ **Zařízení (Android AudioTrack) zatím
> neověřeno** — v době opravy nebylo připojené; postup ověření níže.
> **Soubory:** `player/src/av_sync.rs` (nový), `player/src/player.rs` (MediaClock,
> `video_sync_loop`, `audio_sync_loop`, `av_sync_handler`), `renderers/audio.rs`,
> `audio_cpal.rs`, `audio_track_pcm.rs`, `examples/conformance.rs`.

## Příznak

„Rozjeté audio/video, hlavně na Androidu, hlavně po přepnutí (track / kvalita) nebo
seeku." Konstantní offset, který se nezmění do dalšího seeku. Interní gauge
`av_drift_ms` byl při tom čistý — měří jen **rychlostní** rozjezd dvou hodin, ne
konstantní offset zaviněný špatným ukotvením.

## Příčiny (tři nezávislé, všechny v enginu)

### 1. Hodiny počítaly starý audio tail jako nové audio (Android, největší)

`MediaClock` = `seek_offset + (played_ms − audio_base)`, kde `audio_base` byl snímek
kumulativní prezentované pozice v okamžiku spawnu nové pipeline. Na seek/switch se
ale **nikdy nevolá `AudioTrack.flush()`** — vyhodí se jen fronta v Rustu a buffer
tracku (2× minBufferSize ≈ 100–300 ms) dohrává **starý** obsah. Nový první vzorek je
slyšet až po dohrání tailu, ale hodiny už od spawnu běžely → video před audiem
o hloubku bufferu, po každém rebuildu. Na cpal (desktop) je tail jen jeden callback
buffer, proto to na desktopu skoro nebylo vidět.

### 2. Video ukotvené na „kdy dorazil první snímek"

`pts_base = raw_pts − clock_v_okamžiku_příchodu`. Správnost pak závisela na závodě
mezi příchodem prvního snímku, startem audia a hodnotou `output_latency_ms`
(na cold startu 0, po rebuildu už cachovaná → korekce latence se anulovala).

### 3. Trim audia srovnával absolutní PTS s 0-based cílem

`audio_sync_loop` dostával `seek_offset` (0-based), ale dekodované audio má
absolutní composition PTS. U obsahu s nenulovým origin (BMDT ≠ 0, např. stream se
segmentem 0 od 83 ms) se na každém startu napadovalo `origin` ms ticha → audio trvale
pozdě o origin. Passthrough cesta to měla správně (absolutní `discard_below_us`),
PCM cesta ne.

Bonus: flush byl jen flag zpracovaný asynchronně v konzumentu → mohl zahodit i
**nové** vzorky zařazené těsně po flushi (vzácné, ale možné). A každý zahozený
poškozený audio AU (`send_packet failed`, běžné u E-AC-3) posunul všechno další
audio o 32 ms dřív — natrvalo, kumulativně.

## Oprava

Vše v `av_sync.rs` (čisté, unit-testované) + napojení:

* **`FlushState` + `AudioChunk{gen}`** — `flush()` zvýší generaci; PCM se do sinku
  posílá po chunkách (1 dekodovaný frame = 1 send, dřív 96 000 sendů/s po vzorku)
  s generací. Konzument (cpal callback / null sink / Android writer) staré generace
  zahazuje (race-free) a při prvním chunku nové generace **označí hranici** = pozice
  zařízení, kde nové audio začíná. `AudioSink::played_since_flush_ms()` měří od ní:
  `Some(0)` dokud tail nedohraje. Android loguje
  `[audio-pcm] flush boundary gen=N …; X ms of previous audio still queued` — X je
  velikost chyby, kterou to opravuje.
* **`MediaClock`** = `seek_offset + played_since_flush − output_latency`. Žádný
  `audio_base`. Do startu nového audia stojí na `seek_offset`, video drží první
  snímek.
* **Video absolutně:** `pts_ms = (frame.pts_us − origin_us)/1000`, žádný `pts_base`.
  Snímek se ukáže, když hodiny čtou jeho PTS.
* **`AudioAligner`:** trim/pad na absolutní ose (`origin + target`), pak kontinuita
  — mezera se vyplní tichem, překryv ořízne (tolerance 10 ms < 1 frame, cap 5 s).
  Loguje `[async] audio discontinuity #n …`.
* Start gate v `av_sync_handler` už jen pro passthrough (PCM cesta čekala 500 ms
  na pozici, která se před spuštěním smyček nemohla hnout → −500 ms TTFF na každém
  seeku).
* Nové gauge `video_late_frames` (snímek prezentovaný >45 ms po svém čase):
  `ConformanceSummary`, `Stats` event, bridge JSON `frames_late`.

## Testy

* `cargo test -p player --lib av_sync` — 12 testů: hranice po flushi (tail nepočítá),
  dvojitý flush, kurzor zahazuje starou generaci, aligner (drop/trim/pad, origin,
  ms-rounding, vypadlý frame, překryv, cap). `vtt` — 4 nové (entity, `&lrm;`).
* **Conformance harness** (`examples/conformance.rs`) měří lip-sync **nezávisle na
  hodinách enginu**: `Player::with_sinks` + dekorátory sinků. Audio tap detekuje
  pípnutí v PCM (obsah, ne timestampy) a z `played_since_flush_ms` + latence spočítá,
  kdy bylo slyšet; video tap zaznamená prezentaci flash snímku (pts = 2k s).
  Nové checky `lipsync` (max |flash−beep| ≤ 80 ms), `lipsync-median` (±40 ms),
  `lipsync-coverage`, `late-frames` (≤ 2 %). CI běží stejný příkaz — checky platí
  automaticky.

### Výsledek (Windows, 90 s, 3 soft switche, 2 seeky)

| gauge | hodnota |
|---|---|
| lipsync median / max | −26 ms / 37 ms (45 párů; ~5–16 ms z toho je zpoždění pípnutí v assetu kvůli AAC rámcům) |
| offset po seeku vs. před | stejný (žádný skok) |
| av_drift max | 43 ms |
| late frames | 7 / 2174 = 0,3 % |
| stalls / errors / retries | 0 |

Před opravou detektor neexistoval; starý `av_drift` by offset z (1)–(3) neviděl.

## Ověření na zařízení (TODO, až bude box)

1. `.\test_android.ps1`, přehrát, několikrát seek + přepnout audio track.
2. logcat: `[audio-pcm] flush boundary` — sledovat `X ms of previous audio still
   queued` (očekávám 100–300 ms; to byl dřívější offset). `[vsync gen N] first frame
   pts=… clock=…` — první snímek by měl být v řádu ms od `target`.
   `[async] aligned first audio` — skip/pad < 1 s. `[av_sync gen N] spawning sync
   loops: … (audio start waited 0ms)`.
3. Sluchem: lip-sync po seeku/switchi stejný jako od startu. Passthrough (E-AC-3
   přes HDMI) — gate 4 s zachován, chování beze změny.

## Známé zbytky / další kroky

* Po seeku do středu segmentu se ořezávají snímky od keyframu k cíli až ve vsync
  smyčce; na pomalém dekodéru (4K) tak audio může začít dřív než první cílový snímek
  → pár LATE snímků na startu, pak sync. Lepší: signalizovat `video_ready` až
  prvním snímkem ≥ target.
* Soft ABR switch má díru ~250–320 ms (BOUNDARY_LEAD + rozjezd dekodéru) → LATE
  drain; jen video, audio nedotčeno. Samostatné téma.
* Měření lip-syncu na zařízení (HDMI capture) = Phase 2 conformance, beze změny.
