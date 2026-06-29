//! Cue rasterization — CPU side, via `fontdue`.
//!
//! Lays a WebVTT cue's text out into an RGBA8 coverage bitmap (wrap, measure,
//! per-glyph blit with a drop shadow). Platform-agnostic: the wgpu overlay
//! uploads the result to a texture, and the Android GLES hook uploads the same
//! bytes itself. Extracted from `subtitle.rs` so the renderer (`overlay`) and
//! the rasterizer live in separate files (mirrors the `video` module split).

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

/// Lay out a cue's text into an RGBA8 bitmap. Returns (width, height,
/// pixels) or None when the text is empty or the layout doesn't fit.
///
/// Glyph fill, outline colour and size come from `style`; the layout is
/// still Phase 1 (a 1px drop-shadow offset each direction, line break at
/// `\n` and at word boundaries when a single line would exceed 90% of the
/// target width).
pub(super) fn rasterize_cue(
    font: &fontdue::Font,
    text: &str,
    target_w: u32,
    target_h: u32,
    style: &SubtitleStyle,
) -> Option<(u32, u32, Vec<u8>)> {
    if text.is_empty() {
        return None;
    }
    // Font size: ~5% of video height scaled by the user's size_scale,
    // clamped to a readable range. The lower 12px floor keeps the 0.5×
    // setting legible on tiny preview windows; the upper bound caps the
    // cue bitmap so a 3× setting on a 4K surface can't exceed texture
    // limits.
    let px_size = (target_h as f32 * 0.05 * style.size_scale).clamp(12.0, 160.0);
    let max_line_w = (target_w as f32 * 0.9) as i32;
    let line_height = (px_size * 1.25).ceil() as i32;
    let shadow = 2i32;

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

    let bitmap_w = (max_width + shadow * 2).max(8) as u32;
    let bitmap_h = (line_height * layout_lines.len() as i32 + shadow * 2).max(8) as u32;

    let mut rgba = vec![0u8; (bitmap_w * bitmap_h * 4) as usize];

    // Second pass: rasterize each line centered horizontally in the bitmap.
    for (idx, line) in layout_lines.iter().enumerate() {
        let line_w = line_widths[idx];
        let x_start = (bitmap_w as i32 - line_w) / 2;
        let y_start = idx as i32 * line_height + shadow;
        rasterize_line(
            font, line, px_size, x_start, y_start, bitmap_w, bitmap_h,
            style.text_color, style.outline_color, &mut rgba,
        );
    }
    Some((bitmap_w, bitmap_h, rgba))
}

fn wrap_line(
    font: &fontdue::Font,
    line: &str,
    px_size: f32,
    max_w: i32,
    out: &mut Vec<String>,
) {
    if measure_text(font, line, px_size) <= max_w {
        out.push(line.to_string());
        return;
    }
    let mut current = String::new();
    for word in line.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{} {}", current, word)
        };
        if measure_text(font, &candidate, px_size) <= max_w {
            current = candidate;
        } else {
            if !current.is_empty() {
                out.push(current.clone());
            }
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
}

fn measure_text(font: &fontdue::Font, line: &str, px_size: f32) -> i32 {
    let mut total = 0.0f32;
    for ch in line.chars() {
        let metrics = font.metrics(ch, px_size);
        total += metrics.advance_width;
    }
    total.ceil() as i32
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
