//! End-to-end proof of the live recording hot path: capture → zero-copy DMA-BUF
//! import → RGBA/BGRx→NV12 → gpu-video H.264 encode → write Annex-B chunks to a
//! `.h264` file. A spike that proves the whole pipeline runs and reports the
//! CPU/GPU overhead of doing it inline per frame.
//!
//! `cargo run -p rewynd-capture --example record_h264`
//!
//! Records ~300 frames (~5 s at 60fps) by default; override with `RECORD_FRAMES`.
//! The output file is `$TMPDIR/rewynd-capture.h264`. Play it with e.g.
//! `ffplay $TMPDIR/rewynd-capture.h264` or `mpv`.

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::cell::RefCell;
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::ops::ControlFlow;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use rewynd_capture::linux::{CapturedDmabuf, capture_stream, open_portal};
    use rewynd_encode::{EncodeParams, Encoder, GpuVideoEncoder, Nv12Converter};
    use rewynd_gpu::{DmabufImport, GpuContext};

    /// Default number of frames to record when `RECORD_FRAMES` is unset
    /// (~5 s at 60fps).
    const DEFAULT_FRAMES: u32 = 300;

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();

        let max_frames: u32 = std::env::var("RECORD_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_FRAMES);

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

        // Shared device (Vulkan, external-memory + video features), the RGBA→NV12
        // converter, and the H.264 encoder all live on the same wgpu device. Encoder
        // params (resolution / framerate / bitrate / GOP) come from EncodeParams, never
        // hard-coded in the loop.
        //
        // These are held in `Rc` so the `'static` capture closure can borrow them while
        // `run` keeps the owning handles. That keeps GPU teardown out of the closure
        // (which the PipeWire stream drops mid-shutdown) and lets it happen here, after
        // capture has fully stopped, in dependency order (encoder → converter → device):
        // tearing the device down before the encoder it backs crashes the NVIDIA driver.
        let gpu = Rc::new(runtime.block_on(GpuContext::new())?);
        let conv = Rc::new(Nv12Converter::new(&gpu)?);
        let params = EncodeParams::default();
        let enc = Rc::new(RefCell::new(GpuVideoEncoder::new(&gpu, params)?));

        let out_path = std::env::temp_dir().join("rewynd-capture.h264");
        let file = File::create(&out_path)?;
        let writer = BufWriter::new(file);
        tracing::info!(path = %out_path.display(), max_frames, "recording H.264 to file");

        // The per-frame callback must be `'static`: it borrows the pipeline via `Rc`
        // clones and owns `writer` outright. The accounting results must be read back
        // here afterwards, so they live in a shared `Rc<RefCell<..>>`.
        let stats = Rc::new(RefCell::new(Stats::default()));

        // Overhead accounting: wall clock + this process's CPU time across the loop.
        let cpu_start = read_self_cpu_seconds();
        let wall_start = Instant::now();

        capture_stream(
            portal.node_id,
            portal.take_fd(),
            wall_start,
            rewynd_capture::linux::StreamPrefs::default(),
            None,
            {
                // The whole hot path runs inline on the PipeWire process thread — import +
                // convert + encode + file write all happen before the next frame is dequeued.
                let stats = stats.clone();
                let gpu = gpu.clone();
                let conv = conv.clone();
                let enc = enc.clone();
                let mut writer = writer;
                move |captured: CapturedDmabuf| -> ControlFlow<()> {
                    let mut s = stats.borrow_mut();
                    let frame_index = s.frames;
                    let span = tracing::debug_span!("frame", index = frame_index);
                    let _e = span.enter();

                    match encode_one(
                        &gpu,
                        &conv,
                        &mut enc.borrow_mut(),
                        captured,
                        frame_index,
                        &mut writer,
                    ) {
                        Ok(chunk) => {
                            s.total_bytes += chunk.bytes as u64;
                            if chunk.is_keyframe {
                                s.keyframes += 1;
                            }
                            tracing::debug!(
                                index = frame_index,
                                bytes = chunk.bytes,
                                is_keyframe = chunk.is_keyframe,
                                "encoded frame"
                            );
                            s.frames += 1;
                            if s.frames >= max_frames {
                                // Flush before the writer is dropped so all bytes hit the file.
                                if let Err(e) = writer.flush() {
                                    s.error = Some(e.to_string());
                                }
                                ControlFlow::Break(())
                            } else {
                                ControlFlow::Continue(())
                            }
                        }
                        Err(e) => {
                            // Don't hang on a per-frame failure: log it, surface it, and stop.
                            tracing::error!(index = frame_index, error = %e, "frame failed; stopping");
                            let _ = writer.flush();
                            s.error = Some(e.to_string());
                            ControlFlow::Break(())
                        }
                    }
                }
            },
        )?;

        let wall = wall_start.elapsed();
        let cpu = read_self_cpu_seconds().map(|end| end - cpu_start.unwrap_or(end));

        let stats = stats.borrow();
        if let Some(err) = &stats.error {
            return Err(format!("recording stopped on a frame error: {err}").into());
        }

        report(
            &out_path,
            stats.frames,
            stats.keyframes,
            stats.total_bytes,
            wall,
            cpu,
            params,
        );

        // Tear down in dependency order: the encoder and converter hold Vulkan objects on
        // the device, so they must drop before the device. Then release the portal session
        // and the tokio runtime.
        drop(enc);
        drop(conv);
        drop(gpu);
        drop(portal);
        drop(runtime);
        Ok(())
    }

    /// Loop accounting shared between the `'static` capture callback and `run`.
    #[derive(Default)]
    struct Stats {
        frames: u32,
        keyframes: u32,
        total_bytes: u64,
        error: Option<String>,
    }

    /// One frame of the hot path: import the DMA-BUF, convert to NV12, encode, and
    /// write the resulting Annex-B chunk to `writer`. Returns the chunk's size +
    /// keyframe flag for accounting.
    struct ChunkInfo {
        bytes: usize,
        is_keyframe: bool,
    }

    fn encode_one(
        gpu: &GpuContext,
        conv: &Nv12Converter,
        enc: &mut GpuVideoEncoder,
        captured: CapturedDmabuf,
        frame_index: u32,
        writer: &mut impl Write,
    ) -> Result<ChunkInfo, Box<dyn std::error::Error>> {
        let pts = captured.pts;
        let format = captured.texture_format().ok_or_else(|| {
            format!(
                "unsupported DRM fourcc {:#010x} (expected packed 32-bit RGB)",
                captured.fourcc
            )
        })?;

        let import = DmabufImport {
            // The import consumes the owned fd (Vulkan takes ownership on success).
            fd: captured.fd,
            width: captured.width,
            height: captured.height,
            format,
            drm_modifier: captured.drm_modifier,
            stride: u32::try_from(captured.stride)
                .map_err(|_| format!("negative DMA-BUF stride {}", captured.stride))?,
            offset: u32::try_from(captured.offset)
                .map_err(|_| format!("negative DMA-BUF offset {}", captured.offset))?,
        };

        // SAFETY: `captured` came straight from the PipeWire negotiation, so the fd is a
        // valid single-plane DMA-BUF whose format/modifier/stride/offset match `import`.
        let texture = unsafe { gpu.import_dmabuf(import)? };
        let nv12 = conv.convert(gpu, &texture, texture.width(), texture.height());

        // Force a keyframe on the very first frame so the file is decodable from the
        // start (the encoder's own GOP handles the rest via EncodeParams.idr_period).
        let chunk = enc.encode(&nv12, frame_index == 0, pts)?;
        writer.write_all(&chunk.bytes)?;

        Ok(ChunkInfo {
            bytes: chunk.bytes.len(),
            is_keyframe: chunk.is_keyframe,
        })
    }

    /// Read this process's consumed CPU seconds (utime + stime) from
    /// `/proc/self/stat`. Returns `None` if the field can't be parsed (so the
    /// report degrades gracefully rather than failing the run).
    fn read_self_cpu_seconds() -> Option<f64> {
        // Fields after the (possibly space-containing) comm field, which is wrapped in
        // parentheses: split on the closing ')' to skip past it, then count fields.
        // utime is field 14 and stime field 15 (1-based) in proc(5); after the ')' the
        // first token is field 3 (state), so utime/stime are tokens 12/13 there.
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let after_comm = stat.rsplit_once(')')?.1;
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        // _SC_CLK_TCK is 100 on Linux (USER_HZ): ticks → seconds.
        const CLK_TCK: f64 = 100.0;
        Some((utime + stime) as f64 / CLK_TCK)
    }

    /// Sample GPU + encoder utilisation once via `nvidia-smi` (best-effort; returns
    /// `None` if the tool is absent or fails, so non-NVIDIA hosts just omit it).
    ///
    /// This single sample is taken when the report prints, i.e. just after the loop
    /// ends, so it reflects near-idle state; for live encode utilisation, watch
    /// `nvidia-smi dmon` (or `-l 1`) in another shell during the run.
    fn sample_gpu() -> Option<String> {
        let out = std::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=utilization.gpu,utilization.encoder",
                "--format=csv,noheader",
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if s.is_empty() { None } else { Some(s) }
    }

    #[allow(clippy::too_many_arguments)]
    fn report(
        out_path: &std::path::Path,
        frames: u32,
        keyframes: u32,
        total_bytes: u64,
        wall: Duration,
        cpu: Option<f64>,
        params: EncodeParams,
    ) {
        let wall_secs = wall.as_secs_f64();
        let fps = if wall_secs > 0.0 {
            f64::from(frames) / wall_secs
        } else {
            0.0
        };
        let avg_bytes = if frames > 0 {
            total_bytes as f64 / f64::from(frames)
        } else {
            0.0
        };
        // Achieved bitrate over the wall clock (file bytes are the encoder's output).
        let bitrate_mbps = if wall_secs > 0.0 {
            (total_bytes as f64 * 8.0) / wall_secs / 1.0e6
        } else {
            0.0
        };

        let cpu_line = match cpu {
            Some(cpu_secs) => {
                let cpu_pct = if wall_secs > 0.0 {
                    cpu_secs / wall_secs * 100.0
                } else {
                    0.0
                };
                format!("CPU time:        {cpu_secs:.3} s  ({cpu_pct:.1}% of wall)")
            }
            None => "CPU time:        (unavailable)".to_owned(),
        };
        let gpu_line = match sample_gpu() {
            Some(s) => format!("GPU (nvidia-smi gpu%,enc%): {s}"),
            None => "GPU (nvidia-smi): (unavailable)".to_owned(),
        };

        tracing::info!(
            frames,
            keyframes,
            total_bytes,
            wall_ms = wall.as_millis() as u64,
            "recording finished"
        );

        // A clear, copy-pasteable summary block (the issue's acceptance artifact).
        println!("\n==================== rewynd record_h264 overhead ====================");
        println!("Output file:     {}", out_path.display());
        println!(
            "Configured:      {}x{} @ {} fps, target {} Mbps, GOP {}",
            params.width,
            params.height,
            params.framerate,
            f64::from(params.bitrate_bps) / 1.0e6,
            params.idr_period
        );
        println!("Frames encoded:  {frames}  ({keyframes} keyframes)");
        println!("Wall time:       {wall_secs:.3} s");
        println!("Achieved fps:    {fps:.1}");
        println!("Total bytes:     {total_bytes}  (avg {avg_bytes:.0} B/frame)");
        println!("Achieved rate:   {bitrate_mbps:.2} Mbps");
        println!("{cpu_line}");
        println!("{gpu_line}");
        println!("=====================================================================\n");
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("Linux only");
}
