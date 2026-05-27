//! Apple zero-copy NV12 import: CVPixelBuffer → CVMetalTextureCache →
//! MTLTexture → wgpu::Texture. Shared by macOS (FFmpeg VideoToolbox HW
//! frames; AVFrame.data[3] = CVPixelBufferRef) and iOS (native
//! VTDecompressionSession output, CVPixelBufferRef stored directly in
//! PlatformFrame::CvPixelBuffer).
//!
//! Flow:
//!   1. `MetalTextureCache::new(device)` extracts the underlying MTLDevice
//!      from the wgpu device via the Metal HAL and creates a
//!      CVMetalTextureCache against it. One per VideoRenderer.
//!   2. `MetalNV12Frame::new(cache, device, cv_pixel_buffer)` for each
//!      decoded frame: asks the cache for two CVMetalTextureRefs (planes
//!      0 and 1), wraps each MTLTexture as a `wgpu::Texture` via
//!      `wgpu_hal::metal::Device::texture_from_raw`, and holds the
//!      CVMetalTextureRefs alive until the frame is dropped (so the GPU
//!      doesn't sample a recycled buffer).

#![cfg(any(target_os = "macos", target_os = "ios"))]

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLDevice, MTLTexture, MTLTextureType};

// -------------------------------------------------------------------------
// CoreVideo / Metal C FFI
// -------------------------------------------------------------------------
// We only need a small slice of CoreVideo, so write the bindings inline
// rather than pulling in core-video / core-foundation crates. CVMetalTexture
// and CVPixelBuffer are CFType subclasses → released via CFRelease.

type CFTypeRef = *const c_void;
type CFAllocatorRef = CFTypeRef;
type CFDictionaryRef = CFTypeRef;
type CVImageBufferRef = CFTypeRef; // alias for CVPixelBufferRef in this context
type CVMetalTextureRef = CFTypeRef;
type CVMetalTextureCacheRef = CFTypeRef;
type CVReturn = i32; // 0 = kCVReturnSuccess
type MTLPixelFormat = u64;

// Subset of MTLPixelFormat values we use.
const MTL_PIXEL_FORMAT_R8_UNORM: MTLPixelFormat = 10;
const MTL_PIXEL_FORMAT_RG8_UNORM: MTLPixelFormat = 30;

#[link(name = "CoreVideo", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CVMetalTextureCacheCreate(
        allocator: CFAllocatorRef,
        cache_attributes: CFDictionaryRef,
        metal_device: *const c_void, // id<MTLDevice>
        texture_attributes: CFDictionaryRef,
        cache_out: *mut CVMetalTextureCacheRef,
    ) -> CVReturn;

    fn CVMetalTextureCacheCreateTextureFromImage(
        allocator: CFAllocatorRef,
        texture_cache: CVMetalTextureCacheRef,
        source_image: CVImageBufferRef,
        texture_attributes: CFDictionaryRef,
        pixel_format: MTLPixelFormat,
        width: usize,
        height: usize,
        plane_index: usize,
        texture_out: *mut CVMetalTextureRef,
    ) -> CVReturn;

    fn CVMetalTextureCacheFlush(texture_cache: CVMetalTextureCacheRef, options: u64);

    fn CVMetalTextureGetTexture(image: CVMetalTextureRef) -> *mut c_void; // id<MTLTexture>

    fn CVPixelBufferGetWidthOfPlane(pixel_buffer: CFTypeRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pixel_buffer: CFTypeRef, plane_index: usize) -> usize;

    fn CFRelease(cf: CFTypeRef);
}

// -------------------------------------------------------------------------
// CVMetalTextureCache wrapper
// -------------------------------------------------------------------------

/// One CVMetalTextureCache per VideoRenderer. Internally retains the
/// underlying MTLDevice; the cache itself is a CFType and is released on
/// Drop.
///
/// CVMetalTextureCache is documented as thread-safe by Apple, so the
/// wrapper is `Send + Sync`. The raw `CVMetalTextureCacheRef` is just a
/// `*const c_void` so we have to mark these manually.
pub struct MetalTextureCache {
    raw: CVMetalTextureCacheRef,
}

unsafe impl Send for MetalTextureCache {}
unsafe impl Sync for MetalTextureCache {}

impl MetalTextureCache {
    /// Create the cache bound to the wgpu device's underlying MTLDevice.
    /// Returns None when the wgpu backend isn't Metal — caller must guard.
    pub fn new(device: &wgpu::Device) -> Option<Arc<Self>> {
        // Drop down to the Metal HAL to extract the raw MTLDevice. The
        // hal::metal::Device borrow lives only as long as the closure /
        // Deref guard; the MTLDevice pointer we pass to CV is retained
        // by CVMetalTextureCacheCreate internally, so it survives.
        let mtl_device_ptr: *const c_void = unsafe {
            let hal_dev = device.as_hal::<wgpu::hal::api::Metal>()?;
            let mtl_dev: &Retained<ProtocolObject<dyn MTLDevice>> = hal_dev.raw_device();
            Retained::as_ptr(mtl_dev) as *const c_void
        };

        let mut cache: CVMetalTextureCacheRef = ptr::null();
        let rc = unsafe {
            CVMetalTextureCacheCreate(
                ptr::null(),
                ptr::null(),
                mtl_device_ptr,
                ptr::null(),
                &mut cache,
            )
        };
        if rc != 0 || cache.is_null() {
            log::warn!("CVMetalTextureCacheCreate failed: {}", rc);
            return None;
        }
        Some(Arc::new(MetalTextureCache { raw: cache }))
    }

    /// Periodically flush stale CVMetalTextures the cache has hung onto.
    /// Apple recommends calling this every frame; cost is negligible.
    pub fn flush(&self) {
        unsafe { CVMetalTextureCacheFlush(self.raw, 0) };
    }
}

impl Drop for MetalTextureCache {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { CFRelease(self.raw) };
        }
    }
}

// -------------------------------------------------------------------------
// MetalNV12Frame — Y + UV wgpu::Textures backed by CVMetalTextures
// -------------------------------------------------------------------------

/// Holds the wgpu textures for one decoded frame plus the CVMetalTextureRefs
/// keeping the backing IOSurface / MTLTexture alive. Drop the whole struct
/// after submitting the render pass; CFRelease on the CVMetalTextures
/// releases the wgpu MTLTexture retain we created via texture_from_raw.
pub struct MetalNV12Frame {
    pub y_texture: wgpu::Texture,
    pub uv_texture: wgpu::Texture,
    // CFType references that own the imported textures. Kept opaque; never
    // dereferenced after construction — just CFRelease'd on Drop.
    _cv_y: CVMetalTextureRef,
    _cv_uv: CVMetalTextureRef,
}

// CVMetalTextureRef is a *const c_void wrapper; the underlying CFType is
// thread-safe to release from any thread.
unsafe impl Send for MetalNV12Frame {}

impl MetalNV12Frame {
    /// Import a CVPixelBuffer (NV12) as two wgpu textures.
    /// `cv_pixel_buffer` is a `CVPixelBufferRef` cast to a raw pointer.
    /// Caller is responsible for keeping the pixel buffer retained for the
    /// duration of this call; the cache may also retain it internally.
    ///
    /// # Safety
    /// `cv_pixel_buffer` must be a valid, non-null CVPixelBufferRef
    /// containing NV12 (kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange /
    /// FullRange) data.
    pub unsafe fn new(
        cache: &MetalTextureCache,
        device: &wgpu::Device,
        cv_pixel_buffer: *const c_void,
    ) -> Result<Self, String> {
        if cv_pixel_buffer.is_null() {
            return Err("CVPixelBuffer is null".into());
        }
        let pixel_buffer: CFTypeRef = cv_pixel_buffer;

        // Plane dims come from the CVPixelBuffer (NV12: UV plane is half size
        // in both dimensions). AVFrame.width/height isn't used here — we trust
        // the underlying CVPixelBuffer.
        let y_w = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, 0) };
        let y_h = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, 0) };
        let uv_w = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, 1) };
        let uv_h = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, 1) };

        // Build the Y plane wgpu::Texture.
        let (cv_y, y_texture) = build_plane_texture(
            cache,
            device,
            pixel_buffer,
            0,
            MTL_PIXEL_FORMAT_R8_UNORM,
            wgpu::TextureFormat::R8Unorm,
            y_w as u32,
            y_h as u32,
        )?;

        // Build the UV plane wgpu::Texture.
        let (cv_uv, uv_texture) = match build_plane_texture(
            cache,
            device,
            pixel_buffer,
            1,
            MTL_PIXEL_FORMAT_RG8_UNORM,
            wgpu::TextureFormat::Rg8Unorm,
            uv_w as u32,
            uv_h as u32,
        ) {
            Ok(v) => v,
            Err(e) => {
                // Roll back the Y-plane CVMetalTexture so we don't leak.
                unsafe { CFRelease(cv_y) };
                return Err(e);
            }
        };

        // Flush so the cache discards CVMetalTextures whose CVPixelBuffer
        // has been released. Cheap; recommended once per frame.
        cache.flush();

        Ok(MetalNV12Frame {
            y_texture,
            uv_texture,
            _cv_y: cv_y,
            _cv_uv: cv_uv,
        })
    }
}

impl Drop for MetalNV12Frame {
    fn drop(&mut self) {
        // CFRelease the two CVMetalTextures. The wgpu::Textures (and their
        // hal::metal::Texture wrappers) hold their own +1 retain on the
        // MTLTexture, so the GPU side keeps a live reference until wgpu's
        // own command buffer is finalised and the wgpu::Texture is dropped.
        unsafe {
            if !self._cv_y.is_null() {
                CFRelease(self._cv_y);
            }
            if !self._cv_uv.is_null() {
                CFRelease(self._cv_uv);
            }
        }
    }
}

// -------------------------------------------------------------------------
// Plane → wgpu::Texture import helper
// -------------------------------------------------------------------------

fn build_plane_texture(
    cache: &MetalTextureCache,
    device: &wgpu::Device,
    pixel_buffer: CFTypeRef,
    plane_index: usize,
    mtl_pixel_format: MTLPixelFormat,
    wgpu_format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> Result<(CVMetalTextureRef, wgpu::Texture), String> {
    // Step 1: CV creates / fetches a cached CVMetalTexture for this plane.
    let mut cv_tex: CVMetalTextureRef = ptr::null();
    let rc = unsafe {
        CVMetalTextureCacheCreateTextureFromImage(
            ptr::null(),
            cache.raw,
            pixel_buffer,
            ptr::null(),
            mtl_pixel_format,
            width as usize,
            height as usize,
            plane_index,
            &mut cv_tex,
        )
    };
    if rc != 0 || cv_tex.is_null() {
        return Err(format!(
            "CVMetalTextureCacheCreateTextureFromImage(plane {}) failed: {}",
            plane_index, rc
        ));
    }

    // Step 2: extract the MTLTexture handle. CV returns an autoreleased
    // id<MTLTexture> — Retained::retain bumps the refcount to +1 so the
    // texture survives even if CV drops its internal reference.
    let mtl_tex_raw: *mut c_void = unsafe { CVMetalTextureGetTexture(cv_tex) };
    if mtl_tex_raw.is_null() {
        unsafe { CFRelease(cv_tex) };
        return Err(format!("CVMetalTextureGetTexture(plane {}) returned null", plane_index));
    }
    let retained: Retained<ProtocolObject<dyn MTLTexture>> = unsafe {
        let typed: *mut ProtocolObject<dyn MTLTexture> = mtl_tex_raw.cast();
        match Retained::retain(typed) {
            Some(r) => r,
            None => {
                CFRelease(cv_tex);
                return Err(format!("Retained::retain(plane {}) failed", plane_index));
            }
        }
    };

    // Step 3: wrap the MTLTexture as a wgpu::Texture via Metal HAL. The
    // descriptor mirrors how we'd create a regular wgpu texture of the
    // same shape — wgpu uses these fields to validate views/binds, not to
    // (re-)allocate any storage.
    let desc = wgpu::TextureDescriptor {
        label: Some(if plane_index == 0 { "vt-nv12-y" } else { "vt-nv12-uv" }),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu_format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };

    let hal_texture = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            retained,
            wgpu_format,
            MTLTextureType::Type2D,
            1, // array_layers
            1, // mip_levels
            wgpu::hal::CopyExtent {
                width,
                height,
                depth: 1,
            },
        )
    };

    let wgpu_texture = unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Metal>(
            hal_texture,
            &desc,
            wgpu::TextureUses::RESOURCE,
        )
    };

    Ok((cv_tex, wgpu_texture))
}
