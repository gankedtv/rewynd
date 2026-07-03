//! End-to-end check of the software-encoder path on a render-only device: RGBA → NV12
//! (via the shared converter) → CPU H.264 → decode. Exercises the GPU-readback adapter that
//! CI can't cover.
//!
//! Needs a Vulkan GPU, so it is `#[ignore]`d. Run on the dev box with:
//!
//! ```sh
//! cargo test -p rewynd-encode --test software_texture -- --ignored
//! ```
#![cfg(vulkan)]

use std::time::Duration;

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use rewynd_encode::{EncodeParams, Encoder, Nv12Converter, SoftwareTextureEncoder};
use rewynd_gpu::GpuContext;

const SIZE: u32 = 64;

/// A solid-colour RGBA8 source texture (usable as converter input).
fn make_rgba(gpu: &GpuContext, color: [u8; 4]) -> wgpu::Texture {
    let rgba = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test rgba source"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let pixels: Vec<u8> = color
        .iter()
        .copied()
        .cycle()
        .take((SIZE * SIZE * 4) as usize)
        .collect();
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &rgba,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(SIZE * 4),
            rows_per_image: Some(SIZE),
        },
        wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
    );
    rgba
}

#[test]
#[ignore = "requires a Vulkan GPU; run with --ignored on the dev box"]
fn software_encoder_encodes_from_nv12_texture() {
    let gpu = pollster::block_on(GpuContext::new_render_only()).expect("render-only device");
    let converter = Nv12Converter::new(&gpu).expect("RGBA->NV12 converter");
    let params = EncodeParams {
        width: SIZE,
        height: SIZE,
        framerate: 30,
        bitrate_bps: 2_000_000,
        idr_period: 4,
    };
    let mut enc = SoftwareTextureEncoder::new(&gpu, params).expect("software encoder");
    let mut dec = Decoder::new().expect("decoder");

    let mut decoded_any = false;
    for i in 0..8u8 {
        // Vary the colour per frame so the encoder does real work rather than skipping.
        let src = make_rgba(&gpu, [128, i.wrapping_mul(8), 200, 255]);
        let nv12 = converter.convert(&gpu, &src, SIZE, SIZE);
        let chunk = enc
            .encode(&nv12, i == 0, Duration::from_millis(u64::from(i) * 33))
            .expect("encodes");
        if i == 0 {
            assert!(chunk.is_keyframe, "first frame must be a keyframe");
        }
        if let Some(yuv) = dec.decode(&chunk.bytes).expect("decodes") {
            assert_eq!(yuv.dimensions(), (SIZE as usize, SIZE as usize));
            decoded_any = true;
        }
    }
    assert!(decoded_any, "decoder should yield at least one frame");
}
