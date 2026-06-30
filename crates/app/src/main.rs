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
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow};
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_util::StreamExt;
    use rewynd_buffer::{AudioRingBuffer, EncodedChunk, RingBuffer};
    use rewynd_capture::linux::{
        AudioParams, CapturedDmabuf, capture_stream, capture_system_audio, open_portal,
    };
    use rewynd_encode::{
        AudioEncodeParams, EncodeParams, Encoder, GpuVideoEncoder, Nv12Converter, OpusAudioEncoder,
    };
    use rewynd_gpu::{DmabufImport, GpuContext};
    use rewynd_mux::{AudioTrack, Mp4Muxer, Muxer};

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
    /// Shared audio ring: the audio thread pushes Opus packets, the hotkey handler cuts.
    type SharedAudioBuffer = Arc<Mutex<AudioRingBuffer>>;

    /// Audio encode parameters: 48 kHz stereo Opus (matching capture + Opus's native rate),
    /// with the bitrate overridable from the environment (`REWYND_AUDIO_BITRATE_BPS`).
    fn audio_params_from_env() -> AudioEncodeParams {
        audio_params_from(|key| std::env::var(key).ok())
    }

    /// Build [`AudioEncodeParams`] from a key→value lookup: the default with a positive
    /// `REWYND_AUDIO_BITRATE_BPS` override applied. Split from the environment so it's testable.
    fn audio_params_from(get: impl Fn(&str) -> Option<String>) -> AudioEncodeParams {
        let mut params = AudioEncodeParams::default();
        if let Some(v) = get("REWYND_AUDIO_BITRATE_BPS")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&v| v > 0)
        {
            params.bitrate_bps = v;
        }
        params
    }

    /// Encode parameters: the 1080p60 default, with each field overridable from the
    /// environment (`REWYND_WIDTH` / `_HEIGHT` / `_FPS` / `_BITRATE_BPS` / `_IDR_PERIOD`).
    fn encode_params_from_env() -> EncodeParams {
        params_from(|key| std::env::var(key).ok())
    }

    /// Build [`EncodeParams`] from a key→value lookup: the 1080p60 default with any
    /// positive `u32` overrides applied. Split out from the environment so it's testable.
    fn params_from(get: impl Fn(&str) -> Option<String>) -> EncodeParams {
        // Ignore unparseable or non-positive overrides: every field must be > 0 (the
        // encoder rejects zero), so fall back to the default rather than fail at startup.
        let u32_of = |key: &str| {
            get(key)
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|&v| v > 0)
        };
        let mut params = EncodeParams::default();
        if let Some(v) = u32_of("REWYND_WIDTH") {
            params.width = v;
        }
        if let Some(v) = u32_of("REWYND_HEIGHT") {
            params.height = v;
        }
        if let Some(v) = u32_of("REWYND_FPS") {
            params.framerate = v;
        }
        if let Some(v) = u32_of("REWYND_BITRATE_BPS") {
            params.bitrate_bps = v;
        }
        if let Some(v) = u32_of("REWYND_IDR_PERIOD") {
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
        let audio_params = audio_params_from_env();
        tracing::info!(
            sample_rate = audio_params.sample_rate,
            channels = audio_params.channels,
            bitrate_bps = audio_params.bitrate_bps,
            "audio encode parameters"
        );
        let buffer: SharedBuffer = Arc::new(Mutex::new(RingBuffer::new(BUFFER_WINDOW)));
        let audio_buffer: SharedAudioBuffer =
            Arc::new(Mutex::new(AudioRingBuffer::new(BUFFER_WINDOW)));

        // One monotonic epoch shared by both capture threads, so the video and audio PTS
        // are on the same clock and the muxer can align the tracks.
        let epoch = Instant::now();

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
                if let Err(e) =
                    run_capture(node_id, fd, params, epoch, capture_buffer, &capture_stop)
                {
                    tracing::error!(error = %e, "capture loop stopped");
                }
            })
            .context("spawning the capture thread")?;

        // System audio on its own thread: capture the sink monitor, Opus-encode, push
        // packets into the audio ring. No portal — PipeWire connects directly. The Opus
        // encoder is built inside the thread so it never crosses a thread boundary.
        let audio_capture_buffer = audio_buffer.clone();
        let audio_stop = stop.clone();
        let audio_capture = std::thread::Builder::new()
            .name("rewynd-audio".to_owned())
            .spawn(move || {
                if let Err(e) =
                    run_audio_capture(epoch, audio_params, audio_capture_buffer, &audio_stop)
                {
                    tracing::error!(error = %e, "audio capture loop stopped");
                }
            })
            .context("spawning the audio capture thread")?;

        // Dev aid: flush once after N seconds without a keypress, so the pipeline can be
        // exercised headlessly. The hotkey is the real trigger.
        if let Ok(value) = std::env::var("REWYND_FLUSH_AFTER") {
            match value.parse::<u64>() {
                Ok(secs) => {
                    let buffer = buffer.clone();
                    let audio_buffer = audio_buffer.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(secs));
                        save_clip(&buffer, &audio_buffer, params, audio_params);
                    });
                }
                Err(e) => tracing::warn!(value, error = %e, "ignoring invalid REWYND_FLUSH_AFTER"),
            }
        }

        // Block on the hotkey loop until the shortcut session ends (or the process is
        // killed). The capture threads keep filling the buffers in the background.
        let result = runtime.block_on(run_hotkey_loop(
            &buffer,
            &audio_buffer,
            params,
            audio_params,
        ));

        // Shut the capture loop down, then join it so the GPU pipeline tears down on its
        // own thread rather than during process exit. The loop only observes `stop` when a
        // frame arrives, so explicitly close the portal session first: that removes the
        // PipeWire node, the stream errors out, and the loop quits even on an idle screen.
        stop.store(true, Ordering::Relaxed);
        let _ = runtime.block_on(portal.close());
        let _ = capture.join();
        // The audio loop observes `stop` on its next buffer (the sink monitor delivers
        // continuously, including silence), so it quits promptly; join to drop libopus and
        // the PipeWire stream cleanly rather than racing process exit.
        let _ = audio_capture.join();
        result
    }

    /// Build the GPU pipeline and pump captured frames into `buffer` until the stream
    /// ends. The encoder/converter/device are dropped in dependency order afterwards
    /// (tearing the device down before the encoder it backs crashes the driver).
    fn run_capture(
        node_id: u32,
        fd: std::os::fd::OwnedFd,
        params: EncodeParams,
        epoch: Instant,
        buffer: SharedBuffer,
        stop: &Arc<AtomicBool>,
    ) -> Result<()> {
        let gpu = Rc::new(pollster::block_on(GpuContext::new())?);
        let conv = Rc::new(Nv12Converter::new(&gpu)?);
        let enc = Rc::new(RefCell::new(GpuVideoEncoder::new(&gpu, params)?));
        tracing::info!("capture pipeline ready; filling the ring buffer");

        capture_stream(node_id, fd, epoch, {
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
                        // The chunk carries the frame's real capture timestamp, so the
                        // window evicts by wall-clock time regardless of the capture rate.
                        // Recover from a poisoned lock rather than panicking across the
                        // PipeWire C callback boundary (which would be undefined behaviour).
                        buffer
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(chunk);
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

    /// Capture the system-audio sink monitor, Opus-encode it, and push packets into the
    /// audio ring until `stop` is set (observed on the next buffer). The encoder is built
    /// here so it stays on this thread; `epoch` matches the video capture's clock.
    fn run_audio_capture(
        epoch: Instant,
        audio_params: AudioEncodeParams,
        buffer: SharedAudioBuffer,
        stop: &Arc<AtomicBool>,
    ) -> Result<()> {
        let mut encoder = OpusAudioEncoder::new(audio_params)?;
        let capture_params = AudioParams {
            sample_rate: audio_params.sample_rate,
            channels: audio_params.channels,
        };
        tracing::info!("audio pipeline ready; filling the audio ring");

        // No idle timeout (capture runs until shutdown), but hand the stop flag to the
        // watchdog so the loop quits promptly even if the sink suspends and stops delivering
        // buffers — the per-buffer check alone wouldn't fire then.
        capture_system_audio(
            capture_params,
            None,
            Some(stop.clone()),
            epoch,
            move |pcm, pts| {
                let result = encoder.push(pcm, pts, |chunk| {
                    // Recover from a poisoned lock instead of panicking: a panic here would
                    // unwind across the PipeWire C callback boundary (undefined behaviour).
                    buffer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(chunk);
                });
                if let Err(e) = result {
                    tracing::error!(error = %e, "audio encode failed; stopping audio capture");
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            },
        )?;
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
        let pts = captured.pts;
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
        Ok(enc.encode(&nv12, force_keyframe, pts)?)
    }

    /// Render a path as a single quoted `Exec` value, applying both unescaping layers
    /// the Desktop Entry spec runs on read: wrap in double quotes and backslash-escape
    /// the reserved characters (`"` `` ` `` `$` `\`), then escape every backslash again
    /// for the string-value layer. So a literal `\` ends up as four backslashes, and a
    /// path with spaces is simply quoted.
    fn desktop_exec_value(path: &str) -> String {
        let mut quoted = String::with_capacity(path.len() + 2);
        quoted.push('"');
        for ch in path.chars() {
            if matches!(ch, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(ch);
        }
        quoted.push('"');
        quoted.replace('\\', "\\\\")
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
        let exec_value = desktop_exec_value(&exec.to_string_lossy());
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=rewynd\n\
             Comment=Instant-replay clip recorder\n\
             Exec={exec_value}\n\
             Terminal=false\n\
             Categories=AudioVideo;Recorder;\n",
        );
        std::fs::create_dir_all(path.parent().expect("path has a parent"))?;
        std::fs::write(&path, entry)?;
        tracing::info!(path = %path.display(), "wrote desktop entry for the global shortcut");
        Ok(())
    }

    /// Register the global shortcut and flush a clip whenever it fires.
    async fn run_hotkey_loop(
        buffer: &SharedBuffer,
        audio_buffer: &SharedAudioBuffer,
        params: EncodeParams,
        audio_params: AudioEncodeParams,
    ) -> Result<()> {
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
                save_clip(buffer, audio_buffer, params, audio_params);
            }
        }

        session.close().await.ok();
        Ok(())
    }

    /// Cut the most recent clip from both rings and write it to an MP4 on disk.
    fn save_clip(
        buffer: &SharedBuffer,
        audio_buffer: &SharedAudioBuffer,
        params: EncodeParams,
        audio_params: AudioEncodeParams,
    ) {
        // Hold the lock only for the cut (which clones the clip's chunks), then release it
        // so the capture thread keeps filling the buffer while we write the file.
        let clip = buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush_last(BUFFER_WINDOW);
        let chunks = match clip {
            Ok(chunks) => chunks,
            Err(e) => {
                tracing::warn!(error = %e, "nothing to save yet");
                return;
            }
        };

        // The clip starts at its first (keyframe) chunk; take the audio from that instant on
        // — both PTS share the capture epoch, so this keeps the tracks aligned.
        let clip_base = chunks.first().map_or(Duration::ZERO, |c| c.pts);
        let audio_chunks = audio_buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush_from(clip_base);

        let path = clip_output_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let mut muxer = Mp4Muxer::new(params.width, params.height, params.framerate);
        let result = if audio_chunks.is_empty() {
            muxer.write_mp4(&chunks, &path)
        } else {
            let audio = AudioTrack {
                chunks: &audio_chunks,
                channels: audio_params.channels as u8,
                sample_rate: audio_params.sample_rate,
                // Mid-stream cut: the encoder startup priming isn't present at the clip's
                // first packet, so don't trim (ADR 0004).
                pre_skip: 0,
            };
            muxer.write_mp4_with_audio(&chunks, &audio, &path)
        };

        match result {
            Ok(()) => {
                let span = match (chunks.first(), chunks.last()) {
                    (Some(first), Some(last)) => last.pts.saturating_sub(first.pts),
                    _ => Duration::ZERO,
                };
                tracing::info!(
                    path = %path.display(),
                    frames = chunks.len(),
                    audio_packets = audio_chunks.len(),
                    span_s = span.as_secs_f64(),
                    "saved clip"
                );
            }
            Err(e) => tracing::error!(error = %e, path = %path.display(), "failed to write clip"),
        }
    }

    /// Where to write a saved clip: `$REWYND_OUTPUT_DIR` (default: the temp dir) with a
    /// millisecond-stamped, per-process-sequenced name. The sequence number disambiguates
    /// two saves landing in the same millisecond (e.g. the dev-hook flush racing a hotkey
    /// press), which a bare timestamp would collide on.
    fn clip_output_path() -> std::path::PathBuf {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::var_os("REWYND_OUTPUT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("rewynd-{stamp}-{seq}.mp4"))
    }

    #[cfg(test)]
    mod tests {
        use super::{audio_params_from, desktop_exec_value, params_from};

        #[test]
        fn audio_params_from_applies_bitrate_and_falls_back() {
            let p =
                audio_params_from(|k| (k == "REWYND_AUDIO_BITRATE_BPS").then(|| "96000".into()));
            assert_eq!(p.bitrate_bps, 96_000); // overridden
            assert_eq!(p.sample_rate, 48_000); // default
            assert_eq!(p.channels, 2); // default

            // Zero and garbage are rejected → default 128 kbps.
            let zero = audio_params_from(|_| Some("0".into()));
            assert_eq!(zero.bitrate_bps, 128_000);
            let garbage = audio_params_from(|_| Some("nope".into()));
            assert_eq!(garbage.bitrate_bps, 128_000);
        }

        #[test]
        fn params_from_applies_overrides_and_falls_back() {
            let env = std::collections::HashMap::from([
                ("REWYND_WIDTH", "1280"),
                ("REWYND_FPS", "30"),
                ("REWYND_HEIGHT", "0"),
                ("REWYND_BITRATE_BPS", "not-a-number"),
            ]);
            let p = params_from(|k| env.get(k).map(|s| (*s).to_owned()));
            assert_eq!(p.width, 1280); // overridden
            assert_eq!(p.framerate, 30); // overridden
            assert_eq!(p.height, 1080); // zero rejected → default
            assert_eq!(p.bitrate_bps, 12_000_000); // unparseable → default
            assert_eq!(p.idr_period, 60); // absent → default
        }

        #[test]
        fn exec_value_quotes_plain_and_spaced_paths() {
            assert_eq!(
                desktop_exec_value("/usr/bin/rewynd"),
                r#""/usr/bin/rewynd""#
            );
            assert_eq!(
                desktop_exec_value("/home/a b/rewynd"),
                r#""/home/a b/rewynd""#
            );
        }

        #[test]
        fn exec_value_double_escapes_reserved_characters() {
            // `$` -> `\$` (quote layer) -> `\\$` (string layer).
            assert_eq!(desktop_exec_value("/x/$y/rewynd"), r#""/x/\\$y/rewynd""#);
            // A literal backslash becomes four.
            assert_eq!(desktop_exec_value("/x\\y"), r#""/x\\\\y""#);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("rewynd currently runs on Linux only");
}
