//! WebVTT subtitle overlay rendered via wgpu.
//!
//! Text is plain (white with a dark drop shadow by default, `SubtitleStyle`
//! for colours/size); placement follows the cue's WebVTT settings
//! (`line:`, `position:`, `align:`, `size:`) with ExoPlayer's geometry —
//! see [`CueParent`] and [`place_cue`], which are a port of media3's
//! `SubtitlePainter.setupTextLayout` so the same cue lands where an
//! ExoPlayer app shows it.
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
//!
//! That rasterization does NOT happen on the render thread. A dedicated
//! worker (`raster_worker`) keeps the cue under the playhead — and the
//! one after it — rasterized ahead of time; the render path only looks
//! the finished bitmap up and draws it. Rasterizing inline used to cost a
//! multi-megabyte allocation, the glyph raster and a full texture upload
//! at every cue change, all inside the frame, which dropped a frame each
//! time a new subtitle line appeared. Because the next cue is prefetched
//! as soon as the current one starts, steady-state playback never waits;
//! only a seek can land on a cue that isn't rasterized yet, and that
//! resolves within a frame or two.

use std::sync::{Arc, Mutex};

use wgpu::util::DeviceExt;

use crate::parsers::vtt::{Anchor, CueLayout, CueLine, VttCue};
use crate::{SubtitleAnchor, SubtitleStyle};

// CPU cue rasterization (fontdue) lives in its own file (mirrors `video`).
mod rasterizer;

// ---------------------------------------------------------------------------
// Geometry — a port of media3 `SubtitleView` / `SubtitlePainter` defaults so
// cues sit where ExoPlayer puts them. Keep the numbers in sync with upstream:
// libraries/ui/src/main/java/androidx/media3/ui/SubtitleView.java and
// SubtitlePainter.java.
// ---------------------------------------------------------------------------

/// `SubtitleView.DEFAULT_TEXT_SIZE_FRACTION`: text size as a fraction of the
/// parent (padded picture) height.
pub(crate) const TEXT_SIZE_FRACTION: f32 = 0.0533;
/// `SubtitleView.DEFAULT_BOTTOM_PADDING_FRACTION`: how far above the parent's
/// bottom edge a cue without `line:` sits, as a fraction of the parent height.
pub(crate) const BOTTOM_PADDING_FRACTION: f32 = 0.08;
/// `SubtitlePainter.INNER_PADDING_RATIO`: horizontal box padding relative to
/// the text size.
pub(crate) const INNER_PADDING_RATIO: f32 = 0.125;

/// The rectangle cues are laid out in, in target (surface) pixels.
///
/// [`SubtitleAnchor::Screen`] (default): the whole surface. A cue without
/// `line:` then sits `BOTTOM_PADDING_FRACTION` of the surface height above
/// the surface's bottom edge — in the letterbox bar on a widescreen film.
///
/// [`SubtitleAnchor::Picture`]: ExoPlayer's geometry. Its `SubtitleView`
/// lives inside `PlayerView`'s `AspectRatioFrameLayout`, so the parent rect
/// is the aspect-fitted picture — a 2.39:1 film on a 16:9 screen gets its
/// cues inside the picture, `BOTTOM_PADDING_FRACTION` of the *picture*
/// height above the picture's bottom edge.
///
/// Either way the host's bottom inset (`Player::set_subtitle_safe_insets`:
/// system bars, TV overscan) plays the role of the view's bottom padding:
/// it raises the parent's bottom edge, and — as in `CanvasSubtitleOutput`,
/// which resolves the text size against `viewHeightMinusPadding` — shrinks
/// the height the text size is derived from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CueParent {
    pub target_w: u32,
    pub target_h: u32,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    /// Height the TEXT SIZE and the auto bottom padding are derived from:
    /// always the aspect-fitted picture, whichever anchor is in use.
    ///
    /// The anchor decides where a cue sits; it has no business deciding how
    /// big the glyphs are. Deriving the size from the layout box instead made
    /// `Screen` scale the text with the surface, so a letterboxed film got
    /// roughly twice ExoPlayer's size — 57px rather than 29px on a
    /// 958×1064 window — and a three-line cue then grew taller than the
    /// letterbox bar and climbed back into the picture, which is the one
    /// thing placing it in the bar was meant to avoid.
    picture_h: f32,
}

impl CueParent {
    /// From the aspect-fit scale the video quad is drawn with (`scale_x`,
    /// `scale_y` ∈ (0, 1], the letterbox factors) — what the GLES hook has.
    pub fn from_scale(
        target_w: u32,
        target_h: u32,
        scale_x: f32,
        scale_y: f32,
        bottom_inset_px: u32,
        anchor: SubtitleAnchor,
    ) -> Self {
        let tw = target_w as f32;
        let th = target_h as f32;
        let picture_scale_y = scale_y;
        let (scale_x, scale_y) = match anchor {
            SubtitleAnchor::Screen => (1.0, 1.0),
            SubtitleAnchor::Picture => (scale_x, scale_y),
        };
        let sane = |s: f32| if s.is_finite() && s > 0.0 { s.min(1.0) } else { 1.0 };
        // The picture's own height, taken before the anchor may have flattened
        // the scale to 1.0 — see `picture_h`.
        let picture_h = (th * sane(picture_scale_y)).round();
        let pw = (tw * sane(scale_x)).round();
        let ph = (th * sane(scale_y)).round();
        let left = ((tw - pw) / 2.0).floor();
        let top = ((th - ph) / 2.0).floor();
        // Clamp the inset like the old path did so a bogus inset can't push
        // the whole layout box off the top of the screen.
        let inset = (bottom_inset_px as f32).min(th * 0.45);
        let bottom = (top + ph).min(th - inset).max(top + 1.0);
        Self {
            target_w,
            target_h,
            left,
            top,
            right: left + pw,
            bottom,
            // The inset shrinks what the size derives from, as it does for the
            // box itself (media3 resolves text size against
            // `viewHeightMinusPadding`).
            picture_h: (picture_h - inset).max(1.0),
        }
    }

    /// From the content (frame) size; 0×0 = unknown → the whole target.
    pub fn fit(
        target_w: u32,
        target_h: u32,
        content_w: u32,
        content_h: u32,
        bottom_inset_px: u32,
        anchor: SubtitleAnchor,
    ) -> Self {
        let (scale_x, scale_y) = if content_w > 0 && content_h > 0 && target_w > 0 && target_h > 0 {
            let wa = target_w as f32 / target_h as f32;
            let fa = content_w as f32 / content_h as f32;
            if fa > wa {
                (1.0, wa / fa)
            } else {
                (fa / wa, 1.0)
            }
        } else {
            (1.0, 1.0)
        };
        Self::from_scale(target_w, target_h, scale_x, scale_y, bottom_inset_px, anchor)
    }

    /// Layout box size the rasterizer works in.
    pub fn width(&self) -> u32 {
        (self.right - self.left).round().max(1.0) as u32
    }

    pub fn height(&self) -> u32 {
        (self.bottom - self.top).round().max(1.0) as u32
    }

    /// Height the text size and the auto bottom padding scale with. See
    /// [`CueParent::picture_h`].
    pub fn type_height(&self) -> u32 {
        self.picture_h.round().max(1.0) as u32
    }
}

/// Top-left corner (target px) of a `bmp_w`×`bmp_h` cue bitmap inside
/// `parent`, per the cue's settings. Line by line
/// `SubtitlePainter.setupTextLayout`:
///
/// * horizontal: anchor at `position` × parent width, the box's
///   start/middle/end edge on it, then clamped into the parent;
/// * `line:N%`: anchor at N × parent height, the box's start/middle/end
///   edge on it (`line_anchor`);
/// * `line:N` (integer): `N ≥ 0` → N line heights below the top,
///   `N < 0` → the box's bottom `(N+1)` line heights above the bottom
///   (`-1` = flush with it);
/// * no `line:` → `BOTTOM_PADDING_FRACTION` of the parent height above the
///   bottom;
/// * finally clamped into the parent (bottom first, then top).
pub fn place_cue(
    bmp_w: u32,
    bmp_h: u32,
    line_height_px: u32,
    layout: &CueLayout,
    parent: &CueParent,
) -> (f32, f32) {
    let w = bmp_w as f32;
    let h = bmp_h as f32;
    let pw = parent.right - parent.left;
    let ph = parent.bottom - parent.top;

    let anchor_x = (pw * layout.position_or_derived()).round() + parent.left;
    let x = match layout.position_anchor_or_derived() {
        Anchor::Start => anchor_x,
        Anchor::Middle => anchor_x - w / 2.0,
        Anchor::End => anchor_x - w,
    };
    // media3 only clamps the left edge and clips the right; keeping the
    // whole box visible is the friendlier reading of the same intent.
    let x = x.min(parent.right - w).max(parent.left);

    let lh = line_height_px as f32;
    let mut y = match layout.line {
        CueLine::Fraction(f) => {
            let anchor_y = (ph * f).round() + parent.top;
            match layout.line_anchor {
                Anchor::Start => anchor_y,
                Anchor::Middle => anchor_y - h / 2.0,
                Anchor::End => anchor_y - h,
            }
        }
        CueLine::Number(n) if n >= 0 => (n as f32 * lh).round() + parent.top,
        CueLine::Number(n) => ((n + 1) as f32 * lh).round() + parent.bottom - h,
        // Padding off the picture height too: 8 % of a tall surface is a wide
        // empty band that pushes the block up towards the picture, which is
        // the opposite of what anchoring to the screen is for.
        CueLine::Auto => {
            parent.bottom - h - (parent.type_height() as f32 * BOTTOM_PADDING_FRACTION).floor()
        }
    };
    if y + h > parent.bottom {
        y = parent.bottom - h;
    }
    if y < parent.top {
        y = parent.top;
    }
    (x, y)
}

/// The quad both draw paths share: `[center_x, center_y, half_w, half_h]`
/// in NDC (y up) for `bitmap` placed by [`place_cue`] on `parent`'s target.
pub fn cue_quad(bitmap: &SubtitleBitmap, parent: &CueParent) -> [f32; 4] {
    let (x, y) = place_cue(bitmap.width, bitmap.height, bitmap.line_height_px, &bitmap.layout, parent);
    let tw = parent.target_w as f32;
    let th = parent.target_h as f32;
    let bw = bitmap.width as f32;
    let bh = bitmap.height as f32;
    // Half-extent in NDC = (px/2) / (target/2) = px / target.
    [
        (x + bw / 2.0) / tw * 2.0 - 1.0,
        1.0 - (y + bh / 2.0) / th * 2.0,
        bw / tw,
        bh / th,
    ]
}

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

/// GPU-side mirror of the bitmap currently on screen, owned by the render
/// thread. Rebuilt only when the cue actually changes: a new cue with the
/// same pixel dimensions reuses the texture (and therefore the view and
/// the bind group) and costs one `write_texture`. Previously every frame
/// allocated a fresh bind group and re-wrote the uniform even when nothing
/// had moved.
struct GpuCue {
    /// `SubtitleBitmap::generation` of the content currently uploaded.
    generation: u64,
    bitmap_w: u32,
    bitmap_h: u32,
    /// Held to keep the underlying GPU resource alive for as long as `view`
    /// is referenced. Never read directly — the `view` does all the work.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    /// Same: kept alive because `bind_group` refers to it.
    #[allow(dead_code)]
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    /// Last value written to `uniform_buffer`, so a static cue doesn't
    /// re-upload the same four floats every frame.
    transform: [f32; 4],
}

pub struct SubtitleOverlay {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface_format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buffer: wgpu::Buffer,

    /// Cue state + rasterizer output, shared with the worker thread.
    shared: Arc<Shared>,
    /// Render-thread-owned GPU mirror. Never touched by the worker.
    gpu: Mutex<Option<GpuCue>>,
    /// Joined in `Drop`. `None` only if the thread failed to spawn, in
    /// which case cues simply never rasterize (rendering degrades to no
    /// subtitles rather than stalling the frame).
    worker: Option<std::thread::JoinHandle<()>>,
}

/// State the render path, the host-facing setters and the rasterizer
/// worker all touch. One mutex: every critical section is a handful of
/// integer comparisons, and the worker deliberately drops the lock while
/// it rasterizes.
struct Shared {
    inner: Mutex<Inner>,
    /// Signalled when `want_epoch` moves, i.e. the worker may have new
    /// work. Also signals shutdown.
    wake: std::sync::Condvar,
}

struct Inner {
    /// Sorted by start_ms ascending. Old cues are pruned when the
    /// current PTS passes their end. Bounded to a few thousand to
    /// avoid pathological memory growth on hours-long streams.
    cues: Vec<VttCue>,
    /// Current playback PTS in ms, updated by the render path before
    /// each draw call.
    current_pts_ms: i64,
    /// fontdue::Font held behind an `Arc` so the render path can take a
    /// reference out of the lock for the price of a refcount bump.
    /// `fontdue::Font` is `Clone`, but that clone deep-copies every
    /// glyph's outline geometry (~7 ms for DejaVu Sans on a desktop x86,
    /// several times that on a phone/TV SoC) — doing it per frame stalled
    /// the render thread for as long as a cue was on screen. `set_font`
    /// swaps the whole Arc. Initialised to the embedded DejaVu default so
    /// cues render without a host-supplied font; `None` only if that
    /// default somehow fails to parse, in which case render is a no-op.
    font: Option<Arc<fontdue::Font>>,
    /// Visual style (colours + size multiplier). Swapped by `set_style`;
    /// changing it drops the cached rasterization so the next draw rebuilds.
    style: SubtitleStyle,
    /// Rasterized cues the worker has finished, newest last. Holds the
    /// cue on screen plus the prefetched next one; `READY_SLOTS` gives it
    /// one slot of slack so an advance never evicts something still
    /// wanted. Both draw paths read from here and never rasterize.
    ready: Vec<std::sync::Arc<SubtitleBitmap>>,
    generation: u64,
    /// Layout box (`CueParent` width/height) the render path last drew
    /// at. The worker needs it to size a cue, so before the first draw
    /// (0×0) it has nothing to do.
    target_w: u32,
    target_h: u32,
    /// Picture height the text size derives from — see `CueParent::picture_h`.
    type_h: u32,
    /// Cue indices the worker was last asked to have ready — `[active,
    /// next]`. Kept so `set_pts_ms`, which runs every frame, can tell a
    /// PTS update that changes nothing from one that crosses a cue
    /// boundary, and only wake the worker for the latter.
    wanted: [Option<usize>; 2],
    /// Set by `Drop` to retire the worker.
    shutdown: bool,
    /// Longest cue duration currently in `cues`, in ms. Bounds how far
    /// back of the current PTS an active cue can start, which is what
    /// makes the binary-search window in `active_index` exact — see
    /// there. Only ever grows while cues are pushed (a stale-large value
    /// just widens the window, never hides a cue); reset by `clear`.
    max_cue_span_ms: i64,
}

impl Inner {
    /// Index of the cue active at `pts_ms`, or `None`.
    ///
    /// Same answer as `cues.iter().find(|c| c.is_active(pts))` — the first
    /// active cue in start order — but without the linear walk. That walk
    /// scanned every cue from the start of the list on *every frame*, so
    /// its cost grew with playback position (~5000 comparisons per frame
    /// near the end of a MAX_CUES-capped movie).
    ///
    /// `cues` is sorted by `start_ms`, so two binary searches bracket the
    /// only indices that can be active:
    ///   * `hi` — cues at or past it start after `pts`, so they're future.
    ///   * `lo` — cues before it start at or before `pts - max_cue_span_ms`,
    ///     so their end (≤ start + max span) is at or before `pts`: expired.
    ///
    /// Whatever is left in `lo..hi` is a handful of entries even for
    /// pathological overlapping-cue tracks.
    fn active_index(&self, pts_ms: i64) -> Option<usize> {
        let hi = self.cues.partition_point(|c| c.start_ms <= pts_ms);
        if hi == 0 {
            return None;
        }
        let lo = self
            .cues
            .partition_point(|c| c.start_ms <= pts_ms - self.max_cue_span_ms);
        self.cues[lo..hi]
            .iter()
            .position(|c| c.is_active(pts_ms))
            .map(|i| lo + i)
    }

    /// The cues the worker should keep rasterized: the one under the
    /// playhead and the one that starts next. Prefetching the second is
    /// what keeps a cue change off the render thread's critical path —
    /// by the time it becomes active its bitmap has been ready for
    /// seconds.
    fn wanted_indices(&self, pts_ms: i64) -> [Option<usize>; 2] {
        let active = self.active_index(pts_ms);
        let next = self.cues.partition_point(|c| c.start_ms <= pts_ms);
        let next = (next < self.cues.len()).then_some(next);
        // A cue that is both active and "next" can't happen (next starts
        // strictly after pts), so these never collide.
        [active, next]
    }

    /// Finished bitmap for `text` at roughly `target_w`, if the worker has
    /// produced one. The 5% width tolerance matches the old cache rule:
    /// a window drag resizes continuously and re-rasterizing on every
    /// pixel would be pointless churn.
    fn ready_for(
        &self,
        text: &str,
        layout: &CueLayout,
        target_w: u32,
    ) -> Option<&std::sync::Arc<SubtitleBitmap>> {
        self.ready
            .iter()
            .find(|b| b.text == text && b.layout == *layout && width_close(b.target_w, target_w))
    }

    /// Record the surface size the render path is drawing at. Returns
    /// true when it moved enough to invalidate what the worker produced.
    fn note_target(&mut self, target_w: u32, target_h: u32, type_h: u32) -> bool {
        self.type_h = type_h;
        if self.target_h == target_h && width_close(self.target_w, target_w) {
            // Keep the exact numbers current even inside the tolerance so
            // the drift is measured against what we last drew.
            self.target_w = target_w;
            return false;
        }
        self.target_w = target_w;
        self.target_h = target_h;
        self.ready.clear();
        true
    }
}

/// Rasterized-at width close enough to the drawn width to reuse. Mirrors
/// the tolerance the pre-worker cache used.
fn width_close(cached_w: u32, target_w: u32) -> bool {
    (cached_w as i64 - target_w as i64).abs() <= (target_w as i64 / 20).max(8)
}

/// Bitmaps kept around: the cue on screen, the prefetched next one, and
/// one slot of slack so producing the new prefetch can't evict either.
const READY_SLOTS: usize = 3;

/// A rasterized cue as plain pixels, for sinks that own their texture
/// upload (the Android GLES hook). The same `rasterize_cue` output the
/// wgpu path uploads — a libass backend would feed this exact shape.
pub struct SubtitleBitmap {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Monotonic content identity — changes whenever the visible bitmap
    /// changes. Lets callers cache uploads and detect updates cheaply.
    pub generation: u64,
    /// One text line's height, for `line:N` placement (see `place_cue`).
    pub line_height_px: u32,
    /// The cue's placement settings — `cue_quad` positions by them.
    pub layout: CueLayout,
    /// Identity for cache validation: same text + same settings at about
    /// the same parent width = same bitmap.
    text: String,
    target_w: u32,
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

        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                cues: Vec::new(),
                current_pts_ms: 0,
                font: rasterizer::default_font().map(Arc::new),
                style: SubtitleStyle::DEFAULT,
                ready: Vec::new(),
                generation: 0,
                target_w: 0,
                target_h: 0,
                type_h: 0,
                wanted: [None, None],
                shutdown: false,
                max_cue_span_ms: 0,
            }),
            wake: std::sync::Condvar::new(),
        });

        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("subtitle-raster".to_string())
                .spawn(move || raster_worker(shared))
                .map_err(|e| {
                    // No threads on wasm32: expected, the render path
                    // rasterizes inline (`rasterize_inline`). Anywhere else
                    // a failed spawn is a real problem worth shouting about.
                    if cfg!(target_arch = "wasm32") {
                        log::debug!("[subs] no rasterizer thread ({}); rasterizing inline", e);
                    } else {
                        log::error!("[subs] rasterizer thread failed to spawn: {}", e);
                    }
                })
                .ok()
        };

        SubtitleOverlay {
            device,
            queue,
            surface_format,
            pipeline,
            bind_group_layout,
            sampler,
            uniform_buffer,
            shared,
            gpu: Mutex::new(None),
            worker,
        }
    }

    /// Lock, mutate, wake the worker. Every setter that can change what
    /// needs rasterizing goes through here so none of them forgets the
    /// notify. Waking a worker that turns out to have nothing to do is
    /// harmless — it re-parks immediately.
    fn with_inner_notify<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let out = {
            let mut inner = self.shared.inner.lock().unwrap();
            f(&mut inner)
        };
        self.shared.wake.notify_one();
        out
    }

    /// Install a TTF/OTF font for cue rasterization, replacing the
    /// embedded DejaVu default. Invalidates any cached rasterization. On
    /// invalid bytes the previous font is kept and an Err is returned.
    pub fn set_font(&self, bytes: Vec<u8>) -> Result<(), String> {
        let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
            .map_err(|e| e.to_string())?;
        self.with_inner_notify(|inner| {
            inner.font = Some(Arc::new(font));
            inner.ready.clear();
        });
        Ok(())
    }

    /// The layout-box anchor of the current style — the draw paths build
    /// their `CueParent` with it.
    pub fn anchor(&self) -> SubtitleAnchor {
        self.shared.inner.lock().unwrap().style.anchor
    }

    /// Replace the visual style. Drops the cached cue bitmap so the next
    /// draw re-rasterizes with the new colours/size. Cheap; safe to call
    /// from any thread at any time.
    pub fn set_style(&self, style: SubtitleStyle) {
        self.with_inner_notify(|inner| {
            inner.style = style;
            inner.ready.clear();
        });
    }

    /// Push new cues into the active list. text_play sends a batch per
    /// segment; raw single-file delivery sends the whole list once.
    pub fn queue_cues(&self, cues: Vec<VttCue>) {
        self.with_inner_notify(|inner| {
            for c in cues {
                inner.max_cue_span_ms = inner.max_cue_span_ms.max(c.end_ms - c.start_ms);
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
            // Indices shifted; force the worker to recompute rather than
            // trust `wanted`.
            inner.wanted = [None, None];
        });
    }

    /// Drop everything — called when the consumer switches subtitle
    /// track or disables subtitles.
    pub fn clear(&self) {
        self.with_inner_notify(|inner| {
            inner.cues.clear();
            inner.max_cue_span_ms = 0;
            inner.ready.clear();
            inner.wanted = [None, None];
        });
    }

    /// GLES-hook variant of `draw_into`: the bitmap for the cue active at
    /// the current PTS, or `None` when there's nothing to show (no cue, or
    /// the worker hasn't caught up yet after a seek). `generation`
    /// identifies the content so the hook can skip redundant uploads; the
    /// hook places it with [`cue_quad`] on the same `parent`.
    ///
    /// Pure lookup — no rasterization, no allocation. Runs on the render
    /// thread for every frame.
    pub fn active_bitmap(&self, parent: &CueParent) -> Option<std::sync::Arc<SubtitleBitmap>> {
        let (target_w, target_h) = (parent.width(), parent.height());
        let type_h = parent.type_height();
        let (bitmap, resized) = {
            let mut inner = self.shared.inner.lock().unwrap();
            let resized = inner.note_target(target_w, target_h, type_h);
            let pts = inner.current_pts_ms;
            let bitmap = inner
                .active_index(pts)
                .and_then(|idx| {
                    // Borrow the text for the lookup; nothing is cloned
                    // unless we actually have a bitmap to hand back.
                    let cue = &inner.cues[idx];
                    inner.ready_for(cue.text.as_str(), &cue.layout, target_w)
                })
                .cloned();
            (bitmap, resized)
        };
        if resized {
            self.shared.wake.notify_one();
        }
        // A zero-size bitmap is the worker's record that the cue produced
        // no pixels (empty after layout). Kept in `ready` so it doesn't
        // retry forever; never drawn.
        bitmap.filter(|b| b.width > 0 && b.height > 0)
    }

    /// Called by the video sync loop before each render. Updates the
    /// "current PTS" the cue picker reads, nothing else.
    ///
    /// We do NOT evict cues here: time-based eviction made backward
    /// seeks lose subtitles permanently — once the user paused and
    /// rewound 10s to re-read a missed line, the cue had already been
    /// drained at the higher PTS and queue_cues never re-pushed it
    /// (text_play fetches each VTT segment once per playback). The
    /// `queue_cues` MAX_CUES cap is the only safety valve we need;
    /// for typical 2h content with 1 cue per ~3s we're well below it.
    pub fn set_pts_ms(&self, pts_ms: i64) {
        let wake = {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.current_pts_ms = pts_ms;
            // Called once per frame, so this must stay cheap: two binary
            // searches, and a notify only when the playhead actually moved
            // into a different cue (roughly once every few seconds).
            let wanted = inner.wanted_indices(pts_ms);
            if wanted == inner.wanted {
                false
            } else {
                inner.wanted = wanted;
                true
            }
        };
        if wake {
            self.shared.wake.notify_one();
        }
    }

    /// Issue the draw into a caller-owned render pass. The caller has
    /// already attached the surface color target; we just emit one
    /// textured-quad draw placed by [`cue_quad`] inside `parent`.
    ///
    /// `parent` is the layout box (picture rect minus insets) on the
    /// surface being drawn; its size is what the worker rasterizes at.
    ///
    /// Draws whatever the worker has ready for the active cue. Nothing
    /// ready (no cue, or a seek the worker hasn't caught up with) simply
    /// draws nothing this frame.
    pub fn draw_into(&self, render_pass: &mut wgpu::RenderPass<'_>, parent: &CueParent) {
        if parent.target_w == 0 || parent.target_h == 0 {
            return;
        }
        let (target_w, target_h) = (parent.width(), parent.height());
        let type_h = parent.type_height();
        // No rasterizer thread (the browser build, or a spawn failure):
        // bake the wanted cue right here. Cue changes are rare (seconds
        // apart) and one rasterization is a few ms, so paying it on the
        // render path beats having no subtitles at all.
        if self.worker.is_none() {
            self.rasterize_inline(target_w, target_h, type_h);
        }
        let (bitmap, resized) = {
            let mut inner = self.shared.inner.lock().unwrap();
            let resized = inner.note_target(target_w, target_h, type_h);
            let pts = inner.current_pts_ms;
            let bitmap = inner
                .active_index(pts)
                .and_then(|idx| {
                    let cue = &inner.cues[idx];
                    inner.ready_for(cue.text.as_str(), &cue.layout, target_w)
                })
                .cloned();
            (bitmap, resized)
        };
        if resized {
            self.shared.wake.notify_one();
        }
        let bitmap = match bitmap {
            // Zero-size = the worker found nothing to draw for this cue.
            Some(b) if b.width > 0 && b.height > 0 => b,
            _ => return,
        };

        // Same geometry as the GLES path: `cue_quad` is the one place that
        // turns cue settings + parent rect into a quad.
        let transform = cue_quad(&bitmap, parent);

        let mut gpu = self.gpu.lock().unwrap();
        // Upload only when the content changed. A texture of the same size
        // is reused, which also keeps the view and the bind group valid —
        // this used to allocate a bind group every single frame.
        let stale = match gpu.as_ref() {
            Some(g) => g.generation != bitmap.generation,
            None => true,
        };
        if stale {
            let reuse = gpu
                .as_ref()
                .is_some_and(|g| g.bitmap_w == bitmap.width && g.bitmap_h == bitmap.height);
            if !reuse {
                *gpu = Some(self.create_gpu_cue(bitmap.width, bitmap.height));
            }
            let g = gpu.as_mut().expect("just created");
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &g.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &bitmap.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bitmap.width * 4),
                    rows_per_image: Some(bitmap.height),
                },
                wgpu::Extent3d {
                    width: bitmap.width,
                    height: bitmap.height,
                    depth_or_array_layers: 1,
                },
            );
            g.generation = bitmap.generation;
        }

        let g = gpu.as_mut().expect("populated above");
        if g.transform != transform {
            self.queue.write_buffer(
                &self.uniform_buffer,
                0,
                bytemuck::cast_slice(&[QuadUniform { transform }]),
            );
            g.transform = transform;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &g.bind_group, &[]);
        render_pass.draw(0..6, 0..1);
        // suppress unused-warning on surface_format
        let _ = self.surface_format;
    }

    /// Allocate the texture/view/bind-group triple for a cue bitmap of the
    /// given size. Only called when the size changes.
    fn create_gpu_cue(&self, bitmap_w: u32, bitmap_h: u32) -> GpuCue {
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
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("subtitle_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
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
        GpuCue {
            // Never a real generation (the worker starts at 1), so the
            // first draw always uploads.
            generation: 0,
            bitmap_w,
            bitmap_h,
            texture,
            view,
            bind_group,
            transform: [f32::NAN; 4],
        }
    }
}

impl SubtitleOverlay {
    /// One `raster_worker` iteration, run synchronously by the render path
    /// when there is no worker thread. Bakes at most one cue per call — the
    /// active one first, the prefetch on the next frame.
    fn rasterize_inline(&self, target_w: u32, target_h: u32, type_h: u32) {
        let job = {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.note_target(target_w, target_h, type_h);
            next_job(&inner)
        };
        let Some(job) = job else { return };
        let rasterized = rasterizer::rasterize_cue(
            &job.font, &job.text, &job.layout, job.target_w, job.type_h, &job.style,
        );
        let mut inner = self.shared.inner.lock().unwrap();
        if inner.target_w != job.target_w || inner.target_h != job.target_h {
            return;
        }
        inner.generation += 1;
        let generation = inner.generation;
        let bitmap = finished_bitmap(job, rasterized, generation);
        log::debug!(
            "[subs] rasterized cue inline gen={} {}x{} at {}x{}",
            generation, bitmap.width, bitmap.height, bitmap.target_w, inner.target_h
        );
        inner.ready.push(std::sync::Arc::new(bitmap));
        if inner.ready.len() > READY_SLOTS {
            inner.ready.remove(0);
        }
    }
}

impl Drop for SubtitleOverlay {
    fn drop(&mut self) {
        if let Some(handle) = self.worker.take() {
            {
                let mut inner = self.shared.inner.lock().unwrap();
                inner.shutdown = true;
            }
            self.shared.wake.notify_all();
            // The worker only ever holds the lock for bookkeeping, and
            // rasterizing one cue is bounded work, so this can't hang.
            let _ = handle.join();
        }
    }
}

/// Rasterizer thread: keeps the cue under the playhead and the one after
/// it baked into `ready`, so the render path only ever does a lookup.
///
/// Parks on the condvar whenever everything wanted is already baked, and
/// is woken by whatever could have changed that: a cue boundary, new cues,
/// a restyle, a new font, or a resize. Rasterizes one cue at a time,
/// releasing the lock for each (the expensive part must not block a
/// frame), then re-checks — by then the playhead may have moved on.
///
/// The park condition is evaluated under the lock and `Condvar::wait`
/// releases it atomically, so a notify can't slip in between the check and
/// the wait.
fn raster_worker(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut inner = shared.inner.lock().unwrap();
            loop {
                if inner.shutdown {
                    return;
                }
                match next_job(&inner) {
                    Some(job) => break job,
                    // Everything wanted is ready. Park until it isn't.
                    None => inner = shared.wake.wait(inner).unwrap(),
                }
            }
        };

        // Lock released: this is the multi-millisecond part.
        let rasterized = rasterizer::rasterize_cue(
            &job.font, &job.text, &job.layout, job.target_w, job.type_h, &job.style,
        );

        let mut inner = shared.inner.lock().unwrap();
        if inner.shutdown {
            return;
        }
        // A resize or a restyle while we were working invalidates the
        // result — `ready` was cleared, and storing this would hand the
        // render path a bitmap for the wrong geometry.
        if inner.target_w != job.target_w || inner.target_h != job.target_h {
            continue;
        }
        inner.generation += 1;
        let generation = inner.generation;
        let bitmap = finished_bitmap(job, rasterized, generation);
        log::debug!(
            "[subs] rasterized cue gen={} {}x{} at {}x{}",
            generation, bitmap.width, bitmap.height, bitmap.target_w, inner.target_h
        );
        inner.ready.push(std::sync::Arc::new(bitmap));
        if inner.ready.len() > READY_SLOTS {
            inner.ready.remove(0);
        }
        // Loop round: the other wanted cue may still need baking.
    }
}

/// One cue to rasterize: everything the worker needs, copied out so the
/// lock is not held while it works.
struct RasterJob {
    font: Arc<fontdue::Font>,
    text: String,
    layout: CueLayout,
    style: SubtitleStyle,
    target_w: u32,
    target_h: u32,
    /// See `CueParent::picture_h` — what the glyph size scales with.
    type_h: u32,
}

/// The next cue that needs rasterizing, or `None` when everything wanted
/// is already in `ready`. Split out of the worker so the borrow of
/// `inner` ends before the guard is re-assigned in the wait loop.
fn next_job(inner: &Inner) -> Option<RasterJob> {
    let font = inner.font.as_ref()?;
    // Before the first draw we don't know the surface size, so there is
    // nothing sensible to rasterize at yet.
    if inner.target_w == 0 || inner.target_h == 0 {
        return None;
    }
    for idx in inner.wanted_indices(inner.current_pts_ms).into_iter().flatten() {
        let cue = &inner.cues[idx];
        if inner.ready_for(cue.text.as_str(), &cue.layout, inner.target_w).is_none() {
            return Some(RasterJob {
                font: Arc::clone(font),
                text: cue.text.clone(),
                layout: cue.layout,
                style: inner.style,
                target_w: inner.target_w,
                target_h: inner.target_h,
                type_h: inner.type_h,
            });
        }
    }
    None
}

/// Wrap a rasterizer result as the `ready` entry for `job`. `None` (the
/// cue laid out to nothing) becomes a zero-size bitmap so the job counts
/// as done and the worker doesn't spin on it; both draw paths skip
/// zero-size entries.
fn finished_bitmap(job: RasterJob, rasterized: Option<rasterizer::CueRaster>, generation: u64) -> SubtitleBitmap {
    let (width, height, rgba, line_height_px) = match rasterized {
        Some(r) => (r.width, r.height, r.rgba, r.line_height_px),
        None => (0, 0, Vec::new(), 0),
    };
    SubtitleBitmap {
        rgba,
        width,
        height,
        generation,
        line_height_px,
        layout: job.layout,
        text: job.text,
        target_w: job.target_w,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_ms: i64, end_ms: i64, text: &str) -> VttCue {
        VttCue {
            start_ms,
            end_ms,
            text: text.to_string(),
            settings: String::new(),
            layout: CueLayout::DEFAULT,
        }
    }

    /// Build an `Inner` the way `queue_cues` would, so `max_cue_span_ms`
    /// stays consistent with the cue list.
    fn inner_with(cues: Vec<VttCue>) -> Inner {
        let mut sorted = cues;
        sorted.sort_by_key(|c| c.start_ms);
        let max_cue_span_ms = sorted
            .iter()
            .map(|c| c.end_ms - c.start_ms)
            .max()
            .unwrap_or(0);
        Inner {
            cues: sorted,
            current_pts_ms: 0,
            font: None,
            style: SubtitleStyle::DEFAULT,
            ready: Vec::new(),
            generation: 0,
            target_w: 1920,
            target_h: 1080,
            wanted: [None, None],
            shutdown: false,
            max_cue_span_ms,
        }
    }

    /// The linear scan `active_index` replaced. The binary-searched
    /// version must agree with it for every PTS, or a cue would silently
    /// stop showing up.
    fn linear_index(inner: &Inner, pts_ms: i64) -> Option<usize> {
        inner.cues.iter().position(|c| c.is_active(pts_ms))
    }

    fn assert_matches_linear(inner: &Inner, range: std::ops::Range<i64>) {
        for pts in range {
            assert_eq!(
                inner.active_index(pts),
                linear_index(inner, pts),
                "mismatch at pts={pts}ms"
            );
        }
    }

    #[test]
    fn active_index_matches_linear_scan_on_sequential_cues() {
        let inner = inner_with(vec![
            cue(0, 2000, "first"),
            cue(2000, 4000, "second"),
            cue(5000, 7500, "third after a gap"),
            cue(7500, 9000, "fourth"),
        ]);
        // Covers before the first cue, every boundary, both gaps, and past
        // the last cue.
        assert_matches_linear(&inner, -500..10_000);
    }

    #[test]
    fn active_index_matches_linear_scan_on_overlapping_cues() {
        // Overlapping cues are legal WebVTT: a long cue can still be on
        // screen when a later one starts. The window must not miss the
        // earlier (and therefore lower-indexed) one that is still active.
        let inner = inner_with(vec![
            cue(0, 8000, "a very long cue"),
            cue(1000, 2000, "short overlapping cue"),
            cue(3000, 4000, "another short one"),
            cue(9000, 10_000, "後"),
        ]);
        assert_matches_linear(&inner, -500..11_000);
        // First-in-start-order wins, exactly as `find` did.
        assert_eq!(inner.active_index(1500), Some(0));
    }

    #[test]
    fn active_index_handles_backward_seek() {
        let inner = inner_with(vec![
            cue(0, 2000, "first"),
            cue(2000, 4000, "second"),
            cue(4000, 6000, "third"),
        ]);
        // No cursor state to invalidate: jumping around must be stable.
        assert_eq!(inner.active_index(5000), Some(2));
        assert_eq!(inner.active_index(1000), Some(0));
        assert_eq!(inner.active_index(5000), Some(2));
        assert_eq!(inner.active_index(3000), Some(1));
    }

    #[test]
    fn wanted_indices_prefetches_the_next_cue() {
        let inner = inner_with(vec![
            cue(0, 2000, "first"),
            cue(3000, 5000, "second"),
            cue(6000, 8000, "third"),
        ]);
        // Mid-cue: the active one plus the one that starts next.
        assert_eq!(inner.wanted_indices(1000), [Some(0), Some(1)]);
        // In the gap: nothing active, but the upcoming cue is prefetched
        // so it's ready the instant it starts.
        assert_eq!(inner.wanted_indices(2500), [None, Some(1)]);
        // Past the last cue: nothing left to bake.
        assert_eq!(inner.wanted_indices(9000), [None, None]);
        // Before anything starts.
        assert_eq!(inner.wanted_indices(-1000), [None, Some(0)]);
    }

    #[test]
    fn note_target_tolerates_small_drift_but_drops_on_resize() {
        let mut inner = inner_with(vec![cue(0, 2000, "first")]);
        inner.ready.push(std::sync::Arc::new(SubtitleBitmap {
            rgba: vec![0; 4],
            width: 1,
            height: 1,
            generation: 1,
            line_height_px: 1,
            layout: CueLayout::DEFAULT,
            text: "first".to_string(),
            target_w: 1920,
        }));
        // Within 5%: keep the rasterization, a dragged window resizes by
        // a pixel at a time and re-baking each step is pure churn.
        assert!(!inner.note_target(1940, 1080));
        assert!(inner.ready_for("first", &CueLayout::DEFAULT, 1940).is_some());
        // A real resize invalidates it.
        assert!(inner.note_target(1280, 720));
        assert!(inner.ready.is_empty());
    }

    /// Spin the real worker against a hand-built `Shared` (no GPU
    /// involved) and wait for it to produce bitmaps, with a deadline so a
    /// regression that parks the worker fails instead of hanging.
    fn run_worker_until(
        shared: &Arc<Shared>,
        label: &str,
        done: impl Fn(&Inner) -> bool,
    ) {
        let deadline = crate::rt::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            {
                let inner = shared.inner.lock().unwrap();
                if done(&inner) {
                    return;
                }
            }
            assert!(
                crate::rt::Instant::now() < deadline,
                "worker never reached: {label}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn spawn_worker(inner: Inner) -> (Arc<Shared>, std::thread::JoinHandle<()>) {
        let shared = Arc::new(Shared {
            inner: Mutex::new(inner),
            wake: std::sync::Condvar::new(),
        });
        let handle = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || raster_worker(shared))
        };
        (shared, handle)
    }

    fn stop_worker(shared: &Arc<Shared>, handle: std::thread::JoinHandle<()>) {
        shared.inner.lock().unwrap().shutdown = true;
        shared.wake.notify_all();
        handle.join().unwrap();
    }

    #[test]
    fn worker_bakes_the_active_cue_and_prefetches_the_next() {
        let mut inner = inner_with(vec![
            cue(0, 2000, "first line"),
            cue(3000, 5000, "second line"),
            cue(9000, 10_000, "third line"),
        ]);
        inner.font = rasterizer::default_font().map(Arc::new);
        inner.current_pts_ms = 500;
        let (shared, handle) = spawn_worker(inner);
        shared.wake.notify_one();

        // Both the cue on screen and the one after it, without any
        // further prompting — that prefetch is what keeps a cue change
        // off the render thread.
        run_worker_until(&shared, "first two cues baked", |i| {
            i.ready_for("first line", &CueLayout::DEFAULT, 1920).is_some()
                && i.ready_for("second line", &CueLayout::DEFAULT, 1920).is_some()
        });
        {
            let inner = shared.inner.lock().unwrap();
            let bmp = inner.ready_for("first line", &CueLayout::DEFAULT, 1920).unwrap();
            assert!(bmp.width > 0 && bmp.height > 0, "cue rasterized to nothing");
            assert_eq!(bmp.rgba.len(), (bmp.width * bmp.height * 4) as usize);
            // The third cue is neither active nor next, so it must not
            // have been baked speculatively.
            assert!(inner.ready_for("third line", &CueLayout::DEFAULT, 1920).is_none());
        }
        stop_worker(&shared, handle);
    }

    #[test]
    fn worker_follows_the_playhead_across_a_cue_boundary() {
        let mut inner = inner_with(vec![
            cue(0, 2000, "first line"),
            cue(3000, 5000, "second line"),
            cue(6000, 8000, "third line"),
        ]);
        inner.font = rasterizer::default_font().map(Arc::new);
        let (shared, handle) = spawn_worker(inner);
        shared.wake.notify_one();
        run_worker_until(&shared, "initial bake", |i| {
            i.ready_for("first line", &CueLayout::DEFAULT, 1920).is_some()
        });

        // Jump the playhead onto the second cue, the way set_pts_ms does.
        {
            let mut inner = shared.inner.lock().unwrap();
            inner.current_pts_ms = 3500;
        }
        shared.wake.notify_one();
        run_worker_until(&shared, "third cue prefetched", |i| {
            i.ready_for("third line", &CueLayout::DEFAULT, 1920).is_some()
        });
        {
            // Bounded memory: the oldest entry is evicted rather than the
            // list growing for the whole movie.
            let inner = shared.inner.lock().unwrap();
            assert!(inner.ready.len() <= READY_SLOTS);
            // The cue actually on screen must have survived the eviction.
            assert!(inner.ready_for("second line", &CueLayout::DEFAULT, 1920).is_some());
        }
        stop_worker(&shared, handle);
    }

    #[test]
    fn worker_parks_when_there_is_no_surface_size_yet() {
        // Before the first draw the target is 0x0 and nothing can be
        // sized, so the worker must idle rather than spin or panic.
        let mut inner = inner_with(vec![cue(0, 2000, "first line")]);
        inner.font = rasterizer::default_font().map(Arc::new);
        inner.target_w = 0;
        inner.target_h = 0;
        let (shared, handle) = spawn_worker(inner);
        shared.wake.notify_one();
        std::thread::sleep(std::time::Duration::from_millis(50));
        {
            let inner = shared.inner.lock().unwrap();
            assert!(inner.ready.is_empty());
        }

        // First draw reports a size — now it has something to do.
        {
            let mut inner = shared.inner.lock().unwrap();
            inner.note_target(1280, 720);
        }
        shared.wake.notify_one();
        run_worker_until(&shared, "bake after first draw", |i| {
            i.ready_for("first line", &CueLayout::DEFAULT, 1280).is_some()
        });
        stop_worker(&shared, handle);
    }

    #[test]
    fn active_index_on_empty_list_is_none() {
        let inner = inner_with(Vec::new());
        assert_eq!(inner.active_index(0), None);
        assert_eq!(inner.active_index(1_000_000), None);
    }

    #[test]
    fn queue_cues_keeps_max_span_current() {
        // A stale-small span would shrink the search window and hide a
        // long cue, so pushing a longer cue has to widen it.
        let mut inner = inner_with(vec![cue(0, 1000, "short")]);
        assert_eq!(inner.max_cue_span_ms, 1000);
        let long = cue(500, 20_000, "very long cue");
        inner.max_cue_span_ms = inner.max_cue_span_ms.max(long.end_ms - long.start_ms);
        inner.cues.push(long);
        inner.cues.sort_by_key(|c| c.start_ms);
        assert_eq!(inner.max_cue_span_ms, 19_500);
        assert_eq!(inner.active_index(19_000), Some(1));
    }

    fn bitmap(w: u32, h: u32, lh: u32, settings: &str) -> SubtitleBitmap {
        SubtitleBitmap {
            rgba: Vec::new(),
            width: w,
            height: h,
            generation: 1,
            line_height_px: lh,
            layout: CueLayout::parse(settings),
            text: String::new(),
            target_w: 0,
        }
    }

    #[test]
    fn parent_is_the_aspect_fitted_picture_minus_the_bottom_inset() {
        // 16:9 content on a 16:9 surface: the whole surface.
        let p = CueParent::fit(1920, 1080, 1920, 1080, 0, SubtitleAnchor::Picture);
        assert_eq!((p.left, p.top, p.right, p.bottom), (0.0, 0.0, 1920.0, 1080.0));
        // 2.39:1 film on 16:9: letterboxed, the parent is the picture
        // (PlayerView puts SubtitleView inside the AspectRatioFrameLayout).
        let p = CueParent::fit(1920, 1080, 2390, 1000, 0, SubtitleAnchor::Picture);
        assert_eq!((p.left, p.right), (0.0, 1920.0));
        let pic_h = (1920.0f32 / 2.39).round();
        assert!((p.top - ((1080.0 - pic_h) / 2.0).floor()).abs() <= 1.0);
        assert!((p.bottom - (p.top + pic_h)).abs() <= 1.0);
        // Pillarboxed 4:3.
        let p = CueParent::fit(1920, 1080, 640, 480, 0, SubtitleAnchor::Picture);
        assert_eq!((p.top, p.bottom), (0.0, 1080.0));
        assert_eq!(p.left, 240.0);
        assert_eq!(p.right, 1680.0);
        // The inset only raises the bottom (view padding), never below it.
        let p = CueParent::fit(1920, 1080, 1920, 1080, 100, SubtitleAnchor::Picture);
        assert_eq!(p.bottom, 980.0);
        let p = CueParent::fit(1920, 1080, 2390, 1000, 40, SubtitleAnchor::Picture);
        assert!((p.bottom - (p.top + pic_h)).abs() <= 1.0, "inset inside the bar changes nothing");
        // Unknown content size → whole target.
        assert_eq!(CueParent::fit(1280, 720, 0, 0, 0, SubtitleAnchor::Picture), CueParent::fit(1280, 720, 1280, 720, 0, SubtitleAnchor::Picture));
    }

    #[test]
    fn screen_anchor_ignores_the_letterbox() {
        // Same widescreen film: with the default Screen anchor the box is
        // the whole surface (minus the inset), so the cue lands in the bar.
        let p = CueParent::fit(1920, 1080, 2390, 1000, 0, SubtitleAnchor::Screen);
        assert_eq!((p.left, p.top, p.right, p.bottom), (0.0, 0.0, 1920.0, 1080.0));
        let (x, y) = place_cue(400, 60, 40, &CueLayout::DEFAULT, &p);
        assert_eq!(x, 760.0);
        // 8 % of the PICTURE (803px), not of the 1080px surface. Taking it
        // from the surface left a 86px gap under the cue and pushed the block
        // up towards the picture — on a tall window far enough that a
        // three-line cue ended up back inside it.
        let pic_h = (1080.0f32 * (1920.0 / 1080.0) / 2.39).round();
        assert_eq!(pic_h, 803.0);
        assert_eq!(y, 1080.0 - 60.0 - (pic_h * BOTTOM_PADDING_FRACTION).floor());
        let p = CueParent::fit(1920, 1080, 2390, 1000, 100, SubtitleAnchor::Screen);
        assert_eq!(p.bottom, 980.0);
    }

    #[test]
    fn text_size_tracks_the_picture_not_the_surface() {
        // 16:9 film on a nearly-portrait window — the shape that exposed this.
        // With the Screen anchor the box is the whole surface, so the cue
        // lands in the letterbox bar; but the glyphs must still scale with the
        // picture, or the block grows taller than the bar and climbs into the
        // picture it was deliberately placed below.
        let p = CueParent::fit(958, 1064, 1920, 1080, 0, SubtitleAnchor::Screen);
        assert_eq!(p.height(), 1064, "box is the surface");
        assert_eq!(p.type_height(), 539, "size comes from the picture");

        // Picture anchor: the two agree, which is ExoPlayer's geometry.
        let p = CueParent::fit(958, 1064, 1920, 1080, 0, SubtitleAnchor::Picture);
        assert_eq!(p.height(), p.type_height());

        // The inset shrinks what the size derives from, like view padding.
        let p = CueParent::fit(958, 1064, 1920, 1080, 39, SubtitleAnchor::Screen);
        assert_eq!(p.type_height(), 500);
    }

    #[test]
    fn default_cue_sits_bottom_padding_above_the_parent_bottom_centered() {
        let p = CueParent::fit(1920, 1080, 1920, 1080, 0, SubtitleAnchor::Picture);
        let (x, y) = place_cue(400, 60, 40, &CueLayout::DEFAULT, &p);
        assert_eq!(x, 760.0);
        // textTop = parentBottom - textHeight - (int)(parentHeight * 0.08)
        assert_eq!(y, 1080.0 - 60.0 - 86.0);
        // As NDC: centered horizontally, bottom edge 8% up.
        let q = cue_quad(&bitmap(400, 60, 40, ""), &p);
        assert!((q[0]).abs() < 1e-6);
        assert!((q[1] - (-1.0 + 60.0 / 1080.0 + 2.0 * 86.0 / 1080.0)).abs() < 1e-5);
        assert!((q[2] - 400.0 / 1920.0).abs() < 1e-6);
    }

    #[test]
    fn line_settings_place_vertically_like_subtitle_painter() {
        let p = CueParent::fit(1000, 1000, 1000, 1000, 0, SubtitleAnchor::Picture);
        // line:10% → box top at 10% (lineAnchor start).
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:10%"), &p).1, 100.0);
        // ,center / ,end move the anchored edge.
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:10%,center"), &p).1, 75.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:10%,end"), &p).1, 50.0);
        // Integer lines: 0 = top, 2 = two line heights down, -1 = flush
        // with the bottom, -2 one line height up.
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:0"), &p).1, 0.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:2"), &p).1, 50.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:-1"), &p).1, 950.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:-2"), &p).1, 925.0);
        // Clamped into the parent: 100% with a start anchor would hang
        // below the bottom; -100 lines would poke out the top.
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:100%"), &p).1, 950.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:-100"), &p).1, 0.0);
        // The bottom inset raises the parent bottom for `-1` and default alike.
        let p = CueParent::fit(1000, 1000, 1000, 1000, 100, SubtitleAnchor::Picture);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:-1"), &p).1, 850.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::DEFAULT, &p).1, 900.0 - 50.0 - 72.0);
    }

    #[test]
    fn position_and_align_place_horizontally_like_subtitle_painter() {
        let p = CueParent::fit(1000, 1000, 1000, 1000, 0, SubtitleAnchor::Picture);
        // align:left → position 0 / anchor start → flush left; right → flush right.
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:left"), &p).0, 0.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:right"), &p).0, 800.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:start"), &p).0, 500.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:end"), &p).0, 300.0);
        // Explicit position with derived anchor (center → middle).
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:30%"), &p).0, 200.0);
        // Explicit anchor.
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:30%,line-left"), &p).0, 300.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:30%,line-right"), &p).0, 100.0);
        // Clamped into the parent on both sides.
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:5%"), &p).0, 0.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:95%"), &p).0, 800.0);
        // Inside a pillarboxed picture the parent's own edges apply.
        let p = CueParent::fit(1920, 1080, 640, 480, 0, SubtitleAnchor::Picture);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:left"), &p).0, 240.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:right"), &p).0, 1480.0);
    }
}
