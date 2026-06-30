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

use std::cell::Cell;
use std::io::Cursor;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::SpaTypes;

use crate::CaptureError;

/// Bytes per interleaved sample. We always negotiate `F32LE`.
const F32_BYTES: usize = std::mem::size_of::<f32>();

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
    /// The most recently negotiated raw audio format.
    format: AudioInfoRaw,
    /// Per-buffer callback. Receives the interleaved PCM (frames of
    /// [`AudioParams::channels`] samples each) plus a monotonic capture-relative PTS, and
    /// returns [`ControlFlow::Break`] to stop the loop.
    on_samples: F,
    /// Reused decode buffer so the hot path allocates only when it has to grow.
    scratch: Vec<f32>,
    /// Set once the callback breaks deliberately; read after the loop to tell a clean
    /// stop from an error/empty run.
    success: Rc<Cell<bool>>,
    /// Clone of the main loop so a callback can `quit()`.
    main_loop: pw::main_loop::MainLoopRc,
    /// When the stream started, so each buffer gets a monotonic capture-relative PTS.
    stream_start: Instant,
    /// The format we requested, to flag a mismatch the caller's framing wouldn't expect.
    want: AudioParams,
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

/// Serialize a pod [`Object`] to bytes (suitable for `Pod::from_bytes`).
fn serialize_object(obj: Object) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("pod serialization cannot fail for in-memory buffer")
        .0
        .into_inner()
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

/// Capture the default sink's monitor (system audio output) and hand each buffer's
/// interleaved PCM to `on_samples`.
///
/// `on_samples(pcm, pts)` receives interleaved `f32` samples — frames of
/// [`AudioParams::channels`] samples each — and the buffer's monotonic capture-relative
/// timestamp. It returns [`ControlFlow::Break`] to stop the loop (a deliberate, successful
/// end) or [`ControlFlow::Continue`] to keep receiving. The slice is valid only for the
/// call; copy out anything that must outlive it. Keep the callback cheap — it runs on the
/// PipeWire main loop, so heavy work there stalls capture.
///
/// Blocks (runs the PipeWire main loop) on the calling thread until the callback breaks or
/// the stream errors. Returns `Ok(())` on a deliberate stop, otherwise a
/// [`CaptureError::PipeWire`].
pub fn capture_system_audio(
    params: AudioParams,
    on_samples: impl FnMut(&[f32], Duration) -> ControlFlow<()> + 'static,
) -> Result<(), CaptureError> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| CaptureError::PipeWire(format!("create main loop: {e}")))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| CaptureError::PipeWire(format!("create context: {e}")))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| CaptureError::PipeWire(format!("connect to pipewire: {e}")))?;

    let stream = pw::stream::StreamRc::new(
        core,
        "rewynd-audio-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            // Attach to the default sink's monitor ports = the system output mix (what you
            // hear), not a microphone. This is the whole point of the sink-monitor route.
            *pw::keys::STREAM_CAPTURE_SINK => "true",
        },
    )
    .map_err(|e| CaptureError::PipeWire(format!("create stream: {e}")))?;

    let success = Rc::new(Cell::new(false));
    let user_data = UserData {
        format: AudioInfoRaw::default(),
        on_samples,
        scratch: Vec::new(),
        success: success.clone(),
        main_loop: main_loop.clone(),
        stream_start: Instant::now(),
        want: params,
    };

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(|_stream, ud, old, new| {
            tracing::info!(?old, ?new, "audio stream state changed");
            // run() doesn't exit on its own when the stream errors — quit so the caller
            // gets an error instead of hanging.
            if let pw::stream::StreamState::Error(err) = &new {
                tracing::error!(error = %err, "pipewire audio stream error; stopping");
                ud.main_loop.quit();
            }
        })
        .param_changed(|_stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) = match format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            if let Err(e) = ud.format.parse(param) {
                tracing::error!(error = ?e, "failed to parse negotiated audio format");
                return;
            }
            let (rate, channels) = (ud.format.rate(), ud.format.channels());
            tracing::info!(
                rate,
                channels,
                format = ?ud.format.format(),
                "negotiated audio format"
            );
            // We pin the format, so audioconvert should match it; warn loudly if not,
            // since the caller deinterleaves by the requested channel count.
            if rate != ud.want.sample_rate || channels != ud.want.channels {
                tracing::warn!(
                    got_rate = rate,
                    got_channels = channels,
                    want_rate = ud.want.sample_rate,
                    want_channels = ud.want.channels,
                    "negotiated audio format differs from requested"
                );
            }
        })
        .process(move |stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                tracing::warn!("audio process: out of buffers");
                return;
            };
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

    let format = serialize_object(build_audio_format(params));
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

    tracing::info!("audio stream connected; entering main loop");
    main_loop.run();
    tracing::info!("audio main loop exited");

    if success.get() {
        Ok(())
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
}
