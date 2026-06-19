//! Runtime-tunable subtitle styling.
//!
//! This is the small subset of an ASS/libass style that the current CPU
//! rasterizer (see `renderers::subtitle`) can honour. Field names and
//! semantics deliberately mirror ASS so a future libass backend can map
//! them 1:1 instead of inventing a parallel vocabulary:
//!
//!   * `text_color`    → ASS `PrimaryColour`
//!   * `outline_color` → ASS `OutlineColour` / `BackColour` (today we use
//!                       one dark colour for the 1px drop-shadow)
//!   * `size_scale`    → a multiplier on the auto-computed `Fontsize`
//!
//! Anything libass adds later — bold, italic, alignment, margins, border
//! style — slots in here as new fields without breaking call sites: the
//! struct is `#[non_exhaustive]`-friendly via `DEFAULT` + struct-update
//! syntax, and every consumer reads fields by name.

/// Visual styling for the subtitle overlay. `Copy` so it can be read out
/// from behind the overlay's mutex without cloning. Construct from
/// `DEFAULT` and override the fields you care about:
///
/// ```ignore
/// let yellow = SubtitleStyle { text_color: [255, 204, 0, 255], ..SubtitleStyle::DEFAULT };
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubtitleStyle {
    /// Glyph fill colour, RGBA. The alpha channel multiplies the glyph
    /// coverage (255 = fully opaque, 0 = invisible). Default: opaque
    /// white.
    pub text_color: [u8; 4],
    /// Drop-shadow / outline colour, RGBA. Default: opaque black — the
    /// dark halo that keeps white text readable over bright video.
    pub outline_color: [u8; 4],
    /// Multiplier applied to the auto-computed font size (which tracks
    /// ~5% of the video height). `1.0` leaves the responsive default
    /// untouched; `1.3` is noticeably larger. Clamped to `[0.5, 3.0]` by
    /// [`sanitised`](Self::sanitised) so a wild value can't blow the cue
    /// bitmap up past texture limits.
    pub size_scale: f32,
}

impl SubtitleStyle {
    /// The Phase-1 look: opaque white text, black shadow, auto size.
    pub const DEFAULT: Self = Self {
        text_color: [255, 255, 255, 255],
        outline_color: [0, 0, 0, 255],
        size_scale: 1.0,
    };

    /// Clamp every field into a safe rendering range. Mirrors
    /// `HdrTonemapParams::sanitised` — the public setter runs this so a
    /// host can't push a value that breaks rasterization. Colours need no
    /// clamping (every `u8` is valid); only `size_scale` is bounded.
    pub fn sanitised(self) -> Self {
        Self {
            size_scale: if self.size_scale.is_finite() {
                self.size_scale.clamp(0.5, 3.0)
            } else {
                1.0
            },
            ..self
        }
    }

    /// Parse a CSS-ish colour string into RGBA, defaulting alpha to 255.
    /// Accepts `#RGB`, `#RRGGBB`, `#RRGGBBAA` (the `#` is optional) and a
    /// handful of names common in subtitle settings UIs. Returns `None`
    /// on anything unrecognised so the caller keeps its existing colour.
    ///
    /// Useful at the host/config layer (env var, settings file) to turn a
    /// user's `"yellow"` or `"#FFCC00"` into a `text_color`.
    pub fn parse_color(s: &str) -> Option<[u8; 4]> {
        let s = s.trim();
        let named = match s.to_ascii_lowercase().as_str() {
            "white" => Some([255, 255, 255, 255]),
            "black" => Some([0, 0, 0, 255]),
            "yellow" => Some([255, 204, 0, 255]), // the classic "TV subtitle" amber
            "red" => Some([255, 64, 64, 255]),
            "green" => Some([64, 255, 64, 255]),
            "cyan" => Some([0, 255, 255, 255]),
            "magenta" => Some([255, 0, 255, 255]),
            "gray" | "grey" => Some([190, 190, 190, 255]),
            _ => None,
        };
        if named.is_some() {
            return named;
        }

        let hex = s.strip_prefix('#').unwrap_or(s);
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        match hex.len() {
            // #RGB → expand each nibble (F → FF)
            3 => {
                let n = |i: usize| {
                    u8::from_str_radix(&hex[i..i + 1], 16)
                        .ok()
                        .map(|v| v << 4 | v)
                };
                Some([n(0)?, n(1)?, n(2)?, 255])
            }
            6 => Some([byte(0)?, byte(2)?, byte(4)?, 255]),
            8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
            _ => None,
        }
    }
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_named_and_hex() {
        assert_eq!(SubtitleStyle::parse_color("yellow"), Some([255, 204, 0, 255]));
        assert_eq!(SubtitleStyle::parse_color("#FFCC00"), Some([255, 204, 0, 255]));
        assert_eq!(SubtitleStyle::parse_color("ffcc00"), Some([255, 204, 0, 255]));
        assert_eq!(SubtitleStyle::parse_color("#f00"), Some([255, 0, 0, 255]));
        assert_eq!(
            SubtitleStyle::parse_color("#80808080"),
            Some([128, 128, 128, 128])
        );
        assert_eq!(SubtitleStyle::parse_color("not-a-colour"), None);
        assert_eq!(SubtitleStyle::parse_color("#12"), None);
    }

    #[test]
    fn sanitise_clamps_size_only() {
        assert_eq!(SubtitleStyle { size_scale: 9.0, ..Default::default() }.sanitised().size_scale, 3.0);
        assert_eq!(SubtitleStyle { size_scale: 0.1, ..Default::default() }.sanitised().size_scale, 0.5);
        assert_eq!(SubtitleStyle { size_scale: f32::NAN, ..Default::default() }.sanitised().size_scale, 1.0);
    }
}
