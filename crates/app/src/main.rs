//! rewynd — instant-replay clip recorder.
//!
//! Wires the pipeline together: capture → RGBA/BGRx→NV12 → encode → keyframe-aware
//! ring buffer, and flushes a self-decodable clip on a global hotkey. The ring buffer
//! is filled on a dedicated capture thread.
//!
//! On Linux the main thread drives the XDG portals (ScreenCast for capture,
//! GlobalShortcuts for the hotkey) plus the tray, and every exit path — hotkey session
//! end, tray Quit, SIGTERM/SIGINT — funnels through one orderly shutdown (stop flag →
//! portal close → thread joins → audio flush). On Windows the capture is WGC, the
//! hotkey is a `RegisterHotKey` message loop, and Ctrl+C drives the same
//! stop-flag-then-join shutdown (video-only for now; audio/tray/upload follow).

#[cfg(target_os = "linux")]
mod tray;

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    let result = linux::run();
    #[cfg(target_os = "windows")]
    let result = windows::run();
    if let Err(e) = &result {
        // The recorder is a windowless background app (often autostarted): without this, a
        // fatal startup error is invisible. Blocking `show` is fine — no runtime is live here.
        let body = format!("{e:#}")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let mut note = notify_rust::Notification::new();
        note.summary("rewynd could not start")
            .body(&body)
            .icon(rewynd_config::APP_ID)
            .appname("rewynd");
        #[cfg(target_os = "windows")]
        note.app_id(rewynd_config::APP_ID);
        let _ = note.show();
    }
    result
}

/// The audio half of the pipeline, shared by the platform recorders: capture threads
/// summing into the mixer, and the mixer thread draining into the Opus encoder + ring.
/// Only `capture_audio` itself is platform-specific (PipeWire vs WASAPI); the callback
/// shape and everything downstream are identical.
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod audio_pipeline {
    use std::ops::ControlFlow;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    #[cfg(target_os = "linux")]
    use rewynd_capture::linux::capture_audio;
    #[cfg(target_os = "windows")]
    use rewynd_capture::windows::capture_audio;
    use rewynd_capture::{AudioParams, AudioSource};
    use rewynd_clip::{SharedAudioBuffer, lock_unpoisoned};
    use rewynd_encode::{
        AudioEncodeParams, AudioMixer, OpusAudioEncoder, apply_gain, center_mono_into,
    };

    /// Shared mixer: the system + mic capture threads sum into it, the mixer thread drains it.
    pub(crate) type SharedMixer = Arc<Mutex<AudioMixer>>;

    /// How far behind real time the mixer holds audio before encoding it, so the system and
    /// mic streams have both contributed. Latency is irrelevant for a replay buffer; this
    /// just absorbs the two streams' jitter.
    pub(crate) const AUDIO_SETTLE: Duration = Duration::from_millis(120);
    /// How often the mixer thread drains settled audio into the encoder.
    const AUDIO_DRAIN_INTERVAL: Duration = Duration::from_millis(20);

    /// Spawn a thread that captures `source` (from `device`, or the platform default
    /// when `None`), applies `gain`, and sums each buffer into the shared `mixer`,
    /// aligned by its capture-relative PTS. A capture error is logged at a severity
    /// matching the source; a failed system capture loses the clips' primary audio, so
    /// that one also fires `on_system_failure` (the platform surfaces it: tray or toast).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_audio_capture(
        name: &str,
        source: AudioSource,
        device: Option<String>,
        audio_params: AudioEncodeParams,
        gain: f32,
        mixer: SharedMixer,
        stop: &Arc<AtomicBool>,
        epoch: Instant,
        on_system_failure: Option<Box<dyn FnOnce(String) + Send>>,
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
                let panicked = std::rc::Rc::new(std::cell::Cell::new(false));
                // No idle timeout (capture runs until shutdown); the stop flag drives the
                // watchdog so the loop quits promptly even if the endpoint suspends.
                let panicked_flag = panicked.clone();
                let mut buffers: u64 = 0;
                let result = capture_audio(
                    capture_params,
                    source,
                    device.as_deref(),
                    None,
                    Some(stop.clone()),
                    epoch,
                    move |pcm, pts| {
                        // Level telemetry for chasing "why is this clip silent" reports —
                        // ~once a second at the usual 10 ms buffer cadence, debug only.
                        buffers += 1;
                        if buffers % 100 == 1 && tracing::enabled!(tracing::Level::DEBUG) {
                            let peak = pcm.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
                            tracing::debug!(
                                ?source,
                                buffers,
                                pts_ms = pts.as_millis() as u64,
                                peak,
                                "audio level"
                            );
                        }
                        // A panic must not unwind across the PipeWire C callback boundary (UB);
                        // treat it as a stream failure instead (harmless-but-uniform on WASAPI,
                        // where the loop is plain Rust).
                        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let prepared = match source {
                                AudioSource::Microphone => {
                                    center_mono_into(pcm, channels, &mut prep);
                                    apply_gain(&mut prep, gain);
                                    prep.as_slice()
                                }
                                // Only copy to scale when the gain isn't (near) unity; the
                                // common gain == 1.0 case passes the buffer through untouched.
                                // The predicate matches `apply_gain`'s own no-op threshold.
                                AudioSource::SinkMonitor
                                    if (gain - 1.0).abs() >= f32::EPSILON =>
                                {
                                    prep.clear();
                                    prep.extend_from_slice(pcm);
                                    apply_gain(&mut prep, gain);
                                    prep.as_slice()
                                }
                                AudioSource::SinkMonitor => pcm,
                            };
                            lock_unpoisoned(&mixer).add(prepared, pts);
                        }));
                        match outcome {
                            Ok(()) => ControlFlow::Continue(()),
                            Err(_) => {
                                tracing::error!("audio callback panicked; stopping this capture");
                                panicked_flag.set(true);
                                ControlFlow::Break(())
                            }
                        }
                    },
                );
                // A panic Break reads as a clean stream end; surface it like a capture error.
                let result = match result {
                    Ok(()) if panicked.get() => Err(rewynd_capture::CaptureError::PipeWire(
                        "the audio callback panicked".to_owned(),
                    )),
                    other => other,
                };
                if let Err(e) = result {
                    // A missing mic is expected; a failed system capture means the clip loses
                    // its primary audio, so surface that louder.
                    match source {
                        AudioSource::Microphone => {
                            tracing::info!(error = %e, "no microphone capture; clips use system audio only");
                        }
                        AudioSource::SinkMonitor => {
                            tracing::error!(error = %e, "system-audio capture failed; clips will have no system sound");
                            if let Some(surface) = on_system_failure {
                                surface(e.to_string());
                            }
                        }
                    }
                }
            })
            .with_context(|| format!("spawning the {name} thread"))
    }

    /// Drain settled mixed audio from `mixer`, Opus-encode it, and push packets into the
    /// audio ring. Runs until `captures_done` is set — which the shutdown path raises only
    /// *after* the capture threads are joined, so the final `drain_all` catches every sample
    /// they added. `drain_now` (raised by a clip save) forces an immediate full drain so the
    /// clip's audio reaches the cut instant. The encoder is built here so it stays on this
    /// thread; `epoch` matches the mixer's alignment clock.
    pub(crate) fn run_audio_mixer(
        epoch: Instant,
        audio_params: AudioEncodeParams,
        mixer: SharedMixer,
        buffer: SharedAudioBuffer,
        captures_done: &Arc<AtomicBool>,
        drain_now: &Arc<AtomicBool>,
    ) -> Result<()> {
        let mut encoder = OpusAudioEncoder::new(audio_params)?;
        tracing::info!("audio pipeline ready; mixing system + mic into the audio ring");

        let push_packet = |buffer: &SharedAudioBuffer, chunk| {
            lock_unpoisoned(buffer).push(chunk);
        };

        loop {
            std::thread::sleep(AUDIO_DRAIN_INTERVAL);
            let finalize = captures_done.load(Ordering::Relaxed);
            let drain_all = finalize || drain_now.load(Ordering::SeqCst);

            // Drain under the mixer lock, encode outside it. A full drain ignores the settle
            // delay: at shutdown no more samples arrive; at a clip cut the last ~140 ms matter
            // more than a mic that hasn't contributed to them yet.
            let drained = {
                let mut guard = lock_unpoisoned(&mixer);
                if drain_all {
                    guard.drain_all()
                } else {
                    guard.drain_settled(epoch.elapsed())
                }
            };
            if let Some((pts, pcm)) = drained
                && let Err(e) = encoder.push(&pcm, pts, |chunk| push_packet(&buffer, chunk))
            {
                // Drop this chunk but keep mixing: a transient encode error shouldn't kill
                // audio for the rest of the session (and would skip the shutdown flush).
                tracing::error!(error = %e, "audio encode failed; dropping this chunk");
            }
            // Cleared only after the drained packets are in the ring: the saver waits on this
            // (also on the finalize pass, so a save racing shutdown is not left waiting).
            if drain_all {
                drain_now.store(false, Ordering::SeqCst);
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
}

/// Config → encoder parameter mapping, shared by the platform recorders.
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod params {
    use rewynd_config::{AudioSettings, VideoSettings};
    use rewynd_encode::{AudioEncodeParams, EncodeParams};

    /// Map the GPU-free [`VideoSettings`] from the config onto the encoder's [`EncodeParams`].
    /// A test guards that the config defaults stay in lockstep with [`EncodeParams::default`].
    pub(crate) fn encode_params(v: VideoSettings) -> EncodeParams {
        EncodeParams {
            width: v.width,
            height: v.height,
            framerate: v.framerate,
            bitrate_bps: v.bitrate_bps,
            idr_period: v.idr_period,
        }
    }

    /// Map [`AudioSettings`] onto [`AudioEncodeParams`] (frame size stays at the encoder default).
    pub(crate) fn audio_encode_params(a: AudioSettings) -> AudioEncodeParams {
        AudioEncodeParams {
            sample_rate: a.sample_rate,
            channels: a.channels,
            bitrate_bps: a.bitrate_bps,
            ..Default::default()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{audio_encode_params, encode_params};
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
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::cell::RefCell;
    use std::ops::ControlFlow;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow};
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_util::StreamExt;
    use rewynd_capture::AudioSource;
    use rewynd_capture::linux::{CapturedDmabuf, StreamPrefs, capture_stream, open_portal_with};
    use rewynd_clip::{ClipSaver, SaveError, SharedAudioBuffer, SharedBuffer, lock_unpoisoned};
    use rewynd_encode::{AudioMixer, EncodeParams, Encoder, GpuVideoEncoder, Nv12Converter};

    use rewynd_config::{self as config};

    use crate::audio_pipeline::{AUDIO_SETTLE, SharedMixer, run_audio_mixer, spawn_audio_capture};
    use crate::params::{audio_encode_params, encode_params};
    use crate::tray;
    use rewynd_buffer::{AudioRingBuffer, EncodedChunk, RingBuffer};
    use rewynd_gpu::{DmabufImport, GpuContext};

    /// Application id, registered with the portal so the GlobalShortcuts backend (e.g.
    /// KWin) can attribute and persist our shortcut. Unsandboxed apps must register one.
    const APP_ID: &str = config::APP_ID;
    /// Stable id for our one shortcut; the compositor binds a trigger to it.
    const SHORTCUT_ID: &str = "save-clip";

    /// How long Quit waits for an in-flight upload before abandoning it.
    const QUIT_UPLOAD_GRACE: Duration = Duration::from_secs(5);
    /// Rebind attempts before the hotkey is declared gone (the recorder keeps running).
    const HOTKEY_REBIND_ATTEMPTS: u32 = 3;

    /// Pipeline failures surfaced to the user via the tray (tooltip + toast); the process
    /// keeps running so already-buffered footage stays saveable.
    enum RecorderEvent {
        CaptureFailed(String),
        SystemAudioFailed(String),
    }

    /// The recorder's threads, joined in dependency order by [`Recorder::shutdown`]. Fields are
    /// optional so a startup failure can tear down exactly what was already spawned.
    struct Recorder {
        stop: Arc<AtomicBool>,
        captures_done: Arc<AtomicBool>,
        system_audio: Option<std::thread::JoinHandle<()>>,
        mic_audio: Option<std::thread::JoinHandle<()>>,
        audio_mixer: Option<std::thread::JoinHandle<()>>,
        capture: Option<std::thread::JoinHandle<()>>,
        flush_hook: Option<std::thread::JoinHandle<()>>,
    }

    impl Recorder {
        /// Stop and join everything, in the order the pipeline requires: the capture loop's GPU
        /// teardown must happen on its own thread; the audio captures must stop adding to the
        /// mixer before `captures_done` releases the mixer's final drain + Opus flush.
        fn shutdown(&mut self, runtime: &tokio::runtime::Runtime, portal: PortalHandle) {
            self.stop.store(true, Ordering::Relaxed);
            // Closing the portal removes the PipeWire node so the capture loop errors out even
            // on an idle screen; the stop watchdog inside the stream is the belt to this brace.
            let _ = runtime.block_on(portal.close());
            if let Some(h) = self.capture.take() {
                let _ = h.join();
            }
            if let Some(h) = self.system_audio.take() {
                let _ = h.join();
            }
            if let Some(h) = self.mic_audio.take() {
                let _ = h.join();
            }
            self.captures_done.store(true, Ordering::Relaxed);
            if let Some(h) = self.audio_mixer.take() {
                let _ = h.join();
            }
            if let Some(h) = self.flush_hook.take() {
                let _ = h.join();
            }
        }
    }

    type PortalHandle = rewynd_capture::linux::PortalSession;

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
        // The system + mic capture threads sum into this; the mixer thread drains + encodes it.
        let mixer: SharedMixer = Arc::new(Mutex::new(AudioMixer::new(
            audio_params.sample_rate,
            audio_params.channels,
            AUDIO_SETTLE,
        )));
        // Raised by a clip save so the mixer drains its in-flight tail before the audio cut.
        let audio_drain_now = Arc::new(AtomicBool::new(false));
        let saver = ClipSaver::new(
            buffer.clone(),
            audio_buffer.clone(),
            params,
            audio_params,
            buffer_window,
            output_dir,
            Some(audio_drain_now.clone()),
        );

        // One monotonic epoch shared by all capture threads, so the video, system-audio and
        // mic PTS are on the same clock and the mixer/muxer can align them.
        let epoch = Instant::now();

        // ashpd's portals are async; reuse one runtime for ScreenCast setup and the
        // GlobalShortcuts event loop. (capture runs on its own std thread.)
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        // The portal Registry only accepts an app id that has an installed desktop entry,
        // so make sure one exists before registering. The login autostart entry is the settings
        // app's business alone.
        match std::env::current_exe() {
            Ok(exe) => {
                if let Err(e) = config::install_launcher_entry(&exe) {
                    tracing::warn!(error = %e, "could not write a desktop entry; the hotkey may not bind");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not locate our own binary"),
        }
        // Per-user hicolor icons back the entry's `Icon=` name: taskbar, tray fallback and
        // notifications all resolve through it.
        if let Err(e) = config::install_icons() {
            tracing::warn!(error = %e, "could not install app icons");
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
        let captures_done = Arc::new(AtomicBool::new(false));
        let mut recorder = Recorder {
            stop: stop.clone(),
            captures_done: captures_done.clone(),
            system_audio: None,
            mic_audio: None,
            audio_mixer: None,
            capture: None,
            flush_hook: None,
        };
        // Pipeline failures flow to the tray task, which owns the user-visible state.
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel::<RecorderEvent>();

        // From here on, any startup error must tear down the threads spawned so far — a
        // detached PipeWire callback racing process exit is undefined behaviour.
        let started = (|| -> Result<()> {
            // Audio runs on three threads: the system (sink-monitor) and microphone captures
            // each sum their PCM into the shared mixer; the mixer thread drains the aligned
            // mix, Opus-encodes it, and fills the audio ring. No portal — PipeWire connects
            // directly. Spawned BEFORE the GPU video thread so a spawn failure here can't
            // leave the video thread running and racing process exit.
            recorder.system_audio = Some(spawn_audio_capture(
                "rewynd-audio-system",
                AudioSource::SinkMonitor,
                None,
                audio_params,
                config.system_gain(),
                mixer.clone(),
                &stop,
                epoch,
                Some(Box::new({
                    let events = events_tx.clone();
                    move |e| {
                        let _ = events.send(RecorderEvent::SystemAudioFailed(e));
                    }
                })),
            )?);
            // The mic is optional: with no input device the mixer simply never receives mic
            // samples (the stream idles until shutdown), so clips are system-only.
            recorder.mic_audio = Some(spawn_audio_capture(
                "rewynd-audio-mic",
                AudioSource::Microphone,
                config.microphone().map(str::to_owned),
                audio_params,
                config.mic_gain(),
                mixer.clone(),
                &stop,
                epoch,
                None,
            )?);

            let mixer_buffer = audio_buffer.clone();
            let mixer_mixer = mixer.clone();
            let mixer_done = captures_done.clone();
            let mixer_drain_now = audio_drain_now.clone();
            recorder.audio_mixer = Some(
                std::thread::Builder::new()
                    .name("rewynd-audio-mixer".to_owned())
                    .spawn(move || {
                        if let Err(e) = run_audio_mixer(
                            epoch,
                            audio_params,
                            mixer_mixer,
                            mixer_buffer,
                            &mixer_done,
                            &mixer_drain_now,
                        ) {
                            tracing::error!(error = %e, "audio mixer loop stopped");
                        }
                    })
                    .context("spawning the audio mixer thread")?,
            );

            // Fill the video ring on its own thread: the PipeWire loop blocks, and the GPU
            // pipeline lives there start to finish (so it also tears down there, in order).
            let capture_buffer = buffer.clone();
            let capture_stop = stop.clone();
            let capture_events = events_tx.clone();
            recorder.capture = Some(
                std::thread::Builder::new()
                    .name("rewynd-capture".to_owned())
                    .spawn(move || {
                        if let Err(e) =
                            run_capture(node_id, fd, params, epoch, capture_buffer, &capture_stop)
                        {
                            tracing::error!(error = %e, "capture loop stopped");
                            let _ =
                                capture_events.send(RecorderEvent::CaptureFailed(format!("{e:#}")));
                        }
                    })
                    .context("spawning the capture thread")?,
            );

            // Dev aid: flush once after N seconds without a keypress, so the pipeline can be
            // exercised headlessly. Stop-aware and joined at shutdown like every other thread.
            if let Ok(value) = std::env::var("REWYND_FLUSH_AFTER") {
                match value.parse::<u64>() {
                    Ok(secs) => {
                        let flush_saver = saver.clone();
                        let flush_stop = stop.clone();
                        recorder.flush_hook = Some(
                            std::thread::Builder::new()
                                .name("rewynd-flush-hook".to_owned())
                                .spawn(move || {
                                    let deadline = Instant::now() + Duration::from_secs(secs);
                                    while Instant::now() < deadline {
                                        if flush_stop.load(Ordering::Relaxed) {
                                            return;
                                        }
                                        std::thread::sleep(Duration::from_millis(250));
                                    }
                                    if let Err(e) = flush_saver.save() {
                                        tracing::warn!(error = %e, "dev flush produced no clip");
                                    }
                                })
                                .context("spawning the flush hook thread")?,
                        );
                    }
                    Err(e) => {
                        tracing::warn!(value, error = %e, "ignoring invalid REWYND_FLUSH_AFTER");
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = started {
            recorder.shutdown(&runtime, portal);
            return Err(e);
        }

        // Every exit path funnels through this signal: tray Quit sends it, SIGTERM/SIGINT
        // trigger it, and the hotkey loop returning (session gone for good) implies it.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Tray icon + menu on a background task of the same runtime (no GTK, no extra event
        // loop). It owns all user-visible state: menu commands, pipeline-failure tooltips,
        // upload orchestration.
        runtime.spawn(run_tray(saver.clone(), events_rx, shutdown_tx.clone()));

        // Drive the hotkey until shutdown is requested (or the session is gone for good).
        let result = runtime.block_on(run_hotkey_loop(
            saver.clone(),
            config.hotkey_trigger(),
            shutdown_rx,
        ));

        recorder.shutdown(&runtime, portal);
        // The pid file isn't removed on exit: the kernel releases the `flock` when the process
        // dies, and unlinking it would race a relock by an incoming instance. A leftover pid is
        // harmless — the settings app verifies it against `/proc` before signalling it.
        result
    }

    /// The tray task: menu commands, upload orchestration, and pipeline-failure display.
    async fn run_tray(
        saver: Arc<ClipSaver>,
        mut events: tokio::sync::mpsc::UnboundedReceiver<RecorderEvent>,
        shutdown: tokio::sync::watch::Sender<bool>,
    ) {
        let (handle, mut rx) = match tray::spawn().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "tray unavailable; continuing without it");
                return;
            }
        };
        let upload_busy = Arc::new(AtomicBool::new(false));
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { continue };
                    let (title, body) = match event {
                        RecorderEvent::CaptureFailed(e) => (
                            "Recording stopped",
                            format!("The screen capture failed: {e}. Already-buffered footage can still be saved."),
                        ),
                        RecorderEvent::SystemAudioFailed(e) => (
                            "System audio lost",
                            format!("Clips will have no system sound: {e}"),
                        ),
                    };
                    handle
                        .update(|tray: &mut tray::RewyndTray| tray.status = title.to_owned())
                        .await;
                    tray::toast(title, &body).await;
                }
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        tray::TrayCmd::SaveClip => {
                            let s = saver.clone();
                            let saved = tokio::task::spawn_blocking(move || s.save()).await;
                            toast_save_outcome(saved).await;
                        }
                        tray::TrayCmd::UploadClip => {
                            // Fresh config each click: enabling uploads or changing the key in
                            // the settings works without restarting the recorder.
                            match (build_uploader(config::load().upload()), saver.last_clip()) {
                                (UploaderStatus::Ready(up), Some(path)) => {
                                    if upload_busy.swap(true, Ordering::SeqCst) {
                                        tray::toast(
                                            "Upload already running",
                                            "Wait for the current upload to finish.",
                                        )
                                        .await;
                                    } else {
                                        // Its own task so a slow upload never stalls the tray menu.
                                        tokio::spawn(upload_and_toast(up, path, upload_busy.clone()));
                                    }
                                }
                                (UploaderStatus::BadUrl(e), _) => {
                                    tray::toast(
                                        "Upload misconfigured",
                                        &format!("The API server URL in the settings is invalid: {e}"),
                                    )
                                    .await;
                                }
                                (UploaderStatus::Disabled, _) => {
                                    tray::toast(
                                        "Upload not configured",
                                        "Enable uploads and log in with ganked.tv in the settings.",
                                    )
                                    .await;
                                }
                                (UploaderStatus::Ready(_), None) => {
                                    tray::toast("No clip yet", "Save a clip first, then upload it.")
                                        .await;
                                }
                            }
                        }
                        tray::TrayCmd::OpenSettings => open_settings().await,
                        tray::TrayCmd::Quit => {
                            tracing::info!("quit requested from the tray");
                            if upload_busy.load(Ordering::SeqCst) {
                                tray::toast("Finishing upload", "rewynd quits when it completes (a few seconds at most).").await;
                                let waited = Instant::now();
                                while upload_busy.load(Ordering::SeqCst)
                                    && waited.elapsed() < QUIT_UPLOAD_GRACE
                                {
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                }
                                if upload_busy.load(Ordering::SeqCst) {
                                    tray::toast(
                                        "Upload abandoned",
                                        "rewynd quit while an upload was still running.",
                                    )
                                    .await;
                                }
                            }
                            let _ = shutdown.send(true);
                            break;
                        }
                    }
                }
            }
        }
        drop(handle); // removes the icon
    }

    /// Toast a save outcome, including the failures a user can act on.
    async fn toast_save_outcome(saved: Result<Result<PathBuf, SaveError>, tokio::task::JoinError>) {
        match saved {
            Ok(Ok(path)) => tray::clip_saved_toast(&path).await,
            Ok(Err(SaveError::Empty(reason))) => {
                tray::toast("Nothing to save yet", &reason).await;
            }
            Ok(Err(e @ SaveError::Write { .. })) => {
                tracing::error!(error = %e, "clip save failed");
                tray::toast("Could not save the clip", &e.to_string()).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "save task failed");
                tray::toast(
                    "Could not save the clip",
                    "The save task crashed; see the logs.",
                )
                .await;
            }
        }
    }

    /// Launch the sibling settings binary for the tray's "Open settings", reaping the child in
    /// the background (an unwaited child stays a zombie for the recorder's whole lifetime).
    async fn open_settings() {
        let Some(settings) = config::sibling_binary("rewynd-settings") else {
            tray::toast(
                "Could not open settings",
                "The settings binary was not found.",
            )
            .await;
            return;
        };
        match std::process::Command::new(&settings).spawn() {
            Ok(mut child) => {
                // A plain thread, NOT spawn_blocking: dropping the runtime at shutdown waits
                // for blocking tasks, and this one parks until the settings window closes.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %settings.display(), "could not open settings");
                tray::toast(
                    "Could not open settings",
                    &format!("{}: {e}", settings.display()),
                )
                .await;
            }
        }
    }

    /// ganked.tv upload wiring, built from the config at the moment of use.
    struct Uploader {
        client: rewynd_upload::GankedClient,
        visibility: rewynd_upload::Visibility,
        share_base: String,
    }

    /// Why there is (or isn't) an uploader — the tray tells the user different things for
    /// "switched off" versus "misconfigured".
    enum UploaderStatus {
        Ready(Uploader),
        Disabled,
        BadUrl(String),
    }

    fn build_uploader(settings: rewynd_config::UploadSettings) -> UploaderStatus {
        if !settings.enabled {
            return UploaderStatus::Disabled;
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
            Ok(client) => UploaderStatus::Ready(Uploader {
                client,
                visibility: rewynd_upload::Visibility::parse(vis),
                share_base: settings.share_url,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "uploads unavailable: could not build the ganked.tv client");
                UploaderStatus::BadUrl(e.to_string())
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
                tray::toast("Upload failed", &user_facing_upload_error(&e)).await;
            }
        }
    }

    /// Upload errors in words a user can act on; the full error goes to the log.
    fn user_facing_upload_error(e: &rewynd_upload::UploadError) -> String {
        use rewynd_upload::UploadError;
        match e {
            UploadError::Http(_) => {
                "Could not reach ganked.tv — check your connection and the API server URL."
                    .to_owned()
            }
            UploadError::Io(_) => "The clip file could not be read.".to_owned(),
            other => other.to_string(),
        }
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

        // Ask the compositor for the configured size/rate; whatever it actually delivers is
        // scaled to the encoder's dimensions in the NV12 pass.
        let prefs = StreamPrefs {
            width: params.width,
            height: params.height,
            framerate: params.framerate,
        };
        // A callback Break reads as a clean stop to the stream; record the real reason so it
        // still surfaces as this function's Err (and from there as a RecorderEvent).
        let failure: Rc<std::cell::Cell<Option<&'static str>>> =
            Rc::new(std::cell::Cell::new(None));
        capture_stream(node_id, fd, epoch, prefs, Some(stop.clone()), {
            let gpu = gpu.clone();
            let conv = conv.clone();
            let enc = enc.clone();
            let stop = stop.clone();
            let failure = failure.clone();
            let mut frame_index: u64 = 0;
            move |captured: CapturedDmabuf| -> ControlFlow<()> {
                if stop.load(Ordering::Relaxed) {
                    return ControlFlow::Break(());
                }
                // Force an IDR on the very first frame so the buffer always has an early
                // keyframe to cut on; the encoder's GOP supplies the rest. A wgpu panic must
                // not unwind into the PipeWire C callback (UB) — catch it and stop cleanly.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    encode_captured(
                        &gpu,
                        &conv,
                        &mut enc.borrow_mut(),
                        params,
                        captured,
                        frame_index == 0,
                    )
                }));
                match outcome {
                    Ok(Ok(chunk)) => {
                        // The chunk carries the frame's real capture timestamp, so the
                        // window evicts by wall-clock time regardless of the capture rate.
                        lock_unpoisoned(&buffer).push(chunk);
                        frame_index += 1;
                        ControlFlow::Continue(())
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "frame failed; stopping capture");
                        failure.set(Some("a frame failed to encode"));
                        ControlFlow::Break(())
                    }
                    Err(_) => {
                        tracing::error!("frame panicked (GPU error?); stopping capture");
                        failure.set(Some("the GPU pipeline panicked"));
                        ControlFlow::Break(())
                    }
                }
            }
        })?;

        drop(enc);
        drop(conv);
        drop(gpu);
        match failure.get() {
            Some(reason) => Err(anyhow!("{reason} (see the log for details)")),
            None => Ok(()),
        }
    }

    /// One frame of the hot path: import the DMA-BUF, convert (and scale) to NV12, encode.
    fn encode_captured(
        gpu: &GpuContext,
        conv: &Nv12Converter,
        enc: &mut GpuVideoEncoder,
        params: EncodeParams,
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
        let nv12 = conv.convert(gpu, &texture, params.width, params.height);
        Ok(enc.encode(&nv12, force_keyframe, pts)?)
    }

    /// Drive the global shortcut until shutdown: bind it, save a clip on every activation, and
    /// re-bind (with backoff) if the portal session drops. A hotkey that cannot be (re)bound
    /// degrades to tray-only saving instead of killing the recorder.
    async fn run_hotkey_loop(
        saver: Arc<ClipSaver>,
        hotkey_trigger: &str,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("installing the SIGTERM handler")?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("installing the SIGINT handler")?;

        let mut attempts: u32 = 0;
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            match bind_and_listen(
                &saver,
                hotkey_trigger,
                &mut shutdown,
                &mut sigterm,
                &mut sigint,
            )
            .await
            {
                Ok(HotkeyExit::Shutdown) => return Ok(()),
                Ok(HotkeyExit::SessionEnded { lasted }) => {
                    // A session that ran for a while resets the budget: the counter guards
                    // against a rapid rebind loop, not a compositor restarting once a week.
                    if lasted > Duration::from_secs(60) {
                        attempts = 0;
                    }
                    attempts += 1;
                    if attempts > HOTKEY_REBIND_ATTEMPTS {
                        tracing::error!("hotkey session keeps ending; continuing tray-only");
                        tray::toast(
                            "Hotkey unavailable",
                            "The shortcut stopped working; use the tray's Save clip now.",
                        )
                        .await;
                        wait_for_shutdown(&mut shutdown, &mut sigterm, &mut sigint).await;
                        return Ok(());
                    }
                    let backoff = Duration::from_secs(2u64.pow(attempts));
                    tracing::warn!(
                        attempt = attempts,
                        ?backoff,
                        "hotkey session ended; rebinding"
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => {
                    // No hotkey portal at all: the recorder still records; only the shortcut
                    // is gone. Tell the user and park until shutdown.
                    tracing::error!(error = %e, "global shortcut unavailable; tray-only mode");
                    tray::toast(
                        "Hotkey unavailable",
                        "Clips can still be saved from the tray menu (Save clip now).",
                    )
                    .await;
                    wait_for_shutdown(&mut shutdown, &mut sigterm, &mut sigint).await;
                    return Ok(());
                }
            }
        }
    }

    enum HotkeyExit {
        Shutdown,
        /// The portal session ended; `lasted` distinguishes a healthy long-lived session (a
        /// compositor restart — rebinding is fine indefinitely) from a rapid failure loop.
        SessionEnded {
            lasted: Duration,
        },
    }

    /// One bind-and-listen session of the GlobalShortcuts portal.
    async fn bind_and_listen(
        saver: &Arc<ClipSaver>,
        hotkey_trigger: &str,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
        sigterm: &mut tokio::signal::unix::Signal,
        sigint: &mut tokio::signal::unix::Signal,
    ) -> Result<HotkeyExit> {
        let shortcuts = GlobalShortcuts::new().await?;
        let session = shortcuts.create_session(Default::default()).await?;
        // From here on the session must be closed on EVERY path (its Drop does not send Close,
        // and the shared D-Bus connection lives as long as the process), so the fallible rest
        // runs in a block whose result is awaited before the close.
        let started = Instant::now();
        let result = async {
            // Subscribe before binding so no early activation is missed.
            let mut activated = shortcuts.receive_activated().await?;
            let save_description = format!("Save the last {} seconds", saver_window_secs(saver));
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

            let exit = loop {
                tokio::select! {
                    activation = activated.next() => {
                        let Some(activation) = activation else {
                            break HotkeyExit::SessionEnded { lasted: started.elapsed() };
                        };
                        tracing::info!(shortcut_id = activation.shortcut_id(), "shortcut activated");
                        if activation.shortcut_id() == SHORTCUT_ID {
                            // Blocking cut + mux off the runtime worker so activations keep flowing.
                            let s = saver.clone();
                            let saved = tokio::task::spawn_blocking(move || s.save()).await;
                            toast_save_outcome(saved).await;
                        }
                    }
                    _ = shutdown.changed() => break HotkeyExit::Shutdown,
                    _ = sigterm.recv() => {
                        tracing::info!("SIGTERM received; shutting down");
                        break HotkeyExit::Shutdown;
                    }
                    _ = sigint.recv() => {
                        tracing::info!("SIGINT received; shutting down");
                        break HotkeyExit::Shutdown;
                    }
                }
            };
            Ok(exit)
        }
        .await;

        session.close().await.ok();
        result
    }

    /// Park until shutdown is requested by the tray or a signal (degraded, tray-only mode).
    async fn wait_for_shutdown(
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
        sigterm: &mut tokio::signal::unix::Signal,
        sigint: &mut tokio::signal::unix::Signal,
    ) {
        tokio::select! {
            _ = shutdown.changed() => {}
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }

    fn saver_window_secs(saver: &Arc<ClipSaver>) -> u64 {
        saver.window().as_secs()
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use std::ops::ControlFlow;
    use std::os::windows::io::AsHandle;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow};
    use rewynd_buffer::{AudioRingBuffer, EncodedChunk, RingBuffer};
    use rewynd_capture::windows::{CapturedD3d11Frame, capture_game_stream, capture_stream};
    use rewynd_capture::{AudioSource, StreamPrefs};
    use rewynd_clip::{ClipSaver, SaveError, SharedAudioBuffer, SharedBuffer, lock_unpoisoned};
    use rewynd_config::{self as config};
    use rewynd_encode::{AudioMixer, EncodeParams, Encoder, GpuVideoEncoder, Nv12Converter};
    use rewynd_gpu::{D3d11HandleImport, GpuContext};
    use windows::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
        UnregisterHotKey, VK_F1,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONHAND, MB_OK, MSG, PM_REMOVE, PeekMessageW, WM_HOTKEY,
    };

    use crate::audio_pipeline::{AUDIO_SETTLE, SharedMixer, run_audio_mixer, spawn_audio_capture};
    use crate::params::{audio_encode_params, encode_params};

    /// Our one thread-queue hotkey registration.
    const HOTKEY_ID: i32 = 1;
    /// How often the hotkey message pump wakes to drain its queue and check the stop
    /// flag — the worst-case added latency on a press, well under perception.
    const HOTKEY_POLL: Duration = Duration::from_millis(30);

    pub fn run() -> Result<()> {
        tracing_subscriber::fmt::init();

        // Per-monitor DPI awareness, set before any threads or windows exist: without
        // it, window/monitor rects arrive DPI-virtualized on scaled displays and the
        // fullscreen-game check compares mismatched coordinate spaces.
        // SAFETY: trivially safe FFI (process-wide flag).
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

        config::ensure_default_file();
        let config = config::load();

        // Best-effort toast identity, so notifications carry rewynd's name and icon
        // instead of the launching host's (e.g. "Windows PowerShell").
        if let Err(e) = config::register_toast_identity() {
            tracing::warn!(error = %e, "could not register the toast identity");
        }

        // Single-instance guard (named mutex): two recorders would mean two WGC sessions
        // and two hotkey registrations fighting each other. Degraded start on IO error,
        // matching the Linux recorder.
        let _instance = match config::acquire_recorder_lock() {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => {
                tracing::error!("another rewynd recorder is already running; exiting");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not acquire the recorder lock; starting without one");
                None
            }
        };

        // Resolution / framerate / bitrate stay parameters (PLAN §9), sourced from the config.
        let params = encode_params(config.video());
        let buffer_window = config.buffer_window();
        let output_dir = config.output_dir();
        tracing::info!(
            width = params.width,
            height = params.height,
            fps = params.framerate,
            bitrate_bps = params.bitrate_bps,
            idr_period = params.idr_period,
            buffer_s = buffer_window.as_secs(),
            "encode parameters"
        );

        let audio_params = audio_encode_params(config.audio());
        tracing::info!(
            sample_rate = audio_params.sample_rate,
            channels = audio_params.channels,
            bitrate_bps = audio_params.bitrate_bps,
            mic_gain = config.mic_gain(),
            system_gain = config.system_gain(),
            "audio parameters"
        );

        let buffer: SharedBuffer = Arc::new(Mutex::new(RingBuffer::new(buffer_window)));
        let audio_buffer: SharedAudioBuffer =
            Arc::new(Mutex::new(AudioRingBuffer::new(buffer_window)));
        // The system (loopback) + mic capture threads sum into this; the mixer thread
        // drains + encodes it.
        let mixer: SharedMixer = Arc::new(Mutex::new(AudioMixer::new(
            audio_params.sample_rate,
            audio_params.channels,
            AUDIO_SETTLE,
        )));
        // Raised by a clip save so the mixer drains its in-flight tail before the audio cut.
        let audio_drain_now = Arc::new(AtomicBool::new(false));
        let saver = ClipSaver::new(
            buffer.clone(),
            audio_buffer.clone(),
            params,
            audio_params,
            buffer_window,
            output_dir,
            Some(audio_drain_now.clone()),
        );

        // One monotonic epoch shared by all capture threads, so the video, system-audio
        // and mic PTS are on the same clock and the mixer/muxer can align them.
        let epoch = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        // Raised only after the audio capture threads are joined, releasing the mixer's
        // final drain + Opus flush.
        let captures_done = Arc::new(AtomicBool::new(false));

        // Audio: system loopback + mic sum into the mixer; the mixer thread drains it.
        let system_audio = spawn_audio_capture(
            "rewynd-audio-system",
            AudioSource::SinkMonitor,
            None,
            audio_params,
            config.system_gain(),
            mixer.clone(),
            &stop,
            epoch,
            Some(Box::new(|e: String| {
                toast(
                    "System audio lost",
                    &format!("Clips will have no system sound: {e}"),
                );
            })),
        )?;
        // The mic is optional: with no input device the mixer simply never receives mic
        // samples, so clips are system-only.
        let mic_audio = spawn_audio_capture(
            "rewynd-audio-mic",
            AudioSource::Microphone,
            config.microphone().map(str::to_owned),
            audio_params,
            config.mic_gain(),
            mixer.clone(),
            &stop,
            epoch,
            None,
        )?;
        let mixer_buffer = audio_buffer.clone();
        let mixer_mixer = mixer.clone();
        let mixer_done = captures_done.clone();
        let mixer_drain_now = audio_drain_now.clone();
        let audio_mixer = std::thread::Builder::new()
            .name("rewynd-audio-mixer".to_owned())
            .spawn(move || {
                if let Err(e) = run_audio_mixer(
                    epoch,
                    audio_params,
                    mixer_mixer,
                    mixer_buffer,
                    &mixer_done,
                    &mixer_drain_now,
                ) {
                    tracing::error!(error = %e, "audio mixer loop stopped");
                }
            })
            .context("spawning the audio mixer thread")?;

        // Fill the video ring on its own thread: the WGC watchdog loop blocks, and the
        // GPU pipeline lives (and tears down) there.
        let capture_buffer = buffer.clone();
        let capture_stop = stop.clone();
        let capture_desktop = config.capture_desktop();
        let capture = std::thread::Builder::new()
            .name("rewynd-capture".to_owned())
            .spawn(move || {
                if let Err(e) = run_capture(
                    params,
                    epoch,
                    &capture_buffer,
                    &capture_stop,
                    capture_desktop,
                ) {
                    tracing::error!(error = %e, "capture loop stopped");
                    toast(
                        "Recording stopped",
                        &format!(
                            "The screen capture failed: {e:#}. Already-buffered footage can still be saved."
                        ),
                    );
                }
            })
            .context("spawning the capture thread")?;

        // Global hotkey on its own thread: RegisterHotKey delivers WM_HOTKEY to the
        // registering thread's queue, so that thread must also pump messages.
        let hotkey_saver = saver.clone();
        let hotkey_stop = stop.clone();
        let trigger = config.hotkey_trigger().to_owned();
        let hotkey = std::thread::Builder::new()
            .name("rewynd-hotkey".to_owned())
            .spawn(move || run_hotkey_loop(&trigger, &hotkey_saver, &hotkey_stop))
            .context("spawning the hotkey thread")?;

        // Dev aid: flush once after N seconds without a keypress, so the pipeline can be
        // exercised headlessly. Stop-aware and joined at shutdown like every other thread.
        let mut flush_hook = None;
        if let Ok(value) = std::env::var("REWYND_FLUSH_AFTER") {
            match value.parse::<u64>() {
                Ok(secs) => {
                    let flush_saver = saver.clone();
                    let flush_stop = stop.clone();
                    flush_hook = Some(
                        std::thread::Builder::new()
                            .name("rewynd-flush-hook".to_owned())
                            .spawn(move || {
                                let deadline = Instant::now() + Duration::from_secs(secs);
                                while Instant::now() < deadline {
                                    if flush_stop.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    std::thread::sleep(Duration::from_millis(250));
                                }
                                if let Err(e) = flush_saver.save() {
                                    tracing::warn!(error = %e, "dev flush produced no clip");
                                }
                            })
                            .context("spawning the flush hook thread")?,
                    );
                }
                Err(e) => {
                    tracing::warn!(value, error = %e, "ignoring invalid REWYND_FLUSH_AFTER");
                }
            }
        }

        // Park until Ctrl+C or the named stop event (the settings app's restart request
        // — the Windows SIGTERM stand-in), then run the same stop-flag-then-join
        // shutdown as the Linux recorder. Both waiter threads are detached: they hold
        // nothing that needs teardown and die with the process.
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<&'static str>();
        match config::RecorderStopEvent::create() {
            Ok(stop_event) => {
                let tx = shutdown_tx.clone();
                std::thread::Builder::new()
                    .name("rewynd-stop-event".to_owned())
                    .spawn(move || {
                        stop_event.wait();
                        let _ = tx.send("stop requested (settings restart)");
                    })
                    .context("spawning the stop-event thread")?;
            }
            // Without the event the settings restart falls back to terminating us.
            Err(e) => tracing::warn!(error = %e, "could not create the stop event"),
        }
        {
            let tx = shutdown_tx;
            std::thread::Builder::new()
                .name("rewynd-ctrl-c".to_owned())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let ok = runtime.map(|rt| rt.block_on(tokio::signal::ctrl_c()));
                    if matches!(ok, Ok(Ok(()))) {
                        let _ = tx.send("Ctrl+C");
                    }
                    // A failed handler just leaves the stop event as the only trigger.
                })
                .context("spawning the Ctrl+C thread")?;
        }
        let reason = shutdown_rx
            .recv()
            .unwrap_or("all shutdown waiters died; shutting down");
        tracing::info!(reason, "shutting down");

        // Join in the order the pipeline requires (the Linux Recorder::shutdown mirror):
        // the audio captures must stop adding to the mixer before `captures_done`
        // releases the mixer's final drain + Opus flush.
        stop.store(true, Ordering::Relaxed);
        let _ = capture.join();
        let _ = system_audio.join();
        let _ = mic_audio.join();
        captures_done.store(true, Ordering::Relaxed);
        let _ = audio_mixer.join();
        let _ = hotkey.join();
        if let Some(h) = flush_hook {
            let _ = h.join();
        }
        Ok(())
    }

    /// Build the GPU pipeline and pump captured frames into `buffer` until the stream
    /// ends. `desktop` picks the source: the whole primary monitor, or (the default)
    /// only the active fullscreen game — between games nothing is captured, so the
    /// desktop stays out of the ring. The encoder/converter/device are dropped in
    /// dependency order afterwards (tearing the device down before the encoder it
    /// backs crashes the driver); by the time the stream returns, the WGC thread is
    /// joined, so these Arcs are the last owners.
    fn run_capture(
        params: EncodeParams,
        epoch: Instant,
        buffer: &SharedBuffer,
        stop: &Arc<AtomicBool>,
        desktop: bool,
    ) -> Result<()> {
        let gpu = Arc::new(pollster::block_on(GpuContext::new())?);
        // Mutexes, not RefCell/Rc as on Linux: the per-frame callback runs on the WGC
        // capture thread, so everything it captures must be Send (+Sync via Arc).
        let conv = Arc::new(Mutex::new(Nv12Converter::new(&gpu)?));
        let enc = Arc::new(Mutex::new(GpuVideoEncoder::new(&gpu, params)?));
        tracing::info!(desktop, "capture pipeline ready; filling the ring buffer");

        // WGC captures the source at its native size; whatever arrives is scaled to the
        // encoder's dimensions in the NV12 pass. The framerate pref caps delivery.
        let prefs = StreamPrefs {
            width: params.width,
            height: params.height,
            framerate: params.framerate,
        };
        // A callback Break reads as a clean stop to the stream; record the real reason so
        // it still surfaces as this function's Err. (Mutex, not Cell: the callback runs on
        // the WGC capture thread.)
        let failure: Arc<Mutex<Option<&'static str>>> = Arc::new(Mutex::new(None));
        let on_frame = {
            let gpu = gpu.clone();
            let conv = conv.clone();
            let enc = enc.clone();
            let stop = stop.clone();
            let buffer = buffer.clone();
            let failure = failure.clone();
            let mut frame_index: u64 = 0;
            move |captured: CapturedD3d11Frame| -> ControlFlow<()> {
                if stop.load(Ordering::Relaxed) {
                    return ControlFlow::Break(());
                }
                // Force an IDR on the very first frame so the buffer always has an early
                // keyframe to cut on. A wgpu panic must not unwind into the WGC callback
                // (an FFI boundary) — catch it and stop cleanly.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    encode_captured(
                        &gpu,
                        &lock_unpoisoned(&conv),
                        &mut lock_unpoisoned(&enc),
                        params,
                        &captured,
                        frame_index == 0,
                    )
                }));
                match outcome {
                    Ok(Ok(chunk)) => {
                        lock_unpoisoned(&buffer).push(chunk);
                        frame_index += 1;
                        ControlFlow::Continue(())
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "frame failed; stopping capture");
                        *lock_unpoisoned(&failure) = Some("a frame failed to encode");
                        ControlFlow::Break(())
                    }
                    Err(_) => {
                        tracing::error!("frame panicked (GPU error?); stopping capture");
                        *lock_unpoisoned(&failure) = Some("the GPU pipeline panicked");
                        ControlFlow::Break(())
                    }
                }
            }
        };
        if desktop {
            capture_stream(None, epoch, prefs, Some(stop.clone()), on_frame)?;
        } else {
            capture_game_stream(epoch, prefs, Some(stop.clone()), on_frame)?;
        }

        drop(enc);
        drop(conv);
        drop(gpu);
        let reason = *lock_unpoisoned(&failure);
        match reason {
            Some(reason) => Err(anyhow!("{reason} (see the log for details)")),
            None => Ok(()),
        }
    }

    /// One frame of the hot path: import the shared NT handle, convert (and scale) to
    /// NV12, encode. The handle closes when `captured` drops — Vulkan holds its own
    /// reference to the D3D11 resource.
    fn encode_captured(
        gpu: &GpuContext,
        conv: &Nv12Converter,
        enc: &mut GpuVideoEncoder,
        params: EncodeParams,
        captured: &CapturedD3d11Frame,
        force_keyframe: bool,
    ) -> Result<EncodedChunk> {
        let format = captured.texture_format().ok_or_else(|| {
            anyhow!(
                "unsupported DXGI format {:?} (expected packed 32-bit RGB)",
                captured.dxgi_format
            )
        })?;

        let import = D3d11HandleImport {
            handle: captured.handle.as_handle(),
            width: captured.width,
            height: captured.height,
            format,
        };

        // SAFETY: `captured` came straight from the WGC backend, so the handle refers to
        // a shareable D3D11 texture matching these dimensions/format, fully written (the
        // backend waits on the copy before handing the handle out).
        let texture = unsafe { gpu.import_d3d11_shared_handle(import)? };
        let nv12 = conv.convert(gpu, &texture, params.width, params.height);
        Ok(enc.encode(&nv12, force_keyframe, captured.pts)?)
    }

    /// Register the configured trigger and pump this thread's message queue until the
    /// stop flag rises, saving a clip on every WM_HOTKEY. A trigger that cannot be
    /// parsed or registered degrades to a recorder without a hotkey (footage still
    /// flushes via the dev hook) instead of killing the process.
    fn run_hotkey_loop(trigger: &str, saver: &Arc<ClipSaver>, stop: &Arc<AtomicBool>) {
        let Some((mods, vk)) = parse_trigger(trigger) else {
            tracing::error!(trigger, "could not parse the hotkey trigger; no hotkey");
            toast(
                "Hotkey unavailable",
                &format!("The configured hotkey \"{trigger}\" could not be understood."),
            );
            return;
        };
        // SAFETY: FFI; a NULL hwnd registers on this thread's queue, unregistered below.
        if let Err(e) = unsafe { RegisterHotKey(None, HOTKEY_ID, mods | MOD_NOREPEAT, vk) } {
            tracing::error!(error = %e, trigger, "could not register the global hotkey (in use elsewhere?)");
            toast(
                "Hotkey unavailable",
                &format!("Could not register {trigger}: {e}"),
            );
            return;
        }
        tracing::info!(trigger, "global hotkey ready; press it to save a clip");

        loop {
            let mut msg = MSG::default();
            // SAFETY: FFI; drains this thread's own queue.
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                if msg.message == WM_HOTKEY && msg.wParam.0 == HOTKEY_ID as usize {
                    tracing::info!("hotkey activated");
                    save_and_toast(saver);
                }
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(HOTKEY_POLL);
        }
        // SAFETY: FFI; pairs the registration above.
        let _ = unsafe { UnregisterHotKey(None, HOTKEY_ID) };
    }

    /// Parse a `CTRL+ALT+R`-style trigger (the config's format, shared with the Linux
    /// portal hint) into `RegisterHotKey` modifiers + a virtual-key code. Exactly one
    /// non-modifier key is required: a letter, a digit, or `F1`–`F24`.
    fn parse_trigger(trigger: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
        let mut mods = HOT_KEY_MODIFIERS(0);
        let mut key = None;
        for part in trigger.split('+') {
            match part.trim().to_ascii_uppercase().as_str() {
                "CTRL" | "CONTROL" => mods |= MOD_CONTROL,
                "ALT" => mods |= MOD_ALT,
                "SHIFT" => mods |= MOD_SHIFT,
                "SUPER" | "META" | "WIN" | "LOGO" => mods |= MOD_WIN,
                k => {
                    if key.replace(parse_key(k)?).is_some() {
                        return None;
                    }
                }
            }
        }
        Some((mods, key?))
    }

    /// The virtual-key code for a single non-modifier key name (already uppercased).
    /// Letters and digits map to their ASCII codes per the Win32 VK table.
    fn parse_key(key: &str) -> Option<u32> {
        let mut bytes = key.bytes();
        if let (Some(c), None) = (bytes.next(), bytes.next()) {
            return (c.is_ascii_uppercase() || c.is_ascii_digit()).then_some(u32::from(c));
        }
        let n: u32 = key.strip_prefix('F')?.parse().ok()?;
        (1..=24).contains(&n).then(|| u32::from(VK_F1.0) + n - 1)
    }

    /// Cut + mux a clip, toast the outcome, and beep: Windows suppresses toasts while
    /// a fullscreen game has focus (focus assist), so the sound is the in-game
    /// confirmation that the press landed.
    fn save_and_toast(saver: &Arc<ClipSaver>) {
        let (beep, title, body) = match saver.save() {
            Ok(path) => (MB_OK, "Clip saved", path.display().to_string()),
            Err(SaveError::Empty(reason)) => (MB_ICONHAND, "Nothing to save yet", reason),
            Err(e) => {
                tracing::error!(error = %e, "clip save failed");
                (MB_ICONHAND, "Could not save the clip", e.to_string())
            }
        };
        // SAFETY: trivially safe FFI.
        let _ = unsafe { MessageBeep(beep) };
        toast(title, &body);
    }

    /// Fire-and-forget desktop toast (blocking is fine on the hotkey/capture threads).
    fn toast(title: &str, body: &str) {
        let _ = notify_rust::Notification::new()
            .summary(title)
            .body(body)
            // The registered AUMID (see `register_toast_identity`), so the toast
            // carries rewynd's name and icon instead of the launching host's.
            .app_id(config::APP_ID)
            .appname("rewynd")
            .show();
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn triggers_parse_to_modifiers_and_keys() {
            let (mods, vk) = parse_trigger("CTRL+ALT+R").expect("parses");
            assert_eq!(mods, MOD_CONTROL | MOD_ALT);
            assert_eq!(vk, u32::from(b'R'));

            let (mods, vk) = parse_trigger(" shift + win + f12 ").expect("parses");
            assert_eq!(mods, MOD_SHIFT | MOD_WIN);
            assert_eq!(vk, u32::from(VK_F1.0) + 11);

            let (mods, vk) = parse_trigger("CTRL+9").expect("parses");
            assert_eq!(mods, MOD_CONTROL);
            assert_eq!(vk, u32::from(b'9'));
        }

        #[test]
        fn bad_triggers_are_rejected() {
            assert!(parse_trigger("").is_none(), "no key");
            assert!(parse_trigger("CTRL+ALT").is_none(), "modifiers only");
            assert!(parse_trigger("CTRL+R+S").is_none(), "two keys");
            assert!(parse_trigger("CTRL+F25").is_none(), "F-key out of range");
            assert!(parse_trigger("CTRL+ESC?").is_none(), "unsupported key name");
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn main() {
    println!("rewynd currently runs on Linux and Windows only");
}
