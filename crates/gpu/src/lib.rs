//! Shared wgpu device/queue and capture-import helpers (PLAN §4.2, §6.1).
//!
//! [`GpuContext::new`] creates the wgpu device via `gpu-video`'s
//! `request_device_with_video_support`, so the device is shared with the encoder
//! (same wgpu source end-to-end — see docs/adr/0001-wgpu-rev.md). It enables the
//! external-memory features the capture-import path needs (DMA-BUF on Linux, the
//! D3D11 shared handle on Windows). DMA-BUF import lives in the `import` submodule.

use thiserror::Error;

#[cfg(target_os = "linux")]
mod import;

#[cfg(target_os = "linux")]
pub use import::DmabufImport;

/// Errors from GPU setup.
#[derive(Debug, Error)]
pub enum GpuError {
    /// No Vulkan adapter exposing H.264 video-encode support was found.
    #[error("no suitable Vulkan adapter with H.264 video encode support")]
    NoAdapter,
    /// The wgpu device could not be created with video support.
    #[error("failed to create the shared wgpu device: {0}")]
    DeviceCreation(String),
    /// A captured GPU resource (DMA-BUF / shared handle) could not be imported as
    /// a [`wgpu::Texture`].
    #[error("failed to import external GPU memory: {0}")]
    Import(String),
}

/// The wgpu device/queue shared across the pipeline and handed to `gpu-video`.
#[derive(Debug)]
pub struct GpuContext {
    /// The wgpu device, created on the Vulkan backend with interop features enabled.
    pub device: wgpu::Device,
    /// The queue paired with [`device`](GpuContext::device).
    pub queue: wgpu::Queue,
}

// The shared device is created through gpu-video's video-capable device path, which
// only exists where Vulkan does (Windows + non-Apple unixes); macOS is out of scope.
#[cfg(vulkan)]
impl GpuContext {
    /// Create the shared device on the Vulkan backend, enabling whichever
    /// external-memory features the adapter advertises (so the capture-import path
    /// can import DMA-BUF / D3D11 memory zero-copy).
    pub async fn new() -> Result<Self, GpuError> {
        use gpu_video::{VideoAdapterExt, parameters::VideoDeviceDescriptor};

        // Vulkan only: the encoder requires it and we enumerate Vulkan adapters below, so
        // there's no reason to initialise the other backends' drivers at instance creation.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let adapter = instance
            .enumerate_adapters(wgpu::Backends::VULKAN)
            .await
            .into_iter()
            .find(|adapter| {
                adapter
                    .video_adapter_info()
                    .is_some_and(|info| info.encode_capabilities.h264.is_some())
            })
            .ok_or(GpuError::NoAdapter)?;

        // Enable the external-memory features for the capture-import path, but only the
        // ones this adapter actually advertises (so device creation isn't rejected).
        let interop = wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF
            | wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD
            | wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32;
        let features = wgpu::Features::IMMEDIATES | (adapter.features() & interop);

        let (device, queue) = adapter
            .request_device_with_video_support(&VideoDeviceDescriptor {
                wgpu_features: features,
                wgpu_limits: wgpu::Limits {
                    max_immediate_size: 4,
                    ..Default::default()
                },
                ..Default::default()
            })
            .map_err(|e| GpuError::DeviceCreation(e.to_string()))?;

        Ok(Self { device, queue })
    }
}
