//! Apple (macOS / iOS) output paths of [`VideoRenderer`]. A decoded frame
//! reaches the screen one of three ways:
//!
//!   1. **Renderer-drawn, SDR surface** — VideoToolbox planes through the
//!      SDR shader or the HDR→SDR tonemap (`render_metal_nv12`).
//!   2. **Renderer-drawn, EDR session** — the same planes handed to an HDR
//!      display as PQ: the `CAMetalLayer` switches to rgba16float + BT.2100
//!      PQ ([`apple_hdr_output`]) and `shader_hdr_output.wgsl` skips the
//!      tonemap. Entered on the first HDR frame while the host's display
//!      mask allows it, left when it stops allowing it.
//!   3. **Direct mode** — the picture is in the host's
//!      `AVSampleBufferDisplayLayer` below us (`decoders::apple_direct`);
//!      the render layer turns transparent and only presents subtitles,
//!      and only when they change.
//!
//! [`AppleOutput`] holds the state of 2 and 3; the mode a frame takes is
//! decided per frame from the frame's own `PlatformFrame` and colour info,
//! so ABR swaps between representations stay correct frame by frame.

#![cfg(any(target_os = "macos", target_os = "ios"))]

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use wgpu::TextureFormat;

use super::apple_hdr_output::{self, EdrSession, HdrOutputPipelines, SessionStep};
use crate::renderers::subtitle::{CueParent, SubtitleOverlay};
use crate::renderers::video_offscreen::OffscreenTarget;
use super::video_metal::MetalNV12Frame;
use super::{PlaneDraw, Vertex, VideoRenderer};
use crate::decoders::{CvPixelBufferOwned, TransferFunction, VideoColorInfo};

/// Apple-only renderer state (one per renderer).
pub(super) struct AppleOutput {
    pub(super) edr: EdrSession,
    direct: DirectOverlay,
    /// Last logged (pipeline, plane depth) combination of
    /// `render_metal_nv12`, so the log line fires on changes only.
    last_metal_path: AtomicU8,
}

impl AppleOutput {
    pub(super) fn new() -> Self {
        Self {
            edr: EdrSession::new(),
            direct: DirectOverlay::new(),
            last_metal_path: AtomicU8::new(u8::MAX),
        }
    }

    /// The EDR-session pipeline for an `Out*` draw mode. Only called inside
    /// a session, which built the pipelines on entry.
    pub(super) fn edr_pipeline(&self, mode: PlaneDraw) -> &wgpu::RenderPipeline {
        let p: &HdrOutputPipelines = self.edr.pipelines.get().expect("EDR session without pipelines");
        match mode {
            PlaneDraw::OutPq => &p.pq,
            PlaneDraw::OutHlg => &p.hlg,
            PlaneDraw::OutSdr => &p.sdr,
            other => unreachable!("{other:?} is not an EDR output mode"),
        }
    }
}

/// Direct mode: the render layer is non-opaque above the host's video
/// layer and carries subtitles only.
struct DirectOverlay {
    active: AtomicBool,
    /// What the overlay last presented — subtitle generation (0 = no cue)
    /// and surface size — so an unchanged overlay isn't re-presented every
    /// frame. `None` = must present.
    presented: Mutex<Option<(u64, (u32, u32))>>,
}

impl DirectOverlay {
    fn new() -> Self {
        Self { active: AtomicBool::new(false), presented: Mutex::new(None) }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Forget what is on screen: the next frame presents again.
    fn invalidate(&self) {
        *self.presented.lock().unwrap() = None;
    }

    /// Record `(generation, size)` as presented; `false` when it already was.
    fn take_present(&self, generation: u64, size: (u32, u32)) -> bool {
        let mut last = self.presented.lock().unwrap();
        if *last == Some((generation, size)) {
            return false;
        }
        *last = Some((generation, size));
        true
    }
}

/// Which pipeline renderer-drawn planes take: inside an EDR session every
/// transfer is re-encoded as PQ for the HDR surface; outside it HDR
/// (PQ / HLG) is tonemapped and SDR drawn as-is.
fn plane_draw(edr_session: bool, color: VideoColorInfo) -> PlaneDraw {
    match (edr_session, color.transfer) {
        (true, TransferFunction::Pq) => PlaneDraw::OutPq,
        (true, TransferFunction::Hlg) => PlaneDraw::OutHlg,
        (true, TransferFunction::Sdr) => PlaneDraw::OutSdr,
        (false, _) if color.is_hdr() => PlaneDraw::Hdr,
        (false, _) => PlaneDraw::Sdr,
    }
}

fn plane_draw_label(mode: PlaneDraw) -> &'static str {
    match mode {
        PlaneDraw::Sdr => "SDR",
        PlaneDraw::Hdr => "HDR tonemap",
        PlaneDraw::OutPq => "HDR output (PQ passthrough)",
        PlaneDraw::OutHlg => "HDR output (HLG→PQ)",
        PlaneDraw::OutSdr => "HDR output (SDR→PQ)",
    }
}

impl VideoRenderer {
    /// macOS / iOS: render a `CVPixelBufferOwned` from VTDecompressionSession.
    /// Wraps the buffer into a `MetalNV12Frame` (two zero-copy MTLTextures)
    /// and draws it. `color` is the frame's signalled colour info — it
    /// decides the pipeline (the plane bit depth alone can't: VT's 8-bit
    /// fallback still carries a PQ/BT.2020 signal).
    pub async fn render_cv_pixel_buffer(&self, buf: CvPixelBufferOwned, color: VideoColorInfo) {
        let Some(cache) = self.metal_cache.clone() else {
            log::warn!("[renderer] metal_cache missing, dropping frame");
            return;
        };
        let frame = match unsafe { MetalNV12Frame::new(&cache, &self.device, buf.as_ptr()) } {
            Ok(f) => f,
            Err(e) => {
                log::warn!("[renderer] MetalNV12Frame::new failed: {}", e);
                return;
            }
        };
        let frame = self.render_metal_nv12(frame, color).await;
        // Apple requires the CVMetalTextures and the CVPixelBuffer to stay
        // alive until the command buffer that samples them completes. A
        // retained MTLTexture alone does not stop the decoder pool from
        // recycling the IOSurface, which would show a later frame's picture.
        // The callback fires from the maintain() of a later submit.
        self.queue.on_submitted_work_done(move || drop((frame, buf)));
    }

    /// Draw VideoToolbox output: two single-plane textures (Y = R8/R16Unorm,
    /// UV = Rg8/Rg16Unorm). Hands `metal_frame` back once the pass is
    /// submitted; the caller keeps it (with its CVPixelBuffer) alive until
    /// the GPU is done with it.
    ///
    /// The pipeline follows the frame's signalled transfer, NOT the plane
    /// bit depth: VTDecompressionSession only converts pixel format, never
    /// colour, so when the 10-bit 'x420' destination is refused the 8-bit
    /// NV12 fallback still carries a PQ/BT.2020 signal that must be
    /// tonemapped (at 8-bit quantization cost). The shaders' limited-range
    /// expansion is bit-depth-agnostic — R8Unorm and P010-in-R16Unorm
    /// normalise to the same [0,1] codes.
    ///
    /// MetalNV12Frame is `Send` but not `Sync` (raw CFTypeRef), so we take
    /// it by value to keep `render_frame`'s future `Send` across the await.
    async fn render_metal_nv12(&self, metal_frame: MetalNV12Frame, color: VideoColorInfo) -> MetalNV12Frame {
        // Back from direct mode (failover / SDR display): opaque again.
        self.set_direct_overlay(false).await;
        let edr_session = self.update_edr_session(color.is_hdr()).await;
        let mode = plane_draw(edr_session, color);
        self.log_metal_path(mode, metal_frame.y_texture.format() == TextureFormat::R16Unorm);

        let y_view = metal_frame.y_texture.create_view(&Default::default());
        let uv_view = metal_frame.uv_texture.create_view(&Default::default());
        let (w, h) = (metal_frame.y_texture.width(), metal_frame.y_texture.height());
        self.draw_planes(&y_view, &uv_view, w, h, mode, None).await;
        metal_frame
    }

    /// One log line whenever the (pipeline, plane depth) combination
    /// changes — makes the field-debug question "did the tonemap actually
    /// run, and on which planes?" answerable from logs.
    fn log_metal_path(&self, mode: PlaneDraw, is_10bit: bool) {
        let state = mode as u8 | (is_10bit as u8) << 4;
        if self.apple_output.last_metal_path.swap(state, Ordering::Relaxed) != state {
            log::info!(
                "Metal NV12 path: {} planes → {} pipeline",
                if is_10bit { "10-bit" } else { "8-bit" },
                plane_draw_label(mode),
            );
        }
    }

    /// The swapchain and its config, for a reconfigure (windowed mode only;
    /// `None` offscreen).
    async fn lock_surface(
        &self,
    ) -> Option<(
        tokio::sync::MutexGuard<'_, wgpu::Surface<'static>>,
        tokio::sync::RwLockWriteGuard<'_, wgpu::SurfaceConfiguration>,
    )> {
        let (surface, config) = (self.surface.as_ref()?, self.surface_config.as_ref()?);
        Some((surface.lock().await, config.write().await))
    }

    // ----- EDR session -----

    /// Enter / leave the EDR session for this frame; returns whether it is
    /// active afterwards. Any failure to enter retires the session for this
    /// renderer and the tonemap stays.
    async fn update_edr_session(&self, frame_is_hdr: bool) -> bool {
        let mask = self.display_hdr_types.load(Ordering::Relaxed);
        match self.apple_output.edr.step(mask, frame_is_hdr, self.apple_output.direct.is_active()) {
            SessionStep::Keep { active } => active,
            SessionStep::Enter => self.enter_edr_session().await,
            SessionStep::Leave => {
                self.leave_edr_session().await;
                false
            }
        }
    }

    async fn enter_edr_session(&self) -> bool {
        let edr = &self.apple_output.edr;
        let (Some(layout), Some((surface, mut cfg))) =
            (self.texture_bind_group_layout.as_ref(), self.lock_surface().await)
        else {
            // Offscreen (in-app) target or no pipelines: tonemap only.
            edr.fail();
            return false;
        };
        if !apple_hdr_output::layer_supports_edr(&surface) {
            log::warn!("[hdr-output] layer has no EDR support — staying on tonemap");
            edr.fail();
            return false;
        }
        edr.pipelines
            .get_or_init(|| HdrOutputPipelines::new(&self.device, layout, Vertex::desc()));
        cfg.format = apple_hdr_output::HDR_OUTPUT_FORMAT;
        cfg.view_formats = vec![apple_hdr_output::HDR_OUTPUT_FORMAT];
        surface.configure(&self.device, &cfg);
        match apple_hdr_output::enter_layer_pq(&surface, apple_hdr_output::DEFAULT_MASTERING_PEAK_NITS) {
            Some(saved) => {
                *edr.saved.lock().unwrap() = Some(saved);
                edr.set_active(true);
                self.set_subtitles_pq(true);
                log::info!("[hdr-output] HDR output ON — rgba16float + BT.2100 PQ + EDR, no tonemap");
                true
            }
            None => {
                cfg.format = self.surface_format;
                cfg.view_formats = vec![self.surface_format];
                surface.configure(&self.device, &cfg);
                edr.fail();
                false
            }
        }
    }

    async fn leave_edr_session(&self) {
        let edr = &self.apple_output.edr;
        if let Some((surface, mut cfg)) = self.lock_surface().await {
            if let Some(saved) = edr.saved.lock().unwrap().take() {
                apple_hdr_output::exit_layer_pq(&surface, saved);
            }
            cfg.format = self.surface_format;
            cfg.view_formats = vec![self.surface_format];
            surface.configure(&self.device, &cfg);
        }
        edr.set_active(false);
        self.set_subtitles_pq(false);
        log::info!("[hdr-output] HDR output OFF — display no longer HDR, back to tonemap");
    }

    fn set_subtitles_pq(&self, pq: bool) {
        if let Some(overlay) = self.subtitle_overlay.lock().unwrap().as_ref() {
            overlay.set_pq_output(pq);
        }
    }

    // ----- Direct mode overlay -----

    /// Direct mode on/off for the render layer: non-opaque (post-multiplied
    /// alpha) so the `AVSampleBufferDisplayLayer` below shows through, or
    /// back to the normal opaque video surface.
    async fn set_direct_overlay(&self, on: bool) {
        let direct = &self.apple_output.direct;
        if direct.is_active() == on {
            return;
        }
        if on && self.apple_output.edr.is_active() {
            // An EDR session belongs to the renderer-drawn video only.
            self.leave_edr_session().await;
        }
        // Offscreen there is no surface of ours to reconfigure: the host
        // composites the published texture above its video layer, and
        // `present_direct_overlay` publishes a transparent one.
        if let Some((surface, mut cfg)) = self.lock_surface().await {
            cfg.alpha_mode =
                if on { wgpu::CompositeAlphaMode::PostMultiplied } else { wgpu::CompositeAlphaMode::Auto };
            surface.configure(&self.device, &cfg);
        }
        direct.active.store(on, Ordering::Relaxed);
        direct.invalidate();
        log::info!(
            "[direct] render layer → {}",
            match (on, self.offscreen.is_some()) {
                (true, false) => "transparent subtitle overlay above the video layer",
                (true, true) => "transparent offscreen texture (subtitles only) above the host's video layer",
                (false, _) => "opaque video surface",
            }
        );
    }

    /// Direct mode: present the render layer (transparent + the active cue)
    /// only when what it shows changed — a cue appearing, changing or
    /// clearing, or a resize. Cue-less playback leaves it idle.
    pub(super) async fn present_direct_overlay(&self) {
        self.set_direct_overlay(true).await;
        if let Some(off) = self.offscreen.clone() {
            self.publish_direct_offscreen(&off).await;
            return;
        }
        let (Some(surface), Some(config)) = (self.surface.as_ref(), self.surface_config.as_ref()) else {
            return;
        };
        let (size, format) = {
            let cfg = config.read().await;
            ((cfg.width, cfg.height), cfg.format)
        };
        let Some((cue_parent, generation, overlay)) = self.direct_overlay_cue(size).await else { return };

        let surface = surface.lock().await;
        let texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => {
                // Occluded window etc. — retried on the next frame.
                log::debug!("[direct] overlay texture not available: {:?}", other);
                self.apple_output.direct.invalidate();
                return;
            }
        };
        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor { format: Some(format), ..Default::default() });
        self.draw_direct_overlay(&view, generation, overlay.as_ref(), &cue_parent);
        self.pre_present_notify();
        texture.present();
    }

    /// Offscreen direct mode: publish a transparent texture carrying only
    /// the active cue, so the host's picture of us stops covering its video
    /// layer — instead of the last renderer-drawn frame staying in the ring.
    /// Same change-only rule as the windowed overlay.
    async fn publish_direct_offscreen(&self, off: &OffscreenTarget) {
        // Acquiring applies a pending resize, so the cue is laid out for the
        // size actually drawn; an unchanged overlay leaves the slot unused.
        let (idx, view, sz) = off.acquire();
        let Some((cue_parent, generation, overlay)) = self.direct_overlay_cue((sz.width, sz.height)).await
        else {
            return;
        };
        self.draw_direct_overlay(&view, generation, overlay.as_ref(), &cue_parent);
        off.publish(idx);
    }

    /// The cue layout for a `size` overlay and the cue's generation (0 =
    /// none) — or `None` when exactly this was already presented.
    async fn direct_overlay_cue(
        &self,
        size: (u32, u32),
    ) -> Option<(CueParent, u64, Option<Arc<SubtitleOverlay>>)> {
        let frame = *self.frame_size.read().await;
        let overlay = self.subtitle_overlay.lock().unwrap().clone();
        let cue_parent = CueParent::fit(
            size.0,
            size.1,
            frame.width,
            frame.height,
            self.subtitle_safe_bottom_px.load(Ordering::Relaxed),
            overlay.as_ref().map(|o| o.anchor()).unwrap_or_default(),
        );
        // 0 = no cue; a cue's generation is never 0 on screen.
        let generation = overlay
            .as_ref()
            .and_then(|o| o.active_bitmap(&cue_parent))
            .map_or(0, |bitmap| bitmap.generation.max(1));
        if !self.apple_output.direct.take_present(generation, size) {
            return None;
        }
        Some((cue_parent, generation, overlay))
    }

    /// Clear `view` to transparent and draw the cue (if any) into it.
    fn draw_direct_overlay(
        &self,
        view: &wgpu::TextureView,
        generation: u64,
        overlay: Option<&Arc<SubtitleOverlay>>,
        cue_parent: &CueParent,
    ) {
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("direct subtitle overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let (true, Some(overlay)) = (generation != 0, overlay) {
                overlay.draw_into(&mut pass, cue_parent);
            }
        }
        self.queue.submit([encoder.finish()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color(transfer: TransferFunction) -> VideoColorInfo {
        VideoColorInfo { transfer, ..Default::default() }
    }

    #[test]
    fn edr_session_re_encodes_every_transfer_as_pq() {
        assert_eq!(plane_draw(true, color(TransferFunction::Pq)), PlaneDraw::OutPq);
        assert_eq!(plane_draw(true, color(TransferFunction::Hlg)), PlaneDraw::OutHlg);
        assert_eq!(plane_draw(true, color(TransferFunction::Sdr)), PlaneDraw::OutSdr);
    }

    #[test]
    fn sdr_surface_tonemaps_hdr_and_passes_sdr() {
        assert_eq!(plane_draw(false, color(TransferFunction::Pq)), PlaneDraw::Hdr);
        assert_eq!(plane_draw(false, color(TransferFunction::Hlg)), PlaneDraw::Hdr);
        assert_eq!(plane_draw(false, color(TransferFunction::Sdr)), PlaneDraw::Sdr);
    }

    #[test]
    fn direct_overlay_presents_on_change_only() {
        let overlay = DirectOverlay::new();
        assert!(overlay.take_present(0, (800, 600)), "first frame");
        assert!(!overlay.take_present(0, (800, 600)), "nothing changed");
        assert!(overlay.take_present(7, (800, 600)), "cue appeared");
        assert!(overlay.take_present(7, (1024, 768)), "resized");
        assert!(overlay.take_present(0, (1024, 768)), "cue cleared");
        overlay.invalidate();
        assert!(overlay.take_present(0, (1024, 768)), "after invalidate");
    }
}
