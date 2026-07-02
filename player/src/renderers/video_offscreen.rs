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
//!   * publish-after-submit + single-queue ordering — `publish` exposes the
//!     just-submitted texture; because the host composites on the SAME queue,
//!     its sampling submission is ordered after our draw, so it can never
//!     observe a half-drawn frame. The `RING` buffers keep the previously
//!     published frame intact while the next one is being drawn;
//!   * resize-between-frames — `resize` only *requests* a new size; the ring
//!     is swapped by `acquire` on the render task, between frames, so a swap
//!     can never race an in-flight draw/publish.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::PhysicalSize;

/// Format handed to the host. `Rgba8Unorm` + `RENDER_ATTACHMENT | TEXTURE_BINDING`
/// is what Slint documents for `Image::try_from`; it is also a universally
/// available render target. The HDR path tonemaps to SDR before writing here,
/// so 8-bit output is fine for a first cut.
pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Ring depth: host holds one, GPU finishes one, renderer draws into a third.
const RING: usize = 3;

/// `Arc` (not `Box`) so `publish` can clone the callback out and invoke host
/// code with no lock held.
type FrameReadyCb = Arc<dyn Fn() + Send + Sync>;

/// Textures + their size, swapped together so they can never disagree.
struct Ring {
    textures: Vec<wgpu::Texture>,
    size: PhysicalSize<u32>,
}

/// Ring of offscreen textures + the bookkeeping to hand the freshest finished
/// one to the host. Shared (`Arc`) between the renderer and the `Player` API.
pub struct OffscreenTarget {
    device: wgpu::Device,
    ring: RwLock<Ring>,
    /// Host-requested size, applied by the next `acquire` (render task).
    pending_size: Mutex<Option<PhysicalSize<u32>>>,
    write_idx: AtomicUsize,
    /// Freshest submitted texture. A handle (not a ring index) so it stays
    /// valid across a ring reallocation — after a resize the host keeps
    /// showing the last good frame until the first frame at the new size
    /// is published, instead of flashing a never-drawn black texture.
    published: RwLock<Option<wgpu::Texture>>,
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
            ring: RwLock::new(Ring { textures, size }),
            pending_size: Mutex::new(None),
            write_idx: AtomicUsize::new(0),
            published: RwLock::new(None),
            on_ready: Mutex::new(None),
        })
    }

    /// Next buffer to render into + a view of it (RENDER_ATTACHMENT target) +
    /// the ring size the view actually has (use THIS for the viewport /
    /// subtitle layout, not a separate `size()` read — a resize may land in
    /// between). Applies any pending resize first; render task only.
    pub fn acquire(&self) -> (usize, wgpu::TextureView, PhysicalSize<u32>) {
        if let Some(size) = self.pending_size.lock().unwrap().take() {
            let fresh = Self::alloc(&self.device, size);
            let mut ring = self.ring.write().unwrap();
            ring.textures = fresh;
            ring.size = size;
            self.write_idx.store(0, Ordering::Relaxed);
        }
        let ring = self.ring.read().unwrap();
        let idx = self.write_idx.load(Ordering::Relaxed) % RING;
        let view = ring.textures[idx].create_view(&wgpu::TextureViewDescriptor {
            format: Some(OFFSCREEN_FORMAT),
            ..Default::default()
        });
        (idx, view, ring.size)
    }

    /// Mark `idx` host-visible (call AFTER `queue.submit`) and fire the
    /// callback. The ring cannot have been swapped since `acquire` (swaps
    /// happen only inside `acquire`, on this same task), so `idx` is valid.
    pub fn publish(&self, idx: usize) {
        let tex = self.ring.read().unwrap().textures[idx].clone();
        *self.published.write().unwrap() = Some(tex);
        self.write_idx.store((idx + 1) % RING, Ordering::Relaxed);
        // Clone the callback out and invoke it with NO lock held: a callback
        // that calls back into the player (e.g. `set_frame_ready_callback`
        // re-registering on a state change) must not deadlock the render task.
        let cb = self.on_ready.lock().unwrap().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    /// Freshest finished texture for the host to import. Returns buffer 0 (a
    /// cleared/black texture) until the first real publish, so never `None`.
    pub fn current_texture(&self) -> wgpu::Texture {
        if let Some(t) = self.published.read().unwrap().as_ref() {
            return t.clone();
        }
        self.ring.read().unwrap().textures[0].clone()
    }

    pub fn set_on_ready(&self, cb: impl Fn() + Send + Sync + 'static) {
        *self.on_ready.lock().unwrap() = Some(Arc::new(cb));
    }

    pub fn size(&self) -> PhysicalSize<u32> {
        self.ring.read().unwrap().size
    }

    /// Request the ring be reallocated at a new size (host resized). Applied
    /// by the next `acquire` on the render task — never mid-frame. Until the
    /// first frame at the new size is published, `current_texture` keeps
    /// returning the last published (old-size) frame.
    pub fn resize(&self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        let mut pending = self.pending_size.lock().unwrap();
        // Last request wins; resizing back to the live size cancels a queued swap.
        *pending = if size == self.ring.read().unwrap().size {
            None
        } else {
            Some(size)
        };
    }
}
