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

use crate::{EncodeError, EncodeParams, Encoder, fit};

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
    /// The aspect-preserving pre-pass, built the first time a frame actually needs it. A
    /// recorder whose encode size came from the display never touches it, so matched-aspect
    /// capture pays neither the pipeline compile nor the extra texture.
    letterbox: std::cell::RefCell<Option<LetterboxPass>>,
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
            letterbox: std::cell::RefCell::new(None),
        })
    }

    /// Convert an RGBA/BGRA texture (usage must include `TEXTURE_BINDING`) into an NV12
    /// [`wgpu::Texture`] of `out_width`×`out_height` and return its handle. The pass samples
    /// with normalized UVs through a linear sampler, so a differing output size scales the
    /// frame for free (captured monitor size → configured encode size).
    ///
    /// That free scale is non-uniform, so a source whose aspect ratio differs from the output's
    /// would come out stretched. When they disagree the frame first goes through a "contain"
    /// pre-pass ([`fit::contain_scale`]) that fits it inside the output with black bars — a
    /// pinned 16:9 recording of an ultrawide is bordered, never squashed. Matched aspects (the
    /// normal case, since the recorder derives the encode size from the display) skip it
    /// entirely.
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
        // Fit the frame into the output's shape first if the two disagree; `source` then always
        // has the output's aspect ratio, so the NV12 pass' uniform scale is a true resize.
        let fitted =
            (!fit::aspect_matches(rgba.width(), rgba.height(), width, height)).then(|| {
                let mut slot = self.letterbox.borrow_mut();
                slot.get_or_insert_with(|| LetterboxPass::new(&gpu.device))
                    .render(gpu, rgba, width, height)
            });
        let source = fitted.as_ref().unwrap_or(rgba);

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
        let rgba_bind_group = self.inner.create_input_bind_group(source);

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

/// The intermediate's format. Non-sRGB and 8-bit like every capture format we import, so the
/// pre-pass moves the pixels without re-encoding their values.
const LETTERBOX_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// A fullscreen quad, shrunk on one axis, drawn over a black frame — the "contain" fit that keeps
/// a mismatched source from being stretched by the NV12 pass.
///
/// The same shape the clip player uses to letterbox playback (`crates/settings/src/video.rs`),
/// which is why recording and playback agree on what a fitted frame looks like.
struct LetterboxPass {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// `vec2<f32>` quad scale (padded to 16 bytes, the uniform alignment), rewritten per frame.
    fit: wgpu::Buffer,
    /// The fitted frame, reused until the output size changes.
    target: Option<LetterboxTarget>,
}

struct LetterboxTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl LetterboxPass {
    fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rewynd letterbox shader"),
            source: wgpu::ShaderSource::Wgsl(LETTERBOX_SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rewynd letterbox bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rewynd letterbox pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rewynd letterbox pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: LETTERBOX_FORMAT,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rewynd letterbox sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let fit = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rewynd letterbox fit uniform"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            sampler,
            fit,
            target: None,
        }
    }

    /// Draw `src` into a `width`×`height` frame, centred, aspect intact, the remainder black.
    fn render(
        &mut self,
        gpu: &GpuContext,
        src: &wgpu::Texture,
        width: u32,
        height: u32,
    ) -> wgpu::Texture {
        if self
            .target
            .as_ref()
            .is_none_or(|t| t.width != width || t.height != height)
        {
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("rewynd letterbox frame"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: LETTERBOX_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.target = Some(LetterboxTarget {
                texture,
                view,
                width,
                height,
            });
        }
        let target = self.target.as_ref().expect("target set above");

        let [x, y] = fit::contain_scale(src.width(), src.height(), width, height);
        gpu.queue.write_buffer(&self.fit, 0, &fit_uniform(x, y));

        // Like the NV12 pass, the source is a fresh import each frame, so its bind group is
        // per-frame too; only the pipeline, sampler and target survive.
        let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rewynd letterbox bind group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.fit.as_entire_binding(),
                },
            ],
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rewynd letterbox"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rewynd letterbox"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Clear every frame: the bars must be black, not last frame's picture
                        // showing through where this one is narrower.
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);

        target.texture.clone()
    }
}

/// The fit uniform's bytes: a `vec2<f32>` padded to the 16-byte uniform alignment.
fn fit_uniform(x: f32, y: f32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&x.to_le_bytes());
    bytes[4..8].copy_from_slice(&y.to_le_bytes());
    bytes
}

/// A textured quad shrunk by `fit.scale`. Four vertices as a triangle strip; `v = 0` is the
/// texture's top row, matching the capture textures' orientation.
const LETTERBOX_SHADER: &str = r"
struct Fit { scale: vec2<f32> }
@group(0) @binding(2) var<uniform> fit: Fit;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    var out: VsOut;
    let u = f32(idx & 1u);
    let v = f32((idx >> 1u) & 1u);
    out.uv = vec2<f32>(u, v);
    let ndc = vec2<f32>(u * 2.0 - 1.0, 1.0 - v * 2.0) * fit.scale;
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
";
