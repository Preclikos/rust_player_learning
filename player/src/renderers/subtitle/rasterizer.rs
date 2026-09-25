//! Cue rasterization — CPU side, via `fontdue`.
//!
//! Lays a WebVTT cue's text out into an RGBA8 coverage bitmap (wrap, measure,
//! per-glyph blit with a drop shadow). Platform-agnostic: the wgpu overlay
//! uploads the result to a texture, and the Android GLES hook uploads the same
//! bytes itself. Extracted from `subtitle.rs` so the renderer (`overlay`) and
//! the rasterizer live in separate files (mirrors the `video` module split).

use crate::parsers::vtt::{CueLayout, TextAlign};
use crate::SubtitleStyle;

/// Default font baked into the binary: DejaVu Sans (Bitstream Vera +
/// public-domain changes — redistributable, see assets/fonts/LICENSE).
/// Chosen over the platform default because narrow system fonts (Android's
/// Roboto-Regular in particular) lack the symbols that show up in real
/// subtitles — most notably the music note ♪ (U+266A) used for song lyrics,
/// plus dashes, smart quotes and full Latin diacritics. Without coverage
/// those render as the `.notdef` tofu box. DejaVu covers all of them, so
/// the overlay is readable out of the box; `set_font` still lets a host
/// override it.
const DEFAULT_FONT: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");

/// Parse the embedded default font. Infallible in practice (the bytes are
/// compiled in and known-good); returns `None` only if a future font swap
/// breaks it, in which case the overlay degrades to drawing nothing rather
/// than panicking the render thread.
pub(super) fn default_font() -> Option<fontdue::Font> {
    match fontdue::Font::from_bytes(DEFAULT_FONT, fontdue::FontSettings::default()) {
        Ok(f) => Some(f),
        Err(e) => {
            log::error!("[subs] embedded default font failed to parse: {}", e);
            None
        }
    }
}

/// A rasterized cue: the bitmap plus the line height the vertical
/// `line:N` placement counts in.
pub(super) struct CueRaster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Height of one text line in the bitmap (ExoPlayer's
    /// `firstLineHeight` in `SubtitlePainter`).
    pub line_height_px: u32,
}

/// Lay out a cue's text into an RGBA8 bitmap sized for a parent box of
/// `parent_w`×`parent_h` px (the picture rectangle minus insets — see
/// `CueParent`). `None` when the text is empty or lays out to nothing.
///
/// Sizing follows ExoPlayer's `SubtitleView`/`SubtitlePainter`: text size
/// is `TEXT_SIZE_FRACTION` of the parent height (times the style's
/// `size_scale`), the box gets `INNER_PADDING_RATIO` × text size of
/// horizontal padding, the wrap width is the padded parent width times
/// the cue's `size:`, and lines are aligned inside the box by `align:`.
/// Glyph fill and outline colour come from `style`; the outline is a 1px
/// drop shadow.
pub(super) fn rasterize_cue(
    font: &fontdue::Font,
    text: &str,
    layout: &CueLayout,
    parent_w: u32,
    parent_h: u32,
    style: &SubtitleStyle,
) -> Option<CueRaster> {
    if text.is_empty() {
        return None;
    }
    // The 12px floor keeps the 0.5× setting legible on tiny preview
    // windows; the cap keeps a 3× setting on a 4K surface inside texture
    // limits.
    let px_size = (parent_h as f32 * super::TEXT_SIZE_FRACTION * style.size_scale).clamp(12.0, 160.0);
    // SubtitlePainter: textPaddingX = (int)(textSize * INNER_PADDING_RATIO + 0.5)
    let shadow = 2i32;
    let pad_x = ((px_size * super::INNER_PADDING_RATIO + 0.5) as i32).max(shadow);
    // availableWidth = parentWidth - 2*textPaddingX, then × cue size.
    let mut available_w = parent_w as i32 - 2 * pad_x;
    if layout.size < 1.0 {
        available_w = (available_w as f32 * layout.size) as i32;
    }
    let max_line_w = available_w.max(1);
    let line_height = (px_size * 1.25).ceil() as i32;

    // Wrap each input line, then concatenate into a flat list of layout lines.
    let mut layout_lines: Vec<String> = Vec::new();
    for raw_line in text.lines() {
        wrap_line(font, raw_line, px_size, max_line_w, &mut layout_lines);
    }
    if layout_lines.is_empty() {
        return None;
    }

    // First pass: measure each line.
    let mut line_widths: Vec<i32> = Vec::with_capacity(layout_lines.len());
    let mut max_width = 0i32;
    for line in &layout_lines {
        let w = measure_text(font, line, px_size);
        line_widths.push(w);
        if w > max_width {
            max_width = w;
        }
    }

    // SubtitlePainter: textWidth = widest line + 2*textPaddingX. The
    // padding also gives the drop shadow room on the right/bottom.
    let bitmap_w = (max_width + pad_x * 2).max(8) as u32;
    let bitmap_h = (line_height * layout_lines.len() as i32 + shadow * 2).max(8) as u32;

    let mut rgba = vec![0u8; (bitmap_w * bitmap_h * 4) as usize];

    // Second pass: rasterize each line, aligned inside the box by `align:`
    // (start/end have no bidi layout behind them → left/right).
    for (idx, line) in layout_lines.iter().enumerate() {
        let line_w = line_widths[idx];
        let x_start = match layout.align {
            TextAlign::Left | TextAlign::Start => pad_x,
            TextAlign::Right | TextAlign::End => bitmap_w as i32 - pad_x - line_w,
            TextAlign::Center => (bitmap_w as i32 - line_w) / 2,
        };
        let y_start = idx as i32 * line_height + shadow;
        rasterize_line(
            font, line, px_size, x_start, y_start, bitmap_w, bitmap_h,
            style.text_color, style.outline_color, &mut rgba,
        );
    }
    Some(CueRaster {
        width: bitmap_w,
        height: bitmap_h,
        rgba,
        line_height_px: line_height as u32,
    })
}

/// Greedy word wrap at `max_w`. Widths accumulate as words are appended:
/// the previous version rebuilt the candidate line with `format!` and
/// re-measured it from the first character for every word, so a long cue
/// measured the same prefix over and over (quadratic in words, plus one
/// allocation each).
fn wrap_line(font: &fontdue::Font, line: &str, px_size: f32, max_w: i32, out: &mut Vec<String>) {
    let max_w = max_w as f32;
    if measure_width(font, line, px_size) <= max_w {
        // Common case — one line, kept verbatim so its original spacing
        // survives (the greedy path below collapses whitespace runs).
        out.push(line.to_string());
        return;
    }
    let space_w = font.metrics(' ', px_size).advance_width;
    let mut current = String::new();
    let mut current_w = 0.0f32;
    for word in line.split_whitespace() {
        let word_w = measure_width(font, word, px_size);
        if current.is_empty() {
            current.push_str(word);
            current_w = word_w;
            continue;
        }
        let candidate_w = current_w + space_w + word_w;
        if candidate_w <= max_w {
            current.push(' ');
            current.push_str(word);
            current_w = candidate_w;
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
            current_w = word_w;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
}

/// Advance width of `line` in pixels. Unrounded — callers that accumulate
/// widths must not compound a per-word rounding error.
fn measure_width(font: &fontdue::Font, line: &str, px_size: f32) -> f32 {
    line.chars()
        .map(|ch| font.metrics(ch, px_size).advance_width)
        .sum()
}

fn measure_text(font: &fontdue::Font, line: &str, px_size: f32) -> i32 {
    measure_width(font, line, px_size).ceil() as i32
}

/// Draw glyphs left-to-right starting at (x, y_baseline-ish). `text_color`
/// is the fill, `outline_color` the drop-shadow drawn first at a (+1, +1)
/// offset; both are RGBA with the alpha multiplying glyph coverage.
#[allow(clippy::too_many_arguments)]
fn rasterize_line(
    font: &fontdue::Font,
    line: &str,
    px_size: f32,
    x_start: i32,
    y_start: i32,
    bitmap_w: u32,
    bitmap_h: u32,
    text_color: [u8; 4],
    outline_color: [u8; 4],
    rgba: &mut [u8],
) {
    let baseline = y_start + (px_size * 0.9) as i32;
    let mut pen_x = x_start as f32;
    for ch in line.chars() {
        let (metrics, glyph_bitmap) = font.rasterize(ch, px_size);
        let gx = pen_x.round() as i32 + metrics.xmin;
        let gy = baseline - metrics.height as i32 - metrics.ymin;
        // Drop shadow first (offset +1, +1)
        blit_coverage(
            &glyph_bitmap,
            metrics.width as i32,
            metrics.height as i32,
            gx + 1,
            gy + 1,
            outline_color,
            bitmap_w,
            bitmap_h,
            rgba,
        );
        // Foreground fill
        blit_coverage(
            &glyph_bitmap,
            metrics.width as i32,
            metrics.height as i32,
            gx,
            gy,
            text_color,
            bitmap_w,
            bitmap_h,
            rgba,
        );
        pen_x += metrics.advance_width;
    }
}

/// Blit an alpha-coverage glyph bitmap with a flat color over an RGBA8
/// buffer using premultiplied-alpha "over" composition.
#[allow(clippy::too_many_arguments)]
fn blit_coverage(
    coverage: &[u8],
    glyph_w: i32,
    glyph_h: i32,
    dst_x: i32,
    dst_y: i32,
    color: [u8; 4],
    bitmap_w: u32,
    bitmap_h: u32,
    rgba: &mut [u8],
) {
    let bw = bitmap_w as i32;
    let bh = bitmap_h as i32;
    let color_a = color[3] as u32;
    for gy in 0..glyph_h {
        let py = dst_y + gy;
        if py < 0 || py >= bh {
            continue;
        }
        for gx in 0..glyph_w {
            let px = dst_x + gx;
            if px < 0 || px >= bw {
                continue;
            }
            // Effective alpha = glyph coverage scaled by the colour's own
            // alpha, so a translucent text/outline colour fades the glyph.
            let alpha = (coverage[(gy * glyph_w + gx) as usize] as u32 * color_a) / 255;
            if alpha == 0 {
                continue;
            }
            let idx = ((py * bw + px) as usize) * 4;
            // premultiplied "over": dst = src + dst*(1 - a)
            // here src = (color * a / 255), premultiplied form.
            let inv = 255 - alpha;
            let blend = |dst: u8, src: u8| -> u8 {
                let s = (src as u32 * alpha) / 255;
                let d = (dst as u32 * inv) / 255;
                (s + d).min(255) as u8
            };
            rgba[idx] = blend(rgba[idx], color[0]);
            rgba[idx + 1] = blend(rgba[idx + 1], color[1]);
            rgba[idx + 2] = blend(rgba[idx + 2], color[2]);
            // Alpha channel composites independently.
            let a_dst = rgba[idx + 3] as u32;
            let a_src = alpha;
            let a_out = a_src + (a_dst * inv) / 255;
            rgba[idx + 3] = a_out.min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> fontdue::Font {
        default_font().expect("embedded font")
    }

    /// First and last bitmap column holding any ink.
    fn ink_span(r: &CueRaster) -> (u32, u32) {
        let mut first = u32::MAX;
        let mut last = 0;
        for y in 0..r.height {
            for x in 0..r.width {
                if r.rgba[((y * r.width + x) * 4 + 3) as usize] > 0 {
                    first = first.min(x);
                    last = last.max(x);
                }
            }
        }
        (first, last)
    }

    #[test]
    fn size_setting_narrows_the_wrap_width() {
        let f = font();
        let text = "a fairly long subtitle line that will certainly need wrapping somewhere";
        let full = rasterize_cue(&f, text, &CueLayout::DEFAULT, 1280, 720, &SubtitleStyle::DEFAULT).unwrap();
        let narrow = rasterize_cue(&f, text, &CueLayout::parse("size:40%"), 1280, 720, &SubtitleStyle::DEFAULT).unwrap();
        assert!(narrow.width < full.width, "{} !< {}", narrow.width, full.width);
        assert!(narrow.height > full.height, "narrower box must wrap onto more lines");
        // Text size follows the parent height, so the line height does too.
        let small = rasterize_cue(&f, "x", &CueLayout::DEFAULT, 640, 360, &SubtitleStyle::DEFAULT).unwrap();
        assert!(small.line_height_px < full.line_height_px);
    }

    #[test]
    fn align_moves_short_lines_inside_the_box() {
        let f = font();
        let text = "a much longer first line of text\nshort";
        let left = rasterize_cue(&f, text, &CueLayout::parse("align:left"), 1280, 720, &SubtitleStyle::DEFAULT).unwrap();
        let right = rasterize_cue(&f, text, &CueLayout::parse("align:right"), 1280, 720, &SubtitleStyle::DEFAULT).unwrap();
        let center = rasterize_cue(&f, text, &CueLayout::DEFAULT, 1280, 720, &SubtitleStyle::DEFAULT).unwrap();
        // Same widest line → same bitmap for all three; only the short
        // second line moves. Compare where its ink sits on its own row band.
        assert_eq!(left.width, right.width);
        let row = |r: &CueRaster| {
            let y0 = r.line_height_px + 2;
            let mut first = u32::MAX;
            let mut last = 0;
            for y in y0..r.height {
                for x in 0..r.width {
                    if r.rgba[((y * r.width + x) * 4 + 3) as usize] > 0 {
                        first = first.min(x);
                        last = last.max(x);
                    }
                }
            }
            (first, last)
        };
        let (l0, l1) = row(&left);
        let (r0, r1) = row(&right);
        let (c0, c1) = row(&center);
        assert!(l0 < c0 && c0 < r0, "left {l0} center {c0} right {r0}");
        assert!(l1 < c1 && c1 < r1, "left {l1} center {c1} right {r1}");
        // And the whole bitmap still has ink within its padding.
        let (a, b) = ink_span(&left);
        assert!(a < left.width && b < left.width);
    }
}
