//! Offscreen render target for in-app video.
//!
//! PURELY ADDITIVE. `VideoRenderer` gains an *alternate output*: instead of a
//! swapchain `Surface`, it can render into a small ring of `wgpu::Texture`s that
//! a host (Slint / iced / egui) samples directly on a device it SHARES with the
//! player. No window, no swapchain, no CPU readback. The windowed path is
//! unchanged — `VideoRenderer` simply carries `surface: Some(..)` XOR
//! `offscreen: Some(..)`, and the render bookends branch on which is set.
//!
//! Zero-copy correctness rests on:
//!   * device sharing — the host passes the same `device`/`queue` it composites
//!     with (created with `TEXTURE_FORMAT_NV12 | P010 | 16BIT_NORM`);
//!   * publish-after-submit + ping-pong — a texture is exposed via
//!     `current_texture()` only after its commands are submitted, and we cycle
//!     `RING` buffers so the host samples frame N-1 while frame N is drawn.

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::PhysicalSize;

/// Format handed to the host. `Rgba8Unorm` + `RENDER_ATTACHMENT | TEXTURE_BINDING`
/// is what Slint documents for `Image::try_from`; it is also a universally
/// available render target. The HDR path tonemaps to SDR before writing here,
/// so 8-bit output is fine for a first cut.
pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Ring depth: host holds one, GPU finishes one, renderer draws into a third.
const RING: usize = 3;

type FrameReadyCb = Box<dyn Fn() + Send + Sync>;

/// Ring of offscreen textures + the bookkeeping to hand the freshest finished
/// one to the host. Shared (`Arc`) between the renderer and the `Player` API.
pub struct OffscreenTarget {
    device: wgpu::Device,
    textures: RwLock<Vec<wgpu::Texture>>,
    size: RwLock<PhysicalSize<u32>>,
    write_idx: AtomicUsize,
    /// Most-recently-published (host-visible) buffer; -1 = none yet.
    ready_idx: AtomicI64,
    on_ready: Mutex<Option<FrameReadyCb>>,
}

impl OffscreenTarget {
    fn alloc(device: &wgpu::Device, size: PhysicalSize<u32>) -> Vec<wgpu::Texture> {
        let (w, h) = (size.width.max(1), size.height.max(1));
        (0..RING)
            .map(|_| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("blackzone offscreen video"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: OFFSCREEN_FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[OFFSCREEN_FORMAT],
                })
            })
            .collect()
    }

    pub fn new(device: wgpu::Device, size: PhysicalSize<u32>) -> Arc<Self> {
        let textures = Self::alloc(&device, size);
        Arc::new(Self {
            device,
            textures: RwLock::new(textures),
            size: RwLock::new(size),
            write_idx: AtomicUsize::new(0),
            ready_idx: AtomicI64::new(-1),
            on_ready: Mutex::new(None),
        })
    }

    /// Next buffer to render into + a view of it (RENDER_ATTACHMENT target).
    pub fn acquire(&self) -> (usize, wgpu::TextureView) {
        let idx = self.write_idx.load(Ordering::Relaxed) % RING;
        let view = self.textures.read().unwrap()[idx].create_view(&wgpu::TextureViewDescriptor {
            format: Some(OFFSCREEN_FORMAT),
            ..Default::default()
        });
        (idx, view)
    }

    /// Mark `idx` host-visible (call AFTER `queue.submit`) and fire the callback.
    pub fn publish(&self, idx: usize) {
        self.ready_idx.store(idx as i64, Ordering::Release);
        self.write_idx.store((idx + 1) % RING, Ordering::Relaxed);
        if let Some(cb) = self.on_ready.lock().unwrap().as_ref() {
            cb();
        }
    }

    /// Freshest finished texture for the host to import. Returns buffer 0 (a
    /// cleared/black texture) until the first real publish, so never `None`.
    pub fn current_texture(&self) -> wgpu::Texture {
        let r = self.ready_idx.load(Ordering::Acquire);
        let idx = if r < 0 { 0 } else { r as usize };
        self.textures.read().unwrap()[idx].clone()
    }

    pub fn set_on_ready(&self, cb: FrameReadyCb) {
        *self.on_ready.lock().unwrap() = Some(cb);
    }

    pub fn size(&self) -> PhysicalSize<u32> {
        *self.size.read().unwrap()
    }

    /// Reallocate the ring at a new size (host resized). Next published frame
    /// uses the new textures; the host re-imports on the following frame-ready.
    pub fn resize(&self, size: PhysicalSize<u32>) {
        if *self.size.read().unwrap() == size || size.width == 0 || size.height == 0 {
            return;
        }
        let fresh = Self::alloc(&self.device, size);
        *self.textures.write().unwrap() = fresh;
        *self.size.write().unwrap() = size;
        self.write_idx.store(0, Ordering::Relaxed);
        self.ready_idx.store(-1, Ordering::Release);
    }
}
