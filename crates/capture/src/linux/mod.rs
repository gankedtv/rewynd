//! Linux capture: XDG screencast portal (`ashpd`) negotiates a session and
//! PipeWire delivers frames as DMA-BUF fds, imported into a [`wgpu::Texture`]
//! (PLAN §3.5, §6.1). Implemented in #4 (portal + PipeWire) and #5 (DMABUF import).
//!
//! This module is split into:
//! - [`portal`]: the `ashpd` XDG ScreenCast handshake (interactive share dialog),
//!   yielding a PipeWire node id + remote fd.
//! - [`pipewire_capture`]: the PipeWire stream setup that negotiates a DMA-BUF
//!   format and reads frames, exposing them as [`DmabufFrame`] descriptors.
//!
//! The real `FrameSource` → [`GpuFrame`] wiring (DMA-BUF import into a wgpu texture)
//! lands in #5; for now [`PipewireCapture`] returns [`CaptureError::NotImplemented`].

use super::{CaptureError, FrameSource, GpuFrame};

pub mod pipewire_capture;
pub mod portal;
pub mod vulkan_modifiers;

pub use pipewire_capture::{DmabufFrame, run_capture_probe};
pub use portal::{PortalSession, open_portal};
pub use vulkan_modifiers::query_drm_format_modifiers;

/// PipeWire/portal-backed frame source for Wayland (and X11 via the portal).
#[derive(Debug, Default)]
pub struct PipewireCapture;

impl FrameSource for PipewireCapture {
    async fn next_frame(&mut self) -> Result<GpuFrame, CaptureError> {
        // Portal + PipeWire DMA-BUF capture and import land in #4/#5.
        Err(CaptureError::NotImplemented)
    }
}
