//! The `gpu-video`-backed [`Encoder`] implementation. Gated to targets where
//! `gpu-video` (Vulkan Video) builds; see the parent module's `#[cfg]`.

use std::num::NonZeroU32;

use gpu_video::{
    VideoDeviceExt, WgpuRgbaToNv12Converter,
    parameters::{
        ColorRange, ColorSpace, EncoderParametersH264, RateControl, VideoParameters,
        WgpuConverterParameters,
    },
};
use rewynd_buffer::EncodedChunk;
use rewynd_gpu::GpuContext;

use crate::{EncodeError, EncodeParams, Encoder};

/// Burst headroom the VBV rate control may use over the average bitrate.
/// Provisional default — encoder-param tuning is revisited (and ADR'd) later.
const MAX_BITRATE_RATIO: u64 = 2;
/// Rate-control averaging window (virtual buffer size). Provisional default.
const VBV_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// `gpu-video`-backed H.264 encoder, constructed against the shared [`GpuContext`].
pub struct GpuVideoEncoder {
    params: EncodeParams,
    // Owns the live gpu-video encoder so it lives as long as this wrapper.
    inner: gpu_video::WgpuTexturesEncoderH264,
    /// Count of frames encoded so far, used to stamp each chunk's presentation time.
    frames_encoded: u64,
}

impl GpuVideoEncoder {
    /// Build the encoder on the shared device (PLAN §4.3, §3.3): a `gpu-video`
    /// encoder constructed on the *same* wgpu device as the rest of the pipeline.
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

        Ok(Self {
            params,
            inner,
            frames_encoded: 0,
        })
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
        // gpu-video takes the NV12 texture by value; the converter hands us a fresh
        // texture each frame, but the trait borrows it, so clone the wgpu handle (a
        // cheap ref-count bump, not a pixel copy).
        let chunk = self
            .inner
            .encode(
                gpu_video::InputFrame {
                    data: frame.clone(),
                    // gpu-video drives its own GOP/rate control from target_framerate;
                    // we stamp the chunk's PTS ourselves below.
                    pts: None,
                },
                force_keyframe,
            )
            .map_err(|e| EncodeError::Encode(e.to_string()))?;

        // Presentation timestamp derived from the configured (constant) framerate:
        // chunk N presents at N / framerate. The ring buffer's window eviction
        // measures elapsed time as the difference between chunk PTSs, so this must be
        // monotonic. (Drop-accurate timing would need capture timestamps threaded in.)
        let fps = u64::from(self.params.framerate).max(1);
        let pts = std::time::Duration::from_nanos(self.frames_encoded * 1_000_000_000 / fps);
        self.frames_encoded += 1;

        Ok(EncodedChunk {
            bytes: chunk.data,
            is_keyframe: chunk.is_keyframe,
            pts,
        })
    }
}

/// RGBA→NV12 colour-space converter, backed by `gpu-video`'s
/// [`WgpuRgbaToNv12Converter`]. Produces the NV12 input [`Encoder::encode`] expects.
pub struct Nv12Converter {
    inner: WgpuRgbaToNv12Converter,
}

impl Nv12Converter {
    /// Build a BT.709 limited-range RGBA→NV12 converter on the shared device.
    /// `gpu-video` only supports BT.709 limited; other combinations are rejected.
    pub fn new(gpu: &GpuContext) -> Result<Self, EncodeError> {
        let inner = WgpuRgbaToNv12Converter::new(
            &gpu.device,
            WgpuConverterParameters {
                color_space: ColorSpace::BT709,
                color_range: ColorRange::Limited,
            },
        )
        .map_err(|e| EncodeError::Init(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Convert an RGBA/BGRA texture (usage must include `TEXTURE_BINDING`) into a
    /// fresh NV12 [`wgpu::Texture`] of the same size.
    ///
    /// Allocates the NV12 texture per call; the live encode path reuses one.
    #[must_use]
    pub fn convert(&self, gpu: &GpuContext, rgba: &wgpu::Texture) -> wgpu::Texture {
        let nv12 = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rewynd nv12 frame"),
            size: wgpu::Extent3d {
                width: rgba.width(),
                height: rgba.height(),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::NV12,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let y_view = nv12.create_view(&wgpu::TextureViewDescriptor {
            aspect: wgpu::TextureAspect::Plane0,
            ..Default::default()
        });
        let uv_view = nv12.create_view(&wgpu::TextureViewDescriptor {
            aspect: wgpu::TextureAspect::Plane1,
            ..Default::default()
        });
        let rgba_bind_group = self.inner.create_input_bind_group(rgba);

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rewynd rgba->nv12"),
            });
        self.inner
            .convert(&mut encoder, &rgba_bind_group, &y_view, &uv_view);
        gpu.queue.submit([encoder.finish()]);

        nv12
    }
}
