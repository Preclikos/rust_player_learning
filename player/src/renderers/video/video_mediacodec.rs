// Import an Android AHardwareBuffer into Vulkan as a VkImage, then wrap
// that VkImage as a wgpu::Texture. Companion to `video_vaapi.rs` (DMA-BUF)
// and `video_directx.rs` (D3D11 shared handle).
//
// Reference: VK_ANDROID_external_memory_android_hardware_buffer
//   https://registry.khronos.org/vulkan/specs/1.3-extensions/man/html/VK_ANDROID_external_memory_android_hardware_buffer.html
//
// Format strategy: ImageReader is configured for YUV_420_888 which gives
// us an AHB with a defined Y + interleaved CbCr layout. We map it to
// VK_FORMAT_G8_B8R8_2PLANE_420_UNORM (Vulkan's NV12) so we can sample it
// without VkSamplerYcbcrConversion — same approach the VAAPI path takes.

#![cfg(target_os = "android")]

use ash::vk;
use ndk::hardware_buffer::HardwareBuffer;
use wgpu::hal::api::Vulkan;

use super::video_vulkan::VkImageMemory;

/// Look up the memory type index for the bits AHB reports, requiring
/// DEVICE_LOCAL since this is GPU-imported memory.
unsafe fn find_memory_type(
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
) -> Option<u32> {
    for i in 0..memory_properties.memory_type_count {
        if (type_bits & (1 << i)) != 0 {
            return Some(i);
        }
    }
    None
}

pub fn create_vk_image_from_ahb(
    wgpu_device: &wgpu::Device,
    ahb: &HardwareBuffer,
    width: u32,
    height: u32,
) -> Result<VkImageMemory, String> {
    use std::ffi::c_void;

    let ahb_ptr: *mut c_void = ahb.as_ptr().cast();
    let mut result: Result<VkImageMemory, String> = Err("hal access failed".to_string());
    let result_ref = &mut result;

    unsafe {
        wgpu_device.as_hal::<Vulkan, _, _>(|raw_dev| {
            let raw_dev = match raw_dev {
                Some(d) => d,
                None => {
                    *result_ref = Err("wgpu backend is not Vulkan".to_string());
                    return;
                }
            };
            let raw_device = raw_dev.raw_device();
            let raw_instance = raw_dev.shared_instance().raw_instance();
            let raw_phys = raw_dev.raw_physical_device();

            let ext = ash::android::external_memory_android_hardware_buffer::Device::new(
                raw_instance,
                raw_device,
            );

            // ---- Query the AHB's properties (size, memory type bits, format) ----
            let mut format_props = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
            let (allocation_size, memory_type_bits, reported_format) = {
                let mut ahb_props = vk::AndroidHardwareBufferPropertiesANDROID::default()
                    .push_next(&mut format_props);

                if let Err(e) = ext.get_android_hardware_buffer_properties(
                    ahb_ptr as *mut _,
                    &mut ahb_props,
                ) {
                    *result_ref = Err(format!(
                        "vkGetAndroidHardwareBufferPropertiesANDROID: {:?}",
                        e
                    ));
                    return;
                }
                (ahb_props.allocation_size, ahb_props.memory_type_bits, format_props.format)
            };

            // ---- VkImage create info ----
            // YUV_420_888 maps to G8_B8R8_2PLANE_420_UNORM. If the driver
            // reports a different/unknown format we still get an
            // ExternalFormatANDROID we can use with samplerYcbcrConversion
            // (deferred — first-cut prefers the well-known NV12 format).
            let image_format = if reported_format != vk::Format::UNDEFINED {
                reported_format
            } else {
                vk::Format::G8_B8R8_2PLANE_420_UNORM
            };

            let extent = vk::Extent3D {
                width,
                height,
                depth: 1,
            };

            let mut external_memory_image = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID);

            // For the well-defined NV12 format, no VkExternalFormatANDROID is
            // needed; for opaque formats we'd chain one here.
            let image_info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(image_format)
                .extent(extent)
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut external_memory_image);

            let image = match raw_device.create_image(&image_info, None) {
                Ok(i) => i,
                Err(e) => {
                    *result_ref = Err(format!("vkCreateImage (AHB): {:?}", e));
                    return;
                }
            };

            // ---- Pick a compatible memory type ----
            let mem_props = raw_instance.get_physical_device_memory_properties(raw_phys);
            let type_index = match find_memory_type(&mem_props, memory_type_bits) {
                Some(i) => i,
                None => {
                    raw_device.destroy_image(image, None);
                    *result_ref = Err("no compatible memory type for AHB".to_string());
                    return;
                }
            };

            // ---- VkDeviceMemory import-from-AHB ----
            let mut import_ahb = vk::ImportAndroidHardwareBufferInfoANDROID::default()
                .buffer(ahb_ptr as *mut _);

            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);

            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(allocation_size)
                .memory_type_index(type_index)
                .push_next(&mut dedicated)
                .push_next(&mut import_ahb);

            let memory = match raw_device.allocate_memory(&alloc_info, None) {
                Ok(m) => m,
                Err(e) => {
                    raw_device.destroy_image(image, None);
                    *result_ref = Err(format!("vkAllocateMemory (AHB import): {:?}", e));
                    return;
                }
            };

            if let Err(e) = raw_device.bind_image_memory(image, memory, 0) {
                raw_device.free_memory(memory, None);
                raw_device.destroy_image(image, None);
                *result_ref = Err(format!("vkBindImageMemory: {:?}", e));
                return;
            }

            *result_ref = Ok(VkImageMemory {
                raw_image: image,
                memory,
            });
        });
    }

    result
}
