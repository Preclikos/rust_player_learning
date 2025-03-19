use ffmpeg_next::frame::Video;
use ffmpeg_sys_next::AVHWFramesContext;
use std::sync::Arc;
use wgpu::{Backend, Extent3d, Texture};

use ash::vk::{self, ImageCreateInfo};
use cros_libva::{VADisplay, VAImage, VASurfaceID};

use super::video_vulkan::create_texture_from_vk_image;
use nix::errno::Errno;
use nix::fcntl;
use std::fs::{self, File};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::io::FromRawFd;
use wgpu::wgc::api::Vulkan;

#[cfg(target_os = "linux")]
use super::video_vaapi::*;

#[cfg(target_os = "windows")]
use super::video_directx::*;
#[cfg(target_os = "windows")]
use windows::{
    core::Interface,
    Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
    },
};

#[repr(C)]
pub struct PrimeSurfaceDescriptor {
    fourcc: PixelFormat,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [PrimeObject; 4],
    num_layers: u32,
    //layers: [PrimeLayer; 4],
}

pub struct PixelFormat(u32);

/// Describes a DRM PRIME object, represented as a DMA-BUF file descriptor.
#[derive(Debug)]
#[repr(C)]
pub struct PrimeObject {
    fd: RawFd,
    size: u32,
    drm_format_modifier: u64,
}

pub struct VideoFrame {
    wgpu_device: wgpu::Device,
    wgpu_backend: wgpu::Backend,
}

impl VideoFrame {
    pub fn new(wgpu_device: wgpu::Device, wgpu_backend: wgpu::Backend) -> Self {
        VideoFrame {
            wgpu_device,
            wgpu_backend,
        }
    }

    #[cfg(target_os = "linux")]
    pub unsafe fn is_dmabuf_supported(
        instance: &ash::Instance,
        physical_device: vk::PhysicalDevice,
        format: vk::Format,
        modifier: u64,
        usage: vk::ImageUsageFlags,
    ) -> bool {
        let mut drm_props = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut drm_props);

        let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier);

        let mut external_format_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .usage(usage)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .push_next(&mut external_format_info)
            .push_next(&mut modifier_info);

        match instance.get_physical_device_image_format_properties2(
            physical_device,
            &format_info,
            &mut props,
        ) {
            Ok(_) => (),
            Err(_) => {
                //debug!(?format, ?modifier, "format not supported for dma import");
                return false;
            }
        }

        drm_props
            .external_memory_properties
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
    }

    #[cfg(target_os = "linux")]
    pub fn get_texture(&self, frame: Arc<Video>) -> Texture {
        use std::{ffi::c_void, mem::MaybeUninit};

        use cros_libva::DrmPrimeSurfaceDescriptor;

        unsafe {
            let frame_ptr = frame.as_ptr();

            let hw_frames_ctx = (*frame_ptr).hw_frames_ctx;
            if hw_frames_ctx.is_null() {
                panic!("No hardware frames context associated with the frame.");
            }

            let hw_frames_ctx_ref = (*hw_frames_ctx).data as *mut AVHWFramesContext;
            let hw_device_ctx = (*hw_frames_ctx_ref).device_ctx;
            if hw_device_ctx.is_null() {
                panic!("No hardware device context associated with the frames context.");
            }

            let hwctx = (*hw_device_ctx).hwctx as *mut AVVAAPIDeviceContext;

            let va_display_ptr = (*hwctx).display as *mut _;

            let texture = (*frame_ptr).data[3] as VASurfaceID;

            let mut va_image = VAImage::default();
            cros_libva::vaDeriveImage(va_display_ptr, texture, &mut va_image);

            // Convert raw pointer to cros-libva's VaDisplay

            let sync = cros_libva::vaSyncSurface(va_display_ptr, texture);

            let mut descriptor: MaybeUninit<PrimeSurfaceDescriptor> = MaybeUninit::uninit();

            let status = cros_libva::vaExportSurfaceHandle(
                va_display_ptr,
                texture,
                0x40000000,      //For DMA-BUF
                0x8000 | 0x1000, // Optional flag for read-only access
                descriptor.as_mut_ptr().cast(),
            );

            let descriptor = descriptor.assume_init();

            let dma_fd = descriptor.objects[0].fd;

            let raw_image =
                self.wgpu_device
                    .as_hal::<Vulkan, _, _>(|device| {
                        device.map(|device| {
                            let raw_device = device.raw_device();
                            let physical_device = device.raw_physical_device();
                            let instance = device.shared_instance().raw_instance();

                            let mut usage_flags = vk::ImageUsageFlags::empty();
                            usage_flags |= vk::ImageUsageFlags::SAMPLED;
                            usage_flags |= vk::ImageUsageFlags::TRANSFER_DST;

                            let mut external_image_create_info =
                                vk::ExternalMemoryImageCreateInfo::default()
                                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

                            let mut export_memory_alloc_info = vk::ImportMemoryFdInfoKHR::default()
                                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                                .fd(dma_fd);

                            let extent = vk::Extent3D {
                                width: descriptor.width,
                                height: descriptor.height,
                                depth: 1,
                            };

                            let vk_info = vk::ImageCreateInfo::default()
                                .flags(
                                    vk::ImageCreateFlags::ALIAS
                                        | vk::ImageCreateFlags::MUTABLE_FORMAT,
                                )
                                .image_type(vk::ImageType::TYPE_2D)
                                .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
                                .extent(extent)
                                .mip_levels(1)
                                .array_layers(1)
                                .samples(vk::SampleCountFlags::TYPE_1)
                                .tiling(vk::ImageTiling::OPTIMAL)
                                .usage(usage_flags)
                                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                                .initial_layout(vk::ImageLayout::UNDEFINED)
                                //.push_next(&mut modifier_list)
                                .push_next(&mut external_image_create_info);

                            let image = match raw_device.create_image(&vk_info, None) {
                                Err(err) => {
                                    panic!("create_image() failed:");
                                }
                                Ok(image) => image,
                            };

                            let memory_req = raw_device.get_image_memory_requirements(image);

                            let mem_properties =
                                instance.get_physical_device_memory_properties(physical_device);

                            let index = mem_properties.memory_types.iter().enumerate().position(
                                |(i, t)| {
                                    ((1 << i) & memory_req.memory_type_bits) != 0
                                        && t.property_flags
                                            .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                                },
                            );

                            let index = match index {
                                None => {
                                    panic!("Failed to get DEVICE_LOCAL memory index")
                                }
                                Some(index) => index,
                            };

                            let mem_requirements = raw_device.get_image_memory_requirements(image);

                            let allocate_info = vk::MemoryAllocateInfo::default()
                                .allocation_size(0)
                                .push_next(&mut export_memory_alloc_info)
                                .memory_type_index(index as u32);

                            let allocated_memory =
                                raw_device.allocate_memory(&allocate_info, None)?;

                            _ = raw_device.bind_image_memory(image, allocated_memory, 0);

                            Ok::<ash::vk::Image, vk::Result>(image)
                        })
                    })
                    .unwrap()
                    .unwrap(); // TODO: unwrap

            let desc = wgpu::TextureDescriptor {
                label: None,
                size: Extent3d {
                    width: frame.width(),
                    height: frame.height(),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::NV12, //format_dxgi_to_wgpu(desc.Format),
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            };

            create_texture_from_vk_image(
                &self.wgpu_device,
                raw_image,
                desc.size.width,
                desc.size.height,
                desc.format,
                true,
                true,
            )

            /*
            match self.wgpu_backend {
                Backend::Vulkan => {
                    let raw_image = cre(
                        &self.wgpu_device,
                        d3d11_device,
                        d3d11_device_context,
                        frame_texture,
                        Some(index as u32),
                    )
                    .unwrap();

                    create_texture_from_vk_image(
                        &self.wgpu_device,
                        raw_image,
                        desc.size.width,
                        desc.size.height,
                        desc.format,
                        true,
                        true,
                    )
                }
                _ => panic!("Cannot select HW texture conversion"),
            }*/
            //panic!("Cannot select HW texture conversion");
        }
    }

    #[cfg(target_os = "windows")]
    pub fn get_texture(&self, frame: Arc<Video>) -> Texture {
        unsafe {
            let frame_ptr = frame.as_ptr();

            let hw_frames_ctx = (*frame_ptr).hw_frames_ctx;
            if hw_frames_ctx.is_null() {
                panic!("No hardware frames context associated with the frame.");
            }

            let hw_frames_ctx_ref = (*hw_frames_ctx).data as *mut AVHWFramesContext;
            let hw_device_ctx = (*hw_frames_ctx_ref).device_ctx;
            if hw_device_ctx.is_null() {
                panic!("No hardware device context associated with the frames context.");
            }

            let hwctx = (*hw_device_ctx).hwctx as *mut AVD3D11VADeviceContext;

            let d3d11_device = ID3D11Device::from_raw_borrowed(&(*hwctx).device).unwrap();
            let d3d11_device_context =
                ID3D11DeviceContext::from_raw_borrowed(&(*hwctx).device_context).unwrap();

            let ffmpeg_texture = (*frame.as_ptr()).data[0] as *mut _;
            let index = (*frame.as_ptr()).data[1] as i32;

            let frame_texture = ID3D11Texture2D::from_raw_borrowed(&ffmpeg_texture).unwrap();

            let mut dx_desc = D3D11_TEXTURE2D_DESC::default();
            frame_texture.GetDesc(&mut dx_desc);

            let desc = wgpu::TextureDescriptor {
                label: None,
                size: Extent3d {
                    width: dx_desc.Width,
                    height: dx_desc.Height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::NV12, //format_dxgi_to_wgpu(desc.Format),
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            };

            match self.wgpu_backend {
                Backend::Dx12 => {
                    let raw_image = create_dx12_resource_from_d3d11_texture(
                        &self.wgpu_device,
                        d3d11_device,
                        d3d11_device_context,
                        frame_texture,
                        Some(index as u32),
                    )
                    .unwrap();

                    create_texture_from_dx12_resource(&self.wgpu_device, raw_image, &desc)
                }
                Backend::Vulkan => {
                    let raw_image = create_vk_image_from_d3d11_texture(
                        &self.wgpu_device,
                        d3d11_device,
                        d3d11_device_context,
                        frame_texture,
                        Some(index as u32),
                    )
                    .unwrap();

                    create_texture_from_vk_image(
                        &self.wgpu_device,
                        raw_image,
                        desc.size.width,
                        desc.size.height,
                        desc.format,
                        true,
                        true,
                    )
                }
                _ => panic!("Cannot select HW texture conversion"),
            }
        }
    }
}
