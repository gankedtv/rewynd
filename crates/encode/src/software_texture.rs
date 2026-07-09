//! [`Encoder`]-trait adapter that feeds the CPU [`SoftwareEncoder`] from an NV12
//! `wgpu::Texture`.
//!
//! The capture pipeline hands every encoder an NV12 texture on the GPU (produced by
//! [`crate::Nv12Converter`]). The software encoder needs those planes in host memory, so
//! this adapter reads the texture back — Y plane and interleaved UV plane — deinterleaves
//! the chroma into I420, and hands it to the core. Readback buffers are reused across
//! frames; the copy runs on the caller's (capture) thread.
//!
//! This needs a wgpu device, so it can't run in CI without a GPU — it is coverage-excluded
//! and covered by an `#[ignore]`d GPU test, exactly like the gpu-video backend.

use std::time::Duration;

use rewynd_buffer::EncodedChunk;
use rewynd_gpu::GpuContext;

use crate::software::deinterleave_uv;
use crate::{EncodeError, EncodeParams, Encoder, I420Frame, SoftwareEncoder};

/// CPU H.264 encoder driven from GPU NV12 textures.
pub struct SoftwareTextureEncoder {
    device: wgpu::Device,
    queue: wgpu::Queue,
    core: SoftwareEncoder,
    /// Readback staging buffers, (re)created when the frame size changes.
    staging: Option<Staging>,
    /// Reused host-side plane buffers.
    y: Vec<u8>,
    uv: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

struct Staging {
    width: u32,
    height: u32,
    y_buf: wgpu::Buffer,
    uv_buf: wgpu::Buffer,
}

impl SoftwareTextureEncoder {
    /// Build the adapter on the shared device. Validation lives in [`SoftwareEncoder::new`].
    pub fn new(gpu: &GpuContext, params: EncodeParams) -> Result<Self, EncodeError> {
        let core = SoftwareEncoder::new(params)?;
        Ok(Self {
            device: gpu.device.clone(),
            queue: gpu.queue.clone(),
            core,
            staging: None,
            y: Vec::new(),
            uv: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
        })
    }

    /// The parameters this encoder was configured with.
    #[must_use]
    pub fn params(&self) -> EncodeParams {
        self.core.params()
    }

    fn ensure_staging(&mut self, width: u32, height: u32) {
        if self
            .staging
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height)
        {
            return;
        }
        // Y is full-res R8 (1 B/texel); UV is half-res Rg8 (2 B/texel).
        let y_size = u64::from(aligned_bytes_per_row(width)) * u64::from(height);
        let uv_size = u64::from(aligned_bytes_per_row((width / 2) * 2)) * u64::from(height / 2);
        let y_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rewynd sw-enc Y readback"),
            size: y_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let uv_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rewynd sw-enc UV readback"),
            size: uv_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.staging = Some(Staging {
            width,
            height,
            y_buf,
            uv_buf,
        });
    }
}

impl Encoder for SoftwareTextureEncoder {
    fn encode(
        &mut self,
        frame: &wgpu::Texture,
        force_keyframe: bool,
        pts: Duration,
    ) -> Result<EncodedChunk, EncodeError> {
        let width = frame.width();
        let height = frame.height();
        self.ensure_staging(width, height);
        // wgpu::Buffer is a cheap Arc handle; clone so the borrow of `self.staging` doesn't
        // collide with the mutable borrows of the plane buffers below.
        let (y_buf, uv_buf) = {
            let s = self.staging.as_ref().expect("ensured above");
            (s.y_buf.clone(), s.uv_buf.clone())
        };

        // Both plane copies ride one submit and one poll: every extra submit/poll pair is a
        // full CPU-GPU synchronization stall on the capture thread.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        record_plane_copy(
            &mut encoder,
            frame,
            wgpu::TextureAspect::Plane0,
            width,
            height,
            1,
            &y_buf,
        );
        record_plane_copy(
            &mut encoder,
            frame,
            wgpu::TextureAspect::Plane1,
            width / 2,
            height / 2,
            2,
            &uv_buf,
        );
        self.queue.submit([encoder.finish()]);
        // On any readback failure, drop the staging pair instead of unmapping: a failed map
        // can leave one buffer mapped, and copying into a still-mapped buffer next frame is a
        // validation error. The next frame simply recreates them.
        if let Err(e) = map_for_read(&self.device, [&y_buf, &uv_buf]) {
            self.staging = None;
            return Err(e);
        }
        if let Err(e) = copy_depadded(&y_buf, width, 1, &mut self.y)
            .and_then(|()| copy_depadded(&uv_buf, width / 2, 2, &mut self.uv))
        {
            self.staging = None;
            return Err(e);
        }
        y_buf.unmap();
        uv_buf.unmap();
        deinterleave_uv(&self.uv, &mut self.u, &mut self.v);

        self.core.encode_i420(
            I420Frame {
                y: &self.y,
                u: &self.u,
                v: &self.v,
                width,
                height,
            },
            force_keyframe,
            pts,
        )
    }
}

/// Round `bytes_per_row` up to the 256-byte alignment `copy_texture_to_buffer` requires.
fn aligned_bytes_per_row(unpadded: u32) -> u32 {
    const ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    unpadded.div_ceil(ALIGN) * ALIGN
}

/// Record the copy of one texture plane into its staging buffer. `bytes_per_texel` is 1 for
/// the R8 Y plane, 2 for the Rg8 UV plane.
fn record_plane_copy(
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    aspect: wgpu::TextureAspect,
    width: u32,
    height: u32,
    bytes_per_texel: u32,
    staging: &wgpu::Buffer,
) {
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(aligned_bytes_per_row(width * bytes_per_texel)),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

/// Map every staging buffer for reading behind a single device poll.
fn map_for_read<const N: usize>(
    device: &wgpu::Device,
    buffers: [&wgpu::Buffer; N],
) -> Result<(), EncodeError> {
    let (tx, rx) = std::sync::mpsc::channel();
    for buffer in buffers {
        let tx = tx.clone();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
    }
    // Only the callbacks hold senders now: a callback that never fires disconnects the
    // channel and surfaces as an error below instead of blocking the capture thread forever.
    drop(tx);
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| EncodeError::Encode(format!("device poll failed: {e}")))?;
    for _ in 0..N {
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(EncodeError::Encode(format!("failed to map readback: {e}"))),
            Err(e) => return Err(EncodeError::Encode(format!("readback channel closed: {e}"))),
        }
    }
    Ok(())
}

/// Copy a mapped plane into the tightly-packed (de-padded) `dst`. The caller unmaps.
fn copy_depadded(
    staging: &wgpu::Buffer,
    width: u32,
    bytes_per_texel: u32,
    dst: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    let unpadded_row = (width * bytes_per_texel) as usize;
    let padded_row = aligned_bytes_per_row(width * bytes_per_texel) as usize;
    let mapped = staging
        .slice(..)
        .get_mapped_range()
        .map_err(|e| EncodeError::Encode(format!("map readback range: {e}")))?;
    dst.clear();
    dst.reserve(mapped.len() / padded_row * unpadded_row);
    for row in mapped.chunks_exact(padded_row) {
        dst.extend_from_slice(&row[..unpadded_row]);
    }
    Ok(())
}
