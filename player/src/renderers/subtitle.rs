//! WebVTT subtitle overlay rendered via wgpu.
//!
//! Phase 1 scope: plain white text with a dark drop shadow, bottom-center,
//! fixed proportional size. No styling, no positioning, no language
//! mixing — just makes cues readable.
//!
//! Pipeline:
//!   1. `queue_cues` — text_play task pushes parsed cues here as they
//!      arrive; we keep them sorted by start time.
//!   2. `set_pts_ms` — av_sync's video loop sets the current playback
//!      PTS just before drawing. The overlay picks the cue that's
//!      active right now and rasterizes it (cached: same text + same
//!      target width = same texture).
//!   3. `draw_into` — called from VideoRenderer's render path after the
//!      main video draw. Issues one textured-quad draw against the
//!      already-bound surface target.
//!
//! Font rasterization is on the CPU via `fontdue`. No glyph atlas: a
//! whole-cue bitmap is generated once per active cue and reused until
//! the cue expires. Cues are short (~2-5s) so this is cheaper than
//! atlas bookkeeping for our use case.

use std::sync::{Arc, Mutex};

use wgpu::util::DeviceExt;

use crate::parsers::vtt::VttCue;

const SHADER_WGSL: &str = r#"
struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
};

struct Quad {
    /// xy = NDC center, zw = NDC half-extent.
    transform: vec4<f32>,
};

@group(0) @binding(0) var t_tex: texture_2d<f32>;
@group(0) @binding(1) var s_tex: sampler;
@group(0) @binding(2) var<uniform> quad: Quad;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VertexOut {
    // Unit quad in [-1, 1] × [-1, 1], two triangles.
    var pos = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var uv = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );
    let p = pos[vi];
    var out: VertexOut;
    out.position = vec4<f32>(
        quad.transform.x + p.x * quad.transform.z,
        quad.transform.y + p.y * quad.transform.w,
        0.0, 1.0,
    );
    out.tex_coords = uv[vi];
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(t_tex, s_tex, in.tex_coords);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadUniform {
    /// xy = NDC center, zw = NDC half-extent
    transform: [f32; 4],
}

struct CachedCue {
    /// Identity = (text, target_pixel_width). When either changes we
    /// re-rasterize.
    text: String,
    target_w: u32,
    bitmap_w: u32,
    bitmap_h: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

pub struct SubtitleOverlay {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface_format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buffer: wgpu::Buffer,

    inner: Mutex<Inner>,
}

struct Inner {
    /// Sorted by start_ms ascending. Old cues are pruned when the
    /// current PTS passes their end. Bounded to a few thousand to
    /// avoid pathological memory growth on hours-long streams.
    cues: Vec<VttCue>,
    /// Current playback PTS in ms, updated by the render path before
    /// each draw call.
    current_pts_ms: i64,
    /// fontdue::Font held behind RwLock-ish so set_font can swap it
    /// at any time. None when no font has been provided — render is a
    /// no-op until then.
    font: Option<fontdue::Font>,
    /// Cached rasterized cue. Invalidated when the active cue's text
    /// changes or the target width drifts more than 5% from the
    /// cached size.
    cached: Option<CachedCue>,
}

impl SubtitleOverlay {
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        surface_format: wgpu::TextureFormat,
    ) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("subtitle_bind_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("subtitle_shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("subtitle_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("subtitle_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    // Premultiplied alpha so the cue blends naturally
                    // over arbitrary video content.
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("subtitle_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("subtitle_uniform"),
            contents: bytemuck::cast_slice(&[QuadUniform {
                transform: [0.0, 0.0, 0.0, 0.0],
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        SubtitleOverlay {
            device,
            queue,
            surface_format,
            pipeline,
            bind_group_layout,
            sampler,
            uniform_buffer,
            inner: Mutex::new(Inner {
                cues: Vec::new(),
                current_pts_ms: 0,
                font: None,
                cached: None,
            }),
        }
    }

    /// Install a TTF/OTF font for cue rasterization. Until this is
    /// called, the overlay parses + tracks cues but draws nothing.
    /// Replacing the font invalidates any cached rasterization.
    pub fn set_font(&self, bytes: Vec<u8>) -> Result<(), String> {
        let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
            .map_err(|e| e.to_string())?;
        let mut inner = self.inner.lock().unwrap();
        inner.font = Some(font);
        inner.cached = None;
        Ok(())
    }

    /// Push new cues into the active list. text_play sends a batch per
    /// segment; raw single-file delivery sends the whole list once.
    pub fn queue_cues(&self, cues: Vec<VttCue>) {
        let mut inner = self.inner.lock().unwrap();
        for c in cues {
            inner.cues.push(c);
        }
        inner.cues.sort_by_key(|c| c.start_ms);
        // Cap memory: keep at most ~5000 cues. 2h movie at 1 cue/2s =
        // 3600 cues, so plenty of headroom for normal content.
        const MAX_CUES: usize = 5000;
        if inner.cues.len() > MAX_CUES {
            let excess = inner.cues.len() - MAX_CUES;
            inner.cues.drain(0..excess);
        }
    }

    /// Drop everything — called when the consumer switches subtitle
    /// track or disables subtitles.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.cues.clear();
        inner.cached = None;
    }

    /// Called by the video sync loop before each render. Drives both
    /// the active-cue picker and stale-cue eviction.
    pub fn set_pts_ms(&self, pts_ms: i64) {
        let mut inner = self.inner.lock().unwrap();
        inner.current_pts_ms = pts_ms;
        // Drop cues that ended more than ~5s ago — small slack so a
        // brief backward seek still picks them up.
        let cutoff = pts_ms - 5_000;
        if let Some(idx) = inner.cues.iter().position(|c| c.end_ms > cutoff) {
            if idx > 0 {
                inner.cues.drain(0..idx);
            }
        }
    }

    /// Issue the draw into a caller-owned render pass. The caller has
    /// already attached the surface color target; we just emit one
    /// triangle-list draw at the bottom-center of the viewport.
    ///
    /// `target_w`/`target_h` are pixel dimensions of the surface so we
    /// can size the rasterization to match.
    pub fn draw_into(
        &self,
        render_pass: &mut wgpu::RenderPass<'_>,
        target_w: u32,
        target_h: u32,
    ) {
        let mut inner = self.inner.lock().unwrap();
        let pts = inner.current_pts_ms;
        let active = inner
            .cues
            .iter()
            .find(|c| c.is_active(pts))
            .cloned();
        let active = match active {
            Some(c) => c,
            None => return,
        };
        if inner.font.is_none() {
            return;
        }

        // Rasterize or reuse cached bitmap. Cache hit when text and
        // target width match within 5%.
        let rebuild = match &inner.cached {
            Some(c) => {
                c.text != active.text
                    || (c.target_w as i64 - target_w as i64).abs()
                        > (target_w as i64 / 20).max(8)
            }
            None => true,
        };
        if rebuild {
            let font = inner.font.as_ref().unwrap().clone();
            let rasterized = rasterize_cue(&font, &active.text, target_w, target_h);
            if let Some((bitmap_w, bitmap_h, rgba)) = rasterized {
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("subtitle_cue_texture"),
                    size: wgpu::Extent3d {
                        width: bitmap_w,
                        height: bitmap_h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &rgba,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(bitmap_w * 4),
                        rows_per_image: Some(bitmap_h),
                    },
                    wgpu::Extent3d {
                        width: bitmap_w,
                        height: bitmap_h,
                        depth_or_array_layers: 1,
                    },
                );
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                inner.cached = Some(CachedCue {
                    text: active.text.clone(),
                    target_w,
                    bitmap_w,
                    bitmap_h,
                    texture,
                    view,
                });
            } else {
                inner.cached = None;
                return;
            }
        }

        let cached = match inner.cached.as_ref() {
            Some(c) => c,
            None => return,
        };

        // Position: bottom-center with 7% safe area from the bottom and
        // proportional to bitmap aspect.
        let bmp_w = cached.bitmap_w as f32;
        let bmp_h = cached.bitmap_h as f32;
        let tw = target_w as f32;
        let th = target_h as f32;
        // Half-extent in NDC = (px/2) / (target/2) = px / target.
        let half_w = bmp_w / tw;
        let half_h = bmp_h / th;
        let center_x = 0.0; // horizontal center
        let center_y = -1.0 + half_h + (th * 0.07) / (th * 0.5); // 7% safe area

        let uniform = QuadUniform {
            transform: [center_x, center_y, half_w, half_h],
        };
        self.queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::cast_slice(&[uniform]),
        );

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("subtitle_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&cached.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
            ],
        });

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &bind_group, &[]);
        render_pass.draw(0..6, 0..1);
        // suppress unused-warning on surface_format
        let _ = self.surface_format;
    }
}

// ---------------------------------------------------------------------------
// Cue rasterization (CPU side, fontdue)
// ---------------------------------------------------------------------------

/// Lay out a cue's text into an RGBA8 bitmap. Returns (width, height,
/// pixels) or None when the text is empty or the layout doesn't fit.
///
/// Style is fixed Phase 1: white glyphs, 2px black drop-shadow offset
/// by 1px each direction, line break at `\n` and at word boundaries
/// when a single line would exceed 90% of the target width.
fn rasterize_cue(
    font: &fontdue::Font,
    text: &str,
    target_w: u32,
    target_h: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    if text.is_empty() {
        return None;
    }
    // Font size: ~5% of video height, floored at 18px so things stay
    // readable on tiny preview windows.
    let px_size = (target_h as f32 * 0.05).max(18.0).min(80.0);
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
        rasterize_line(font, line, px_size, x_start, y_start, bitmap_w, bitmap_h, &mut rgba);
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

/// Draw glyphs left-to-right starting at (x, y_baseline-ish). White
/// foreground, 2px black drop-shadow offset (1, 1).
fn rasterize_line(
    font: &fontdue::Font,
    line: &str,
    px_size: f32,
    x_start: i32,
    y_start: i32,
    bitmap_w: u32,
    bitmap_h: u32,
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
            [0, 0, 0],
            bitmap_w,
            bitmap_h,
            rgba,
        );
        // White foreground
        blit_coverage(
            &glyph_bitmap,
            metrics.width as i32,
            metrics.height as i32,
            gx,
            gy,
            [255, 255, 255],
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
    color: [u8; 3],
    bitmap_w: u32,
    bitmap_h: u32,
    rgba: &mut [u8],
) {
    let bw = bitmap_w as i32;
    let bh = bitmap_h as i32;
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
            let alpha = coverage[(gy * glyph_w + gx) as usize] as u32;
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
