//! A GPU video-frame widget: one persistent texture, rewritten in place each frame.
//!
//! Playback pushes a fresh decoded frame ~60x/second. Through iced's normal `image` widget on the
//! wgpu backend, each frame is a new image handle whose atlas entry is trimmed the following frame,
//! leaving a one-frame gap that reads as flicker (a black frame). This custom shader primitive owns
//! a single texture and rewrites it with `write_texture` every frame — no atlas, no per-frame
//! allocation, no gap. The frame is letterboxed to fit the widget (never cropped), matching the
//! `ContentFit::Contain` the `image` widget used.

use std::sync::Arc;

use iced::widget::shader::{self, Primitive, Program};
use iced::{Length, Rectangle, mouse};
use iced_wgpu::wgpu;

/// One decoded RGBA8 frame, shared cheaply from the decode thread.
#[derive(Debug, Clone)]
pub struct Frame {
    pub pixels: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

/// A shader widget that fills its space and renders `frame` letterboxed.
pub fn video<'a, Message: 'a>(frame: Frame) -> iced::Element<'a, Message> {
    shader::Shader::new(frame)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

impl<Message> Program<Message> for Frame {
    type State = ();
    type Primitive = FramePrimitive;

    fn draw(&self, _state: &(), _cursor: mouse::Cursor, _bounds: Rectangle) -> FramePrimitive {
        FramePrimitive(self.clone())
    }
}

#[derive(Debug)]
pub struct FramePrimitive(Frame);

impl Primitive for FramePrimitive {
    type Pipeline = Pipeline;

    fn prepare(
        &self,
        pipeline: &mut Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        _viewport: &shader::Viewport,
    ) {
        pipeline.prepare(device, queue, &self.0, bounds);
    }

    fn draw(&self, pipeline: &Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        pipeline.draw(render_pass)
    }
}

/// Shared GPU state: the pipeline, sampler, a fit uniform, and a texture reused across frames
/// (only recreated when the frame's dimensions change).
#[derive(Debug)]
pub struct Pipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    fit: wgpu::Buffer,
    texture: Option<Texture>,
}

#[derive(Debug)]
struct Texture {
    width: u32,
    height: u32,
    handle: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

impl shader::Pipeline for Pipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rewynd video shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rewynd video bind group layout"),
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
            label: Some("rewynd video pipeline layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rewynd video pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rewynd video sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let fit = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rewynd video fit uniform"),
            size: 16, // vec2<f32> scale + pad
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            sampler,
            fit,
            texture: None,
        }
    }
}

impl Pipeline {
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Frame,
        bounds: &Rectangle,
    ) {
        let (w, h) = (frame.width, frame.height);
        if w == 0 || h == 0 || frame.pixels.len() < (w * h * 4) as usize {
            return;
        }
        // (Re)create the texture only when the frame size changes; otherwise reuse it.
        let stale = self
            .texture
            .as_ref()
            .is_none_or(|t| t.width != w || t.height != h);
        if stale {
            let handle = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("rewynd video texture"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = handle.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rewynd video bind group"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
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
            self.texture = Some(Texture {
                width: w,
                height: h,
                handle,
                bind_group,
            });
        }
        let texture = self.texture.as_ref().expect("texture just set");
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture.handle,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        // Letterbox: scale the fullscreen quad down on the wider axis so the frame's aspect ratio
        // is preserved inside the widget bounds (Contain, never cropped).
        let frame_aspect = w as f32 / h as f32;
        let bounds_aspect = (bounds.width / bounds.height).max(f32::EPSILON);
        let scale = if frame_aspect > bounds_aspect {
            [1.0, bounds_aspect / frame_aspect]
        } else {
            [frame_aspect / bounds_aspect, 1.0]
        };
        queue.write_buffer(&self.fit, 0, bytemuck_cast(&[scale[0], scale[1], 0.0, 0.0]));
    }

    fn draw(&self, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some(texture) = self.texture.as_ref() else {
            return false;
        };
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &texture.bind_group, &[]);
        render_pass.draw(0..4, 0..1);
        true
    }
}

/// Reinterpret a `[f32; 4]` as bytes without pulling in the `bytemuck` crate.
fn bytemuck_cast(v: &[f32; 4]) -> &[u8] {
    // SAFETY: `[f32; 4]` is 16 contiguous bytes with no padding or invalid bit patterns.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), std::mem::size_of::<[f32; 4]>()) }
}

const SHADER: &str = r"
struct Fit { scale: vec2<f32> };
@group(0) @binding(2) var<uniform> fit: Fit;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

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
