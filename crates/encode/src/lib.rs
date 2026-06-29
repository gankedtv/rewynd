//! Thin wrapper over `gpu-video`: an NV12 [`wgpu::Texture`] in, an H.264
//! [`EncodedChunk`] out (PLAN §4.3). The crate also provides the RGBA→NV12
//! conversion that produces that NV12 input — a step upstream of [`Encoder`],
//! not something [`Encoder::encode`] does itself.
//!
//! [`GpuVideoEncoder::new`] constructs the encoder against the shared wgpu device
//! ([`rewynd_gpu::GpuContext`]); [`Nv12Converter`] performs the RGBA→NV12 step.

use rewynd_buffer::EncodedChunk;
use thiserror::Error;

/// The pinned `gpu-video` H.264 parameter type this wrapper builds on, re-exported so
/// the workspace compiles against the ADR 0001 pin. Available only where `gpu-video`
/// builds (see Cargo.toml target gating).
#[cfg(vulkan)]
pub type GpuVideoEncoderParameters = gpu_video::parameters::EncoderParametersH264;

/// Errors from the encoder.
#[derive(Debug, Error)]
pub enum EncodeError {
    /// The encoder failed to initialise on the shared device.
    #[error("failed to initialise the gpu-video encoder: {0}")]
    Init(String),
}

/// Encoder configuration.
///
/// Resolution / framerate / bitrate are **parameters, never hard-coded** (PLAN §9);
/// the defaults target 1080p60 but other qualities are addable.
#[derive(Debug, Clone, Copy)]
pub struct EncodeParams {
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Target framerate in frames per second.
    pub framerate: u32,
    /// Average target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Frames between IDRs (GOP length); governs where the ring buffer can cut.
    pub idr_period: u32,
}

impl Default for EncodeParams {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            framerate: 60,
            bitrate_bps: 12_000_000,
            idr_period: 60,
        }
    }
}

/// Encodes NV12 [`wgpu::Texture`]s into H.264 [`EncodedChunk`]s.
///
/// `frame` must already be NV12 (`wgpu::TextureFormat::NV12`); run the crate's
/// [`Nv12Converter`] first when the source isn't NV12.
pub trait Encoder {
    /// Encode one NV12 frame. `force_keyframe` forces an IDR at this frame so a clip
    /// can begin here (PLAN §3.3).
    fn encode(
        &mut self,
        frame: &wgpu::Texture,
        force_keyframe: bool,
    ) -> Result<EncodedChunk, EncodeError>;
}

// The concrete gpu-video-backed encoder and converter only exist where gpu-video
// builds.
#[cfg(vulkan)]
mod gpu_video_backend;
#[cfg(vulkan)]
pub use gpu_video_backend::{GpuVideoEncoder, Nv12Converter};
