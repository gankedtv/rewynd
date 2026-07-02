//! Zero-copy import of a captured D3D11 shared NT handle into a [`wgpu::Texture`]
//! (PLAN §4.2, §6.1).
//!
//! Drops to wgpu-hal's Vulkan backend to import the handle as a Vulkan image
//! (`VK_KHR_external_memory_win32`, handle type `D3D11_TEXTURE`), then wraps it back
//! as a [`wgpu::Texture`]. The shared device ([`crate::GpuContext::new`]) already
//! enables `VULKAN_EXTERNAL_MEMORY_WIN32`, which the import requires.

use std::os::windows::io::{AsRawHandle, BorrowedHandle};

use crate::{GpuContext, GpuError};

/// A D3D11 shared NT handle to import as a [`wgpu::Texture`].
///
/// Built from a captured frame (`rewynd_capture::windows::CapturedD3d11Frame`): the
/// dimensions and `dxgi_format → format` mapping come straight from the capture
/// descriptor. The handle is only borrowed: unlike the DMA-BUF fd import, Vulkan
/// takes its own reference to the underlying resource, so the caller keeps (and
/// eventually closes) its handle.
#[derive(Debug)]
pub struct D3d11HandleImport<'a> {
    /// The NT shared handle (from `IDXGIResource1::CreateSharedHandle`).
    pub handle: BorrowedHandle<'a>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// The wgpu texture format matching the texture's DXGI format (e.g.
    /// [`wgpu::TextureFormat::Bgra8Unorm`] for `DXGI_FORMAT_B8G8R8A8_UNORM`).
    pub format: wgpu::TextureFormat,
}

impl GpuContext {
    /// Import a D3D11 shared NT handle as a sampled/copyable [`wgpu::Texture`]
    /// (zero-copy).
    ///
    /// # Safety
    /// `img.handle` must be a valid NT handle to a D3D11 texture created with
    /// `D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE` whose
    /// size and format exactly match the descriptor, and the producer must have
    /// finished writing it with no concurrent writes (the capture backend's
    /// event-query wait provides that). A mismatch or a concurrent write is
    /// undefined behaviour at the Vulkan level.
    ///
    /// # Errors
    /// Returns [`GpuError::Import`] if the device is not a Vulkan device, the
    /// `VULKAN_EXTERNAL_MEMORY_WIN32` feature is unavailable, or the Vulkan import
    /// itself fails.
    pub unsafe fn import_d3d11_shared_handle(
        &self,
        img: D3d11HandleImport<'_>,
    ) -> Result<wgpu::Texture, GpuError> {
        let size = wgpu::Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        };

        // The hal descriptor and the wgpu descriptor are DIFFERENT types but must
        // describe the SAME texture (see the DMA-BUF import for the full rationale).
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("rewynd-d3d11-import"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: img.format,
            // COPY_SRC for readback; RESOURCE so the scale/encode path can sample it.
            usage: wgpu::TextureUses::COPY_SRC | wgpu::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };

        // SAFETY (as_hal): the borrowed hal device is only used for this import and
        // not retained; the hal texture goes straight back into wgpu below.
        let hal_texture = {
            let hal_device = unsafe { self.device.as_hal::<wgpu::hal::vulkan::Api>() }
                .ok_or_else(|| GpuError::Import("device is not a Vulkan device".to_owned()))?;

            let raw = windows::Win32::Foundation::HANDLE(img.handle.as_raw_handle());
            // SAFETY (texture_from_d3d11_shared_handle): caller guarantees the handle
            // is a valid shareable D3D11 texture matching `hal_desc` (see this fn's
            // `# Safety`). Vulkan references the resource; the handle stays ours.
            unsafe { hal_device.texture_from_d3d11_shared_handle(raw, &hal_desc) }
                .map_err(|e| GpuError::Import(format!("texture_from_d3d11_shared_handle: {e:?}")))?
        };

        let wgpu_desc = wgpu::TextureDescriptor {
            label: Some("rewynd-d3d11-import"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: img.format,
            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };

        // A freshly imported VkImage is in UNDEFINED layout, so the imported state is
        // UNINITIALIZED; the external memory keeps the D3D11 producer's pixels across
        // wgpu's UNDEFINED→COPY_SRC transition (same contract as the DMA-BUF import).
        // SAFETY: `hal_texture` was just made from this device with `hal_desc`/`wgpu_desc`.
        let texture = unsafe {
            self.device
                .create_texture_from_hal::<wgpu::hal::vulkan::Api>(
                    hal_texture,
                    &wgpu_desc,
                    wgpu::TextureUses::UNINITIALIZED,
                )
        };

        Ok(texture)
    }
}
