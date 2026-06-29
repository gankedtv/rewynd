//! Thin wrapper over `gpu-video`: an NV12 [`wgpu::Texture`] in, an H.264
//! [`EncodedChunk`] out (PLAN §4.3). This crate owns the RGBA→NV12 conversion.
//!
//! The encoder is constructed against the shared wgpu device (#3) and wired in #9;
//! RGBA→NV12 (reusing `gpu-video`'s `WgpuRgbaToNv12Converter`) is #8.

use rewynd_buffer::EncodedChunk;
use rewynd_gpu::GpuContext;
use thiserror::Error;

/// The pinned `gpu-video` H.264 parameter type this wrapper builds on, re-exported so
/// the workspace compiles against the ADR 0001 pin from the scaffold onward. Available
/// only where `gpu-video` builds (see Cargo.toml target gating).
#[cfg(any(
    windows,
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "emscripten")
    )
))]
pub type GpuVideoEncoderParameters = gpu_video::parameters::EncoderParametersH264;

/// Errors from the encoder.
#[derive(Debug, Error)]
pub enum EncodeError {
    /// The encoder failed to initialise on the shared device.
    #[error("encoder initialisation failed")]
    Init,
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
    /// Frames between IDRs (GOP length); governs where the ring buffer can cut (#10).
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
pub trait Encoder {
    /// Encode one frame. `force_keyframe` forces an IDR at this frame so a clip can
    /// begin here (PLAN §3.3).
    fn encode(
        &mut self,
        frame: &wgpu::Texture,
        force_keyframe: bool,
    ) -> Result<EncodedChunk, EncodeError>;
}

/// `gpu-video`-backed encoder, constructed against the shared [`GpuContext`].
pub struct GpuVideoEncoder {
    params: EncodeParams,
}

impl GpuVideoEncoder {
    /// Build the encoder on the shared device. The real `create_wgpu_textures_encoder_h264`
    /// wiring lands in #9.
    pub fn new(gpu: &GpuContext, params: EncodeParams) -> Result<Self, EncodeError> {
        let _ = gpu;
        Ok(Self { params })
    }

    /// The parameters this encoder was configured with.
    #[must_use]
    pub fn params(&self) -> EncodeParams {
        self.params
    }
}

impl Encoder for GpuVideoEncoder {
    fn encode(
        &mut self,
        frame: &wgpu::Texture,
        force_keyframe: bool,
    ) -> Result<EncodedChunk, EncodeError> {
        let _ = (frame, force_keyframe);
        todo!("gpu-video encode (NV12 wgpu::Texture → H.264 chunk) — issue #9")
    }
}
