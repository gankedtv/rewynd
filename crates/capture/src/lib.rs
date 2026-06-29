//! OS screen capture as GPU-resident frames (PLAN §3.5, §4.3).
//!
//! Capture is the one genuinely per-platform layer: there is no cross-platform GPU
//! capture API. Each platform imports the captured GPU memory (a PipeWire DMA-BUF fd
//! on Linux, a D3D11 shared NT handle on Windows) into a [`wgpu::Texture`] and yields
//! it as a [`GpuFrame`] behind the common [`FrameSource`] trait, so the rest of the
//! pipeline stays platform-agnostic.

use std::time::Duration;

use thiserror::Error;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod windows;

/// Errors from a [`FrameSource`].
#[derive(Debug, Error)]
pub enum CaptureError {
    /// The platform capture backend could not be started (no portal, no permission, …).
    #[error("capture backend unavailable")]
    Unavailable,
    /// The platform capture backend is not yet implemented.
    #[error("capture backend not yet implemented")]
    NotImplemented,
    /// The user cancelled the screencast share-picker dialog.
    #[error("screencast selection cancelled by the user")]
    Cancelled,
    /// The XDG desktop portal handshake failed (user cancelled, no portal, …).
    #[error("screencast portal error: {0}")]
    Portal(String),
    /// A PipeWire stream / negotiation error.
    #[error("pipewire error: {0}")]
    PipeWire(String),
    /// A Vulkan error while probing GPU capabilities (e.g. DRM format modifiers).
    #[error("vulkan error: {0}")]
    Vulkan(String),
}

/// Pixel layout of a captured frame, as imported into the texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFormat {
    /// 8-bit BGRA (typical desktop capture output).
    Bgra8,
    /// 8-bit RGBA.
    Rgba8,
    /// Planar NV12 (the encoder's required input format).
    Nv12,
}

/// A single captured frame, imported zero-copy into a [`wgpu::Texture`].
#[derive(Debug)]
pub struct GpuFrame {
    /// The imported GPU texture holding the frame.
    pub texture: wgpu::Texture,
    /// The pixel layout of [`texture`](GpuFrame::texture).
    pub format: FrameFormat,
    /// Capture timestamp relative to the start of the stream (used to stamp PTS).
    pub timestamp: Duration,
}

/// A source of GPU-resident frames. Per-platform implementations live in the
/// [`linux`] / [`windows`] submodules.
// The pipeline drives a single capture task; `Send` bounds are handled at the call
// site (#9), so the desugared-RPIT warning does not apply here.
#[allow(async_fn_in_trait)]
pub trait FrameSource {
    /// Yield the next captured frame, awaiting one if necessary.
    async fn next_frame(&mut self) -> Result<GpuFrame, CaptureError>;
}
