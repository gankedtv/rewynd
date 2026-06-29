//! Zero-copy import of a captured DMA-BUF into a [`wgpu::Texture`] (PLAN §4.2, §6.1).
//!
//! Drops to wgpu-hal's Vulkan backend to import a single-plane DMA-BUF fd as a Vulkan
//! image with its DRM modifier/stride/offset, then wraps it back as a [`wgpu::Texture`].
//! The shared device ([`crate::GpuContext::new`]) already enables
//! `VULKAN_EXTERNAL_MEMORY_DMA_BUF`, which `texture_from_dmabuf_fd` requires.

use std::os::fd::OwnedFd;

use crate::{GpuContext, GpuError};

/// A single-plane DMA-BUF to import as a [`wgpu::Texture`].
///
/// Built from a captured frame (`rewynd_capture::linux::CapturedDmabuf`): the
/// `fourcc → format` mapping and the modifier/stride/offset come straight from the
/// PipeWire negotiation. Only single-plane packed-RGB formats are supported
/// (`texture_from_dmabuf_fd` is single-plane).
#[derive(Debug)]
pub struct DmabufImport {
    /// Owned (dup'd) DMA-BUF fd. On a successful import Vulkan takes ownership of
    /// the fd; on failure it is closed by `texture_from_dmabuf_fd`.
    pub fd: OwnedFd,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// The wgpu texture format matching the DRM `fourcc` (e.g.
    /// [`wgpu::TextureFormat::Bgra8Unorm`] for DRM `XRGB8888`/`ARGB8888`).
    pub format: wgpu::TextureFormat,
    /// The DRM format modifier the buffer was allocated with (tiling/compression
    /// layout). Must match the DMA-BUF exactly.
    pub drm_modifier: u64,
    /// Row stride in bytes (the plane's `row_pitch`).
    pub stride: u32,
    /// Byte offset of the plane within the DMA-BUF.
    pub offset: u32,
}

impl GpuContext {
    /// Import a single-plane DMA-BUF as a sampled/copyable [`wgpu::Texture`]
    /// (zero-copy).
    ///
    /// Drops to the Vulkan hal backend, imports `img.fd` as a Vulkan image with the
    /// given DRM modifier / stride / offset, and wraps it as a [`wgpu::Texture`]
    /// usable as a copy source (for readback) and a sampled resource (for the
    /// encode/scale path).
    ///
    /// # Safety
    /// `img.fd` must be a valid single-plane DMA-BUF whose layout (format,
    /// `drm_modifier`, `stride`, `offset`, `width`, `height`) exactly matches the
    /// descriptor, and the producer must have finished writing it with no concurrent
    /// writes (there is no producer-side sync here). A mismatch or a concurrent write
    /// is undefined behaviour at the Vulkan level. On success Vulkan takes ownership
    /// of the fd.
    ///
    /// # Errors
    /// Returns [`GpuError::Import`] if the device is not a Vulkan device, the
    /// `VULKAN_EXTERNAL_MEMORY_DMA_BUF` feature/extensions are unavailable, or the
    /// Vulkan import itself fails.
    pub unsafe fn import_dmabuf(&self, img: DmabufImport) -> Result<wgpu::Texture, GpuError> {
        let size = wgpu::Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        };

        // The hal descriptor and the wgpu descriptor are DIFFERENT types but must
        // describe the SAME texture. Keep their size/format/mip/sample identical.
        // hal usage uses `TextureUses` (resource-state flags); wgpu usage uses
        // `TextureUsages` (capability flags).
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("rewynd-dmabuf-import"),
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

        // SAFETY (as_hal): we only use the borrowed hal device to import the fd and
        // do not retain it past this call; the returned hal texture is immediately
        // handed back to wgpu via `create_texture_from_hal`.
        let hal_texture = {
            let hal_device = unsafe { self.device.as_hal::<wgpu::hal::vulkan::Api>() }
                .ok_or_else(|| GpuError::Import("device is not a Vulkan device".to_owned()))?;

            // SAFETY (texture_from_dmabuf_fd): caller guarantees `img.fd` is a valid
            // single-plane DMA-BUF matching `hal_desc` + modifier/stride/offset (see
            // this fn's `# Safety`). Vulkan takes ownership of the fd on success and
            // closes it on failure.
            unsafe {
                hal_device.texture_from_dmabuf_fd(
                    img.fd,
                    &hal_desc,
                    img.drm_modifier,
                    u64::from(img.stride),
                    u64::from(img.offset),
                )
            }
            .map_err(|e| GpuError::Import(format!("texture_from_dmabuf_fd: {e:?}")))?
        };

        let wgpu_desc = wgpu::TextureDescriptor {
            label: Some("rewynd-dmabuf-import"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: img.format,
            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };

        // A freshly imported VkImage is in UNDEFINED layout, so the imported state is
        // UNINITIALIZED; wgpu then transitions UNDEFINED→COPY_SRC for the readback and
        // the DMA-BUF memory keeps the producer's pixels across it. (Claiming COPY_SRC
        // here would make wgpu emit a barrier with a wrong oldLayout.) Live A/V also
        // needs producer-side sync (explicit-sync semaphore); a static readback doesn't.
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
