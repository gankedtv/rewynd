//! Thin wrapper over `gpu-video`: an NV12 [`wgpu::Texture`] in, an H.264
//! [`EncodedChunk`] out (PLAN §4.3). The crate also provides the RGBA→NV12
//! conversion that produces that NV12 input — a step upstream of [`Encoder`],
//! not something [`Encoder::encode`] does itself.
//!
//! [`GpuVideoEncoder::new`] constructs the encoder against the shared wgpu device
//! ([`rewynd_gpu::GpuContext`]); [`Nv12Converter`] performs the RGBA→NV12 step.

use std::time::Duration;

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
    /// The encoder failed to initialise.
    #[error("failed to initialise the encoder: {0}")]
    Init(String),
    /// The encoder failed while encoding a frame.
    #[error("failed to encode a frame: {0}")]
    Encode(String),
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
    /// Encode one NV12 frame captured at `pts` (capture-relative). `force_keyframe`
    /// forces an IDR at this frame so a clip can begin here (PLAN §3.3). The encoder
    /// stamps `pts` onto the returned chunk verbatim — it doesn't invent timing — so
    /// the ring buffer and muxer see real, wall-clock-accurate timestamps.
    fn encode(
        &mut self,
        frame: &wgpu::Texture,
        force_keyframe: bool,
        pts: Duration,
    ) -> Result<EncodedChunk, EncodeError>;
}

// The concrete gpu-video-backed encoder and converter only exist where gpu-video
// builds.
#[cfg(vulkan)]
mod gpu_video_backend;
#[cfg(vulkan)]
pub use gpu_video_backend::{GpuVideoEncoder, Nv12Converter};

// The CPU H.264 encoder core (libopenh264) is platform-agnostic like the audio codecs, so
// it's unconditional and CI-tested. Its GPU-texture adapter is the part that needs Vulkan.
mod software;
pub use software::{I420Frame, SoftwareEncoder};

// The NV12 texture -> I420 readback adapter that feeds the CPU encoder. Needs a wgpu device,
// so it only exists where the GPU stack builds.
#[cfg(vulkan)]
mod software_texture;
#[cfg(vulkan)]
pub use software_texture::SoftwareTextureEncoder;

// Opus audio encoding is CPU-side and platform-agnostic (libopus), so it's unconditional.
mod opus_audio;
pub use opus_audio::{AudioEncodeError, AudioEncodeParams, OpusAudioEncoder};

// System + mic mixing — pure CPU logic, platform-agnostic.
mod audio_mix;
pub use audio_mix::{AudioMixer, apply_gain, center_mono_into};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_params_target_1080p60() {
        let p = EncodeParams::default();
        assert_eq!(p.width, 1920);
        assert_eq!(p.height, 1080);
        assert_eq!(p.framerate, 60);
        assert_eq!(p.bitrate_bps, 12_000_000);
        assert_eq!(p.idr_period, 60);
    }

    #[test]
    fn init_error_displays_the_cause() {
        let err = EncodeError::Init("boom".to_owned());
        assert_eq!(err.to_string(), "failed to initialise the encoder: boom");
    }

    #[test]
    fn encode_error_displays_the_cause() {
        let err = EncodeError::Encode("kaput".to_owned());
        assert_eq!(err.to_string(), "failed to encode a frame: kaput");
    }
}
