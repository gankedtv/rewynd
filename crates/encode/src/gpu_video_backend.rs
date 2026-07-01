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
        pts: std::time::Duration,
    ) -> Result<EncodedChunk, EncodeError> {
        // gpu-video takes the NV12 texture by value (it copies it into its own input
        // image), but the trait borrows it, so clone the wgpu handle — a cheap ref-count
        // bump, not a pixel copy. The converter reuses one texture across frames; the
        // copy-in here is what makes that reuse safe.
        let chunk = self
            .inner
            .encode(
                gpu_video::InputFrame {
                    data: frame.clone(),
                    // gpu-video drives its own GOP/rate control from target_framerate;
                    // we carry the real capture PTS on the chunk ourselves.
                    pts: None,
                },
                force_keyframe,
            )
            .map_err(|e| EncodeError::Encode(e.to_string()))?;

        Ok(EncodedChunk {
            bytes: chunk.data.into(),
            is_keyframe: chunk.is_keyframe,
            // The capture-relative timestamp, carried through verbatim for the ring
            // buffer's window eviction and the muxer's per-sample timing.
            pts,
        })
    }
}

/// RGBA→NV12 colour-space converter, backed by `gpu-video`'s
/// [`WgpuRgbaToNv12Converter`]. Produces the NV12 input [`Encoder::encode`] expects.
pub struct Nv12Converter {
    inner: WgpuRgbaToNv12Converter,
    /// Reused NV12 output texture + its plane views, (re)created only when the frame
    /// size changes. The capture format is fixed for a stream, so this is allocated
    /// once and rewritten in place each frame instead of per-call. `RefCell` because
    /// the converter is driven single-threaded on the capture thread.
    output: std::cell::RefCell<Option<Nv12Output>>,
}

/// The cached NV12 render target the converter writes each frame.
struct Nv12Output {
    texture: wgpu::Texture,
    y_view: wgpu::TextureView,
    uv_view: wgpu::TextureView,
    width: u32,
    height: u32,
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
        Ok(Self {
            inner,
            output: std::cell::RefCell::new(None),
        })
    }

    /// Convert an RGBA/BGRA texture (usage must include `TEXTURE_BINDING`) into an NV12
    /// [`wgpu::Texture`] of `out_width`×`out_height` and return its handle. The pass samples
    /// with normalized UVs through a linear sampler, so a differing output size scales the
    /// frame for free (captured monitor size → configured encode size).
    ///
    /// The NV12 output texture is reused across calls (re-created only when the frame
    /// size changes), so this allocates no per-frame GPU texture on the hot path. The
    /// caller must consume the returned frame (encode it) before the next `convert`,
    /// which overwrites the same texture; the GPU queue orders that write after the
    /// encoder's read, so per-frame `convert → encode` is safe.
    #[must_use]
    pub fn convert(
        &self,
        gpu: &GpuContext,
        rgba: &wgpu::Texture,
        out_width: u32,
        out_height: u32,
    ) -> wgpu::Texture {
        let (width, height) = (out_width, out_height);
        let mut slot = self.output.borrow_mut();
        if slot
            .as_ref()
            .is_none_or(|o| o.width != width || o.height != height)
        {
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("rewynd nv12 frame"),
                size: wgpu::Extent3d {
                    width,
                    height,
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
            let y_view = texture.create_view(&wgpu::TextureViewDescriptor {
                aspect: wgpu::TextureAspect::Plane0,
                ..Default::default()
            });
            let uv_view = texture.create_view(&wgpu::TextureViewDescriptor {
                aspect: wgpu::TextureAspect::Plane1,
                ..Default::default()
            });
            *slot = Some(Nv12Output {
                texture,
                y_view,
                uv_view,
                width,
                height,
            });
        }
        let output = slot.as_ref().expect("output set above");

        // The input texture is a fresh DMA-BUF import each frame, so its bind group can't
        // be cached; only the output target is reused.
        let rgba_bind_group = self.inner.create_input_bind_group(rgba);

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rewynd rgba->nv12"),
            });
        self.inner.convert(
            &mut encoder,
            &rgba_bind_group,
            &output.y_view,
            &output.uv_view,
        );
        gpu.queue.submit([encoder.finish()]);

        output.texture.clone()
    }
}
