use ash::vk;
use wgpu::hal::api::Dx12;
use wgpu::hal::api::Vulkan;
use wgpu::TextureFormat;
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, E_NOINTERFACE, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::{Direct3D11::*, Direct3D12, Dxgi::Common::*, Dxgi::*};
use windows::Win32::System::Threading::{CreateEventA, WaitForSingleObject};


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

impl AVD3D11VADeviceContext {
    /// Run `f` holding FFmpeg's lock on the shared D3D11 immediate context.
    ///
    /// The decoder thread drives the same `ID3D11DeviceContext` (FFmpeg takes
    /// this lock around its own calls), and the import copies on it from the
    /// render thread. An immediate context is not thread-safe, and FFmpeg's
    /// API contract is that every user takes `lock`/`unlock`; the import did
    /// not. `lock` is set by `av_hwdevice_ctx_init` (a default mutex when
    /// the app gives none).
    ///
    /// # Safety
    /// `this` must point at the live `hwctx` of an initialised D3D11VA device.
    pub unsafe fn with_lock<R>(this: *mut Self, f: impl FnOnce() -> R) -> R {
        let (lock, unlock, ctx) = ((*this).lock, (*this).unlock, (*this).lock_ctx);
        if let Some(lock) = lock {
            lock(ctx);
        }
        let r = f();
        if let Some(unlock) = unlock {
            unlock(ctx);
        }
        r
    }
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

            // SHARED so the D3D12 queue can wait on it on the GPU
            // (`open_on_d3d12`); the CPU wait below stays as the fallback.
            device.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)?;
            let fence = fence.ok_or(windows::core::Error::new(E_FAIL, "Failed to create fence"))?;

            let event = CreateEventA(None, false, false, windows::core::PCSTR::null())?;

            Ok(Self {
                fence,
                event,
                fence_value: Default::default(),
            })
        }
    }
    /// NT handle to this fence (D3D12-fence compatible), e.g. to import it
    /// into Vulkan as a timeline semaphore. The caller closes it.
    pub fn shared_handle(&self) -> windows::core::Result<HANDLE> {
        unsafe { self.fence.CreateSharedHandle(None, GENERIC_ALL.0, windows::core::PCWSTR::null()) }
    }

    /// This fence as a D3D12 fence on `device`, for [`Self::signal`] +
    /// `ID3D12CommandQueue::Wait`.
    pub fn open_on_d3d12(&self, device: &Direct3D12::ID3D12Device) -> windows::core::Result<Direct3D12::ID3D12Fence> {
        unsafe {
            let handle = self.fence.CreateSharedHandle(None, GENERIC_ALL.0, windows::core::PCWSTR::null())?;
            let mut fence = None::<Direct3D12::ID3D12Fence>;
            let opened = device.OpenSharedHandle(handle, &mut fence);
            let _ = CloseHandle(handle);
            opened?;
            fence.ok_or_else(|| windows::core::Error::new(E_FAIL, "OpenSharedHandle returned no fence"))
        }
    }

    /// Signal the next value after the work queued on `context` and flush,
    /// without waiting: the consumer waits for the returned value on the GPU.
    pub fn signal(&self, context: &ID3D11DeviceContext) -> windows::core::Result<u64> {
        let context4 = context.cast::<ID3D11DeviceContext4>()?;
        let v = self
            .fence_value
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        unsafe {
            context4.Signal(&self.fence, v)?;
            // A queued Signal the driver never submits would leave the D3D12
            // wait hanging.
            context.Flush();
        }
        Ok(v)
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
            let waited = WaitForSingleObject(self.event, 5000);
            if waited != windows::Win32::Foundation::WAIT_OBJECT_0 {
                // Not an error for the caller yet (an Err here would panic the
                // render task via the frame import's unwrap); make it visible.
                log::warn!("[d3d11_fence] copy not signalled within 5 s ({:?}); the frame may be incomplete", waited);
            }
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
    /// [`Self::synchronized_copy_from`] without the CPU wait: returns the fence
    /// value the copy signals, for a GPU wait on the consumer queue.
    pub fn signalled_copy_from(
        &self,
        context: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        width: u32,
        height: u32,
        region: Option<u32>,
    ) -> windows::core::Result<u64> {
        unsafe {
            let mutex = self.intermediate_texture.cast::<IDXGIKeyedMutex>()?;
            mutex.AcquireSync(0, 500)?;
            self.copy(context, tex, width, height, region);
            let signalled = self.fence.signal(context);
            mutex.ReleaseSync(0)?;
            signalled
        }
    }

    fn copy(&self, context: &ID3D11DeviceContext, texture: &ID3D11Texture2D, width: u32, height: u32, region: Option<u32>) {
        let crop = D3D11_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
        unsafe {
            match region {
                Some(region) => context.CopySubresourceRegion(&self.intermediate_texture, 0, 0, 0, 0, texture, region, Some(&crop)),
                None => context.CopyResource(&self.intermediate_texture, texture),
            }
        }
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

/// LUID of the GPU wgpu renders on (DX12 backend), packed as
/// `(HighPart << 32) | LowPart`; `None` on another backend.
pub fn dx12_adapter_luid(device: &wgpu::Device) -> Option<u64> {
    unsafe {
        let hdevice = device.as_hal::<Dx12>()?;
        let luid = hdevice.raw_device().GetAdapterLuid();
        Some(((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64)
    }
}

/// LUID of the GPU wgpu renders on, on either Windows backend (DX12 or
/// Vulkan), packed like [`dx12_adapter_luid`]. The D3D11VA decoder must open
/// on this GPU: its frames reach the renderer through a shared handle, which
/// cannot cross adapters.
pub fn render_adapter_luid(device: &wgpu::Device) -> Option<u64> {
    dx12_adapter_luid(device).or_else(|| vulkan_adapter_luid(device))
}

/// LUID of the Vulkan physical device wgpu renders on, when the driver
/// reports one (`VkPhysicalDeviceIDProperties::deviceLUIDValid`).
pub fn vulkan_adapter_luid(device: &wgpu::Device) -> Option<u64> {
    unsafe {
        let hdevice = device.as_hal::<Vulkan>()?;
        let instance = hdevice.shared_instance().raw_instance();
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        instance.get_physical_device_properties2(hdevice.raw_physical_device(), &mut props);
        // LUID { LowPart: u32, HighPart: i32 } in memory order.
        (id.device_luid_valid == vk::TRUE).then(|| u64::from_le_bytes(id.device_luid))
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

/// One slot of [`VulkanImportPool`]: the D3D11 intermediate texture the
/// decoder frame is copied into, and the same memory seen from Vulkan.
struct VulkanImportSlot {
    shared: DirectX11SharedTexture,
    texture: wgpu::Texture,
    memory: vk::DeviceMemory,
    /// The slot's D3D11 copy fence as a Vulkan timeline semaphore; `None`
    /// when the driver can't import it (the copy is then waited on the CPU).
    semaphore: Option<vk::Semaphore>,
}

/// D3D11 -> Vulkan counterpart of [`Dx12ImportPool`]: a few intermediate
/// shared textures, each imported into Vulkan ONCE (VkImage + dedicated
/// imported memory, wrapped as a wgpu texture), reused frame after frame.
/// A frame is then one CopySubresourceRegion on the decoder's D3D11 context.
///
/// The Vulkan queue waits for the copy on the GPU, like the DX12 pool: each
/// slot's D3D11 fence is imported as a timeline semaphore and the copy's
/// value queued with `Queue::add_wait_semaphore` (our wgpu fork) - the wait
/// lands in the next submission, and wgpu chains every later one after it.
/// A driver without `VK_KHR_external_semaphore_win32` falls back to a CPU
/// wait (D3D11 fence + event). Reuse is safe for the same reason as the DX12 pool: a slot comes round
/// again only after `VULKAN_IMPORT_SLOTS` frames, more than the renderer
/// keeps in flight.
struct VulkanImportPool {
    key: (usize, u64, i32, u32, u32),
    device: wgpu::Device,
    slots: Vec<VulkanImportSlot>,
    next: usize,
}

// COM pointers / Vulkan handles used only under the pool mutex.
unsafe impl Send for VulkanImportPool {}

impl Drop for VulkanImportPool {
    fn drop(&mut self) {
        // The textures may still be referenced by frames in flight: let the
        // GPU finish before the memory under them is freed.
        let slots: Vec<(vk::DeviceMemory, Option<vk::Semaphore>)> =
            self.slots.drain(..).map(|s| (s.memory, s.semaphore)).collect();
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        unsafe {
            if let Some(hdevice) = self.device.as_hal::<Vulkan>() {
                for (memory, semaphore) in slots {
                    if let Some(semaphore) = semaphore {
                        hdevice.raw_device().destroy_semaphore(semaphore, None);
                    }
                    hdevice.raw_device().free_memory(memory, None);
                }
            }
        }
    }
}

const VULKAN_IMPORT_SLOTS: usize = 4;

static VULKAN_IMPORT_POOL: std::sync::Mutex<Option<VulkanImportPool>> = std::sync::Mutex::new(None);

/// Import an intermediate shared D3D11 texture (NT handle) into Vulkan:
/// VkImage with external memory, memory type taken from the handle's own
/// properties, dedicated allocation (required for D3D11 textures by most
/// drivers). The handle is NOT consumed (NT handles stay with the caller).
unsafe fn import_d3d11_shared_into_vulkan(
    device: &wgpu::Device,
    handle: HANDLE,
    format: TextureFormat,
    width: u32,
    height: u32,
) -> Result<(wgpu::Texture, vk::DeviceMemory), Box<dyn std::error::Error>> {
    let hdevice = device.as_hal::<Vulkan>().ok_or("wgpu backend is not Vulkan")?;
    let raw = hdevice.raw_device();
    let instance = hdevice.shared_instance().raw_instance();
    let handle_type = vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE;

    let mut ext_image = vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);
    let image_info = vk::ImageCreateInfo::default()
        .push_next(&mut ext_image)
        .image_type(vk::ImageType::TYPE_2D)
        .format(super::video_vulkan::format_wgpu_to_vulkan(format))
        .extent(vk::Extent3D { width, height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        // Plane views (R8/RG8 or R16/RG16 of NV12/P010) need MUTABLE_FORMAT.
        .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = raw.create_image(&image_info, None)?;

    let reqs = raw.get_image_memory_requirements(image);
    let win32 = ash::khr::external_memory_win32::Device::new(instance, raw);
    let mut handle_props = vk::MemoryWin32HandlePropertiesKHR::default();
    if let Err(e) = win32.get_memory_win32_handle_properties(handle_type, handle.0 as _, &mut handle_props) {
        raw.destroy_image(image, None);
        return Err(format!("vkGetMemoryWin32HandlePropertiesKHR: {e:?}").into());
    }
    let usable = reqs.memory_type_bits & handle_props.memory_type_bits;
    let mem_props = instance.get_physical_device_memory_properties(hdevice.raw_physical_device());
    let pick = |want: vk::MemoryPropertyFlags| {
        (0..mem_props.memory_type_count)
            .find(|&i| usable & (1 << i) != 0 && mem_props.memory_types[i as usize].property_flags.contains(want))
    };
    let Some(type_index) = pick(vk::MemoryPropertyFlags::DEVICE_LOCAL).or_else(|| pick(vk::MemoryPropertyFlags::empty())) else {
        raw.destroy_image(image, None);
        return Err(format!(
            "no memory type for the D3D11 import (image 0x{:x}, handle 0x{:x})",
            reqs.memory_type_bits, handle_props.memory_type_bits
        )
        .into());
    };

    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut import = vk::ImportMemoryWin32HandleInfoKHR::default()
        .handle_type(handle_type)
        .handle(handle.0 as _);
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(reqs.size)
        .memory_type_index(type_index)
        .push_next(&mut import)
        .push_next(&mut dedicated);
    let memory = match raw.allocate_memory(&alloc, None) {
        Ok(m) => m,
        Err(e) => {
            raw.destroy_image(image, None);
            return Err(format!("vkAllocateMemory (D3D11 import): {e:?}").into());
        }
    };
    if let Err(e) = raw.bind_image_memory(image, memory, 0) {
        raw.free_memory(memory, None);
        raw.destroy_image(image, None);
        return Err(format!("vkBindImageMemory (D3D11 import): {e:?}").into());
    }
    // wgpu owns (and destroys) the image; the memory is freed by the pool.
    let texture = super::video_vulkan::create_texture_from_vk_image(device, image, width, height, format, true, true);
    Ok((texture, memory))
}

/// Import a D3D11 fence (by its NT handle) as a Vulkan timeline semaphore.
/// `None` when the driver lacks `VK_KHR_external_semaphore_win32` or rejects
/// the handle - the caller then waits on the CPU.
unsafe fn import_d3d11_fence_into_vulkan(device: &wgpu::Device, fence: &DirectX11Fence) -> Option<vk::Semaphore> {
    let hdevice = device.as_hal::<Vulkan>()?;
    let raw = hdevice.raw_device();
    let instance = hdevice.shared_instance().raw_instance();
    let handle = match fence.shared_handle() {
        Ok(h) => h,
        Err(e) => {
            log::warn!("[vk_import] sharing the D3D11 fence failed (hr=0x{:08x}); CPU wait", e.code().0 as u32);
            return None;
        }
    };
    let mut timeline = vk::SemaphoreTypeCreateInfo::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE)
        .initial_value(0);
    let semaphore = match raw.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut timeline), None) {
        Ok(s) => s,
        Err(e) => {
            let _ = CloseHandle(handle);
            log::warn!("[vk_import] timeline semaphore: {e:?}; CPU wait");
            return None;
        }
    };
    // Loading the entry point fails (null) when the extension isn't enabled;
    // the call below then errors instead of crashing - checked first.
    let get = instance.get_device_proc_addr(raw.handle(), c"vkImportSemaphoreWin32HandleKHR".as_ptr());
    let imported = if get.is_none() {
        Err(vk::Result::ERROR_EXTENSION_NOT_PRESENT)
    } else {
        let ext = ash::khr::external_semaphore_win32::Device::new(instance, raw);
        let info = vk::ImportSemaphoreWin32HandleInfoKHR::default()
            .semaphore(semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::D3D12_FENCE)
            .handle(handle.0 as _);
        ext.import_semaphore_win32_handle(&info)
    };
    let _ = CloseHandle(handle); // NT handle import does not take ownership
    match imported {
        Ok(()) => Some(semaphore),
        Err(e) => {
            raw.destroy_semaphore(semaphore, None);
            log::warn!("[vk_import] importing the D3D11 fence as a semaphore failed ({e:?}); CPU wait");
            None
        }
    }
}

/// Import a D3D11 decoder texture into Vulkan: the visible region is copied
/// into an intermediate shared texture from [`VulkanImportPool`], whose
/// Vulkan image was imported once when the slot was created.
pub fn import_d3d11_texture_vulkan_pooled(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    d3d11_device: &ID3D11Device,
    d3d11_device_context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    region: Option<u32>,
) -> Result<wgpu::Texture, Box<dyn std::error::Error>> {
    unsafe {
        let vk_device = {
            let hdevice = device.as_hal::<Vulkan>().ok_or("wgpu backend is not Vulkan")?;
            vk::Handle::as_raw(hdevice.raw_device().handle())
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        let format = format_dxgi_to_wgpu(desc.Format);
        let key = (d3d11_device.as_raw() as usize, vk_device, desc.Format.0, width, height);

        let mut guard = VULKAN_IMPORT_POOL.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().map(|p| p.key) != Some(key) {
            if guard.is_some() {
                log::debug!("[vk_import] pool rebuilt for {}x{} format={:?}", width, height, desc.Format);
            }
            *guard = None; // drop (and free) the old slots first
            *guard = Some(VulkanImportPool { key, device: device.clone(), slots: Vec::new(), next: 0 });
        }
        let pool = guard.as_mut().expect("pool set above");

        if pool.slots.len() < VULKAN_IMPORT_SLOTS {
            let (handle, shared) = get_shared_texture_d3d11(d3d11_device, texture, width, height)?;
            let imported = import_d3d11_shared_into_vulkan(device, handle, format, width, height);
            let _ = CloseHandle(handle);
            let (vk_texture, memory) = match imported {
                Ok(t) => t,
                Err(e) => {
                    log::error!("[vk_import] importing the shared D3D11 texture failed: {e}");
                    log_d3d11_device_removed_reason(d3d11_device);
                    return Err(e);
                }
            };
            if pool.slots.is_empty() {
                log::info!("[vk_import] D3D11 -> Vulkan import pool: {}x{} {:?}", width, height, format);
            }
            let semaphore = import_d3d11_fence_into_vulkan(device, &shared.fence);
            if pool.slots.is_empty() {
                log::info!(
                    "[vk_import] copy sync: {}",
                    if semaphore.is_some() { "GPU wait (imported D3D11 fence)" } else { "CPU wait" }
                );
            }
            pool.slots.push(VulkanImportSlot { shared, texture: vk_texture, memory, semaphore });
        }
        let idx = pool.next % pool.slots.len();
        pool.next = (idx + 1) % VULKAN_IMPORT_SLOTS;
        let slot = &pool.slots[idx];

        let copied = match slot.semaphore {
            Some(semaphore) => slot
                .shared
                .signalled_copy_from(d3d11_device_context, texture, width, height, region)
                .map(|value| {
                    if let Some(hqueue) = queue.as_hal::<Vulkan>() {
                        hqueue.add_wait_semaphore(semaphore, Some(value), vk::PipelineStageFlags::ALL_COMMANDS);
                    }
                }),
            None => slot.shared.synchronized_copy_from(d3d11_device_context, texture, width, height, region),
        };
        if let Err(e) = copied {
            log::error!(
                "[vk_import] synchronized_copy_from failed: hr=0x{:08x} ({})",
                e.code().0 as u32,
                e.message(),
            );
            log_d3d11_device_removed_reason(d3d11_device);
            *guard = None;
            return Err(Box::new(e));
        }
        Ok(slot.texture.clone())
    }
}

/// Intermediate shared textures for the D3D11 -> D3D12 import, reused frame
/// after frame.
///
/// Each frame used to create a new shared texture (visible size, NT handle +
/// keyed mutex), a D3D11 fence, an event, then OpenSharedHandle on the D3D12
/// side: 12-24 MB allocated and freed per 4K frame plus four kernel objects.
/// Measured on an Intel UHD at 720p: 2.15 ms per frame in the import. The
/// pool creates a few slots once per (devices, format, size) and only copies
/// into the next one; the D3D12 queue waits for the copy's fence on the GPU.
///
/// Reuse is safe because a slot comes round again only after `SLOTS` frames:
/// the renderer keeps at most two frames in flight
/// (`desired_maximum_frame_latency: 2`), so the GPU has finished sampling a
/// slot before the next copy into it.
struct Dx12ImportPool {
    key: (usize, usize, i32, u32, u32),
    /// Intermediate texture, its D3D12 view, and its fence opened on D3D12
    /// (`None`: sharing the fence failed, the copy waits on the CPU).
    slots: Vec<(DirectX11SharedTexture, Direct3D12::ID3D12Resource, Option<Direct3D12::ID3D12Fence>)>,
    next: usize,
}

// COM pointers used only under the pool mutex.
unsafe impl Send for Dx12ImportPool {}

const DX12_IMPORT_SLOTS: usize = 4;

static DX12_IMPORT_POOL: std::sync::Mutex<Option<Dx12ImportPool>> = std::sync::Mutex::new(None);

/// Import a D3D11 decoder texture into DX12: the visible region is copied into
/// an intermediate shared texture taken from [`Dx12ImportPool`] and opened on
/// the wgpu DX12 device.
pub fn import_d3d11_texture_pooled(
    device: &wgpu::Device,
    d3d11_device: &ID3D11Device,
    d3d11_device_context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    region: Option<u32>,
) -> Result<Direct3D12::ID3D12Resource, Box<dyn std::error::Error>> {
    unsafe {
        let hdevice = device.as_hal::<Dx12>().ok_or("wgpu backend is not DX12")?;
        let raw_device = hdevice.raw_device();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        let key = (
            d3d11_device.as_raw() as usize,
            raw_device.as_raw() as usize,
            desc.Format.0,
            width,
            height,
        );

        let mut guard = DX12_IMPORT_POOL.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().map(|p| p.key) != Some(key) {
            if guard.is_some() {
                log::debug!("[dx12_import] pool rebuilt for {}x{} format={:?}", width, height, desc.Format);
            }
            *guard = Some(Dx12ImportPool { key, slots: Vec::new(), next: 0 });
        }
        let pool = guard.as_mut().expect("pool set above");

        if pool.slots.len() < DX12_IMPORT_SLOTS {
            let (handle, shared) = get_shared_texture_d3d11(d3d11_device, texture, width, height)?;
            let mut resource = None::<Direct3D12::ID3D12Resource>;
            let opened = raw_device.OpenSharedHandle(handle, &mut resource);
            let _ = CloseHandle(handle);
            if let Err(e) = opened {
                log::error!(
                    "[dx12_import] OpenSharedHandle failed: hr=0x{:08x} ({})",
                    e.code().0 as u32,
                    e.message(),
                );
                log_d3d11_device_removed_reason(d3d11_device);
                log_dx12_device_removed_reason(device);
                return Err(Box::new(e));
            }
            let resource = resource.ok_or("OpenSharedHandle returned no resource")?;
            let fence12 = match shared.fence.open_on_d3d12(raw_device) {
                Ok(f) => Some(f),
                Err(e) => {
                    log::warn!(
                        "[dx12_import] sharing the copy fence failed (hr=0x{:08x}); waiting on the CPU instead",
                        e.code().0 as u32
                    );
                    None
                }
            };
            pool.slots.push((shared, resource, fence12));
        }
        let idx = pool.next % pool.slots.len();
        pool.next = (idx + 1) % DX12_IMPORT_SLOTS;
        let (shared, resource, fence12) = &pool.slots[idx];

        // The D3D12 queue waits for the copy on the GPU: before, the render
        // thread blocked in WaitForSingleObject (0.78 of the 1.19 ms per
        // frame the import cost on an Intel UHD). The wait is queued ahead of
        // the draw that samples `resource`, which wgpu submits later.
        let copied = match fence12 {
            Some(fence12) => shared
                .signalled_copy_from(d3d11_device_context, texture, width, height, region)
                .and_then(|v| hdevice.raw_queue().Wait(fence12, v)),
            None => shared.synchronized_copy_from(d3d11_device_context, texture, width, height, region),
        };
        if let Err(e) = copied {
            log::error!(
                "[dx12_import] synchronized_copy_from failed: hr=0x{:08x} ({})",
                e.code().0 as u32,
                e.message(),
            );
            log_d3d11_device_removed_reason(d3d11_device);
            log_dx12_device_removed_reason(device);
            // A failed slot may be in a bad state: start over next frame.
            *guard = None;
            return Err(Box::new(e));
        }
        Ok(resource.clone())
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
        // Device-removed checks are diagnostics: GetDeviceRemovedReason on
        // every frame is not free, so only when tracing.
        let trace = log::log_enabled!(log::Level::Trace);
        if trace {
            log::trace!("[dx12_wrap] texture_from_raw OK; device pre-check:");
            log_dx12_device_removed_reason(device);
        }

        log::trace!("[dx12_wrap] before create_texture_from_hal");
        // wgpu 29.0.3: create_texture_from_hal derives HAL usage from the
        // descriptor; the old explicit `TextureUses` hint arg was removed. Make
        // the descriptor advertise the same intent (sampling + copy-src).
        let mut desc = desc.clone();
        desc.usage |= wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
        let result = device.create_texture_from_hal::<Dx12>(texture, &desc);
        if trace {
            log::trace!("[dx12_wrap] create_texture_from_hal returned; device post-check:");
            log_dx12_device_removed_reason(device);
        }
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

/// GPU test of the D3D11 -> Vulkan import pool on the machine's GPU (skipped
/// without one): NV12 frames with a known pattern are created on a D3D11
/// device on the renderer's adapter, imported, read back plane by plane and
/// compared byte for byte. Six frames run through four slots, so slot reuse
/// and the GPU wait on the imported D3D11 fence are exercised. (wgpu's DX12
/// backend can't copy single NV12 planes out, so the DX12 pool has no
/// readback test.)
#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;

    const W: u32 = 64;
    const H: u32 = 64;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    fn wgpu_device(backends: wgpu::Backends, extra: wgpu::Features) -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok()?;
        let wanted = wgpu::Features::TEXTURE_FORMAT_NV12 | extra;
        if !adapter.features().contains(wanted) {
            return None;
        }
        block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_features: wanted,
            ..Default::default()
        }))
        .ok()
    }

    /// D3D11 device on the adapter with `luid` (where the decoder would open).
    fn d3d11_on(luid: u64) -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let mut i = 0;
            while let Ok(adapter) = factory.EnumAdapters1(i) {
                i += 1;
                let l = adapter.GetDesc1().ok()?.AdapterLuid;
                if ((l.HighPart as u32 as u64) << 32 | l.LowPart as u64) != luid {
                    continue;
                }
                let (mut dev, mut ctx) = (None, None);
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut dev),
                    None,
                    Some(&mut ctx),
                )
                .ok()?;
                return Some((dev?, ctx?));
            }
            None
        }
    }

    /// NV12 frame `k`: a Y gradient and a UV pattern, both shifted by `k`.
    fn nv12_pattern(k: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((W * H * 3 / 2) as usize);
        for y in 0..H {
            for x in 0..W {
                px.push(((x + y * 3 + k * 17) & 0xFF) as u8);
            }
        }
        for y in 0..H / 2 {
            for x in 0..W / 2 {
                px.push(((x * 5 + k * 29) & 0xFF) as u8);
                px.push(((y * 7 + k * 41) & 0xFF) as u8);
            }
        }
        px
    }

    fn nv12_texture(dev: &ID3D11Device, data: &[u8]) -> Option<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr().cast(), SysMemPitch: W, SysMemSlicePitch: 0 };
        let mut tex = None;
        unsafe { dev.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).ok()? };
        tex
    }

    /// One plane of `tex`, tightly packed.
    fn read_plane(dev: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture, aspect: wgpu::TextureAspect) -> Vec<u8> {
        let (w, h, bpp) = match aspect {
            wgpu::TextureAspect::Plane0 => (W, H, 1),
            _ => (W / 2, H / 2, 2),
        };
        let padded = (w * bpp).div_ceil(256) * 256;
        let buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = dev.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = dev.poll(wgpu::PollType::wait_indefinitely());
        let data = slice.get_mapped_range();
        (0..h).flat_map(|y| data[(y * padded) as usize..(y * padded + w * bpp) as usize].to_vec()).collect()
    }

    /// Six frames through `import`, each read back and compared.
    fn run(
        dev: &wgpu::Device,
        queue: &wgpu::Queue,
        luid: u64,
        import: impl Fn(&ID3D11Device, &ID3D11DeviceContext, &ID3D11Texture2D) -> wgpu::Texture,
    ) {
        let Some((d3d, ctx)) = d3d11_on(luid) else {
            eprintln!("SKIP: no D3D11 device on the renderer's adapter");
            return;
        };
        for k in 0..6 {
            let want = nv12_pattern(k);
            let Some(src) = nv12_texture(&d3d, &want) else {
                eprintln!("SKIP: driver can't create an NV12 shader-resource texture");
                return;
            };
            let tex = import(&d3d, &ctx, &src);
            let y = read_plane(dev, queue, &tex, wgpu::TextureAspect::Plane0);
            let uv = read_plane(dev, queue, &tex, wgpu::TextureAspect::Plane1);
            let (want_y, want_uv) = want.split_at((W * H) as usize);
            let bad_y = y.iter().zip(want_y).filter(|(a, b)| a != b).count();
            let bad_uv = uv.iter().zip(want_uv).filter(|(a, b)| a != b).count();
            assert!(bad_y == 0 && bad_uv == 0, "frame {k}: {bad_y} Y and {bad_uv} UV bytes differ");
        }
    }

    #[test]
    fn d3d11_frames_import_into_vulkan_byte_exact() {
        let Some((dev, queue)) = wgpu_device(wgpu::Backends::VULKAN, wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32) else {
            eprintln!("SKIP: no Vulkan adapter with NV12 + external memory");
            return;
        };
        let Some(luid) = vulkan_adapter_luid(&dev) else {
            eprintln!("SKIP: Vulkan driver reports no LUID");
            return;
        };
        run(&dev, &queue, luid, |d3d, ctx, src| {
            import_d3d11_texture_vulkan_pooled(&dev, &queue, d3d, ctx, src, W, H, Some(0)).expect("vulkan import")
        });
    }
}
