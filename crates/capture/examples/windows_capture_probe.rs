//! Diagnostic probe for the Windows WGC backend: capture a few frames from the
//! primary monitor, log each frame's shared-handle descriptor, then prove the NT
//! handle is genuinely shareable by opening the last one on a *separate* D3D11
//! device, reading it back, and writing a PNG to the temp dir.
//!
//! `cargo run -p rewynd-capture --example windows_capture_probe`

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows_probe::run()
}

#[cfg(target_os = "windows")]
mod windows_probe {
    use std::ops::ControlFlow;
    use std::os::windows::io::AsRawHandle;
    use std::time::Instant;

    use rewynd_capture::StreamPrefs;
    use rewynd_capture::windows::{CapturedD3d11Frame, capture_stream};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::core::Interface;

    const FRAMES_TO_LOG: u32 = 5;

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();

        let prefs = StreamPrefs {
            width: 1920,
            height: 1080,
            framerate: 60,
        };

        // Capture until FRAMES_TO_LOG frames have been logged, keeping the last one
        // for the cross-device readback below.
        let (tx, rx) = std::sync::mpsc::channel::<CapturedD3d11Frame>();
        let mut frames_logged: u32 = 0;
        capture_stream(None, Instant::now(), prefs, None, move |frame| {
            tracing::info!(
                handle = ?frame.handle.as_raw_handle(),
                width = frame.width,
                height = frame.height,
                dxgi_format = ?frame.dxgi_format,
                pts = ?frame.pts,
                "shared-handle frame"
            );
            frames_logged += 1;
            if frames_logged >= FRAMES_TO_LOG {
                tracing::info!(frames = frames_logged, "captured enough frames");
                let _ = tx.send(frame);
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        let last = rx
            .recv()
            .map_err(|_| "capture ended without delivering the final frame")?;

        let out = std::env::temp_dir().join("rewynd-wgc-probe.png");
        let path = open_on_second_device_and_save(&last, &out)?;
        tracing::info!(path = %path, "wrote screenshot via a second D3D11 device");
        println!("Wrote {path}");
        Ok(())
    }

    /// Open the frame's NT shared handle on a freshly created (second) D3D11
    /// device, copy it to a staging texture, and save it as a PNG — proving the
    /// handle crosses device boundaries with the pixels intact.
    fn open_on_second_device_and_save(
        frame: &CapturedD3d11Frame,
        out_path: &std::path::Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let is_bgra = match frame.texture_format() {
            Some(wgpu::TextureFormat::Bgra8Unorm) => true,
            Some(wgpu::TextureFormat::Rgba8Unorm) => false,
            other => return Err(format!("unsupported capture format {other:?}").into()),
        };

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY: FFI.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }?;
        let device = device.ok_or("D3D11CreateDevice returned no device")?;
        let context = context.ok_or("D3D11CreateDevice returned no context")?;
        let device1: ID3D11Device1 = device.cast()?;

        // SAFETY: FFI; the NT handle is valid (owned by `frame`) and was created
        // shareable by the capture backend.
        let opened: ID3D11Texture2D =
            unsafe { device1.OpenSharedResource1(HANDLE(frame.handle.as_raw_handle())) }?;

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: frame.width,
            Height: frame.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: frame.dxgi_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: FFI.
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }?;
        let staging = staging.ok_or("CreateTexture2D returned no staging texture")?;

        // SAFETY: FFI; same device, same size/format.
        unsafe { context.CopyResource(&staging, &opened) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: FFI.
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }?;

        // BGRA→RGBA swizzle for the PNG encoder; alpha forced opaque (desktop
        // capture's "alpha" is meaningless for a screenshot).
        let width = frame.width as usize;
        let height = frame.height as usize;
        let mut rgba = vec![0u8; width * height * 4];
        // SAFETY: the mapping is valid for RowPitch * height bytes while mapped.
        let src = unsafe {
            std::slice::from_raw_parts(mapped.pData.cast::<u8>(), mapped.RowPitch as usize * height)
        };
        for row in 0..height {
            let src_row = &src[row * mapped.RowPitch as usize..][..width * 4];
            let dst_row = &mut rgba[row * width * 4..][..width * 4];
            for (s, d) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
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
        // SAFETY: FFI.
        unsafe { context.Unmap(&staging, 0) };

        let file = std::fs::File::create(out_path)?;
        let mut encoder =
            png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
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
