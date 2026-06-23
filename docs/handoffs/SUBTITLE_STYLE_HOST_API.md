# Drobnost — vystavit SubtitleStyle pro host — ✅ HOTOVO (2026-06-19)

> **Komu:** vlastník playeru.
> **Od:** BlackZone integrace.
> **Stav:** ✅ vyřešeno variantou 1 — `pub use subtitle_style::SubtitleStyle;`
> je v kořeni crate (`player/src/player.rs:26`). Bridge si `SubtitleStyle`
> může pojmenovat přes `use player::SubtitleStyle;`.

---

### Původní požadavek (pro kontext)

`84f65b3` přidal `Player::set_subtitle_style(SubtitleStyle)` + `subtitle_style`
modul; chtěl jsem to napojit na nastavení „Barva titulků" (a velikost) v appce.
`SubtitleStyle` ale tehdy nebyl re-exportovaný z kořene crate, takže ho šlo
v JNI vrstvě jen těžko pojmenovat.

**Vyřešeno:** kořen crate teď re-exportuje `SubtitleStyle` (varianta 1, 1 řádek).
Zbývá host-side: `nativeSetSubtitleStyle` v bridgi (převod Android ARGB → RGBA)
napojit na uložené nastavení (white/yellow + velikost) — bez toho je „Barva
titulků" v Settings zatím jen uložená, ne aplikovaná. Pomocné parsery jsou
`SubtitleStyle::parse_color` (jména i hex), viz `app-shared` `subtitle_style()`.
