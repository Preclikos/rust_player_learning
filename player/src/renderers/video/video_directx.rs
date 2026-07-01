use ash::vk::{self, ImageCreateInfo};
use wgpu::hal::api::Dx12;
use wgpu::hal::api::Vulkan;
use wgpu::TextureFormat;
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, E_NOINTERFACE, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::{Direct3D11::*, Direct3D12, Dxgi::Common::*, Dxgi::*};
use windows::Win32::System::Threading::{CreateEventA, WaitForSingleObject};

use super::video_vulkan::VkImageMemory;

// Define a raw struct for AVD3D11VAContext if it's not exposed in the bindings
#[repr(C)]
pub struct AVD3D11VADeviceContext {
    pub device: *mut std::ffi::c_void,
    pub device_context: *mut std::ffi::c_void,
    video_device: *mut ID3D11VideoDevice,
    video_context: *mut ID3D11VideoContext,
    lock: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    lock_ctx: *mut std::ffi::c_void,
}

pub struct DirectX11Fence {
    fence: ID3D11Fence,
    event: HANDLE,
    fence_value: std::sync::atomic::AtomicU64,
}
unsafe impl Send for DirectX11Fence {}
impl DirectX11Fence {
    pub fn new(device: &ID3D11Device) -> windows::core::Result<Self> {
        unsafe {
            let device = device.cast::<ID3D11Device5>()?;
            let mut fence: Option<ID3D11Fence> = None;

            device.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)?;
            let fence = fence.ok_or(windows::core::Error::new(E_FAIL, "Failed to create fence"))?;

            let event = CreateEventA(None, false, false, windows::core::PCSTR::null())?;

            Ok(Self {
                fence,
                event,
                fence_value: Default::default(),
            })
        }
    }
    pub fn synchronize(&self, context: &ID3D11DeviceContext) -> windows::core::Result<()> {
        let context = context.cast::<ID3D11DeviceContext4>()?;
        let v = self
            .fence_value
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        unsafe {
            context.Signal(&self.fence, v)?;
            self.fence.SetEventOnCompletion(v, self.event)?;
            WaitForSingleObject(self.event, 5000);
        }
        Ok(())
    }
}
impl Drop for DirectX11Fence {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

pub struct DirectX11SharedTexture {
    intermediate_texture: ID3D11Texture2D,
    fence: DirectX11Fence,
}
impl DirectX11SharedTexture {
    pub fn synchronized_copy_from(
        &self,
        context: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        width: u32,
        height: u32,
        region: Option<u32>,
    ) -> windows::core::Result<()> {
        self.synchronized_copy(context, tex, true, width, height, region)
    }
    // Mirror of synchronized_copy_from; consumed by the DX12 interop branch
    // that wgpu currently doesn't take for FFmpeg-imported D3D11 textures.
    #[allow(dead_code)]
    pub fn synchronized_copy_to(
        &self,
        context: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        width: u32,
        height: u32,
        region: Option<u32>,
    ) -> windows::core::Result<()> {
        self.synchronized_copy(context, tex, false, width, height, region)
    }

    fn synchronized_copy(
        &self,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        from: bool,
        width: u32,
        height: u32,
        region: Option<u32>,
    ) -> windows::core::Result<()> {
        unsafe {
            if let Ok(mutex) = self.intermediate_texture.cast::<IDXGIKeyedMutex>() {
                let crop = D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: width,
                    bottom: height,
                    back: 1,
                };
                mutex.AcquireSync(0, 500)?;
                if from {
                    match region {
                        Some(region) => context.CopySubresourceRegion(
                            &self.intermediate_texture,
                            0,
                            0,
                            0,
                            0,
                            texture,
                            region,
                            Some(&crop),
                        ),
                        None => context.CopyResource(&self.intermediate_texture, texture),
                    }
                } else {
                    match region {
                        Some(region) => context.CopySubresourceRegion(
                            texture,
                            region,
                            0,
                            0,
                            0,
                            &self.intermediate_texture,
                            0,
                            Some(&crop),
                        ),
                        None => context.CopyResource(texture, &self.intermediate_texture),
                    }
                }
                self.fence.synchronize(context)?;
                mutex.ReleaseSync(0)?;
                Ok(())
            } else {
                Err(windows::core::Error::new(
                    E_NOINTERFACE,
                    "Failed to query IDXGIKeyedMutex",
                ))
            }
        }
    }
}

pub fn get_shared_texture_d3d11(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<(HANDLE, DirectX11SharedTexture), Box<dyn std::error::Error>> {
    unsafe {
        // Try to open or create shared handle if possible
        /*if let Ok(dxgi_resource) = texture.cast::<IDXGIResource1>() {
            if let Ok(handle) = dxgi_resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            ) {
                if !handle.is_invalid() {
                    return Ok((handle, None));
                }
            }
        }*/

        // No shared handle and not possible to create one.
        // We need to create a new texture and use texture copy from our original one.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        let src_format = desc.Format;
        let src_bind = desc.BindFlags;
        let src_misc = desc.MiscFlags;
        log::debug!(
            "[d3d11_shared] source texture: format={:?} {}x{} bind=0x{:x} misc=0x{:x} array={}",
            src_format,
            desc.Width,
            desc.Height,
            src_bind,
            src_misc,
            desc.ArraySize,
        );

        desc.Width = width;
        desc.Height = height;
        desc.MiscFlags |= D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32
            | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32;
        desc.ArraySize = 1;
        // The source texture has D3D11_BIND_DECODER only (no D3D11_BIND_SHADER_RESOURCE,
        // which Intel Arc rejects). The intermediate shared texture is not decoded into —
        // it just needs to be importable by D3D12/Vulkan via the NTHandle, so
        // D3D11_BIND_SHADER_RESOURCE is the right flag here.
        desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;

        log::debug!(
            "[d3d11_shared] intermediate desc: format={:?} {}x{} bind=0x{:x} misc=0x{:x}",
            desc.Format,
            desc.Width,
            desc.Height,
            desc.BindFlags,
            desc.MiscFlags,
        );

        let mut new_texture = None;
        if let Err(e) = device.CreateTexture2D(&desc, None, Some(&mut new_texture)) {
            log::error!(
                "[d3d11_shared] CreateTexture2D failed: hr=0x{:08x} ({}) — format={:?} bind=0x{:x} misc=0x{:x}",
                e.code().0 as u32,
                e.message(),
                desc.Format,
                desc.BindFlags,
                desc.MiscFlags,
            );
            log_d3d11_device_removed_reason(device);
            return Err(Box::new(e));
        }

        if let Some(new_texture) = new_texture {
            let dxgi_resource: IDXGIResource1 = new_texture.cast::<IDXGIResource1>().map_err(|e| {
                log::error!("[d3d11_shared] cast to IDXGIResource1 failed: {:?}", e);
                e
            })?;
            let handle = match dxgi_resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    log::error!(
                        "[d3d11_shared] CreateSharedHandle failed: hr=0x{:08x} ({})",
                        e.code().0 as u32,
                        e.message(),
                    );
                    log_d3d11_device_removed_reason(device);
                    return Err(Box::new(e));
                }
            };

            Ok((
                handle,
                DirectX11SharedTexture {
                    intermediate_texture: new_texture,
                    fence: DirectX11Fence::new(device)?,
                },
            ))
        } else {
            Err("Call to CreateTexture2D failed (no texture out)".into())
        }
    }
}

fn drr_name(code: u32) -> &'static str {
    match code {
        0x887A0005 => "DXGI_ERROR_DEVICE_REMOVED",
        0x887A0006 => "DXGI_ERROR_DEVICE_HUNG (TDR)",
        0x887A0007 => "DXGI_ERROR_DEVICE_RESET",
        0x887A0020 => "DXGI_ERROR_DRIVER_INTERNAL_ERROR",
        0x887A002D => "DXGI_ERROR_ACCESS_LOST",
        _ => "unknown",
    }
}

/// Query the D3D11 device for a removed/hung/reset reason and log it.
/// Returns whether the device is in a removed state.
fn log_d3d11_device_removed_reason(device: &ID3D11Device) -> bool {
    unsafe {
        match device.GetDeviceRemovedReason() {
            Ok(()) => false,
            Err(e) => {
                let code = e.code().0 as u32;
                log::error!(
                    "[d3d11_shared] D3D11 device-removed reason: 0x{:08x} ({})",
                    code,
                    drr_name(code),
                );
                true
            }
        }
    }
}

/// Same as above but for the DX12 device wgpu is holding.
pub fn log_dx12_device_removed_reason(device: &wgpu::Device) {
    unsafe {
        let Some(hdevice) = device.as_hal::<Dx12>() else {
            return;
        };
        let raw_device = hdevice.raw_device();
        match raw_device.GetDeviceRemovedReason() {
            Ok(()) => {
                log::trace!("[dx12] device reports healthy via GetDeviceRemovedReason");
            }
            Err(e) => {
                let code = e.code().0 as u32;
                log::error!("[dx12] device-removed reason: 0x{:08x} ({})", code, drr_name(code));
            }
        }
    }
}

#[allow(dead_code)]
fn get_dx11_shared_texture_pitch(
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> Result<u32, &'static str> {
    unsafe {
        let mut mapped_resource: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();

        let resource: ID3D11Resource = texture
            .cast()
            .map_err(|_| "Failed to cast to ID3D11Resource")?;

        // Map the shared texture
        let hr = context.Map(
            &resource,
            0,              // Mip level 0
            D3D11_MAP_READ, // Read access
            0,
            Some(&mut mapped_resource),
        );

        if hr.is_err() {
            return Err("Failed to map shared texture.");
        }

        let row_pitch = mapped_resource.RowPitch;

        // Unmap the resource
        context.Unmap(&resource, 0);

        Ok(row_pitch)
    }
}

fn get_vulkan_shared_texture_pitch(device: &ash::Device, image: vk::Image) -> u64 {
    let subresource = vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::PLANE_1, // Y plane (for YUV)
        mip_level: 0,
        array_layer: 0,
    };

    let layout = unsafe { device.get_image_subresource_layout(image, subresource) };
    layout.row_pitch // Vulkan's expected row stride
}

pub fn create_vk_image_from_d3d11_texture(
    device: &wgpu::Device,
    d3d11_device: &ID3D11Device,
    d3d11_device_context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    region: Option<u32>,
) -> Result<VkImageMemory, Box<dyn std::error::Error>> {
    unsafe {
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut src_desc);

        let (handle, shared_texture) =
            get_shared_texture_d3d11(d3d11_device, texture, width, height)?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        shared_texture.intermediate_texture.GetDesc(&mut desc);

        _ = shared_texture.synchronized_copy_from(
            d3d11_device_context,
            texture,
            width,
            height,
            region,
        );

        shared_texture.intermediate_texture.GetDesc(&mut desc);
        d3d11_device_context.Flush();
        /*
                let mut staging_texture = None;
                let texture_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..Default::default()
                };
                let hr = d3d11_device.CreateTexture2D(&texture_desc, None, Some(&mut staging_texture));

                let ss = staging_texture.unwrap();
                d3d11_device_context.CopyResource(&ss, texture);

                let dx_pitch = get_dx11_shared_texture_pitch(d3d11_device_context, &ss).unwrap();
                log::trace!("DX pitch: {}", dx_pitch);
        */
        let raw_image = {
            let raw_dev = device
                .as_hal::<Vulkan>()
                .ok_or("wgpu backend is not Vulkan")?;
            let raw_device = raw_dev.raw_device();
            let physical_device = raw_dev.raw_physical_device();
            let instance = raw_dev.shared_instance().raw_instance();

            let handle_type = vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE;

            let mut import_memory_info = vk::ImportMemoryWin32HandleInfoKHR::default()
                .handle_type(handle_type)
                .handle(handle.0 as isize);

            let mut ext_create_info =
                vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);

            let image_create_info = ImageCreateInfo::default()
                .push_next(&mut ext_create_info)
                .image_type(vk::ImageType::TYPE_2D)
                .format(super::video_vulkan::format_wgpu_to_vulkan(
                    format_dxgi_to_wgpu(desc.Format),
                ))
                .extent(vk::Extent3D {
                    width: desc.Width,
                    height: desc.Height,
                    depth: desc.ArraySize,
                })
                .mip_levels(desc.MipLevels)
                .flags(vk::ImageCreateFlags::ALIAS | vk::ImageCreateFlags::MUTABLE_FORMAT)
                .array_layers(desc.ArraySize)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);

            let raw_image = raw_device.create_image(&image_create_info, None)?;

            let mem_requirements = raw_device.get_image_memory_requirements(raw_image);

            let mem_properties =
                instance.get_physical_device_memory_properties(physical_device);

            let index = mem_properties
                .memory_types
                .iter()
                .enumerate()
                .position(|(i, t)| {
                    ((1 << i) & mem_requirements.memory_type_bits) != 0
                        && t.property_flags
                            .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                });

            let index = index.ok_or("Failed to get DEVICE_LOCAL memory index")?;

            let allocate_info = vk::MemoryAllocateInfo::default()
                .allocation_size(mem_requirements.size)
                .push_next(&mut import_memory_info)
                .memory_type_index(index as u32);

            let allocated_memory = raw_device.allocate_memory(&allocate_info, None)?;

            let pitch = get_vulkan_shared_texture_pitch(raw_device, raw_image);
            log::trace!("Vulkan pitch: {}", pitch);
            raw_device.bind_image_memory(raw_image, allocated_memory, 0)?;

            VkImageMemory {
                raw_image,
                memory: allocated_memory,
            }
        };

        let _ = CloseHandle(handle);

        Ok(raw_image)
    }
}

pub fn create_dx12_resource_from_d3d11_texture(
    device: &wgpu::Device,
    d3d11_device: &ID3D11Device,
    d3d11_device_context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    region: Option<u32>,
) -> Result<Direct3D12::ID3D12Resource, Box<dyn std::error::Error>> {
    unsafe {
        log::trace!(
            "[dx12_import] begin: {}x{} region={:?}",
            width,
            height,
            region
        );

        let (handle, shared_texture) =
            get_shared_texture_d3d11(d3d11_device, texture, width, height)?;
        log::trace!("[dx12_import] got shared NT handle, doing synchronized copy");

        if let Err(e) = shared_texture.synchronized_copy_from(
            d3d11_device_context,
            texture,
            width,
            height,
            region,
        ) {
            log::error!(
                "[dx12_import] synchronized_copy_from failed: hr=0x{:08x} ({})",
                e.code().0 as u32,
                e.message(),
            );
            log_d3d11_device_removed_reason(d3d11_device);
            log_dx12_device_removed_reason(device);
            let _ = CloseHandle(handle);
            return Err(Box::new(e));
        }

        let raw_image = {
            let hdevice = device
                .as_hal::<Dx12>()
                .ok_or("wgpu backend is not DX12")?;
            let raw_device = hdevice.raw_device();
            let mut resource = None::<Direct3D12::ID3D12Resource>;
            if let Err(e) = raw_device.OpenSharedHandle(handle, &mut resource) {
                log::error!(
                    "[dx12_import] OpenSharedHandle failed: hr=0x{:08x} ({})",
                    e.code().0 as u32,
                    e.message(),
                );
                log_d3d11_device_removed_reason(d3d11_device);
                log_dx12_device_removed_reason(device);
                let _ = CloseHandle(handle);
                return Err(Box::new(e));
            }
            let _ = CloseHandle(handle);
            let resource = resource.ok_or("OpenSharedHandle returned no resource")?;
            let imported_desc = resource.GetDesc();
            log::trace!(
                "[dx12_import] OpenSharedHandle OK: format={:?} {}x{} flags=0x{:x}",
                imported_desc.Format,
                imported_desc.Width,
                imported_desc.Height,
                imported_desc.Flags.0,
            );
            resource
        };

        Ok(raw_image)
    }
}

pub fn create_texture_from_dx12_resource(
    device: &wgpu::Device,
    resource: Direct3D12::ID3D12Resource,
    desc: &wgpu::TextureDescriptor,
) -> wgpu::Texture {
    unsafe {
        log::trace!(
            "[dx12_wrap] before texture_from_raw: format={:?} size={}x{}x{}",
            desc.format,
            desc.size.width,
            desc.size.height,
            desc.size.depth_or_array_layers,
        );
        let texture = <Dx12 as wgpu::hal::Api>::Device::texture_from_raw(
            resource,
            desc.format,
            desc.dimension,
            desc.size,
            1,
            1,
        );
        log::trace!("[dx12_wrap] texture_from_raw OK; device pre-check:");
        log_dx12_device_removed_reason(device);

        log::trace!("[dx12_wrap] before create_texture_from_hal");
        // wgpu 29.0.3: create_texture_from_hal derives HAL usage from the
        // descriptor; the old explicit `TextureUses` hint arg was removed. Make
        // the descriptor advertise the same intent (sampling + copy-src).
        let mut desc = desc.clone();
        desc.usage |= wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
        let result = device.create_texture_from_hal::<Dx12>(texture, &desc);
        log::trace!("[dx12_wrap] create_texture_from_hal returned; device post-check:");
        log_dx12_device_removed_reason(device);
        result
    }
}

/*pub fn create_native_shared_texture_dx12(device: &wgpu::Device, desc: &wgpu::TextureDescriptor) -> Result<(::d3d12::Resource, usize, usize), String> {
    unsafe {
        device.as_hal::<Dx12, _, _>(|hdevice| {
            hdevice.map(|hdevice| {
                let raw_device = hdevice.raw_device();

                let mut resource = None::<Direct3D12::ID3D12Resource>;

                { // Texture
                    let raw_desc = Direct3D12::D3D12_RESOURCE_DESC {
                        Dimension: Direct3D12::D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                        Alignment: 0,
                        Width: desc.size.width as u64,
                        Height: desc.size.height,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        Format: format_wgpu_to_dxgi(desc.format).0,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: desc.sample_count,
                            Quality: 0,
                        },
                        Layout: Direct3D12::D3D12_TEXTURE_LAYOUT_UNKNOWN,
                        Flags: Direct3D12::D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                    };
                    let heap_properties = Direct3D12::D3D12_HEAP_PROPERTIES {
                        Type: Direct3D12::D3D12_HEAP_TYPE_CUSTOM,
                        CPUPageProperty: Direct3D12::D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE,
                        MemoryPoolPreference: Direct3D12::D3D12_MEMORY_POOL_L0,
                        CreationNodeMask: 0,
                        VisibleNodeMask: 0,
                    };

                    raw_device.CreateCommittedResource(
                        &heap_properties,
                        Direct3D12::D3D12_HEAP_FLAG_SHARED,
                        &raw_desc,
                        Direct3D12::D3D12_RESOURCE_STATE_COMMON,
                        None, // clear value
                        &mut resource,
                    ).map_err(|e| format!("{e:?}"))?;
                }

                let resource = resource.unwrap();

                let actual_desc = resource.GetDesc();
                let ai = raw_device.GetResourceAllocationInfo(0, &[actual_desc]);
                let actual_size = ai.SizeInBytes as usize;

                match raw_device.CreateSharedHandle(&resource, None, GENERIC_ALL.0, windows::core::PCWSTR::null()) {
                    Ok(handle) => Ok::<(Direct3D12::ID3D12Resource, HANDLE, usize), String>((resource, handle, actual_size)),
                    Err(e) => Err(e.to_string())
                }
            })
        }).unwrap() // TODO: unwrap
    }
}*/

#[allow(dead_code)]
pub fn create_native_shared_buffer_dx12(
    device: &wgpu::Device,
    size: usize,
) -> Result<(Direct3D12::ID3D12Resource, HANDLE, usize), String> {
    unsafe {
        let hdevice = device
            .as_hal::<Dx12>()
            .ok_or_else(|| "wgpu backend is not DX12".to_string())?;
        let raw_device = hdevice.raw_device();

        let mut resource = None::<Direct3D12::ID3D12Resource>;

        let raw_desc = Direct3D12::D3D12_RESOURCE_DESC {
            Dimension: Direct3D12::D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: size as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: Direct3D12::D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: Direct3D12::D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        };
        let heap_properties = Direct3D12::D3D12_HEAP_PROPERTIES {
            Type: Direct3D12::D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: Direct3D12::D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE,
            MemoryPoolPreference: Direct3D12::D3D12_MEMORY_POOL_L0,
            CreationNodeMask: 0,
            VisibleNodeMask: 0,
        };

        raw_device
            .CreateCommittedResource(
                &heap_properties,
                Direct3D12::D3D12_HEAP_FLAG_SHARED,
                &raw_desc,
                Direct3D12::D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            )
            .map_err(|e| format!("{e:?}"))?;

        let resource = resource.ok_or("CreateCommittedResource returned no resource")?;
        let actual_desc = resource.GetDesc();
        let ai = raw_device.GetResourceAllocationInfo(0, &[actual_desc]);
        let actual_size = ai.SizeInBytes as usize;

        let handle = raw_device
            .CreateSharedHandle(&resource, None, GENERIC_ALL.0, windows::core::PCWSTR::null())
            .map_err(|e| e.to_string())?;
        Ok((resource, handle, actual_size))
    }
}

pub fn format_dxgi_to_wgpu(format: DXGI_FORMAT) -> TextureFormat {
    match format {
        DXGI_FORMAT_NV12 => TextureFormat::NV12,
        DXGI_FORMAT_P010 => TextureFormat::P010,
        DXGI_FORMAT_R8_UNORM => TextureFormat::R8Unorm,
        DXGI_FORMAT_R8_SNORM => TextureFormat::R8Snorm,
        DXGI_FORMAT_R8_UINT => TextureFormat::R8Uint,
        DXGI_FORMAT_R8_SINT => TextureFormat::R8Sint,
        DXGI_FORMAT_R16_UINT => TextureFormat::R16Uint,
        DXGI_FORMAT_R16_SINT => TextureFormat::R16Sint,
        DXGI_FORMAT_R16_UNORM => TextureFormat::R16Unorm,
        DXGI_FORMAT_R16_SNORM => TextureFormat::R16Snorm,
        DXGI_FORMAT_R16_FLOAT => TextureFormat::R16Float,
        DXGI_FORMAT_R8G8_UNORM => TextureFormat::Rg8Unorm,
        DXGI_FORMAT_R8G8_SNORM => TextureFormat::Rg8Snorm,
        DXGI_FORMAT_R8G8_UINT => TextureFormat::Rg8Uint,
        DXGI_FORMAT_R8G8_SINT => TextureFormat::Rg8Sint,
        DXGI_FORMAT_R16G16_UNORM => TextureFormat::Rg16Unorm,
        DXGI_FORMAT_R16G16_SNORM => TextureFormat::Rg16Snorm,
        DXGI_FORMAT_R32_UINT => TextureFormat::R32Uint,
        DXGI_FORMAT_R32_SINT => TextureFormat::R32Sint,
        DXGI_FORMAT_R32_FLOAT => TextureFormat::R32Float,
        DXGI_FORMAT_R16G16_UINT => TextureFormat::Rg16Uint,
        DXGI_FORMAT_R16G16_SINT => TextureFormat::Rg16Sint,
        DXGI_FORMAT_R16G16_FLOAT => TextureFormat::Rg16Float,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => TextureFormat::Rgba8Unorm,
        DXGI_FORMAT_R8G8B8A8_UNORM => TextureFormat::Rgba8Unorm,
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => TextureFormat::Rgba8UnormSrgb,
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => TextureFormat::Bgra8UnormSrgb,
        DXGI_FORMAT_R8G8B8A8_SNORM => TextureFormat::Rgba8Snorm,
        DXGI_FORMAT_B8G8R8A8_UNORM => TextureFormat::Bgra8Unorm,
        DXGI_FORMAT_R8G8B8A8_UINT => TextureFormat::Rgba8Uint,
        DXGI_FORMAT_R8G8B8A8_SINT => TextureFormat::Rgba8Sint,
        DXGI_FORMAT_R10G10B10A2_UNORM => TextureFormat::Rgb10a2Unorm,
        DXGI_FORMAT_R10G10B10A2_UINT => TextureFormat::Rgb10a2Uint,
        DXGI_FORMAT_R11G11B10_FLOAT => TextureFormat::Rg11b10Ufloat,
        DXGI_FORMAT_R32G32_UINT => TextureFormat::Rg32Uint,
        DXGI_FORMAT_R32G32_SINT => TextureFormat::Rg32Sint,
        DXGI_FORMAT_R32G32_FLOAT => TextureFormat::Rg32Float,
        DXGI_FORMAT_R16G16B16A16_UINT => TextureFormat::Rgba16Uint,
        DXGI_FORMAT_R16G16B16A16_SINT => TextureFormat::Rgba16Sint,
        DXGI_FORMAT_R16G16B16A16_UNORM => TextureFormat::Rgba16Unorm,
        DXGI_FORMAT_R16G16B16A16_SNORM => TextureFormat::Rgba16Snorm,
        DXGI_FORMAT_R16G16B16A16_FLOAT => TextureFormat::Rgba16Float,
        DXGI_FORMAT_R32G32B32A32_UINT => TextureFormat::Rgba32Uint,
        DXGI_FORMAT_R32G32B32A32_SINT => TextureFormat::Rgba32Sint,
        DXGI_FORMAT_R32G32B32A32_FLOAT => TextureFormat::Rgba32Float,
        DXGI_FORMAT_D32_FLOAT => TextureFormat::Depth32Float,
        DXGI_FORMAT_D32_FLOAT_S8X24_UINT => TextureFormat::Depth32FloatStencil8,
        DXGI_FORMAT_R9G9B9E5_SHAREDEXP => TextureFormat::Rgb9e5Ufloat,
        DXGI_FORMAT_BC1_UNORM => TextureFormat::Bc1RgbaUnorm,
        DXGI_FORMAT_BC1_UNORM_SRGB => TextureFormat::Bc1RgbaUnormSrgb,
        DXGI_FORMAT_BC2_UNORM => TextureFormat::Bc2RgbaUnorm,
        DXGI_FORMAT_BC2_UNORM_SRGB => TextureFormat::Bc2RgbaUnormSrgb,
        DXGI_FORMAT_BC3_UNORM => TextureFormat::Bc3RgbaUnorm,
        DXGI_FORMAT_BC3_UNORM_SRGB => TextureFormat::Bc3RgbaUnormSrgb,
        DXGI_FORMAT_BC4_UNORM => TextureFormat::Bc4RUnorm,
        DXGI_FORMAT_BC4_SNORM => TextureFormat::Bc4RSnorm,
        DXGI_FORMAT_BC5_UNORM => TextureFormat::Bc5RgUnorm,
        DXGI_FORMAT_BC5_SNORM => TextureFormat::Bc5RgSnorm,
        DXGI_FORMAT_BC6H_UF16 => TextureFormat::Bc6hRgbUfloat,
        DXGI_FORMAT_BC6H_SF16 => TextureFormat::Bc6hRgbFloat,
        DXGI_FORMAT_BC7_UNORM => TextureFormat::Bc7RgbaUnorm,
        DXGI_FORMAT_BC7_UNORM_SRGB => TextureFormat::Bc7RgbaUnormSrgb,
        _ => panic!("Unsupported texture format: {:?}", format),
    }
}

#[allow(dead_code)]
pub fn format_wgpu_to_dxgi(format: TextureFormat) -> DXGI_FORMAT {
    match format {
        TextureFormat::NV12 => DXGI_FORMAT_NV12,
        TextureFormat::P010 => DXGI_FORMAT_P010,
        TextureFormat::R8Unorm => DXGI_FORMAT_R8_UNORM,
        TextureFormat::R8Snorm => DXGI_FORMAT_R8_SNORM,
        TextureFormat::R8Uint => DXGI_FORMAT_R8_UINT,
        TextureFormat::R8Sint => DXGI_FORMAT_R8_SINT,
        TextureFormat::R16Uint => DXGI_FORMAT_R16_UINT,
        TextureFormat::R16Sint => DXGI_FORMAT_R16_SINT,
        TextureFormat::R16Unorm => DXGI_FORMAT_R16_UNORM,
        TextureFormat::R16Snorm => DXGI_FORMAT_R16_SNORM,
        TextureFormat::R16Float => DXGI_FORMAT_R16_FLOAT,
        TextureFormat::Rg8Unorm => DXGI_FORMAT_R8G8_UNORM,
        TextureFormat::Rg8Snorm => DXGI_FORMAT_R8G8_SNORM,
        TextureFormat::Rg8Uint => DXGI_FORMAT_R8G8_UINT,
        TextureFormat::Rg8Sint => DXGI_FORMAT_R8G8_SINT,
        TextureFormat::Rg16Unorm => DXGI_FORMAT_R16G16_UNORM,
        TextureFormat::Rg16Snorm => DXGI_FORMAT_R16G16_SNORM,
        TextureFormat::R32Uint => DXGI_FORMAT_R32_UINT,
        TextureFormat::R32Sint => DXGI_FORMAT_R32_SINT,
        TextureFormat::R32Float => DXGI_FORMAT_R32_FLOAT,
        TextureFormat::Rg16Uint => DXGI_FORMAT_R16G16_UINT,
        TextureFormat::Rg16Sint => DXGI_FORMAT_R16G16_SINT,
        TextureFormat::Rg16Float => DXGI_FORMAT_R16G16_FLOAT,
        TextureFormat::Rgba8Unorm => DXGI_FORMAT_R8G8B8A8_UNORM,
        TextureFormat::Rgba8UnormSrgb => DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
        TextureFormat::Bgra8UnormSrgb => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        TextureFormat::Rgba8Snorm => DXGI_FORMAT_R8G8B8A8_SNORM,
        TextureFormat::Bgra8Unorm => DXGI_FORMAT_B8G8R8A8_UNORM,
        TextureFormat::Rgba8Uint => DXGI_FORMAT_R8G8B8A8_UINT,
        TextureFormat::Rgba8Sint => DXGI_FORMAT_R8G8B8A8_SINT,
        TextureFormat::Rgb10a2Unorm => DXGI_FORMAT_R10G10B10A2_UNORM,
        TextureFormat::Rg11b10Ufloat => DXGI_FORMAT_R11G11B10_FLOAT,
        TextureFormat::Rg32Uint => DXGI_FORMAT_R32G32_UINT,
        TextureFormat::Rg32Sint => DXGI_FORMAT_R32G32_SINT,
        TextureFormat::Rg32Float => DXGI_FORMAT_R32G32_FLOAT,
        TextureFormat::Rgba16Uint => DXGI_FORMAT_R16G16B16A16_UINT,
        TextureFormat::Rgba16Sint => DXGI_FORMAT_R16G16B16A16_SINT,
        TextureFormat::Rgba16Unorm => DXGI_FORMAT_R16G16B16A16_UNORM,
        TextureFormat::Rgba16Snorm => DXGI_FORMAT_R16G16B16A16_SNORM,
        TextureFormat::Rgba16Float => DXGI_FORMAT_R16G16B16A16_FLOAT,
        TextureFormat::Rgba32Uint => DXGI_FORMAT_R32G32B32A32_UINT,
        TextureFormat::Rgba32Sint => DXGI_FORMAT_R32G32B32A32_SINT,
        TextureFormat::Rgba32Float => DXGI_FORMAT_R32G32B32A32_FLOAT,
        TextureFormat::Depth32Float => DXGI_FORMAT_D32_FLOAT,
        TextureFormat::Depth32FloatStencil8 => DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
        TextureFormat::Rgb9e5Ufloat => DXGI_FORMAT_R9G9B9E5_SHAREDEXP,
        TextureFormat::Bc1RgbaUnorm => DXGI_FORMAT_BC1_UNORM,
        TextureFormat::Bc1RgbaUnormSrgb => DXGI_FORMAT_BC1_UNORM_SRGB,
        TextureFormat::Bc2RgbaUnorm => DXGI_FORMAT_BC2_UNORM,
        TextureFormat::Bc2RgbaUnormSrgb => DXGI_FORMAT_BC2_UNORM_SRGB,
        TextureFormat::Bc3RgbaUnorm => DXGI_FORMAT_BC3_UNORM,
        TextureFormat::Bc3RgbaUnormSrgb => DXGI_FORMAT_BC3_UNORM_SRGB,
        TextureFormat::Bc4RUnorm => DXGI_FORMAT_BC4_UNORM,
        TextureFormat::Bc4RSnorm => DXGI_FORMAT_BC4_SNORM,
        TextureFormat::Bc5RgUnorm => DXGI_FORMAT_BC5_UNORM,
        TextureFormat::Bc5RgSnorm => DXGI_FORMAT_BC5_SNORM,
        TextureFormat::Bc6hRgbUfloat => DXGI_FORMAT_BC6H_UF16,
        TextureFormat::Bc6hRgbFloat => DXGI_FORMAT_BC6H_SF16,
        TextureFormat::Bc7RgbaUnorm => DXGI_FORMAT_BC7_UNORM,
        TextureFormat::Bc7RgbaUnormSrgb => DXGI_FORMAT_BC7_UNORM_SRGB,
        _ => panic!("Unsupported texture format: {:?}", format),
    }
}
