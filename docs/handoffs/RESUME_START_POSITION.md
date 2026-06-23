# Player API request: deterministic start position (resume)

> **Komu:** vlastník playeru.
> **Od:** BlackZone TV integrace.
> **Proč teď:** resume jsem musel v konzumentovi (bridge) udělat přes tři
> host-side hacky. Patří to do playeru — jakýkoli další konzument (Console…)
> narazí na totéž.

## TL;DR

Přidej do playeru **synchronní „start position"**, kterou `play()` deterministicky
použije jako počáteční offset (a u kterého se úvodní ABR nepřepere se seekem).
Konzument pak jen: `player.set_start_position(resume); player.play();` — konec.

## Co teď musí dělat konzument (a nemělo by)

Resume na uloženou pozici v direct režimu nešlo „normálně". Funkční workaround
v bridge (`run_playback`) je až tohle trojkombo:
1. **NEseekovat před `play()`** — seek před play se nectí: play odstartuje od 0.
   `seek()` je fire-and-forget (spawne zápis `seek_target`), takže `play()`
   přečte `seek_target` dřív, než zápis dosedne → offset 0. Ani 250ms prodleva
   mezi `seek()` a `play()` nepomohla (pořád `[abr] soft switch … pos 0ms`,
   první snímek `pts≈6s`).
2. **Seek až po prvním `PlayerEvent::Playing`** + ~400ms → seek na běžící
   pipeline (osvědčená cesta). Tohle teprve dosedne na správnou pozici.
3. **Odložit ABR**: držet `AbrStrategy::Manual` a `set_abr_strategy(BandwidthEwma)`
   zavolat až ~1,5 s PO resume seeku — jinak úvodní ABR soft-switch (~1 s po
   startu) koliduje s resume seekem a **občas stallne MediaCodec** (direct).

To je křehké (časování) a každý konzument by to opisoval.

## Návrh API

```rust
impl<V, A> Player<V, A> {
    /// Počáteční pozice pro nejbližší play(). Synchronní (žádný spawn).
    /// play() ji použije jako iniciální offset s prioritou nad 0; po
    /// spotřebování se vymaže (one-shot). None = od začátku.
    pub fn set_start_position(&self, pos: Option<Duration>);
}
```

Implementace nejspíš triviální — `pending_resume` už děláš přesně tohle
(`play()` ho čte: `target.take().or_else(|| pending_resume.lock().take())`).
Stačí ho **veřejně vystavit** (setter) + zajistit dvě věci:
- `play()` startuje pipeline na tom offsetu **deterministicky** (žádný race se
  zápisem jako u `seek()`).
- **Úvodní ABR** se navěsí na tenhle start offset (ne na 0), nebo se první
  soft-switch potlačí, dokud první snímek nedosedne — ať se nepotká se startem
  a nestallne kodek. (To je ta kolize, kterou teď obcházím odložením ABR.)

## Po dodání API

Bridge se smrskne na:
```rust
if let Some(ms) = resume_ms { player.set_start_position(Some(Duration::from_millis(ms))); }
player.set_abr_strategy(AbrStrategy::BandwidthEwma { safety_factor: 1.25 });
player.play();
```
…a zahodím seek-po-Playing, delaye i odložené ABR. Konzument zůstane hloupý.

## Pozn.

Save progressu i samotný resume teď z mé strany fungují; tohle je čistě o
přesunu odpovědnosti do playeru, ať je to robustní a znovupoužitelné. Integrace
si po úpravě jen přebuildí (path-dep).
