//! The `gpu-video`-backed [`Encoder`] implementation. Gated to targets where
//! `gpu-video` (Vulkan Video) builds; see the parent module's `#[cfg]`.

use std::num::NonZeroU32;

use gpu_video::{
    VideoDeviceExt,
    parameters::{EncoderParametersH264, RateControl, VideoParameters},
};
use rewynd_buffer::EncodedChunk;
use rewynd_gpu::GpuContext;

use crate::{EncodeError, EncodeParams, Encoder};

/// Burst headroom the VBV rate control may use over the average bitrate.
/// Provisional default — encoder-param tuning is revisited (and ADR'd) in #9.
const MAX_BITRATE_RATIO: u64 = 2;
/// Rate-control averaging window (virtual buffer size). Provisional default — see #9.
const VBV_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// `gpu-video`-backed H.264 encoder, constructed against the shared [`GpuContext`].
pub struct GpuVideoEncoder {
    params: EncodeParams,
    // Owns the live gpu-video encoder so it lives as long as this wrapper. The
    // per-frame encode path that drives it is wired in #9.
    inner: gpu_video::WgpuTexturesEncoderH264,
}

impl GpuVideoEncoder {
    /// Build the encoder on the shared device (PLAN §4.3, §3.3). This is the #3
    /// deliverable: a `gpu-video` encoder constructed on the *same* wgpu device as
    /// the rest of the pipeline. The frame-level encode (NV12 texture → chunk) is #9.
    pub fn new(gpu: &GpuContext, params: EncodeParams) -> Result<Self, EncodeError> {
        let video = gpu
            .device
            .video()
            .map_err(|e| EncodeError::Init(e.to_string()))?;

        let mut output_parameters = video
            .encoder_output_parameters_h264_high_quality(RateControl::VariableBitrate {
                average_bitrate: u64::from(params.bitrate_bps),
                max_bitrate: u64::from(params.bitrate_bps).saturating_mul(MAX_BITRATE_RATIO),
                virtual_buffer_size: VBV_WINDOW,
            })
            .map_err(|e| EncodeError::Init(e.to_string()))?;
        // Ring-buffer-critical knobs (PLAN §3.3): a fixed GOP the buffer can cut on,
        // and inline SPS/PPS so a clip cut from the buffer is self-decodable. A zero
        // GOP would silently fall back to the encoder default (~30), breaking the cut
        // invariant — reject it explicitly, as we do for width/height.
        let idr_period = NonZeroU32::new(params.idr_period)
            .ok_or_else(|| EncodeError::Init("idr_period must be > 0".to_owned()))?;
        output_parameters.idr_period = Some(idr_period);
        output_parameters.inline_stream_params = Some(true);

        let width = NonZeroU32::new(params.width)
            .ok_or_else(|| EncodeError::Init("width must be > 0".to_owned()))?;
        let height = NonZeroU32::new(params.height)
            .ok_or_else(|| EncodeError::Init("height must be > 0".to_owned()))?;

        let inner = video
            .create_wgpu_textures_encoder_h264(
                &gpu.queue,
                EncoderParametersH264 {
                    input_parameters: VideoParameters {
                        width,
                        height,
                        target_framerate: params.framerate.into(),
                    },
                    output_parameters,
                },
            )
            .map_err(|e| EncodeError::Init(e.to_string()))?;

        Ok(Self { params, inner })
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
        let _ = (&self.inner, frame, force_keyframe);
        todo!("gpu-video encode (NV12 wgpu::Texture → H.264 chunk) — issue #9")
    }
}
