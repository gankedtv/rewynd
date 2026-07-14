//! System-audio and microphone capture over ScreenCaptureKit (docs/adr/0015):
//! `capturesAudio` loopback for the system mix ("what you hear") and
//! `captureMicrophone` for the mic, as interleaved f32 PCM.
//!
//! SCK delivers planar (non-interleaved) f32 sample buffers on its own dispatch
//! queue; the output delegate interleaves them into the [`AudioParams`] channel
//! layout and stamps PTS with the same continuous stream clock as the WASAPI
//! backend ([`crate::windows::capture_audio`]): the first buffer anchors on the
//! shared epoch, later ones advance by the frames delivered, and a real delivery
//! gap re-anchors. The blocking entry point keeps the cross-platform
//! `capture_audio` contract: same arguments, same `ControlFlow` callback, same
//! epoch-relative PTS.

use std::ops::ControlFlow;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use cidre::cat::audio::{Format, FormatFlags};
use cidre::sc::StreamOutput;
use cidre::{arc, av, cat, cm, define_obj_type, dispatch, ns, objc, sc};

use super::resample::Resampler;
use super::{
    SessionShared, StreamErrDelegate, StreamErrDelegateInner, ensure_screen_capture_access,
    select_display, session_result, shareable_content, watch_session,
};
use crate::{AudioParams, AudioSource, CaptureError};

/// How far the continuous stream clock may drift behind the wall clock before a
/// buffer re-anchors: the same 100 ms bound as the WASAPI backend — above SCK's
/// buffer cadence so burst-delivered buffers stay on the stream clock, small
/// enough that a real gap re-syncs promptly.
const REANCHOR_DRIFT: Duration = Duration::from_millis(100);

type SamplesCallback = Box<dyn FnMut(&[f32], Duration) -> ControlFlow<()> + Send>;

#[repr(C)]
struct AudioOutputInner {
    shared: Arc<SessionShared>,
    on_samples: SamplesCallback,
    epoch: Instant,
    params: AudioParams,
    /// Which output type this session consumes (`Audio` loopback or `Mic`).
    expected: sc::OutputType,
    /// Reused scratch: the source's frames mapped onto `params.channels`, still at the
    /// source rate.
    mapped: Vec<f32>,
    /// Reused scratch: `mapped` at `params.sample_rate` — what the callback receives.
    interleaved: Vec<f32>,
    /// Rate conversion, built once the first buffer reveals the source's rate. `None`
    /// while the source already runs at `params.sample_rate`.
    resampler: Option<Resampler>,
    /// Whether the source format has been logged/validated (it never changes mid-stream).
    format_seen: bool,
    /// Reused audio-buffer-list storage for the per-buffer plane extraction.
    buf_list: cat::AudioBufListN,
    stream_clock: Option<Duration>,
}

impl AudioOutputInner {
    fn handle(&mut self, sample_buf: &mut cm::SampleBuf) {
        if self.shared.should_stop() {
            return;
        }
        let Some(desc) = sample_buf.format_desc() else {
            return;
        };
        let Some(asbd) = desc.stream_basic_desc() else {
            return;
        };
        // f32 PCM is the one hard requirement (SCK always delivers it); rate, channel
        // count and interleaving are whatever the source runs at — the microphone leg
        // ignores the configured rate and hands over the capture device's native format
        // — so those are converted below rather than rejected.
        if asbd.format != Format::LINEAR_PCM
            || !asbd.format_flags.contains(FormatFlags::IS_FLOAT)
            || asbd.bits_per_channel != 32
        {
            self.shared
                .fail(format!("SCK delivered non-f32 PCM audio: {asbd:?}"));
            return;
        }
        let src_rate = asbd.sample_rate as u32;
        let src_channels = (asbd.channels_per_frame as usize).max(1);
        let planar = asbd.format_flags.contains(FormatFlags::IS_NON_INTERLEAVED);
        if !self.format_seen {
            self.format_seen = true;
            tracing::info!(
                source = ?self.expected,
                src_rate,
                src_channels,
                planar,
                out_rate = self.params.sample_rate,
                out_channels = self.params.channels,
                "SCK audio format"
            );
            if src_rate != self.params.sample_rate {
                self.resampler = Some(Resampler::new(
                    src_rate,
                    self.params.sample_rate,
                    self.params.channels as usize,
                ));
            }
        }

        let num_samples = sample_buf.num_samples();
        if num_samples <= 0 {
            return;
        }
        let list = match sample_buf.audio_buf_list_n(&mut self.buf_list) {
            Ok(list) => list,
            Err(e) => {
                self.shared.fail(format!("audio buffer list: {e}"));
                return;
            }
        };
        let plane_count = list.list.number_buffers();
        let buffers = list.list.buffers();
        let planes = &buffers[..plane_count.min(buffers.len())];
        if planes.is_empty() {
            return;
        }
        let per_plane_channels = if planar { 1 } else { src_channels };
        let frames = planes
            .iter()
            .map(|b| b.data_bytes_size as usize / size_of::<f32>() / per_plane_channels)
            .min()
            .unwrap_or(0)
            .min(num_samples as usize);
        if frames == 0 {
            return;
        }

        // Map onto frames of `params.channels`: a mono source duplicates into every
        // requested channel; extra source channels are dropped. Planar sources carry one
        // plane per channel; a packed source interleaves them all in plane 0.
        let out_channels = self.params.channels as usize;
        self.mapped.clear();
        self.mapped.reserve(frames * out_channels);
        for frame in 0..frames {
            for channel in 0..out_channels {
                let sample = if planar {
                    let plane = &planes[channel.min(planes.len() - 1)];
                    // SAFETY: `frame < frames <= data_bytes_size / 4` for every plane,
                    // and the block buffer keeps the data alive via `list`.
                    unsafe { *plane.data.cast::<f32>().add(frame) }
                } else {
                    let channel = channel.min(src_channels - 1);
                    // SAFETY: as above; a packed buffer holds `frames * src_channels`
                    // samples in plane 0 (`frames` is derived from its byte size).
                    unsafe {
                        *planes[0]
                            .data
                            .cast::<f32>()
                            .add(frame * src_channels + channel)
                    }
                };
                self.mapped.push(sample);
            }
        }

        // Re-establish the pipeline's fixed rate when the source runs at another one.
        match self.resampler.as_mut() {
            Some(resampler) => resampler.process(&self.mapped, &mut self.interleaved),
            None => std::mem::swap(&mut self.mapped, &mut self.interleaved),
        }
        let frames = self.interleaved.len() / out_channels;
        if frames == 0 {
            return;
        }

        // Continuous stream clock (see the WASAPI backend for the rationale):
        // only a delivery gap — wall far ahead of the clock — re-anchors.
        let wall = self.epoch.elapsed();
        let pts = match self.stream_clock {
            Some(clock) if wall.saturating_sub(clock) < REANCHOR_DRIFT => clock,
            _ => wall,
        };
        self.stream_clock = Some(
            pts + Duration::from_nanos(
                frames as u64 * 1_000_000_000 / u64::from(self.params.sample_rate),
            ),
        );

        self.shared.mark_sample();
        // The callback must not unwind across the ObjC/dispatch boundary.
        match catch_unwind(AssertUnwindSafe(|| {
            (self.on_samples)(&self.interleaved, pts)
        })) {
            Ok(ControlFlow::Continue(())) => {}
            Ok(ControlFlow::Break(())) => self.shared.finish(),
            Err(_) => self.shared.fail("audio callback panicked".to_owned()),
        }
    }
}

define_obj_type!(
    AudioOutput + sc::StreamOutputImpl,
    AudioOutputInner,
    REWYND_SCK_AUDIO_OUTPUT
);

impl StreamOutput for AudioOutput {}

#[objc::add_methods]
impl sc::StreamOutputImpl for AudioOutput {
    extern "C" fn impl_stream_did_output_sample_buf(
        &mut self,
        _cmd: Option<&objc::Sel>,
        _stream: &sc::Stream,
        sample_buf: &mut cm::SampleBuf,
        kind: sc::OutputType,
    ) {
        if kind == self.inner().expected {
            self.inner_mut().handle(sample_buf);
        }
    }
}

/// Resolve a configured microphone name to the AVCapture device UID SCK wants:
/// case-insensitive substring match on the localized device name, mirroring the
/// WASAPI endpoint lookup. No match is an error listing what exists, so a typo'd
/// config names its fix.
fn resolve_mic_uid(name: &str) -> Result<arc::R<ns::String>, CaptureError> {
    let types = ns::Array::from_slice(&[av::CaptureDeviceType::microphone()]);
    let session = av::CaptureDeviceDiscoverySession::with_device_types_media_and_pos(
        &types,
        Some(av::MediaType::audio()),
        av::CaptureDevicePos::Unspecified,
    );
    let devices = session.devices();
    let wanted = name.to_lowercase();
    let mut names = Vec::with_capacity(devices.len());
    for device in devices.iter() {
        let label = device.localized_name().to_string();
        if label.to_lowercase().contains(&wanted) {
            tracing::info!(device = label, "using the configured microphone");
            return Ok(device.unique_id());
        }
        names.push(label);
    }
    Err(CaptureError::Sck(format!(
        "no microphone matches \"{name}\" (available: {})",
        names.join(", ")
    )))
}

/// Capture `source` as interleaved f32 PCM in the [`AudioParams`] format,
/// delivering each buffer to `on_samples` with a PTS measured from `epoch` (pass
/// the video capture's epoch so the muxer can align the tracks).
///
/// `device` selects the microphone by name (case-insensitive substring of the
/// localized device name); `None` uses the default. SCK's loopback always
/// follows the system output, so a named device with
/// [`AudioSource::SinkMonitor`] is an error.
///
/// `stop`, when set, is watched off the sample path so shutdown is prompt even
/// while nothing plays. `idle_timeout`, when set, fails the call if no buffer
/// arrives within that window — for one-shot consumers with no stop flag.
///
/// Blocks on the calling thread until the callback breaks, `stop` is raised, the
/// stream fails, or the idle timeout fires. `on_samples` runs on a serial
/// dispatch queue owned by SCK, hence the `Send` bound. Returns `Ok(())` on a
/// deliberate stop, otherwise a [`CaptureError::Sck`].
pub fn capture_audio<F>(
    params: AudioParams,
    source: AudioSource,
    device: Option<&str>,
    idle_timeout: Option<Duration>,
    stop: Option<Arc<AtomicBool>>,
    epoch: Instant,
    on_samples: F,
) -> Result<(), CaptureError>
where
    F: FnMut(&[f32], Duration) -> ControlFlow<()> + Send + 'static,
{
    if params.sample_rate == 0 || params.channels == 0 {
        return Err(CaptureError::Sck(
            "audio sample_rate and channels must be > 0".to_owned(),
        ));
    }
    // Pool for the ObjC temporaries, drained on return: the caller's thread has
    // none of its own (the delegate's dispatch queue is autorelease-pooled).
    let _pool = objc::AutoreleasePoolPage::push();

    ensure_screen_capture_access()?;
    let content = shareable_content()?;
    let display = select_display(&content, None)?;

    let mut cfg = sc::StreamCfg::new();
    // SCK streams always run a video leg; keep it as small and slow as the API
    // allows since only the audio output is attached.
    cfg.set_width(2);
    cfg.set_height(2);
    cfg.set_minimum_frame_interval(cm::Time::new(1, 1));
    cfg.set_sample_rate(i64::from(params.sample_rate));
    cfg.set_channel_count(i64::from(params.channels));
    let expected = match source {
        AudioSource::SinkMonitor => {
            if let Some(name) = device {
                return Err(CaptureError::Sck(format!(
                    "system-audio loopback follows the system output on macOS; \
                     a device selection (\"{name}\") is not supported"
                )));
            }
            cfg.set_captures_audio(true);
            cfg.set_excludes_current_process_audio(true);
            sc::OutputType::Audio
        }
        AudioSource::Microphone => {
            // Apple's own sample enables both on a microphone stream; only the Mic
            // output is attached below, so no system audio is delivered here.
            cfg.set_captures_audio(true);
            cfg.set_excludes_current_process_audio(true);
            cfg.set_capture_mic(true);
            if let Some(name) = device {
                let uid = resolve_mic_uid(name)?;
                cfg.set_mic_capture_device_id(Some(&uid));
            }
            sc::OutputType::Mic
        }
    };

    let shared = SessionShared::new();
    let delegate = StreamErrDelegate::with(StreamErrDelegateInner {
        shared: shared.clone(),
    });
    let output = AudioOutput::with(AudioOutputInner {
        shared: shared.clone(),
        on_samples: Box::new(on_samples),
        epoch,
        params,
        expected,
        mapped: Vec::new(),
        interleaved: Vec::new(),
        resampler: None,
        format_seen: false,
        buf_list: cat::AudioBufListN::default(),
        stream_clock: None,
    });

    let windows = ns::Array::new();
    let filter = sc::ContentFilter::with_display_excluding_windows(&display, &windows);
    let stream = sc::Stream::with_delegate(&filter, &cfg, delegate.as_ref());
    let queue = dispatch::Queue::serial_with_ar_pool();
    stream
        .add_stream_output(output.as_ref(), expected, Some(&queue))
        .map_err(|e| CaptureError::Sck(format!("add audio output: {e}")))?;
    pollster::block_on(stream.start())
        .map_err(|e| CaptureError::Sck(format!("start stream: {e}")))?;
    tracing::info!(
        ?source,
        rate = params.sample_rate,
        channels = params.channels,
        "SCK audio capture started"
    );

    watch_session(&shared, stop.as_ref(), idle_timeout);
    // A failed/already-stopped stream also errors its stop call; nothing to act on.
    if let Err(e) = pollster::block_on(stream.stop()) {
        tracing::debug!(error = %e, "SCK stream stop reported an error");
    }
    session_result(&shared)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs the live AVCapture device layer (ignored like the GPU tests; run
    /// with `-- --ignored` on a macOS box): a name that matches nothing must
    /// come back as an error listing the real microphones.
    #[test]
    #[ignore]
    fn live_mic_discovery_lists_devices() {
        let err = resolve_mic_uid("no-such-microphone-xyzzy").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no microphone matches"), "got: {msg}");
    }
}
