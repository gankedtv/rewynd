//! rewynd — instant-replay clip recorder.
//!
//! Wires the pipeline together: capture → RGBA/BGRx→NV12 → encode → keyframe-aware
//! ring buffer, and flushes a self-decodable clip on a global hotkey. The ring buffer
//! is filled on a dedicated capture thread; the main thread drives the XDG portals
//! (ScreenCast for capture, GlobalShortcuts for the hotkey) and cuts a clip on press.
//!
//! Linux-only at runtime; the binary compiles elsewhere via a stub `main`.

#[cfg(target_os = "linux")]
mod tray;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::cell::RefCell;
    use std::ops::ControlFlow;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow};
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_util::StreamExt;
    use rewynd_buffer::{AudioRingBuffer, EncodedChunk, RingBuffer};
    use rewynd_capture::linux::{
        AudioParams, AudioSource, CapturedDmabuf, capture_audio, capture_stream, open_portal_with,
    };
    use rewynd_encode::{
        AudioEncodeParams, AudioMixer, EncodeParams, Encoder, GpuVideoEncoder, Nv12Converter,
        OpusAudioEncoder, apply_gain, center_mono_into,
    };

    use rewynd_config::{self as config, AudioSettings, VideoSettings};

    use crate::tray;
    use rewynd_gpu::{DmabufImport, GpuContext};
    use rewynd_mux::{AudioTrack, Mp4Muxer, Muxer};

    /// Application id, registered with the portal so the GlobalShortcuts backend (e.g.
    /// KWin) can attribute and persist our shortcut. Unsandboxed apps must register one.
    const APP_ID: &str = "tv.ganked.rewynd";
    /// Stable id for our one shortcut; the compositor binds a trigger to it.
    const SHORTCUT_ID: &str = "save-clip";

    /// Shared, mutable ring buffer: the capture thread pushes, the hotkey handler cuts.
    type SharedBuffer = Arc<Mutex<RingBuffer>>;
    /// Shared audio ring: the mixer thread pushes Opus packets, the hotkey handler cuts.
    type SharedAudioBuffer = Arc<Mutex<AudioRingBuffer>>;
    /// Shared mixer: the system + mic capture threads sum into it, the mixer thread drains it.
    type SharedMixer = Arc<Mutex<AudioMixer>>;
    /// The most recently saved clip, so the tray's "Upload last clip" knows what to send.
    type SharedLastClip = Arc<Mutex<Option<PathBuf>>>;

    /// How far behind real time the mixer holds audio before encoding it, so the system and
    /// mic streams have both contributed. Latency is irrelevant for a replay buffer; this
    /// just absorbs the two streams' jitter.
    const AUDIO_SETTLE: Duration = Duration::from_millis(120);
    /// How often the mixer thread drains settled audio into the encoder.
    const AUDIO_DRAIN_INTERVAL: Duration = Duration::from_millis(20);

    /// Map the GPU-free [`VideoSettings`] from the config onto the encoder's [`EncodeParams`].
    /// A test guards that the config defaults stay in lockstep with [`EncodeParams::default`].
    fn encode_params(v: VideoSettings) -> EncodeParams {
        EncodeParams {
            width: v.width,
            height: v.height,
            framerate: v.framerate,
            bitrate_bps: v.bitrate_bps,
            idr_period: v.idr_period,
        }
    }

    /// Map [`AudioSettings`] onto [`AudioEncodeParams`] (frame size stays at the encoder default).
    fn audio_encode_params(a: AudioSettings) -> AudioEncodeParams {
        AudioEncodeParams {
            sample_rate: a.sample_rate,
            channels: a.channels,
            bitrate_bps: a.bitrate_bps,
            ..Default::default()
        }
    }

    pub fn run() -> Result<()> {
        tracing_subscriber::fmt::init();

        // Settings come from the config file (written on first run) layered under the built-in
        // defaults and over by `REWYND_*` env overrides (see `rewynd_config`).
        config::ensure_default_file();
        let config = config::load();

        // Single-instance guard: hold the recorder lock (which also publishes our pid for the
        // settings app's restart). Two recorders would mean two ScreenCast sessions and two
        // shortcut bindings fighting each other, so a second launch bows out here. The lock is
        // held in `_instance` for the rest of `run` and released when the process exits.
        let _instance = match config::acquire_recorder_lock() {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => {
                tracing::error!("another rewynd recorder is already running; exiting");
                return Ok(());
            }
            // A lock-file IO error is near-impossible at startup; if it happens we still record
            // (degraded: no guard and no pid for the settings restart) rather than refuse to run.
            Err(e) => {
                tracing::warn!(error = %e, "could not acquire the recorder lock; starting without one");
                None
            }
        };

        // Resolution / framerate / bitrate stay parameters (PLAN §9), sourced from the config.
        let params = encode_params(config.video());
        tracing::info!(
            width = params.width,
            height = params.height,
            fps = params.framerate,
            bitrate_bps = params.bitrate_bps,
            idr_period = params.idr_period,
            "encode parameters"
        );
        let audio_params = audio_encode_params(config.audio());
        let buffer_window = config.buffer_window();
        let output_dir = config.output_dir();
        tracing::info!(
            sample_rate = audio_params.sample_rate,
            channels = audio_params.channels,
            bitrate_bps = audio_params.bitrate_bps,
            mic_gain = config.mic_gain(),
            system_gain = config.system_gain(),
            buffer_s = buffer_window.as_secs(),
            "audio + buffer parameters"
        );
        let buffer: SharedBuffer = Arc::new(Mutex::new(RingBuffer::new(buffer_window)));
        let audio_buffer: SharedAudioBuffer =
            Arc::new(Mutex::new(AudioRingBuffer::new(buffer_window)));
        let last_clip: SharedLastClip = Arc::new(Mutex::new(None));
        // The system + mic capture threads sum into this; the mixer thread drains + encodes it.
        let mixer: SharedMixer = Arc::new(Mutex::new(AudioMixer::new(
            audio_params.sample_rate,
            audio_params.channels,
            AUDIO_SETTLE,
        )));

        // One monotonic epoch shared by all capture threads, so the video, system-audio and
        // mic PTS are on the same clock and the mixer/muxer can align them.
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
        // token. `always_prompt` re-shows the picker so a different monitor can be chosen.
        // The fd moves to the capture thread; the session stays alive here.
        let mut portal = runtime.block_on(open_portal_with(config.always_prompt()))?;
        let node_id = portal.node_id;
        let fd = portal.take_fd();
        tracing::info!(node_id, "screencast portal established");

        let stop = Arc::new(AtomicBool::new(false));

        // Audio runs on three threads: the system (sink-monitor) and microphone captures each
        // sum their PCM into the shared mixer; the mixer thread drains the aligned mix, Opus-
        // encodes it, and fills the audio ring. No portal — PipeWire connects directly. The
        // Opus encoder is built inside the mixer thread so it never crosses a thread boundary.
        // These (and the mixer) are spawned BEFORE the GPU video thread so that a spawn
        // failure here can't leave the video thread running and racing process exit.
        let system_audio = spawn_audio_capture(
            "rewynd-audio-system",
            AudioSource::SinkMonitor,
            audio_params,
            config.system_gain(),
            mixer.clone(),
            &stop,
            epoch,
        )?;
        // The mic is optional: with no input device the mixer simply never receives mic
        // samples (the stream idles until shutdown), so clips are system-only.
        let mic_audio = spawn_audio_capture(
            "rewynd-audio-mic",
            AudioSource::Microphone,
            audio_params,
            config.mic_gain(),
            mixer.clone(),
            &stop,
            epoch,
        )?;

        // Set once the capture threads have stopped delivering (after their join), so the
        // mixer's final drain catches every sample they added.
        let captures_done = Arc::new(AtomicBool::new(false));
        let mixer_buffer = audio_buffer.clone();
        let mixer_mixer = mixer.clone();
        let mixer_done = captures_done.clone();
        let audio_mixer = std::thread::Builder::new()
            .name("rewynd-audio-mixer".to_owned())
            .spawn(move || {
                if let Err(e) =
                    run_audio_mixer(epoch, audio_params, mixer_mixer, mixer_buffer, &mixer_done)
                {
                    tracing::error!(error = %e, "audio mixer loop stopped");
                }
            })
            .context("spawning the audio mixer thread")?;

        // Fill the video ring on its own thread: the PipeWire loop blocks, and the GPU
        // pipeline lives there start to finish (so it also tears down there, in order).
        // Spawned LAST — it's the only thread whose teardown must not race process exit, so
        // nothing fallible runs after it; `stop` ends its loop and we join it before return.
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

        // Dev aid: flush once after N seconds without a keypress, so the pipeline can be
        // exercised headlessly. The hotkey is the real trigger.
        if let Ok(value) = std::env::var("REWYND_FLUSH_AFTER") {
            match value.parse::<u64>() {
                Ok(secs) => {
                    let buffer = buffer.clone();
                    let audio_buffer = audio_buffer.clone();
                    let output_dir = output_dir.clone();
                    let flush_last = last_clip.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(secs));
                        if let Some(path) = save_clip(
                            &buffer,
                            &audio_buffer,
                            params,
                            audio_params,
                            buffer_window,
                            output_dir.as_deref(),
                        ) {
                            remember_clip(&flush_last, &path);
                        }
                    });
                }
                Err(e) => tracing::warn!(value, error = %e, "ignoring invalid REWYND_FLUSH_AFTER"),
            }
        }

        // Tray icon + menu, on a background task of the same runtime (no GTK, no extra event
        // loop). Menu clicks arrive as `TrayCmd`s; the hotkey loop below is left untouched.
        {
            let tray_buffer = buffer.clone();
            let tray_audio = audio_buffer.clone();
            let tray_output = output_dir.clone();
            let tray_last = last_clip.clone();
            let upload_busy = Arc::new(AtomicBool::new(false));
            runtime.spawn(async move {
                let (handle, mut rx) = match tray::spawn().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "tray unavailable; continuing without it");
                        return;
                    }
                };
                let _handle = handle; // dropping it would remove the icon
                while let Some(cmd) = rx.recv().await {
                    match cmd {
                        tray::TrayCmd::SaveClip => {
                            // The cut + mux is blocking; run it off the runtime worker, then toast.
                            let (b, a, o) =
                                (tray_buffer.clone(), tray_audio.clone(), tray_output.clone());
                            let saved = tokio::task::spawn_blocking(move || {
                                save_clip(&b, &a, params, audio_params, buffer_window, o.as_deref())
                            })
                            .await
                            .ok()
                            .flatten();
                            if let Some(path) = saved {
                                remember_clip(&tray_last, &path);
                                tray::clip_saved_toast(&path).await;
                            }
                        }
                        tray::TrayCmd::UploadClip => {
                            let clip = tray_last
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone();
                            // Fresh config each click: enabling uploads or changing the key in
                            // the settings works without restarting the recorder.
                            match (build_uploader(config::load().upload()), clip) {
                                (Some(up), Some(path)) => {
                                    if upload_busy.swap(true, Ordering::SeqCst) {
                                        tray::toast(
                                            "Upload already running",
                                            "Wait for the current upload to finish.",
                                        )
                                        .await;
                                    } else {
                                        // Its own task so a slow upload never stalls the tray menu.
                                        tokio::spawn(upload_and_toast(
                                            up,
                                            path,
                                            upload_busy.clone(),
                                        ));
                                    }
                                }
                                (None, _) => {
                                    tray::toast(
                                        "Upload not configured",
                                        "Enable uploads and add your ganked.tv API key in the settings.",
                                    )
                                    .await;
                                }
                                (Some(_), None) => {
                                    tray::toast("No clip yet", "Save a clip first, then upload it.")
                                        .await;
                                }
                            }
                        }
                        tray::TrayCmd::OpenSettings => open_settings(),
                        tray::TrayCmd::Quit => {
                            tracing::info!("quit requested from the tray");
                            if upload_busy.load(Ordering::SeqCst) {
                                tray::toast(
                                    "Upload cancelled",
                                    "rewynd quit while an upload was still running.",
                                )
                                .await;
                            }
                            std::process::exit(0);
                        }
                    }
                }
            });
        }

        // Block on the hotkey loop until the shortcut session ends (or the process is
        // killed). The capture threads keep filling the buffers in the background.
        let result = runtime.block_on(run_hotkey_loop(
            &buffer,
            &audio_buffer,
            params,
            audio_params,
            buffer_window,
            output_dir.as_deref(),
            config.hotkey_trigger(),
            &last_clip,
        ));

        // Shut the capture loop down, then join it so the GPU pipeline tears down on its
        // own thread rather than during process exit. The loop only observes `stop` when a
        // frame arrives, so explicitly close the portal session first: that removes the
        // PipeWire node, the stream errors out, and the loop quits even on an idle screen.
        stop.store(true, Ordering::Relaxed);
        let _ = runtime.block_on(portal.close());
        let _ = capture.join();
        // The audio capture loops poll `stop` via their watchdog timers (so they quit even if
        // an endpoint is suspended). Join them first so they've stopped adding to the mixer,
        // *then* signal `captures_done` so the mixer's final drain catches their last samples
        // before it flushes and exits.
        let _ = system_audio.join();
        let _ = mic_audio.join();
        captures_done.store(true, Ordering::Relaxed);
        let _ = audio_mixer.join();
        // The pid file isn't removed on exit: the kernel releases the `flock` when the process
        // dies, and unlinking it would race a relock by an incoming instance. A leftover pid is
        // harmless — the settings app verifies it against `/proc` before signalling it.
        result
    }

    /// Launch the sibling settings binary (best-effort), for the tray's "Open settings".
    fn open_settings() {
        match std::env::current_exe() {
            Ok(exe) => {
                let settings = exe.with_file_name("rewynd-settings");
                if let Err(e) = std::process::Command::new(&settings).spawn() {
                    tracing::warn!(error = %e, path = %settings.display(), "could not open settings");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not locate the settings binary"),
        }
    }

    /// ganked.tv upload wiring, built from the config at the moment of use.
    struct Uploader {
        client: rewynd_upload::GankedClient,
        visibility: rewynd_upload::Visibility,
        share_base: String,
    }

    fn build_uploader(settings: rewynd_config::UploadSettings) -> Option<Uploader> {
        if !settings.enabled {
            return None;
        }
        let vis = settings.visibility.trim();
        if !vis.eq_ignore_ascii_case("public") && !vis.eq_ignore_ascii_case("unlisted") {
            // parse() fails closed to unlisted; still tell the user their config has a typo.
            tracing::warn!(
                visibility = vis,
                "unknown upload visibility; using unlisted"
            );
        }
        match rewynd_upload::GankedClient::new(&settings.api_url, &settings.api_key) {
            Ok(client) => Some(Uploader {
                client,
                visibility: rewynd_upload::Visibility::parse(vis),
                share_base: settings.share_url,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "uploads unavailable: could not build the ganked.tv client");
                None
            }
        }
    }

    /// Clears the upload-busy flag on drop, so a panicking upload task can't wedge the tray's
    /// "Upload last clip" in the busy state.
    struct BusyGuard(Arc<AtomicBool>);

    impl Drop for BusyGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    /// Upload `path` and toast the outcome, releasing `busy` when done. Runs as its own task so
    /// nothing else waits on it.
    async fn upload_and_toast(up: Uploader, path: PathBuf, busy: Arc<AtomicBool>) {
        let _busy = BusyGuard(busy);
        let title = format!("rewynd {}", jiff::Zoned::now().strftime("%Y-%m-%d %H:%M"));
        tray::toast("Uploading clip", "Sending to ganked.tv...").await;
        match up.client.upload(&path, &title, up.visibility).await {
            Ok(clip) if clip.failed() => {
                tracing::error!(clip_id = %clip.id, "server rejected the clip after upload");
                tray::toast(
                    "Upload failed",
                    "ganked.tv could not process the clip (check its length and format).",
                )
                .await;
            }
            Ok(clip) => {
                let body = clip
                    .share_url(&up.share_base)
                    .unwrap_or_else(|| "Processing on ganked.tv".to_owned());
                tracing::info!(clip_id = %clip.id, "clip uploaded");
                tray::toast("Clip uploaded", &body).await;
            }
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "upload failed");
                tray::toast("Upload failed", &e.to_string()).await;
            }
        }
    }

    fn remember_clip(last: &SharedLastClip, path: &Path) {
        *last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path.to_path_buf());
    }

    /// Spawn a thread that captures `source`, applies `gain`, and sums each buffer into the
    /// shared `mixer`, aligned by its capture-relative PTS. A capture error is logged at a
    /// severity matching the source (a missing mic is benign; a failed system sink loses the
    /// primary audio) and the thread exits — the mixer simply never sees that source.
    fn spawn_audio_capture(
        name: &str,
        source: AudioSource,
        audio_params: AudioEncodeParams,
        gain: f32,
        mixer: SharedMixer,
        stop: &Arc<AtomicBool>,
        epoch: Instant,
    ) -> Result<std::thread::JoinHandle<()>> {
        let stop = stop.clone();
        let capture_params = AudioParams {
            sample_rate: audio_params.sample_rate,
            channels: audio_params.channels,
        };
        let channels = capture_params.channels as usize;
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                // Per-source prep, reused across buffers so the hot path doesn't realloc: the
                // mic is centred to mono (see `center_mono_into`) so a single-sided mic isn't
                // stuck in one ear, system audio keeps its stereo image, and the configured
                // gain is applied to each.
                let mut prep = Vec::new();
                // No idle timeout (capture runs until shutdown); the stop flag drives the
                // watchdog so the loop quits promptly even if the endpoint suspends.
                let result = capture_audio(
                    capture_params,
                    source,
                    None,
                    Some(stop.clone()),
                    epoch,
                    move |pcm, pts| {
                        let prepared = match source {
                            AudioSource::Microphone => {
                                center_mono_into(pcm, channels, &mut prep);
                                apply_gain(&mut prep, gain);
                                prep.as_slice()
                            }
                            // Only copy to scale when the gain isn't (near) unity; the common
                            // gain == 1.0 case passes the buffer through untouched. The predicate
                            // matches `apply_gain`'s own no-op threshold.
                            AudioSource::SinkMonitor if (gain - 1.0).abs() >= f32::EPSILON => {
                                prep.clear();
                                prep.extend_from_slice(pcm);
                                apply_gain(&mut prep, gain);
                                prep.as_slice()
                            }
                            AudioSource::SinkMonitor => pcm,
                        };
                        mixer
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .add(prepared, pts);
                        ControlFlow::Continue(())
                    },
                );
                if let Err(e) = result {
                    // A missing mic is expected; a failed system sink means the clip loses its
                    // primary audio, so surface that louder.
                    match source {
                        AudioSource::Microphone => {
                            tracing::info!(error = %e, "no microphone capture; clips use system audio only");
                        }
                        AudioSource::SinkMonitor => {
                            tracing::error!(error = %e, "system-audio capture failed; clips will have no system sound");
                        }
                    }
                }
            })
            .with_context(|| format!("spawning the {name} thread"))
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

    /// Drain settled mixed audio from `mixer`, Opus-encode it, and push packets into the
    /// audio ring. Runs until `captures_done` is set — which the shutdown path raises only
    /// *after* the capture threads are joined, so the final `drain_all` catches every sample
    /// they added (a tail the steady-state settle window would still be holding). The encoder
    /// is built here so it stays on this thread; `epoch` matches the mixer's alignment clock.
    fn run_audio_mixer(
        epoch: Instant,
        audio_params: AudioEncodeParams,
        mixer: SharedMixer,
        buffer: SharedAudioBuffer,
        captures_done: &Arc<AtomicBool>,
    ) -> Result<()> {
        let mut encoder = OpusAudioEncoder::new(audio_params)?;
        tracing::info!("audio pipeline ready; mixing system + mic into the audio ring");

        let push_packet = |buffer: &SharedAudioBuffer, chunk| {
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(chunk);
        };

        loop {
            std::thread::sleep(AUDIO_DRAIN_INTERVAL);
            let finalize = captures_done.load(Ordering::Relaxed);

            // Drain under the mixer lock, encode outside it. Once finalizing, take the whole
            // tail (ignoring the settle delay) since no more samples will arrive.
            let drained = {
                let mut guard = mixer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if finalize {
                    guard.drain_all()
                } else {
                    guard.drain_settled(epoch.elapsed())
                }
            };
            if let Some((pts, pcm)) = drained {
                if let Err(e) = encoder.push(&pcm, pts, |chunk| push_packet(&buffer, chunk)) {
                    // Drop this chunk but keep mixing: a transient encode error shouldn't kill
                    // audio for the rest of the session (and would skip the shutdown flush).
                    tracing::error!(error = %e, "audio encode failed; dropping this chunk");
                }
            }

            if finalize {
                // Flush the encoder's final sub-frame so the tail isn't dropped.
                if let Err(e) = encoder.flush(|chunk| push_packet(&buffer, chunk)) {
                    tracing::error!(error = %e, "audio flush failed");
                }
                break;
            }
        }
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
    #[allow(clippy::too_many_arguments)]
    async fn run_hotkey_loop(
        buffer: &SharedBuffer,
        audio_buffer: &SharedAudioBuffer,
        params: EncodeParams,
        audio_params: AudioEncodeParams,
        buffer_window: Duration,
        output_dir: Option<&Path>,
        hotkey_trigger: &str,
        last_clip: &SharedLastClip,
    ) -> Result<()> {
        let shortcuts = GlobalShortcuts::new().await?;
        let session = shortcuts.create_session(Default::default()).await?;
        // Subscribe before binding so no early activation is missed.
        let mut activated = shortcuts.receive_activated().await?;
        let save_description = format!("Save the last {} seconds", buffer_window.as_secs());
        let bound = shortcuts
            .bind_shortcuts(
                &session,
                &[NewShortcut::new(SHORTCUT_ID, &save_description)
                    .preferred_trigger(hotkey_trigger)],
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
                shortcut = %save_description,
                "no trigger bound yet — opening the shortcut configuration dialog; assign a key to this shortcut"
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
                if let Some(path) = save_clip(
                    buffer,
                    audio_buffer,
                    params,
                    audio_params,
                    buffer_window,
                    output_dir,
                ) {
                    remember_clip(last_clip, &path);
                    tray::clip_saved_toast(&path).await;
                }
            }
        }

        session.close().await.ok();
        Ok(())
    }

    /// Cut the most recent `buffer_window` from both rings and write it to an MP4 under
    /// `output_dir` (or the temp dir when `None`). Returns the written path on success so the
    /// caller can show a notification (the toast is async, so it can't fire from here).
    fn save_clip(
        buffer: &SharedBuffer,
        audio_buffer: &SharedAudioBuffer,
        params: EncodeParams,
        audio_params: AudioEncodeParams,
        buffer_window: Duration,
        output_dir: Option<&Path>,
    ) -> Option<PathBuf> {
        // Hold the lock only for the cut (which clones the clip's chunks), then release it
        // so the capture thread keeps filling the buffer while we write the file.
        let clip = buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush_last(buffer_window);
        let chunks = match clip {
            Ok(chunks) => chunks,
            Err(e) => {
                tracing::warn!(error = %e, "nothing to save yet");
                return None;
            }
        };

        // The clip starts at its first (keyframe) chunk; take the audio from that instant on
        // — both PTS share the capture epoch, so this keeps the tracks aligned.
        let clip_base = chunks.first().map_or(Duration::ZERO, |c| c.pts);
        let audio_chunks = audio_buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush_from(clip_base);

        let path = clip_output_path(output_dir);
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
                Some(path)
            }
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "failed to write clip");
                None
            }
        }
    }

    /// Where to write a saved clip: `output_dir` if configured, else the user's Videos folder,
    /// else the temp dir — with a millisecond-stamped, per-process-sequenced name. The sequence
    /// number disambiguates two saves landing in the same millisecond (e.g. the dev-hook flush
    /// racing a hotkey press), which a bare timestamp would collide on.
    fn clip_output_path(output_dir: Option<&Path>) -> PathBuf {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = output_dir
            .map(Path::to_path_buf)
            .or_else(config::default_output_dir)
            .unwrap_or_else(std::env::temp_dir);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("rewynd-{stamp}-{seq}.mp4"))
    }

    #[cfg(test)]
    mod tests {
        use super::{audio_encode_params, desktop_exec_value, encode_params};
        use rewynd_config::Config;
        use rewynd_encode::{AudioEncodeParams, EncodeParams};

        #[test]
        fn config_defaults_map_to_encoder_defaults() {
            // rewynd-config is GPU-free and mirrors the encoder defaults as its own constants;
            // this guards that the two never drift (a new EncodeParams default must be reflected
            // in the config crate, or this fails).
            let c = Config::default();
            let v = encode_params(c.video());
            let d = EncodeParams::default();
            assert_eq!(
                (v.width, v.height, v.framerate, v.bitrate_bps, v.idr_period),
                (d.width, d.height, d.framerate, d.bitrate_bps, d.idr_period)
            );
            let a = audio_encode_params(c.audio());
            let ad = AudioEncodeParams::default();
            assert_eq!(
                (a.sample_rate, a.channels, a.bitrate_bps),
                (ad.sample_rate, ad.channels, ad.bitrate_bps)
            );
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
