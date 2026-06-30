//! System-audio capture via PipeWire (PLAN §9, ADR 0003).
//!
//! We capture the *default sink's monitor* — i.e. what the user hears — rather than a
//! microphone. PipeWire exposes this without any portal: we connect a normal session
//! (`Context::connect`, not the ScreenCast `connect_fd`) and set the
//! `stream.capture.sink` property so the stream attaches to the default sink's monitor
//! ports. The negotiated format is forced to interleaved `F32LE` at the requested rate
//! and channel count (PipeWire's audioconvert resamples/remixes transparently), so the
//! delivered samples are ready to hand straight to an Opus encoder.
//!
//! [`capture_system_audio`] runs the PipeWire main loop on the calling thread and hands
//! each buffer's interleaved PCM to a callback, stamped with a monotonic capture-relative
//! timestamp (the same PTS discipline as the video path's [`super::DmabufFrame::pts`]).

use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::{Object, Pod};
use pw::spa::utils::SpaTypes;

use crate::CaptureError;

/// Bytes per interleaved sample. We always negotiate `F32LE`.
const F32_BYTES: usize = std::mem::size_of::<f32>();

/// How often the watchdog timer fires to poll the cooperative `stop` flag and accumulate
/// idle time. Short enough that shutdown is prompt even when the sink delivers no buffers.
const WATCHDOG_POLL: Duration = Duration::from_millis(200);

/// Which audio endpoint to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// The default sink's monitor — the system output mix (what you hear).
    SinkMonitor,
    /// The default source — the microphone.
    Microphone,
}

impl AudioSource {
    /// Stream name for logs / the PipeWire graph.
    fn stream_name(self) -> &'static str {
        match self {
            AudioSource::SinkMonitor => "rewynd-audio-monitor",
            AudioSource::Microphone => "rewynd-audio-mic",
        }
    }
}

/// System-audio capture parameters.
///
/// Opus operates natively at 48 kHz, and stereo matches a typical desktop sink, so those
/// are the defaults. Like [`rewynd_encode::EncodeParams`], rate and channel count are
/// parameters rather than hard-coded constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    /// Sample rate in Hz (samples per channel per second).
    pub sample_rate: u32,
    /// Channel count (interleaved). 2 = stereo.
    pub channels: u32,
}

impl Default for AudioParams {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
        }
    }
}

/// Per-stream state threaded through the PipeWire callbacks.
struct UserData<F> {
    /// Per-buffer callback. Receives the interleaved PCM (frames of
    /// [`AudioParams::channels`] samples each) plus a monotonic capture-relative PTS, and
    /// returns [`ControlFlow::Break`] to stop the loop.
    on_samples: F,
    /// Reused decode buffer so the hot path allocates only when it has to grow.
    scratch: Vec<f32>,
    /// Set true on every dequeued buffer; the idle-timeout timer reads and resets it to
    /// detect a stream that has stopped delivering (e.g. a suspended sink).
    live: Rc<Cell<bool>>,
    /// Set once the callback breaks deliberately; read after the loop to tell a clean stop
    /// from an error/empty run.
    success: Rc<Cell<bool>>,
    /// First fatal reason (stream error, format mismatch, idle timeout), surfaced as the
    /// returned error instead of the generic "no samples" message.
    fatal: Rc<RefCell<Option<String>>>,
    /// Clone of the main loop so a callback can `quit()`.
    main_loop: pw::main_loop::MainLoopRc,
    /// The shared monotonic epoch each buffer's PTS is measured from — the same epoch the
    /// video capture uses, so the muxer can align the two tracks.
    stream_start: Instant,
}

/// Record the first fatal reason; later ones don't overwrite it (the first is the root
/// cause). Single-threaded — only ever touched on the PipeWire loop thread.
fn set_first_fatal(slot: &RefCell<Option<String>>, msg: String) {
    let mut slot = slot.borrow_mut();
    if slot.is_none() {
        *slot = Some(msg);
    }
}

/// Build the `EnumFormat` object pinning interleaved `F32LE` at the requested rate and
/// channel count. Non-zero rate/channels make PipeWire's audioconvert match them, so the
/// delivered buffers carry exactly this layout.
fn build_audio_format(params: AudioParams) -> Object {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(params.sample_rate);
    info.set_channels(params.channels);
    Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    }
}

/// Decode the valid region of an `F32LE` audio buffer into `out` (cleared first).
///
/// `offset`/`size` are the chunk's byte bounds within the mapped buffer; we clamp to the
/// mapped length so a malformed chunk can't read out of bounds, and decode whole samples
/// only (a trailing partial sample, which shouldn't occur for `F32LE`, is dropped).
fn decode_f32le(raw: &[u8], offset: usize, size: usize, out: &mut Vec<f32>) {
    let start = offset.min(raw.len());
    let end = offset.saturating_add(size).min(raw.len());
    out.clear();
    out.extend(
        raw[start..end]
            .chunks_exact(F32_BYTES)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
    );
}

/// Capture an audio endpoint (`source`: the system output mix, or the microphone) and hand
/// each buffer's interleaved PCM to `on_samples`.
///
/// `on_samples(pcm, pts)` receives interleaved `f32` samples — frames of
/// [`AudioParams::channels`] samples each — and the buffer's monotonic capture-relative
/// timestamp. It returns [`ControlFlow::Break`] to stop the loop (a deliberate, successful
/// end) or [`ControlFlow::Continue`] to keep receiving. The slice is valid only for the
/// call; copy out anything that must outlive it. Keep the callback cheap — it runs on the
/// PipeWire main loop, so heavy work there stalls capture.
///
/// `stop`, when set, is polled by a watchdog timer (every [`WATCHDOG_POLL`]) so the loop
/// shuts down promptly even when the sink is suspended and delivers no buffers (the callback
/// alone can't observe a stop request then). Continuous consumers (the app) pass a stop flag
/// they raise on shutdown; one-shot consumers pass `None`.
///
/// `idle_timeout`, when set, makes the call fail if no buffer arrives within that window — a
/// default sink that goes idle can suspend and deliver nothing, which would otherwise block
/// a consumer that has no other stop signal. It is a coarse watchdog (resolution
/// [`WATCHDOG_POLL`]), not a precise deadline; pass a value comfortably larger than
/// stream-startup latency. Continuous consumers driving their own shutdown via `stop` pass
/// `None` here.
///
/// Each buffer's PTS is measured from `epoch`; pass the same epoch to the video capture so
/// the two tracks share one clock and the muxer can align them.
///
/// Blocks (runs the PipeWire main loop) on the calling thread until the callback breaks,
/// `stop` is raised, the stream errors, or the idle timeout fires. Returns `Ok(())` on a
/// deliberate stop, otherwise a [`CaptureError::PipeWire`].
pub fn capture_audio(
    params: AudioParams,
    source: AudioSource,
    idle_timeout: Option<Duration>,
    stop: Option<Arc<AtomicBool>>,
    epoch: Instant,
    on_samples: impl FnMut(&[f32], Duration) -> ControlFlow<()> + 'static,
) -> Result<(), CaptureError> {
    if params.sample_rate == 0 || params.channels == 0 {
        return Err(CaptureError::PipeWire(
            "audio sample_rate and channels must be > 0".to_owned(),
        ));
    }

    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| CaptureError::PipeWire(format!("create main loop: {e}")))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| CaptureError::PipeWire(format!("create context: {e}")))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| CaptureError::PipeWire(format!("connect to pipewire: {e}")))?;

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    // Sink-monitor capture attaches to the default sink's monitor ports = the system output
    // mix (what you hear). Without this property a Capture stream attaches to the default
    // *source* — the microphone.
    if source == AudioSource::SinkMonitor {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    let stream = pw::stream::StreamRc::new(core, source.stream_name(), props)
        .map_err(|e| CaptureError::PipeWire(format!("create stream: {e}")))?;

    let live = Rc::new(Cell::new(false));
    let success = Rc::new(Cell::new(false));
    // Set by the watchdog when `stop` is observed — a clean, requested shutdown (vs. the
    // error/empty-run paths).
    let stopped = Rc::new(Cell::new(false));
    let fatal: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let user_data = UserData {
        on_samples,
        scratch: Vec::new(),
        live: live.clone(),
        success: success.clone(),
        fatal: fatal.clone(),
        main_loop: main_loop.clone(),
        stream_start: epoch,
    };

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(|_stream, ud, old, new| {
            tracing::info!(?old, ?new, "audio stream state changed");
            // run() doesn't exit on its own when the stream errors — record the real reason
            // and quit so the caller gets it instead of hanging or the generic message.
            if let pw::stream::StreamState::Error(err) = &new {
                tracing::error!(error = %err, "pipewire audio stream error; stopping");
                set_first_fatal(&ud.fatal, format!("stream error: {err}"));
                ud.main_loop.quit();
            }
        })
        .param_changed(move |_stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            let mut info = AudioInfoRaw::default();
            if let Err(e) = info.parse(param) {
                tracing::error!(error = ?e, "failed to parse negotiated audio format");
                return;
            }
            let (format, rate, channels) = (info.format(), info.rate(), info.channels());
            tracing::info!(rate, channels, ?format, "negotiated audio format");
            // We pin F32LE at the requested rate/channels, so audioconvert should match all
            // three. If it ever doesn't, `decode_f32le` would reinterpret the bytes (wrong
            // sample format) or the caller would deinterleave by the wrong channel count —
            // either way silently corrupting the audio. Fail loudly instead.
            if format != AudioFormat::F32LE
                || rate != params.sample_rate
                || channels != params.channels
            {
                let msg = format!(
                    "negotiated audio format {format:?} {rate} Hz / {channels} ch differs \
                     from requested F32LE {} Hz / {} ch",
                    params.sample_rate, params.channels
                );
                tracing::error!("{msg}");
                set_first_fatal(&ud.fatal, msg);
                ud.main_loop.quit();
            }
        })
        .process(move |stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                tracing::warn!("audio process: out of buffers");
                return;
            };
            // A buffer arrived: the stream is delivering (not suspended). Mark liveness
            // before any skip path so the idle timer never trips on an active stream.
            ud.live.set(true);
            // Stamp the PTS at dequeue, before any decode work, so it reflects arrival.
            let pts = ud.stream_start.elapsed();
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(raw) = data.data() else {
                return;
            };
            decode_f32le(raw, offset, size, &mut ud.scratch);
            if ud.scratch.is_empty() {
                return;
            }
            match (ud.on_samples)(&ud.scratch, pts) {
                ControlFlow::Break(()) => {
                    ud.success.set(true);
                    ud.main_loop.quit();
                }
                ControlFlow::Continue(()) => {}
            }
        })
        .register()
        .map_err(|e| CaptureError::PipeWire(format!("register audio stream listener: {e}")))?;

    let format = super::serialize_object(build_audio_format(params));
    let mut pod_params = [Pod::from_bytes(&format)
        .ok_or_else(|| CaptureError::PipeWire("invalid audio EnumFormat pod".to_owned()))?];

    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            // MAP_BUFFERS so the buffers are CPU-readable (we read interleaved PCM here);
            // no RT_PROCESS — the callback runs on the main loop, not the realtime thread,
            // so a consumer that locks a mutex can't cause an audio xrun.
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut pod_params,
        )
        .map_err(|e| CaptureError::PipeWire(format!("connect audio stream: {e}")))?;

    // Watchdog timer, armed when there's a `stop` to poll or an idle deadline to enforce.
    // It fires every WATCHDOG_POLL so the loop can quit even when no buffers arrive (a
    // suspended sink), which the per-buffer callback alone cannot do. Held alive (it borrows
    // the loop) until the fn returns.
    let _timer = if stop.is_some() || idle_timeout.is_some() {
        // The callback owns its own clones; `add_timer` borrows the outer `main_loop` (which
        // outlives this timer), so the returned source can be held until return.
        let live = live.clone();
        let fatal = fatal.clone();
        let stopped = stopped.clone();
        let quit_loop = main_loop.clone();
        // Idle time accumulated across polls with no buffer; reset when one arrives.
        let idle_elapsed = Cell::new(Duration::ZERO);
        let timer = main_loop.loop_().add_timer(move |_expirations| {
            // Cooperative shutdown wins: a requested stop is a clean exit.
            if stop.as_ref().is_some_and(|s| s.load(Ordering::Relaxed)) {
                stopped.set(true);
                quit_loop.quit();
                return;
            }
            let Some(limit) = idle_timeout else { return };
            if live.replace(false) {
                idle_elapsed.set(Duration::ZERO);
                return;
            }
            let elapsed = idle_elapsed.get() + WATCHDOG_POLL;
            idle_elapsed.set(elapsed);
            if elapsed >= limit {
                set_first_fatal(
                    &fatal,
                    format!("no audio buffers within {limit:?} (is the default sink active?)"),
                );
                quit_loop.quit();
            }
        });
        if let Err(e) = timer
            .update_timer(Some(WATCHDOG_POLL), Some(WATCHDOG_POLL))
            .into_result()
        {
            tracing::warn!(error = %e, "failed to arm the audio watchdog timer; won't auto-stop");
        }
        Some(timer)
    } else {
        None
    };

    tracing::info!("audio stream connected; entering main loop");
    main_loop.run();
    tracing::info!("audio main loop exited");

    if success.get() || stopped.get() {
        Ok(())
    } else if let Some(msg) = fatal.borrow_mut().take() {
        Err(CaptureError::PipeWire(msg))
    } else {
        Err(CaptureError::PipeWire(
            "audio stream ended without delivering samples \
             (no monitor source, or format negotiation failed — see logs)"
                .to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::serialize_object;
    use pw::spa::pod::Value;
    use pw::spa::pod::deserialize::PodDeserializer;

    #[test]
    fn default_params_are_48k_stereo() {
        let p = AudioParams::default();
        assert_eq!(p.sample_rate, 48_000);
        assert_eq!(p.channels, 2);
    }

    #[test]
    fn build_audio_format_pins_f32le_rate_and_channels() {
        let params = AudioParams {
            sample_rate: 44_100,
            channels: 1,
        };
        let bytes = serialize_object(build_audio_format(params));
        let (_, value) =
            PodDeserializer::deserialize_from::<Value>(&bytes).expect("deserialize format pod");
        let Value::Object(obj) = value else {
            panic!("expected an Object pod");
        };

        let prop = |key: u32| obj.properties.iter().find(|p| p.key == key);

        let format = prop(spa::sys::SPA_FORMAT_AUDIO_format).expect("format prop present");
        assert_eq!(
            format.value,
            Value::Id(spa::utils::Id(AudioFormat::F32LE.as_raw()))
        );

        let rate = prop(spa::sys::SPA_FORMAT_AUDIO_rate).expect("rate prop present");
        assert_eq!(rate.value, Value::Int(44_100));

        let channels = prop(spa::sys::SPA_FORMAT_AUDIO_channels).expect("channels prop present");
        assert_eq!(channels.value, Value::Int(1));
    }

    #[test]
    fn decode_f32le_round_trips_samples() {
        let samples = [-1.0_f32, -0.5, 0.0, 0.25, 1.0];
        let mut bytes = Vec::new();
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = Vec::new();
        decode_f32le(&bytes, 0, bytes.len(), &mut out);
        assert_eq!(out, samples);
    }

    #[test]
    fn decode_f32le_honours_offset_and_clears() {
        let mut bytes = vec![0xAB; F32_BYTES]; // one garbage sample before the valid region
        bytes.extend_from_slice(&0.5_f32.to_le_bytes());
        bytes.extend_from_slice(&(-0.5_f32).to_le_bytes());
        let mut out = vec![123.0]; // pre-existing content must be cleared
        decode_f32le(&bytes, F32_BYTES, 2 * F32_BYTES, &mut out);
        assert_eq!(out, [0.5, -0.5]);
    }

    #[test]
    fn decode_f32le_clamps_out_of_range_chunk() {
        let bytes = 0.75_f32.to_le_bytes();
        let mut out = Vec::new();
        // A size that overruns the buffer must be clamped, not panic.
        decode_f32le(&bytes, 0, 4096, &mut out);
        assert_eq!(out, [0.75]);
    }

    #[test]
    fn decode_f32le_drops_trailing_partial_sample() {
        let mut bytes = 0.25_f32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0x00, 0x01]); // 2 stray bytes — not a whole f32
        let mut out = Vec::new();
        decode_f32le(&bytes, 0, bytes.len(), &mut out);
        assert_eq!(out, [0.25]);
    }

    #[test]
    fn zero_rate_or_channels_is_rejected() {
        let err = capture_audio(
            AudioParams {
                sample_rate: 0,
                channels: 2,
            },
            AudioSource::SinkMonitor,
            None,
            None,
            Instant::now(),
            |_, _| ControlFlow::Break(()),
        )
        .expect_err("zero sample_rate must be rejected");
        assert!(matches!(err, CaptureError::PipeWire(_)));

        let err = capture_audio(
            AudioParams {
                sample_rate: 48_000,
                channels: 0,
            },
            AudioSource::SinkMonitor,
            None,
            None,
            Instant::now(),
            |_, _| ControlFlow::Break(()),
        )
        .expect_err("zero channels must be rejected");
        assert!(matches!(err, CaptureError::PipeWire(_)));
    }

    #[test]
    fn set_first_fatal_keeps_the_first() {
        let slot = RefCell::new(None);
        set_first_fatal(&slot, "first".to_owned());
        set_first_fatal(&slot, "second".to_owned());
        assert_eq!(slot.borrow().as_deref(), Some("first"));
    }
}
