//! System-audio capture over WASAPI: the default render endpoint's loopback (the
//! system mix — what you hear) or the default microphone, as interleaved f32 PCM.
//!
//! Shared-mode streams are opened with `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`, so the
//! audio engine converts whatever the device's mix format is into the [`AudioParams`]
//! format we ask for — rate and channel count stay parameters, exactly like the
//! PipeWire negotiation on Linux. The capture client is polled on the calling thread
//! (WASAPI buffers ~200 ms internally; a 10 ms poll never starves it), which keeps the
//! blocking per-buffer-callback shape of [`crate::linux::capture_audio`]: same
//! arguments, same `ControlFlow` contract, same epoch-relative PTS.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};

use crate::{AudioParams, AudioSource, CaptureError};

/// How often the capture client is polled. WASAPI's shared-mode engine period is 10 ms;
/// polling at that cadence drains every packet without busy-waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// `WAVE_FORMAT_IEEE_FLOAT` (mmreg.h): 32-bit float PCM, the format the mixer expects.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// How far the continuous stream clock may drift from the wall clock before a
/// packet re-anchors. Above the engine period (10 ms) so burst-drained packets
/// stay on the stream clock; small enough that a real gap (idle loopback)
/// re-syncs promptly instead of back-dating the resumed audio.
const REANCHOR_DRIFT: Duration = Duration::from_millis(100);

/// Balances `CoInitializeEx` on drop, so every exit path uninitializes COM exactly once.
struct ComGuard;

impl ComGuard {
    fn init() -> Result<Self, CaptureError> {
        // SAFETY: FFI; paired with `CoUninitialize` in drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| CaptureError::Wasapi(format!("CoInitializeEx: {e}")))?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: FFI; pairs the successful init.
        unsafe { CoUninitialize() };
    }
}

/// Capture `source` as interleaved f32 PCM in the [`AudioParams`] format, delivering
/// each drained packet to `on_samples` with a PTS measured from `epoch` (pass the video
/// capture's epoch so the muxer can align the tracks).
///
/// `stop`, when set, is polled every [`POLL_INTERVAL`] so shutdown is prompt even while
/// the endpoint delivers nothing (a loopback with no audio playing goes quiet).
/// `idle_timeout`, when set, fails the call if no packet arrives within that window —
/// for one-shot consumers with no stop flag of their own.
///
/// Blocks on the calling thread until the callback breaks, `stop` is raised, the device
/// fails, or the idle timeout fires. Returns `Ok(())` on a deliberate stop, otherwise a
/// [`CaptureError::Wasapi`].
pub fn capture_audio(
    params: AudioParams,
    source: AudioSource,
    idle_timeout: Option<Duration>,
    stop: Option<Arc<AtomicBool>>,
    epoch: Instant,
    mut on_samples: impl FnMut(&[f32], Duration) -> ControlFlow<()> + 'static,
) -> Result<(), CaptureError> {
    if params.sample_rate == 0 || params.channels == 0 {
        return Err(CaptureError::Wasapi(
            "audio sample_rate and channels must be > 0".to_owned(),
        ));
    }

    let _com = ComGuard::init()?;

    // SAFETY: FFI; COM is initialized on this thread for the guard's lifetime.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| CaptureError::Wasapi(format!("device enumerator: {e}")))?;

    // Loopback runs on the *render* endpoint: the LOOPBACK flag redirects the capture
    // stream to that endpoint's output mix.
    let flow = match source {
        AudioSource::SinkMonitor => eRender,
        AudioSource::Microphone => eCapture,
    };
    // SAFETY: FFI.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }
        .map_err(|e| CaptureError::Wasapi(format!("no default endpoint: {e}")))?;
    // SAFETY: FFI.
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| CaptureError::Wasapi(format!("activate audio client: {e}")))?;

    let block_align = params.channels as u16 * (size_of::<f32>() as u16);
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: params.channels as u16,
        nSamplesPerSec: params.sample_rate,
        nAvgBytesPerSec: params.sample_rate * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: (size_of::<f32>() as u16) * 8,
        cbSize: 0,
    };
    let mut stream_flags =
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if source == AudioSource::SinkMonitor {
        stream_flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    // 200 ms of engine-side buffering (in 100 ns units): far more than the poll cadence
    // needs, cheap in RAM, and forgiving of a briefly stalled consumer thread.
    const BUFFER_DURATION_HNS: i64 = 2_000_000;
    // SAFETY: FFI; `format` lives across the call.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            stream_flags,
            BUFFER_DURATION_HNS,
            0,
            &format,
            None,
        )
    }
    .map_err(|e| CaptureError::Wasapi(format!("initialize audio client: {e}")))?;

    // SAFETY: FFI.
    let capture: IAudioCaptureClient = unsafe { client.GetService() }
        .map_err(|e| CaptureError::Wasapi(format!("capture client: {e}")))?;
    // SAFETY: FFI.
    unsafe { client.Start() }.map_err(|e| CaptureError::Wasapi(format!("start stream: {e}")))?;
    tracing::info!(
        ?source,
        rate = params.sample_rate,
        channels = params.channels,
        "WASAPI capture started"
    );

    let channels = params.channels as usize;
    let mut silence: Vec<f32> = Vec::new();
    let mut last_packet = Instant::now();
    // A continuous per-stream clock: packets drained in one poll burst arrive
    // microseconds apart while each represents ~10 ms of audio, so stamping them
    // with the dequeue time would make the mixer sum them partially on top of each
    // other — audible crackle that grows with amplitude. Instead the first packet
    // anchors on the wall clock and every next PTS advances by the frames
    // delivered; a real delivery gap (idle loopback, engine discontinuity)
    // re-anchors so the stream clock can't drift from the shared epoch.
    let mut stream_clock: Option<Duration> = None;
    let result = 'run: loop {
        if stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Relaxed))
        {
            break 'run Ok(());
        }
        if idle_timeout.is_some_and(|limit| last_packet.elapsed() > limit) {
            break 'run Err(CaptureError::Wasapi(format!(
                "no audio packet within the idle timeout ({idle_timeout:?})"
            )));
        }

        // Drain every ready packet before sleeping again.
        loop {
            // SAFETY: FFI.
            let ready = match unsafe { capture.GetNextPacketSize() } {
                Ok(frames) => frames,
                Err(e) => {
                    break 'run Err(CaptureError::Wasapi(format!("GetNextPacketSize: {e}")));
                }
            };
            if ready == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            // SAFETY: FFI; out-params receive the packet.
            if let Err(e) =
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
            {
                break 'run Err(CaptureError::Wasapi(format!("GetBuffer: {e}")));
            }
            last_packet = Instant::now();
            let wall = epoch.elapsed();
            let discontinuity = flags & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0;
            let pts = match stream_clock {
                Some(clock) if !discontinuity && clock.abs_diff(wall) < REANCHOR_DRIFT => clock,
                _ => wall,
            };
            stream_clock = Some(
                pts + Duration::from_nanos(
                    u64::from(frames) * 1_000_000_000 / u64::from(params.sample_rate),
                ),
            );
            let samples = frames as usize * channels;
            let action = if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                // The spec says to treat the buffer as silence regardless of content.
                silence.clear();
                silence.resize(samples, 0.0);
                on_samples(&silence, pts)
            } else {
                // SAFETY: the engine hands `frames` frames of our f32 format starting
                // at `data`, valid until `ReleaseBuffer`.
                let pcm = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), samples) };
                on_samples(pcm, pts)
            };
            // SAFETY: FFI; pairs the successful GetBuffer.
            if let Err(e) = unsafe { capture.ReleaseBuffer(frames) } {
                break 'run Err(CaptureError::Wasapi(format!("ReleaseBuffer: {e}")));
            }
            if action.is_break() {
                break 'run Ok(());
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    };

    // SAFETY: FFI; best-effort stop before the client drops.
    let _ = unsafe { client.Stop() };
    result
}
