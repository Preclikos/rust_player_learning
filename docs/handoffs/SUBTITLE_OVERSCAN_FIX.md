# Titulky — spodní okraj ořezává TV overscan (handoff)

> **Komu:** vlastník playeru (subtitle rendering).
> **Od:** BlackZone TV integrace (embed playeru přes JNI bridge).
> **Stav:** popis + návrh, oprava je v playeru (`video_gles_egl.rs`).

## TL;DR

Titulky se na reálné TV ztrácí pod spodní hranou. Player je kotví **7 % od spodku
plochy**, což **ořízne overscan** na Google TV Streameru. Architektura (dvě vrstvy:
full-screen titulkový overlay + video plane pod ním) je správně a měnit se nemá —
stačí **vyladit spodní safe-area margin tak, aby ho overscan neukrojil**.

## Zařízení / kontext

- **Google TV Streamer** (`kirkwood`, MT8696, SDK 34), panel/výstup **armeabi-v7a**, 1080p.
- Vulkan ovladač na MT8696 abortuje → běží **GLES OES cesta** (`video_gles_egl.rs`),
  ne wgpu `draw_into`. Fix proto primárně v GLES cestě (viz níže), wgpu cesta
  (`renderers/subtitle.rs`) má stejnou konstantu kvůli desktop paritě.
- Obsah, na kterém se to projevuje: širokoúhlý film **~2.39:1** → dolní černý
  letterbox pruh ~12 % výšky. Direct dual-surface režim: MediaCodec → video plane
  (host ho aspect-fitne + **vycentruje**, `Gravity.CENTER`), wgpu/GLES → overlay
  (full-screen, translucentní, `setZOrderMediaOverlay`) jen pro titulky.

## Symptom

Na letterboxovaném titulu jsou titulky **pod spodní hranou** (TV je ořízne).
Při posunu nahoru na ~11 % už naopak **lezou na obraz** (žerou spodek obrazu),
což uživatel nechce. Hledá se rozumný střed.

## Příčina

`player/src/renderers/video/video_gles_egl.rs`, fn `draw_subtitle` (~ř. 1318–1320):

```rust
// 7% of full height up from the bottom, but never let the top edge
// leave the screen (tall multi-line cue).
let center_y = (-1.0 + half_h + 0.14).min(1.0 - half_h);
```

`0.14` v NDC = **7 % od spodního kraje** plochy. TV overscan ukrojí spodních
~5–8 %, takže se titulek dostane mimo viditelnou oblast panelu. Player kreslí na
full-surface a **nemá jak vědět o overscanu** ani kde host umístil video.

## Požadované chování (od uživatele)

- Kotvit titulek **ke spodku OBRAZOVKY** (full surface), text **roste nahoru**.
- Jednořádkový sedí u spodního kraje; víceřádkový roste nahoru a **smí lehce
  zasáhnout do obrazu** (2 řádky apod.) — to je OK, „klasicky".
- **Žádné** kotvení k videu / host-driven anchor — chce to jednoduše.

Tzn. chování je v zásadě stávající (bottom-anchored, grow-up) — jen **spodní
margin musí být mimo overscan**.

## Návrh opravy

Vyladit spodní safe-area margin v `draw_subtitle` (a kvůli paritě stejně i ve
wgpu `subtitle.rs::draw_into`, kde je `... + (th * 0.07) / (th * 0.5)` = také 7 %):

- Změřené hodnoty: **7 % ořízne**, **11 % už leze na obraz** → cíl ~**8–9 %**
  (NDC `0.16`–`0.18`). Horní `min(1.0 - half_h)` clamp nech beze změny.
- **Lepší (volitelně):** odvodit margin z reálné TV-safe oblasti místo magické
  konstanty. Android ji dává přes `WindowInsets` (overscan/safe insets) — host
  je umí spočítat a předat playeru (např. `set_subtitle_safe_inset(f32)`), pak
  je to korektní napříč zařízeními. Pokud nechceš API, stačí konzervativnější
  konstanta ~9 %.

### Akceptační kritéria

1. Na Google TV Streameru je u 2.39:1 titulu **celý jednořádkový titulek nad
   spodní hranou** (neořezaný overscanem).
2. Dvouřádkový smí mírně zasáhnout do spodku obrazu (přijatelné).
3. Beze změny chování na 16:9 obsahu a na desktopu (pokud se mění i wgpu cesta).

## Pozn. k integraci

BlackZone TV bere player přes path-dep (`player = { path =
"../../rust_player_learning/player" }`), takže po úpravě stačí říct a integrace
se přebuildí (`cargo ndk -t armeabi-v7a … build --release` → APK → `adb install`).
Host už video plane vycentroval (`PlaybackSupportFragment.onVideoSizeChanged` →
`Gravity.CENTER`), takže overlay titulky teď s obrazem geometricky sedí; zbývá
jen ten spodní margin.
