//! Shared wgpu device/queue and capture-import helpers (PLAN §4.2, §6.1).
//!
//! The device must be created so it can be shared with `gpu-video` — the same wgpu
//! source end-to-end (see docs/adr/0001-wgpu-rev.md), or the `wgpu::Texture` types
//! won't unify. Standing the device up against the encoder is issue #3; the DMABUF
//! (Linux) and D3D11 shared-handle (Windows) interop import helpers land in #5 / #7.

use thiserror::Error;

/// Errors from GPU setup.
#[derive(Debug, Error)]
pub enum GpuError {
    /// No Vulkan adapter exposing the required video-encode + external-memory features.
    #[error("no suitable Vulkan adapter found")]
    NoAdapter,
}

/// The wgpu device/queue shared across the pipeline and handed to `gpu-video`.
#[derive(Debug)]
pub struct GpuContext {
    /// The wgpu device, created on the Vulkan backend with interop features enabled.
    pub device: wgpu::Device,
    /// The queue paired with [`device`](GpuContext::device).
    pub queue: wgpu::Queue,
}

impl GpuContext {
    /// Create the shared device on the Vulkan backend, enabling the external-memory
    /// features the capture-import path needs. Implemented in #3.
    pub async fn new() -> Result<Self, GpuError> {
        todo!("shared wgpu device + gpu-video coexistence — issue #3")
    }
}
