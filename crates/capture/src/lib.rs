//! OS screen capture (PLAN §3.5, §4.3).
//!
//! Capture is the one genuinely per-platform layer: there is no cross-platform GPU
//! capture API. The Linux backend ([`linux`]) drives the XDG ScreenCast portal and a
//! PipeWire stream, delivering each frame's DMA-BUF descriptor to a blocking per-frame
//! callback ([`linux::capture_stream`]); the caller imports it into a `wgpu::Texture`
//! via `rewynd-gpu`. A Windows backend (WGC → D3D11 shared handle) will follow the
//! same callback shape when it lands.

use thiserror::Error;

#[cfg(target_os = "linux")]
pub mod linux;

/// Errors from the platform capture backends.
#[derive(Debug, Error)]
pub enum CaptureError {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_error_variants_display() {
        assert_eq!(
            CaptureError::Cancelled.to_string(),
            "screencast selection cancelled by the user"
        );
        assert_eq!(
            CaptureError::Portal("no portal".to_owned()).to_string(),
            "screencast portal error: no portal"
        );
        assert_eq!(
            CaptureError::PipeWire("stream gone".to_owned()).to_string(),
            "pipewire error: stream gone"
        );
        assert_eq!(
            CaptureError::Vulkan("no modifier".to_owned()).to_string(),
            "vulkan error: no modifier"
        );
    }
}
