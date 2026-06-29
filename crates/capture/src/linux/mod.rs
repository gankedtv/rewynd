//! Linux capture: the XDG ScreenCast portal (`ashpd`) negotiates a session and
//! PipeWire delivers frames as DMA-BUF fds (PLAN §3.5, §6.1).
//!
//! - [`portal`]: the ScreenCast handshake → a PipeWire node id + remote fd.
//! - [`pipewire_capture`]: the stream setup that negotiates DMA-BUF and reads frames.
//!
//! `FrameSource` → [`GpuFrame`] is not yet wired; [`PipewireCapture`] returns
//! [`CaptureError::NotImplemented`].

use super::{CaptureError, FrameSource, GpuFrame};

pub mod pipewire_capture;
pub mod portal;
pub mod vulkan_modifiers;

pub use pipewire_capture::{CapturedDmabuf, DmabufFrame, capture_one_dmabuf, run_capture_probe};
pub use portal::{PortalSession, open_portal};
pub use vulkan_modifiers::query_drm_format_modifiers;

/// PipeWire/portal-backed frame source for Wayland (and X11 via the portal).
#[derive(Debug, Default)]
pub struct PipewireCapture;

impl FrameSource for PipewireCapture {
    async fn next_frame(&mut self) -> Result<GpuFrame, CaptureError> {
        Err(CaptureError::NotImplemented)
    }
}
