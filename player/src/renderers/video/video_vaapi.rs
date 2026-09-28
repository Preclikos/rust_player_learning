use std::mem::MaybeUninit;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use ash::vk::{self, ImageCreateInfo};
use wgpu::hal::api::Vulkan;

pub type VADisplay = *mut std::ffi::c_void;
pub type VASurfaceID = u32;

extern "C" {
    fn vaSyncSurface(dpy: VADisplay, render_target: VASurfaceID) -> i32;
    fn vaExportSurfaceHandle(
        dpy: VADisplay,
        surface_id: VASurfaceID,
        mem_type: u32,
        flags: u32,
        descriptor: *mut std::ffi::c_void,
    ) -> i32;
}

// libva va.h
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;
const VA_EXPORT_SURFACE_READ_ONLY: u32 = 0x0001;
const VA_EXPORT_SURFACE_COMPOSED_LAYERS: u32 = 0x0008;
// drm_fourcc.h
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

use super::video_vulkan::VkImageMemory;

#[repr(C)]
pub struct AVVAAPIDeviceContext {
    pub display: *mut VADisplay, // Pointer to VAAPI display (VADisplay)
}

#[repr(C)]
pub struct PrimeSurfaceDescriptor {
    pub fourcc: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub objects: [PrimeObject; 4],
    pub num_layers: u32,
    pub layers: [PrimeLayer; 4],
}

#[repr(C)]
pub struct PrimeLayer {
    drm_format: PixelFormat,
    num_planes: u32,
    object_index: [u32; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat(u32);

/// Describes a DRM PRIME object, represented as a DMA-BUF file descriptor.
#[derive(Debug)]
#[repr(C)]
pub struct PrimeObject {
    pub fd: RawFd,
    pub size: u32,
    pub drm_format_modifier: u64,
}

impl PixelFormat {
    /// Planar YUV 4:2:0 standard pixel format.
    ///
    /// All samples are 8 bits in size. The plane containing Y samples comes first, followed by a
    /// plane storing packed U and V samples (with U samples in the first byte and V samples in the
    /// second byte).
    ///
    /// This format is widely supported by hardware codecs (and often the *only* supported format),
    /// so it should be supported by all software, and may be used as the default format.
    pub const NV12: Self = f(b"NV12");

    /// Planar YUV 4:2:0 pixel format, with U and V swapped compared to `NV12`.
    pub const NV21: Self = f(b"NV21");

    /// 10-bit planar YUV 4:2:0 — same layout as NV12 but each sample is stored
    /// in the high 10 bits of a 16-bit container. The HEVC Main 10 / HDR10
    /// hardware path produces this; the low 6 bits are unused padding.
    pub const P010: Self = f(b"P010");

    /// Interleaved YUV 4:2:2, stored in memory as `yyyyyyyy uuuuuuuu YYYYYYYY vvvvvvvv`.
    ///
    /// `uuuuuuuu` and `vvvvvvvv` are shared by 2 horizontally neighboring pixels.
    ///
    /// Also known as [`YUYV`](Self::YUYV).
    pub const YUY2: Self = f(b"YUY2");

    /// Identical to [`YUY2`](Self::YUY2).
    pub const YUYV: Self = f(b"YUYV");

    /// Interleaved YUV 4:2:2, stored in memory as `uuuuuuuu yyyyyyyy vvvvvvvv YYYYYYYY`.
    ///
    /// `uuuuuuuu` and `vvvvvvvv` are shared by 2 neighboring pixels.
    pub const UYVY: Self = f(b"UVYV");

    /// `RGBA`: Packed 8-bit RGBA, stored in memory as `aaaaaaaa bbbbbbbb gggggggg rrrrrrrr`.
    pub const RGBA: Self = f(b"RGBA");

    /// `ARGB`: Packed 8-bit RGBA, stored in memory as `bbbbbbbb gggggggg rrrrrrrr aaaaaaaa`.
    pub const ARGB: Self = f(b"ARGB");

    /// Packed 8-bit RGBX.
    ///
    /// The X channel has unspecified values.
    pub const RGBX: Self = f(b"RGBX");

    /// Packed 8-bit BGRA.
    pub const BGRA: Self = f(b"BGRA");

    /// Packed 8-bit BGRX.
    ///
    /// The X channel has unspecified values.
    pub const BGRX: Self = f(b"BGRX");

    pub const fn from_bytes(fourcc: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(fourcc))
    }

    pub const fn from_u32_le(fourcc: u32) -> Self {
        Self(fourcc)
    }

    pub const fn to_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    pub const fn to_u32_le(self) -> u32 {
        self.0
    }

    /// Vulkan multi-planar format matching this VAAPI surface fourcc.
    /// Returns `None` for formats we don't currently import (the HW
    /// decoder only ever produces NV12 / P010 in this player).
    pub fn vk_format(self) -> Option<vk::Format> {
        match self {
            Self::NV12 => Some(vk::Format::G8_B8R8_2PLANE_420_UNORM),
            Self::P010 => Some(vk::Format::G16_B16R16_2PLANE_420_UNORM),
            _ => None,
        }
    }

    /// wgpu TextureFormat matching this VAAPI surface fourcc. The wgpu
    /// descriptor must agree with the Vulkan image we import — a
    /// mismatch is silent until the first draw, where the driver tears
    /// the device down.
    pub fn wgpu_format(self) -> Option<wgpu::TextureFormat> {
        match self {
            Self::NV12 => Some(wgpu::TextureFormat::NV12),
            Self::P010 => Some(wgpu::TextureFormat::P010),
            _ => None,
        }
    }
}

const fn f(fourcc: &[u8; 4]) -> PixelFormat {
    PixelFormat::from_bytes(*fourcc)
}

/// An exported VAAPI surface. Owns the DMA-BUF fds in `objects`: every fd
/// still set is closed on drop. An import that hands an fd to Vulkan (which
/// then owns it) sets it to -1 first.
pub struct PrimeSurface(pub PrimeSurfaceDescriptor);

impl Drop for PrimeSurface {
    fn drop(&mut self) {
        let n = (self.0.num_objects as usize).min(self.0.objects.len());
        for object in &mut self.0.objects[..n] {
            if object.fd >= 0 {
                drop(unsafe { OwnedFd::from_raw_fd(object.fd) });
                object.fd = -1;
            }
        }
    }
}

pub unsafe fn export_shared_handle(
    va_display: VADisplay,
    va_surface_id: VASurfaceID,
) -> Result<PrimeSurface, String> {
    // The decode into this surface can still be in flight on the GPU;
    // FFmpeg only syncs when it maps or downloads a surface itself.
    let status = vaSyncSurface(va_display, va_surface_id);
    if status != 0 {
        return Err(format!("vaSyncSurface failed: VAStatus {status}"));
    }

    let mut descriptor: MaybeUninit<PrimeSurfaceDescriptor> = MaybeUninit::zeroed();

    // Composed layers: one layer carrying both planes, the layout a single
    // multi-planar VkImage imports.
    let status = vaExportSurfaceHandle(
        va_display,
        va_surface_id,
        VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
        VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS,
        descriptor.as_mut_ptr().cast(),
    );

    if status != 0 {
        return Err(format!("vaExportSurfaceHandle failed: VAStatus {status}"));
    }

    Ok(PrimeSurface(descriptor.assume_init()))
}

pub fn create_vk_image_from_dma_fd(
    device: &wgpu::Device,
    surface: &mut PrimeSurface,
) -> Result<VkImageMemory, Box<dyn std::error::Error>> {
    unsafe {
        let raw_dev = device
            .as_hal::<Vulkan>()
            .ok_or("device is not a Vulkan backend")?;

        let raw_device = raw_dev.raw_device();
        let physical_device = raw_dev.raw_physical_device();
        let instance = raw_dev.shared_instance().raw_instance();

        let descriptor = &surface.0;

        let vk_format = descriptor.fourcc.vk_format().ok_or_else(|| {
            format!(
                "unsupported VAAPI surface fourcc {:?} (only NV12 / P010 are mapped)",
                descriptor.fourcc.to_bytes(),
            )
        })?;
        let plane_formats = match descriptor.fourcc {
            PixelFormat::P010 => [vk::Format::R16_UNORM, vk::Format::R16G16_UNORM],
            _ => [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM],
        };

        let layer = &descriptor.layers[0];
        if descriptor.num_layers != 1 || layer.num_planes != 2 {
            return Err(format!(
                "VAAPI export has {} layer(s) / {} plane(s), expected 1 / 2",
                descriptor.num_layers, layer.num_planes,
            )
            .into());
        }
        let object_index = layer.object_index[0] as usize;
        if layer.object_index[1] as usize != object_index
            || object_index >= (descriptor.num_objects as usize).min(descriptor.objects.len())
        {
            return Err("VAAPI export puts the planes in separate objects (not supported)".into());
        }
        let object = &descriptor.objects[object_index];
        let fd = object.fd;

        let handle_type = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
        let usage = vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC;
        // MUTABLE_FORMAT with a modifier needs the view formats listed; the
        // renderer views the planes as R8/RG8 (R16/RG16 for P010).
        let view_formats = [vk_format, plane_formats[0], plane_formats[1]];

        // The surface is laid out by its DRM format modifier and the plane
        // offsets / pitches in the descriptor. When the driver can't import
        // that (no extension, no modifier reported, or a format it has no
        // modifiers for — P010 on Intel ANV), fall back to the previous
        // OPTIMAL-tiling import, which the driver happens to match on Intel.
        let use_modifier = object.drm_format_modifier != DRM_FORMAT_MOD_INVALID
            && raw_dev
                .enabled_device_extensions()
                .contains(&ash::ext::image_drm_format_modifier::NAME)
            && modifier_importable(
                instance,
                physical_device,
                vk_format,
                usage,
                object.drm_format_modifier,
                &view_formats,
            );

        let mut ext_create_info =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);

        let plane_layouts = [0, 1].map(|plane| vk::SubresourceLayout {
            offset: layer.offset[plane] as u64,
            size: 0,
            row_pitch: layer.pitch[plane] as u64,
            array_pitch: 0,
            depth_pitch: 0,
        });
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(object.drm_format_modifier)
            .plane_layouts(&plane_layouts);
        let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(&view_formats);

        let mut image_create_info = ImageCreateInfo::default()
            .push_next(&mut ext_create_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width: descriptor.width,
                height: descriptor.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // ALIAS is not supported with tiled modifiers (ANV) and nothing
        // aliases this memory; the fallback keeps its previous flags.
        image_create_info = if use_modifier {
            image_create_info
                .push_next(&mut modifier_info)
                .push_next(&mut format_list)
                .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        } else {
            image_create_info
                .flags(vk::ImageCreateFlags::ALIAS | vk::ImageCreateFlags::MUTABLE_FORMAT)
                .tiling(vk::ImageTiling::OPTIMAL)
        };

        let raw_image = raw_device.create_image(&image_create_info, None)?;

        let mem_requirements = raw_device.get_image_memory_requirements(raw_image);

        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        ash::khr::external_memory_fd::Device::new(instance, raw_device)
            .get_memory_fd_properties(handle_type, fd, &mut fd_properties)
            .map_err(|e| {
                raw_device.destroy_image(raw_image, None);
                e
            })?;

        let mem_properties = instance.get_physical_device_memory_properties(physical_device);

        let memory_type_bits = mem_requirements.memory_type_bits & fd_properties.memory_type_bits;
        let Some(index) = mem_properties
            .memory_types
            .iter()
            .enumerate()
            .position(|(i, t)| {
                ((1 << i) & memory_type_bits) != 0
                    && t.property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
        else {
            raw_device.destroy_image(raw_image, None);
            return Err("Failed to get DEVICE_LOCAL memory index".into());
        };

        let mut import_memory_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(handle_type)
            .fd(fd);

        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_requirements.size)
            .push_next(&mut import_memory_info)
            .memory_type_index(index as u32);

        let allocated_memory = match raw_device.allocate_memory(&allocate_info, None) {
            Ok(memory) => memory,
            Err(e) => {
                raw_device.destroy_image(raw_image, None);
                return Err(e.into());
            }
        };
        // A successful import owns the fd; the rest close with `surface`.
        surface.0.objects[object_index].fd = -1;

        if let Err(e) = raw_device.bind_image_memory(raw_image, allocated_memory, 0) {
            raw_device.destroy_image(raw_image, None);
            raw_device.free_memory(allocated_memory, None);
            return Err(e.into());
        }

        Ok(VkImageMemory {
            raw_image,
            memory: allocated_memory,
        })
    }
}

/// Whether the driver can import a DMA-BUF with this modifier as the image
/// the renderer samples.
unsafe fn modifier_importable(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
    modifier: u64,
    view_formats: &[vk::Format],
) -> bool {
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(view_formats);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
        .push_next(&mut external_info)
        .push_next(&mut modifier_info)
        .push_next(&mut format_list);

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    instance
        .get_physical_device_image_format_properties2(physical_device, &info, &mut props)
        .is_ok()
        && external_props
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
}
