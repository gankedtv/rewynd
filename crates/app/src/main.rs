//! rewynd — instant-replay clip recorder.
//!
//! Wires the pipeline together: capture → RGBA/BGRx→NV12 → encode → keyframe-aware
//! ring buffer, and flushes a self-decodable clip on a global hotkey. The ring buffer
//! is filled on a dedicated capture thread; the main thread drives the XDG portals
//! (ScreenCast for capture, GlobalShortcuts for the hotkey) and cuts a clip on press.
//!
//! Linux-only at runtime; the binary compiles elsewhere via a stub `main`.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::cell::RefCell;
    use std::ops::ControlFlow;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow};
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_util::StreamExt;
    use rewynd_buffer::{EncodedChunk, RingBuffer};
    use rewynd_capture::linux::{CapturedDmabuf, capture_stream, open_portal};
    use rewynd_encode::{EncodeParams, Encoder, GpuVideoEncoder, Nv12Converter};
    use rewynd_gpu::{DmabufImport, GpuContext};

    /// Buffer retention window for the MVP (PLAN §2). Configurable later.
    const BUFFER_WINDOW: Duration = Duration::from_secs(60);
    /// Application id, registered with the portal so the GlobalShortcuts backend (e.g.
    /// KWin) can attribute and persist our shortcut. Unsandboxed apps must register one.
    const APP_ID: &str = "tv.ganked.rewynd";
    /// Stable id for our one shortcut; the compositor binds a trigger to it.
    const SHORTCUT_ID: &str = "save-clip";
    /// Trigger we *prefer*; the user can rebind it in the desktop's shortcut settings.
    const PREFERRED_TRIGGER: &str = "CTRL+ALT+R";

    /// Shared, mutable ring buffer: the capture thread pushes, the hotkey handler cuts.
    type SharedBuffer = Arc<Mutex<RingBuffer>>;

    /// Encode parameters: the 1080p60 default, with each field overridable from the
    /// environment (`REWYND_WIDTH` / `_HEIGHT` / `_FPS` / `_BITRATE_BPS` / `_IDR_PERIOD`).
    fn encode_params_from_env() -> EncodeParams {
        let env_u32 = |key: &str| std::env::var(key).ok().and_then(|v| v.parse::<u32>().ok());
        let mut params = EncodeParams::default();
        if let Some(v) = env_u32("REWYND_WIDTH") {
            params.width = v;
        }
        if let Some(v) = env_u32("REWYND_HEIGHT") {
            params.height = v;
        }
        if let Some(v) = env_u32("REWYND_FPS") {
            params.framerate = v;
        }
        if let Some(v) = env_u32("REWYND_BITRATE_BPS") {
            params.bitrate_bps = v;
        }
        if let Some(v) = env_u32("REWYND_IDR_PERIOD") {
            params.idr_period = v;
        }
        params
    }

    pub fn run() -> Result<()> {
        tracing_subscriber::fmt::init();

        // Resolution / framerate / bitrate are parameters: the 1080p60 target is the
        // default, overridable via the environment until there's a config file/CLI.
        let params = encode_params_from_env();
        tracing::info!(
            width = params.width,
            height = params.height,
            fps = params.framerate,
            bitrate_bps = params.bitrate_bps,
            idr_period = params.idr_period,
            "encode parameters"
        );
        let buffer: SharedBuffer = Arc::new(Mutex::new(RingBuffer::new(BUFFER_WINDOW)));

        // ashpd's portals are async; reuse one runtime for ScreenCast setup and the
        // GlobalShortcuts event loop. (capture runs on its own std thread.)
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        // The portal Registry only accepts an app id that has an installed desktop entry,
        // so make sure one exists before registering.
        if let Err(e) = ensure_desktop_entry() {
            tracing::warn!(error = %e, "could not write a desktop entry; the hotkey may not bind");
        }

        // Register our app id with the portal before any other portal call: ashpd shares
        // one D-Bus connection across portals, and the connection can only be associated
        // with an app id once — so this must happen before ScreenCast claims it, or the
        // GlobalShortcuts session is later rejected with "an app id is required".
        let app_id: ashpd::AppID = APP_ID.parse().context("invalid app id")?;
        runtime.block_on(ashpd::register_host_app(app_id))?;

        // ScreenCast portal: one share-picker dialog the first time, then a saved restore
        // token. The fd moves to the capture thread; the session stays alive here.
        let mut portal = runtime.block_on(open_portal())?;
        let node_id = portal.node_id;
        let fd = portal.take_fd();
        tracing::info!(node_id, "screencast portal established");

        // Fill the ring buffer on its own thread: the PipeWire loop blocks, and the GPU
        // pipeline lives there start to finish (so it also tears down there, in order).
        // `stop` lets us end that loop and join the thread before returning, so the GPU
        // tears down cleanly instead of racing process exit on the error path.
        let stop = Arc::new(AtomicBool::new(false));
        let capture_buffer = buffer.clone();
        let capture_stop = stop.clone();
        let capture = std::thread::Builder::new()
            .name("rewynd-capture".to_owned())
            .spawn(move || {
                if let Err(e) = run_capture(node_id, fd, params, capture_buffer, &capture_stop) {
                    tracing::error!(error = %e, "capture loop stopped");
                }
            })
            .context("spawning the capture thread")?;

        // Dev aid: flush once after N seconds without a keypress, so the pipeline can be
        // exercised headlessly. The hotkey is the real trigger.
        if let Ok(value) = std::env::var("REWYND_FLUSH_AFTER") {
            match value.parse::<u64>() {
                Ok(secs) => {
                    let buffer = buffer.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(secs));
                        save_clip(&buffer);
                    });
                }
                Err(e) => tracing::warn!(value, error = %e, "ignoring invalid REWYND_FLUSH_AFTER"),
            }
        }

        // Block on the hotkey loop until the shortcut session ends (or the process is
        // killed). The capture thread keeps filling the buffer in the background.
        let result = runtime.block_on(run_hotkey_loop(&buffer));

        // Shut the capture loop down, then join it so the GPU pipeline tears down on its
        // own thread rather than during process exit. The loop only observes `stop` when a
        // frame arrives, so drop the portal first: closing the ScreenCast session ends the
        // PipeWire stream, which unblocks the loop even if no further frame comes.
        stop.store(true, Ordering::Relaxed);
        drop(portal);
        let _ = capture.join();
        result
    }

    /// Build the GPU pipeline and pump captured frames into `buffer` until the stream
    /// ends. The encoder/converter/device are dropped in dependency order afterwards
    /// (tearing the device down before the encoder it backs crashes the driver).
    fn run_capture(
        node_id: u32,
        fd: std::os::fd::OwnedFd,
        params: EncodeParams,
        buffer: SharedBuffer,
        stop: &Arc<AtomicBool>,
    ) -> Result<()> {
        let gpu = Rc::new(pollster::block_on(GpuContext::new())?);
        let conv = Rc::new(Nv12Converter::new(&gpu)?);
        let enc = Rc::new(RefCell::new(GpuVideoEncoder::new(&gpu, params)?));
        tracing::info!("capture pipeline ready; filling the ring buffer");

        capture_stream(node_id, fd, {
            let gpu = gpu.clone();
            let conv = conv.clone();
            let enc = enc.clone();
            let stop = stop.clone();
            let mut frame_index: u64 = 0;
            move |captured: CapturedDmabuf| -> ControlFlow<()> {
                if stop.load(Ordering::Relaxed) {
                    return ControlFlow::Break(());
                }
                // Force an IDR on the very first frame so the buffer always has an early
                // keyframe to cut on; the encoder's GOP supplies the rest.
                match encode_captured(
                    &gpu,
                    &conv,
                    &mut enc.borrow_mut(),
                    captured,
                    frame_index == 0,
                ) {
                    Ok(chunk) => {
                        // The chunk's PTS is constant-framerate (frame index / target fps).
                        // When the source delivers faster than the target (high-refresh
                        // displays), that PTS outruns wall time, so the window retains less
                        // real footage than configured — fixed by capture-timestamp PTS in
                        // the muxing work.
                        buffer.lock().expect("ring buffer mutex").push(chunk);
                        frame_index += 1;
                        ControlFlow::Continue(())
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "frame failed; stopping capture");
                        ControlFlow::Break(())
                    }
                }
            }
        })?;

        drop(enc);
        drop(conv);
        drop(gpu);
        Ok(())
    }

    /// One frame of the hot path: import the DMA-BUF, convert to NV12, encode.
    fn encode_captured(
        gpu: &GpuContext,
        conv: &Nv12Converter,
        enc: &mut GpuVideoEncoder,
        captured: CapturedDmabuf,
        force_keyframe: bool,
    ) -> Result<EncodedChunk> {
        let format = captured.texture_format().ok_or_else(|| {
            anyhow!(
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
                .with_context(|| format!("negative DMA-BUF stride {}", captured.stride))?,
            offset: u32::try_from(captured.offset)
                .with_context(|| format!("negative DMA-BUF offset {}", captured.offset))?,
        };

        // SAFETY: `captured` came straight from the PipeWire negotiation, so the fd is a
        // valid single-plane DMA-BUF whose format/modifier/stride/offset match `import`.
        let texture = unsafe { gpu.import_dmabuf(import)? };
        let nv12 = conv.convert(gpu, &texture);
        Ok(enc.encode(&nv12, force_keyframe)?)
    }

    /// Write a minimal desktop entry for [`APP_ID`] under `$XDG_DATA_HOME/applications`
    /// if one isn't already present. The GlobalShortcuts portal rejects an app id with
    /// no installed desktop entry ("app info not found"); a packaged install ships this
    /// file, so this only matters when running the unpackaged binary.
    fn ensure_desktop_entry() -> Result<()> {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".local/share"))
            })
            .ok_or_else(|| anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;

        let path = data_home
            .join("applications")
            .join(format!("{APP_ID}.desktop"));
        if path.exists() {
            return Ok(());
        }

        let exec = std::env::current_exe()?;
        // Quote + escape the path per the Desktop Entry spec so a path with spaces or
        // reserved characters (" ` $ \) doesn't corrupt the Exec line.
        let exec_escaped = exec
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$");
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=rewynd\n\
             Comment=Instant-replay clip recorder\n\
             Exec=\"{exec_escaped}\"\n\
             Terminal=false\n\
             Categories=AudioVideo;Recorder;\n",
        );
        std::fs::create_dir_all(path.parent().expect("path has a parent"))?;
        std::fs::write(&path, entry)?;
        tracing::info!(path = %path.display(), "wrote desktop entry for the global shortcut");
        Ok(())
    }

    /// Register the global shortcut and flush a clip whenever it fires.
    async fn run_hotkey_loop(buffer: &SharedBuffer) -> Result<()> {
        let shortcuts = GlobalShortcuts::new().await?;
        let session = shortcuts.create_session(Default::default()).await?;
        // Subscribe before binding so no early activation is missed.
        let mut activated = shortcuts.receive_activated().await?;
        let bound = shortcuts
            .bind_shortcuts(
                &session,
                &[NewShortcut::new(SHORTCUT_ID, "Save the last 60 seconds")
                    .preferred_trigger(PREFERRED_TRIGGER)],
                None,
                Default::default(),
            )
            .await?
            .response()?;
        for shortcut in bound.shortcuts() {
            tracing::info!(
                id = shortcut.id(),
                trigger = shortcut.trigger_description(),
                "bound shortcut"
            );
        }

        // A freshly bound shortcut often has no trigger yet (the preferred trigger is only
        // a hint). Open the portal's configuration dialog so the user can assign a key to
        // *this* shortcut — assigning it elsewhere won't deliver the activation signal.
        let needs_trigger = bound
            .shortcuts()
            .iter()
            .all(|s| s.trigger_description().is_empty());
        if needs_trigger && shortcuts.version() >= 2 {
            tracing::info!(
                "no trigger bound yet — opening the shortcut configuration dialog; assign a key to \"Save the last 60 seconds\""
            );
            if let Err(e) = shortcuts
                .configure_shortcuts(&session, None, Default::default())
                .await
            {
                tracing::warn!(error = %e, "could not open the shortcut configuration dialog");
            }
        }

        tracing::info!(
            shortcut = SHORTCUT_ID,
            "global shortcut ready; press the configured key to save a clip"
        );

        while let Some(activation) = activated.next().await {
            tracing::info!(shortcut_id = activation.shortcut_id(), "shortcut activated");
            if activation.shortcut_id() == SHORTCUT_ID {
                save_clip(buffer);
            }
        }

        session.close().await.ok();
        Ok(())
    }

    /// Cut the most recent clip from the buffer and report it. Muxing the chunks to an
    /// MP4 on disk lands in the next issue; for now this proves the hotkey→flush path.
    fn save_clip(buffer: &SharedBuffer) {
        let clip = buffer
            .lock()
            .expect("ring buffer mutex")
            .flush_last(BUFFER_WINDOW);
        match clip {
            Ok(chunks) => {
                let bytes: usize = chunks.iter().map(|c| c.bytes.len()).sum();
                let span = match (chunks.first(), chunks.last()) {
                    (Some(first), Some(last)) => last.pts.saturating_sub(first.pts),
                    _ => Duration::ZERO,
                };
                tracing::info!(
                    frames = chunks.len(),
                    bytes,
                    span_s = span.as_secs_f64(),
                    starts_on_keyframe = chunks.first().is_some_and(|c| c.is_keyframe),
                    "clip flushed (MP4 muxing lands in the next issue)"
                );
            }
            Err(e) => tracing::warn!(error = %e, "nothing to save yet"),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("rewynd currently runs on Linux only");
}
