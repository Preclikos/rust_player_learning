use ffmpeg_next::frame::Video;
use ffmpeg_sys_next::AVHWFramesContext;
use std::sync::Arc;
use wgpu::{Backend, Extent3d, Texture};

#[cfg(target_os = "windows")]
use super::video_directx::*;
use super::video_vulkan::create_texture_from_vk_image;
use windows::{
    core::Interface,
    Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
    },
};

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
