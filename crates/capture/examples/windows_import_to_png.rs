//! End-to-end proof of the D3D11 shared handle → `wgpu::Texture` import: capture one
//! frame via WGC, import its NT handle into the shared Vulkan device zero-copy, read
//! it back, and write a PNG to the temp dir.
//!
//! `cargo run -p rewynd-capture --example windows_import_to_png`

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows_import::run()
}

#[cfg(target_os = "windows")]
mod windows_import {
    use std::ops::ControlFlow;
    use std::os::windows::io::AsHandle;
    use std::sync::mpsc;
    use std::time::Instant;

    use rewynd_capture::StreamPrefs;
    use rewynd_capture::windows::{CapturedD3d11Frame, capture_stream};
    use rewynd_gpu::{D3d11HandleImport, GpuContext};

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();

        let prefs = StreamPrefs {
            width: 1920,
            height: 1080,
            framerate: 60,
        };

        // Capture exactly one frame; the slot texture behind the handle stays alive
        // (and unwritten) after the stream stops, because the NT handle references it.
        let (tx, rx) = mpsc::channel::<CapturedD3d11Frame>();
        capture_stream(None, Instant::now(), prefs, None, move |frame| {
            let _ = tx.send(frame);
            ControlFlow::Break(())
        })?;
        let captured = rx
            .recv()
            .map_err(|_| "capture ended without delivering a frame")?;
        tracing::info!(
            width = captured.width,
            height = captured.height,
            dxgi_format = ?captured.dxgi_format,
            "captured one shared-handle frame for import"
        );

        // Bring up the shared GPU device (Vulkan, external-memory features enabled).
        let gpu = pollster::block_on(GpuContext::new())?;

        let out = std::env::temp_dir().join("rewynd-wgc-import.png");
        let path = import_and_save(&gpu, &captured, &out)?;
        tracing::info!(path = %path, "wrote screenshot");
        println!("Wrote {path}");
        Ok(())
    }

    fn import_and_save(
        gpu: &GpuContext,
        captured: &CapturedD3d11Frame,
        out_path: &std::path::Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let format = captured.texture_format().ok_or_else(|| {
            format!(
                "unsupported DXGI format {:?} (expected packed 32-bit RGB)",
                captured.dxgi_format
            )
        })?;
        // Bgra8Unorm orders channels B,G,R,A in memory, so the readback bytes need a
        // BGRA→RGBA swizzle before PNG (which expects RGBA).
        let is_bgra = format == wgpu::TextureFormat::Bgra8Unorm;

        let width = captured.width;
        let height = captured.height;

        let import = D3d11HandleImport {
            handle: captured.handle.as_handle(),
            width,
            height,
            format,
        };

        // SAFETY: `captured` came straight from the WGC backend, so the handle refers
        // to a shareable D3D11 texture matching these dimensions/format, fully written
        // (the backend waits on the copy before handing the handle out).
        let texture = unsafe { gpu.import_d3d11_shared_handle(import)? };

        // --- Readback: copy the texture into a mapped buffer, padding each row to the
        // 256-byte `bytes_per_row` alignment that `copy_texture_to_buffer` requires. ---
        const BYTES_PER_PIXEL: u32 = 4;
        let unpadded_bytes_per_row = width * BYTES_PER_PIXEL;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
        let buffer_size = u64::from(padded_bytes_per_row) * u64::from(height);

        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rewynd-d3d11-readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rewynd-d3d11-readback"),
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

        // Drop the per-row padding and (if BGRA) swizzle to RGBA; alpha forced opaque.
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
                        d[0] = s[2];
                        d[1] = s[1];
                        d[2] = s[0];
                    } else {
                        d[..3].copy_from_slice(&s[..3]);
                    }
                    d[3] = 0xff;
                }
            }
        }
        readback.unmap();

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

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("Windows only");
}
