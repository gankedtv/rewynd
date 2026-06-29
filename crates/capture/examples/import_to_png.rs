//! End-to-end proof of the DMA-BUF → `wgpu::Texture` import: capture one screen
//! frame, import it zero-copy, read it back, and write a PNG to the temp dir.
//!
//! `cargo run -p rewynd-capture --example import_to_png`

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::mpsc;

    use rewynd_capture::linux::{CapturedDmabuf, capture_one_dmabuf, open_portal};
    use rewynd_gpu::{DmabufImport, GpuContext};

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();

        // The portal flow is async (ashpd uses tokio). Keep the runtime alive for the
        // whole program: the PipeWire main loop blocks the main thread, and the portal
        // Session must outlive it.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        let mut portal = runtime.block_on(open_portal())?;
        tracing::info!(
            node_id = portal.node_id,
            fd = portal.raw_fd(),
            size = ?portal.size,
            "portal session established"
        );

        // Capture exactly one DMA-BUF frame (its fd is dup'd into an OwnedFd that
        // outlives the stream). `portal`/`runtime` stay alive across this call.
        let node_id = portal.node_id;
        let fd = portal.take_fd();
        let captured = capture_one_dmabuf(node_id, fd)?;
        tracing::info!(
            fourcc = format_args!("{:#010x}", captured.fourcc),
            modifier = format_args!("{:#018x}", captured.drm_modifier),
            width = captured.width,
            height = captured.height,
            stride = captured.stride,
            offset = captured.offset,
            "captured one DMA-BUF frame"
        );

        // Bring up the shared GPU device (Vulkan, external-memory features enabled).
        let gpu = runtime.block_on(GpuContext::new())?;

        let out = std::env::temp_dir().join("rewynd-screenshot.png");
        let path = import_and_save(&gpu, captured, &out)?;
        tracing::info!(path = %path, "wrote screenshot");
        println!("Wrote {path}");

        // Keep the portal session + runtime explicitly alive until here.
        drop(portal);
        drop(runtime);
        Ok(())
    }

    fn import_and_save(
        gpu: &GpuContext,
        captured: CapturedDmabuf,
        out_path: &std::path::Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let format = captured.texture_format().ok_or_else(|| {
            format!(
                "unsupported DRM fourcc {:#010x} (expected packed 32-bit RGB)",
                captured.fourcc
            )
        })?;
        // Bgra8Unorm orders channels B,G,R,A in memory, so the readback bytes need a
        // BGRA→RGBA swizzle before PNG (which expects RGBA).
        let is_bgra = format == wgpu::TextureFormat::Bgra8Unorm;

        let width = captured.width;
        let height = captured.height;

        let import = DmabufImport {
            fd: captured.fd,
            width,
            height,
            format,
            drm_modifier: captured.drm_modifier,
            // The stride/offset come from the negotiated chunk; both are non-negative
            // for a real frame (the descriptor stores them as i32 only because SPA does).
            stride: u32::try_from(captured.stride)
                .map_err(|_| format!("negative DMA-BUF stride {}", captured.stride))?,
            offset: u32::try_from(captured.offset)
                .map_err(|_| format!("negative DMA-BUF offset {}", captured.offset))?,
        };

        // SAFETY: `captured` came straight from the PipeWire negotiation, so the fd is a
        // valid single-plane DMA-BUF whose format/modifier/stride/offset match `import`.
        let texture = unsafe { gpu.import_dmabuf(import)? };

        // --- Readback: copy the texture into a mapped buffer, padding each row to the
        // 256-byte `bytes_per_row` alignment that `copy_texture_to_buffer` requires. ---
        const BYTES_PER_PIXEL: u32 = 4;
        let unpadded_bytes_per_row = width * BYTES_PER_PIXEL;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
        let buffer_size = u64::from(padded_bytes_per_row) * u64::from(height);

        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rewynd-dmabuf-readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rewynd-dmabuf-readback"),
            });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        // Live A/V will need to wait on the producer's completion semaphore
        // (VK_KHR_external_semaphore_fd via `Queue::add_wait_semaphore`) before reading;
        // a single static readback relies on the frame being complete + the poll below.
        gpu.queue.submit([encoder.finish()]);

        // Map the readback buffer and block until the GPU work + mapping complete.
        let (tx, rx) = mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        gpu.device.poll(wgpu::PollType::wait_indefinitely())?;
        rx.recv()
            .map_err(|_| "map_async callback dropped without firing")??;

        // Copy out the rows, dropping the per-row padding and (if BGRA) swizzling to
        // the RGBA byte order the `png` encoder expects. Alpha is forced opaque since
        // desktop capture's "alpha" is meaningless for a screenshot.
        let mut rgba = vec![0u8; unpadded_bytes_per_row as usize * height as usize];
        {
            let view = readback.slice(..).get_mapped_range()?;
            for row in 0..height as usize {
                let src =
                    &view[row * padded_bytes_per_row as usize..][..unpadded_bytes_per_row as usize];
                let dst = &mut rgba[row * unpadded_bytes_per_row as usize..]
                    [..unpadded_bytes_per_row as usize];
                for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                    if is_bgra {
                        d[0] = s[2]; // R <- B-position byte (BGRA byte0=B, byte2=R)
                        d[1] = s[1]; // G
                        d[2] = s[0]; // B
                    } else {
                        d[0] = s[0];
                        d[1] = s[1];
                        d[2] = s[2];
                    }
                    d[3] = 0xff; // opaque
                }
            }
        }
        readback.unmap();

        // Write the PNG.
        let file = std::fs::File::create(out_path)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&rgba)?;

        let abs = std::fs::canonicalize(out_path)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| out_path.display().to_string());
        Ok(abs)
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("Linux only");
}
