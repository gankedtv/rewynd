//! Verifies the RGBA→NV12 conversion produces the expected BT.709 limited-range
//! luma/chroma for a known input colour.
//!
//! This needs a Vulkan GPU, so it is `#[ignore]`d (the CI runner has no GPU). Run it
//! on the dev box with:
//!
//! ```sh
//! cargo test -p rewynd-encode --test rgba_to_nv12 -- --ignored
//! ```
#![cfg(vulkan)]

use rewynd_encode::Nv12Converter;
use rewynd_gpu::GpuContext;

/// A solid mid-grey RGBA8 source; every byte 128 except a fully opaque alpha.
const GREY: [u8; 4] = [128, 128, 128, 255];
const SIZE: u32 = 64;

/// Byte tolerance on the read-back planes. The conversion runs through 8-bit unorm
/// render targets and bilinear sampling, so allow a couple of LSBs of slack.
const TOLERANCE: u8 = 3;

/// BT.709 limited-range expectations for [`GREY`], derived from the gpu-video shader:
///   Y' = clamp(luma * 219/255 + 16/255), luma = r·0.2126 + g·0.7152 + b·0.0722.
/// For r=g=b=128/255 the weights sum to 1, so luma = 128/255 and
///   Y' = round((128/255 · 0.85882 + 16/255) · 255) ≈ 126.
const EXPECTED_Y: u8 = 126;
/// Chroma for a neutral (grey) input collapses to zero offset, landing at the
/// limited-range centre: U = V = round((0.5 · 0.87843 + 16/255) · 255) ≈ 128.
const EXPECTED_UV: u8 = 128;

/// Round `bytes_per_row` up to the 256-byte alignment `copy_texture_to_buffer` requires.
fn aligned_bytes_per_row(unpadded: u32) -> u32 {
    const ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    unpadded.div_ceil(ALIGN) * ALIGN
}

/// Copy one NV12 plane into a CPU-readable buffer and return its tightly-packed
/// (de-padded) bytes. `bytes_per_texel` is 1 for the R8 Y plane, 2 for the Rg8 UV plane.
fn read_plane(
    gpu: &GpuContext,
    nv12: &wgpu::Texture,
    aspect: wgpu::TextureAspect,
    width: u32,
    height: u32,
    bytes_per_texel: u32,
) -> Vec<u8> {
    let unpadded_row = width * bytes_per_texel;
    let padded_row = aligned_bytes_per_row(unpadded_row);
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("nv12 plane readback"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: nv12,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);

    buffer.slice(..).map_async(wgpu::MapMode::Read, |r| {
        r.expect("map nv12 plane readback buffer");
    });
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll device for readback");

    let mapped = buffer.slice(..).get_mapped_range().expect("mapped range");
    let mut packed = Vec::with_capacity((unpadded_row * height) as usize);
    for row in mapped.chunks_exact(padded_row as usize) {
        packed.extend_from_slice(&row[..unpadded_row as usize]);
    }
    packed
}

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
fn grey_rgba_converts_to_bt709_limited_nv12() {
    let gpu = pollster::block_on(GpuContext::new()).expect("create shared wgpu device");
    let converter = Nv12Converter::new(&gpu).expect("build BT.709 limited RGBA->NV12 converter");

    let close = |actual: u8, expected: u8| actual.abs_diff(expected) <= TOLERANCE;

    // First frame: mid-grey → Y≈126, neutral chroma ≈128.
    let nv12 = converter.convert(&gpu, &make_rgba(&gpu, GREY));
    let y = read_plane(&gpu, &nv12, wgpu::TextureAspect::Plane0, SIZE, SIZE, 1);
    let uv = read_plane(
        &gpu,
        &nv12,
        wgpu::TextureAspect::Plane1,
        SIZE / 2,
        SIZE / 2,
        2,
    );

    for (i, &v) in y.iter().enumerate() {
        assert!(
            close(v, EXPECTED_Y),
            "Y[{i}] = {v}, expected ~{EXPECTED_Y} (±{TOLERANCE})"
        );
    }
    for (i, chunk) in uv.chunks_exact(2).enumerate() {
        assert!(
            close(chunk[0], EXPECTED_UV),
            "U[{i}] = {} expected ~{EXPECTED_UV} (±{TOLERANCE})",
            chunk[0]
        );
        assert!(
            close(chunk[1], EXPECTED_UV),
            "V[{i}] = {} expected ~{EXPECTED_UV} (±{TOLERANCE})",
            chunk[1]
        );
    }

    // Second frame on the SAME converter exercises the reused output texture: black →
    // limited-range Y≈16, proving the cached target is rewritten in place (not stale).
    const EXPECTED_BLACK_Y: u8 = 16;
    let black = converter.convert(&gpu, &make_rgba(&gpu, [0, 0, 0, 255]));
    let yb = read_plane(&gpu, &black, wgpu::TextureAspect::Plane0, SIZE, SIZE, 1);
    for (i, &v) in yb.iter().enumerate() {
        assert!(
            close(v, EXPECTED_BLACK_Y),
            "black Y[{i}] = {v}, expected ~{EXPECTED_BLACK_Y} (±{TOLERANCE})"
        );
    }
}
