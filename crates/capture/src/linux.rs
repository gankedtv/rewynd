//! Linux capture: XDG screencast portal (`ashpd`) negotiates a session and
//! PipeWire delivers frames as DMA-BUF fds, imported into a [`wgpu::Texture`]
//! (PLAN §3.5, §6.1). Implemented in #4 (portal + PipeWire) and #5 (DMABUF import).

use super::{CaptureError, FrameSource, GpuFrame};

/// PipeWire/portal-backed frame source for Wayland (and X11 via the portal).
#[derive(Debug, Default)]
pub struct PipewireCapture;

impl FrameSource for PipewireCapture {
    async fn next_frame(&mut self) -> Result<GpuFrame, CaptureError> {
        todo!("portal + PipeWire DMA-BUF capture and import — issues #4/#5")
    }
}
