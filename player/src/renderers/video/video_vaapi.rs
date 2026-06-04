use std::mem::MaybeUninit;
use std::os::fd::RawFd;

use ash::vk::{self, ImageCreateInfo};
use wgpu::hal::api::Vulkan;

pub type VADisplay = *mut std::ffi::c_void;
pub type VASurfaceID = u32;

extern "C" {
    fn vaExportSurfaceHandle(
        dpy: VADisplay,
        surface_id: VASurfaceID,
        mem_type: u32,
        flags: u32,
        descriptor: *mut std::ffi::c_void,
    ) -> i32;
}

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

pub unsafe fn export_shared_handle(
    va_display: VADisplay,
    va_surface_id: VASurfaceID,
) -> PrimeSurfaceDescriptor {
    let mut descriptor: MaybeUninit<PrimeSurfaceDescriptor> = MaybeUninit::uninit();

    let status = vaExportSurfaceHandle(
        va_display,
        va_surface_id,
        0x40000000, //For DMA-BUF
        0x1000,     // Optional flag for read-only access
        descriptor.as_mut_ptr().cast(),
    );

    if (status != 0) {
        panic!("Cannot create va shared handle")
    }

    descriptor.assume_init()
}

pub fn create_vk_image_from_dma_fd(
    device: &wgpu::Device,
    va_shared_prime_descriptor: PrimeSurfaceDescriptor,
) -> Result<VkImageMemory, Box<dyn std::error::Error>> {
    unsafe {
        let raw_dev = device
            .as_hal::<Vulkan>()
            .ok_or("device is not a Vulkan backend")?;

        let raw_device = raw_dev.raw_device();
        let physical_device = raw_dev.raw_physical_device();
        let instance = raw_dev.shared_instance().raw_instance();

        let handle_type = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;

        let mut import_memory_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(handle_type)
            .fd(va_shared_prime_descriptor.objects[0].fd);

        let mut ext_create_info =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);

        let vk_format = va_shared_prime_descriptor
            .fourcc
            .vk_format()
            .ok_or_else(|| {
                format!(
                    "unsupported VAAPI surface fourcc {:?} (only NV12 / P010 are mapped)",
                    va_shared_prime_descriptor.fourcc.to_bytes(),
                )
            })?;

        let image_create_info = ImageCreateInfo::default()
            .push_next(&mut ext_create_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width: va_shared_prime_descriptor.width,
                height: va_shared_prime_descriptor.height,
                depth: 1,
            })
            .mip_levels(1)
            .flags(vk::ImageCreateFlags::ALIAS | vk::ImageCreateFlags::MUTABLE_FORMAT)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        let raw_image = raw_device.create_image(&image_create_info, None)?;

        let mem_requirements = raw_device.get_image_memory_requirements(raw_image);

        let mem_properties = instance.get_physical_device_memory_properties(physical_device);

        let index = mem_properties
            .memory_types
            .iter()
            .enumerate()
            .position(|(i, t)| {
                ((1 << i) & mem_requirements.memory_type_bits) != 0
                    && t.property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .ok_or("Failed to get DEVICE_LOCAL memory index")?;

        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_requirements.size)
            .push_next(&mut import_memory_info)
            .memory_type_index(index as u32);

        let allocated_memory = raw_device.allocate_memory(&allocate_info, None)?;

        raw_device.bind_image_memory(raw_image, allocated_memory, 0)?;

        Ok(VkImageMemory {
            raw_image,
            memory: allocated_memory,
        })
    }
}
