use ash::vk;
use ffmpeg_next::frame::Video;
use ffmpeg_sys_next::AVHWFramesContext;
use std::sync::Arc;
use wgpu::{wgc::api::Vulkan, Backend, Extent3d, Texture};

use super::video_vulkan::create_texture_from_vk_image;

#[cfg(target_os = "linux")]
use super::video_vaapi::*;
#[cfg(target_os = "linux")]
use cros_libva::VASurfaceID;

#[cfg(target_os = "windows")]
use super::video_directx::*;
#[cfg(target_os = "windows")]
use windows::{
    core::Interface,
    Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
    },
};

pub struct VideoFrame {
    wgpu_device: wgpu::Device,
    wgpu_backend: wgpu::Backend,
    memory: Option<vk::DeviceMemory>,
    image: Option<vk::Image>,
    texture: Texture,
}

impl VideoFrame {
    #[cfg(target_os = "linux")]
    pub fn new(wgpu_device: wgpu::Device, wgpu_backend: wgpu::Backend, frame: Arc<Video>) -> Self {
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

            let va_display = (*hwctx).display as *mut _;
            let va_surface_id = (*frame_ptr).data[3] as VASurfaceID;

            let descriptor = export_shared_handle(va_display, va_surface_id);

            let image_with_memory = create_vk_image_from_dma_fd(&wgpu_device, descriptor).unwrap();

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

            let texture = create_texture_from_vk_image(
                &wgpu_device,
                image_with_memory.raw_image,
                desc.size.width,
                desc.size.height,
                desc.format,
                true,
                true,
            );

            VideoFrame {
                wgpu_device,
                wgpu_backend,
                memory: Some(image_with_memory.memory),
                image: None,
                texture,
            }
        }
    }

    #[cfg(target_os = "windows")]
    pub fn new(wgpu_device: wgpu::Device, wgpu_backend: wgpu::Backend, frame: Arc<Video>) -> Self {
        use wgpu::Backend;

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

            match wgpu_backend {
                Backend::Dx12 => {
                    let raw_image = create_dx12_resource_from_d3d11_texture(
                        &wgpu_device,
                        d3d11_device,
                        d3d11_device_context,
                        frame_texture,
                        frame.width(),
                        frame.height(),
                        Some(index as u32),
                    )
                    .unwrap();

                    let texture = create_texture_from_dx12_resource(&wgpu_device, raw_image, &desc);

                    VideoFrame {
                        wgpu_device,
                        wgpu_backend,
                        memory: None,
                        image: None,
                        texture,
                    }
                }
                Backend::Vulkan => {
                    let image_with_memory = create_vk_image_from_d3d11_texture(
                        &wgpu_device,
                        d3d11_device,
                        d3d11_device_context,
                        frame_texture,
                        frame.width(),
                        frame.height(),
                        Some(index as u32),
                    )
                    .unwrap();

                    let texture = create_texture_from_vk_image(
                        &wgpu_device,
                        image_with_memory.raw_image,
                        frame.width(),
                        frame.height(),
                        desc.format,
                        true,
                        true,
                    );

                    VideoFrame {
                        wgpu_device,
                        wgpu_backend,
                        memory: Some(image_with_memory.memory),
                        image: Some(image_with_memory.raw_image),
                        texture,
                    }
                }
                _ => panic!("Cannot select HW texture conversion"),
            }
        }
    }

    pub fn get_texture(&self) -> &Texture {
        &self.texture
    }
}

impl Drop for VideoFrame {
    fn drop(&mut self) {
        if self.wgpu_backend == Backend::Vulkan {
            unsafe {
                if let Some(raw_dev) = self.wgpu_device.as_hal::<Vulkan>() {
                    if let Some(memory) = self.memory {
                        raw_dev.raw_device().free_memory(memory, None);
                    }
                }
            }
        }
    }
}
