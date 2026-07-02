//! OS screen capture (PLAN §3.5, §4.3).
//!
//! Capture is the one genuinely per-platform layer: there is no cross-platform GPU
//! capture API. Both backends deliver frames to a blocking per-frame callback and are
//! consumed the same way; only the frame descriptor type differs.
//!
//! - [`linux`]: the XDG ScreenCast portal + a PipeWire stream, delivering each frame's
//!   DMA-BUF descriptor ([`linux::capture_stream`]); the caller imports it into a
//!   `wgpu::Texture` via `rewynd-gpu`.
//! - [`windows`]: Windows Graphics Capture → shareable D3D11 textures, delivering an
//!   NT shared handle per frame ([`windows::capture_stream`]) for the Vulkan
//!   external-memory import.

use thiserror::Error;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

/// Negotiation preferences offered to the platform capture backend, which may still
/// deliver differently (the compositor picks what it can satisfy; WGC captures at the
/// monitor's native size). The format that arrives in the frames is authoritative.
#[derive(Debug, Clone, Copy)]
pub struct StreamPrefs {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
}

/// Which audio endpoint to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// The system output mix (what you hear): the default sink's monitor on Linux,
    /// the default render endpoint's WASAPI loopback on Windows.
    SinkMonitor,
    /// The default input — the microphone.
    Microphone,
}

/// System-audio capture parameters.
///
/// Opus operates natively at 48 kHz, and stereo matches a typical desktop sink, so those
/// are the defaults. Like `rewynd_encode::EncodeParams`, rate and channel count are
/// parameters rather than hard-coded constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    /// Sample rate in Hz (samples per channel per second).
    pub sample_rate: u32,
    /// Channel count (interleaved). 2 = stereo.
    pub channels: u32,
}

impl Default for AudioParams {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
        }
    }
}

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
    /// A Windows Graphics Capture session error.
    #[error("windows graphics capture error: {0}")]
    Wgc(String),
    /// A Direct3D 11 error while preparing shareable capture textures.
    #[error("d3d11 error: {0}")]
    D3d11(String),
    /// A WASAPI audio-capture error.
    #[error("wasapi error: {0}")]
    Wasapi(String),
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
        assert_eq!(
            CaptureError::Wgc("item closed".to_owned()).to_string(),
            "windows graphics capture error: item closed"
        );
        assert_eq!(
            CaptureError::D3d11("device lost".to_owned()).to_string(),
            "d3d11 error: device lost"
        );
        assert_eq!(
            CaptureError::Wasapi("no endpoint".to_owned()).to_string(),
            "wasapi error: no endpoint"
        );
    }
}
