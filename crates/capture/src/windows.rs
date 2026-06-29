//! Windows capture: Windows Graphics Capture / DXGI Desktop Duplication produces a
//! D3D11 texture (shared NT handle), imported into a [`wgpu::Texture`] via
//! `VK_KHR_external_memory_win32` (PLAN §3.5, §6.1).

use super::{CaptureError, FrameSource, GpuFrame};

/// Windows Graphics Capture-backed frame source.
#[derive(Debug, Default)]
pub struct WgcCapture;

impl FrameSource for WgcCapture {
    async fn next_frame(&mut self) -> Result<GpuFrame, CaptureError> {
        // WGC/DXGI capture and D3D11 shared-handle import land later.
        Err(CaptureError::NotImplemented)
    }
}
