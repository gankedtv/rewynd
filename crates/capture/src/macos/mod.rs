//! macOS capture: ScreenCaptureKit streams via `cidre` (docs/adr/0015).
//!
//! - [`video`]: an SCK display stream delivering IOSurface-backed NV12
//!   `CVPixelBuffer`s straight to the per-frame callback (no GPU import layer —
//!   VideoToolbox consumes them directly).
//! - [`audio`]: system-audio loopback (`capturesAudio`) and microphone
//!   (`captureMicrophone`) streams, converted to interleaved f32 PCM.
//! - [`focus`]: an NSWorkspace/CGWindowList poller for game detection.
//!
//! All streams share the plumbing here: the TCC screen-recording preflight, the
//! shareable-content/display lookup, the stream delegate that records failures,
//! and the watchdog the blocking entry points park in (SCK delivers on its own
//! dispatch queues; the callers' threads just poll the stop/done/error flags).

// cidre's `define_obj_type!` expands to reference-to-pointer transmutes clippy
// flags at every invocation site (attributes on macro calls are ignored, so the
// allow has to sit here for the whole module tree).
#![allow(clippy::useless_transmute)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cidre::sc::StreamDelegate;
use cidre::{arc, cg, define_obj_type, ns, objc, sc};

use crate::CaptureError;

pub mod audio;
pub mod focus;
pub mod video;

pub use audio::capture_audio;
pub use focus::{FocusCallback, FocusError, FocusWatcher};
pub use video::{CapturedPixelBuf, capture_stream};

/// How often the watchdog polls the cooperative stop flag. SCK delivers no frames
/// while the screen is static, so the flag must be observed off the sample path.
pub(crate) const STOP_POLL: Duration = Duration::from_millis(200);

/// State shared between a blocking `capture_*` call and the ObjC delegate/output
/// objects SCK drives on its dispatch queue.
pub(crate) struct SessionShared {
    /// A deliberate, successful end: callback `Break` or the stop flag.
    done: AtomicBool,
    /// The stream failed; `error` holds the first recorded message.
    failed: AtomicBool,
    error: Mutex<Option<String>>,
    /// At least one sample was delivered (feeds the audio idle timeout).
    got_sample: AtomicBool,
}

impl SessionShared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            done: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
            got_sample: AtomicBool::new(false),
        })
    }

    pub(crate) fn finish(&self) {
        self.done.store(true, Ordering::Relaxed);
    }

    pub(crate) fn fail(&self, msg: String) {
        let mut error = self
            .error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if error.is_none() {
            *error = Some(msg);
        }
        drop(error);
        self.failed.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Whether the sample handlers should stop doing work (either outcome).
    pub(crate) fn should_stop(&self) -> bool {
        self.is_done() || self.is_failed()
    }

    pub(crate) fn mark_sample(&self) {
        self.got_sample.store(true, Ordering::Relaxed);
    }

    pub(crate) fn got_sample(&self) -> bool {
        self.got_sample.load(Ordering::Relaxed)
    }

    fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

#[repr(C)]
pub(crate) struct StreamErrDelegateInner {
    pub(crate) shared: Arc<SessionShared>,
}

define_obj_type!(
    pub(crate) StreamErrDelegate + sc::StreamDelegateImpl,
    StreamErrDelegateInner,
    REWYND_SCK_STREAM_DELEGATE
);

impl StreamDelegate for StreamErrDelegate {}

#[objc::add_methods]
impl sc::StreamDelegateImpl for StreamErrDelegate {
    extern "C" fn impl_stream_did_stop_with_err(
        &mut self,
        _cmd: Option<&objc::Sel>,
        _stream: &sc::Stream,
        error: &ns::Error,
    ) {
        tracing::debug!("SCK stream reported an error");
        self.inner_mut()
            .shared
            .fail(format!("stream stopped: {error}"));
    }

    extern "C" fn impl_user_did_stop_stream(
        &mut self,
        _cmd: Option<&objc::Sel>,
        _stream: &sc::Stream,
    ) {
        tracing::debug!("SCK stream stopped from the system UI");
        self.inner_mut()
            .shared
            .fail("stream stopped from the system UI".to_owned());
    }
}

/// Verify (and, once, request) the Screen Recording TCC grant. SCK enumeration
/// returns empty/erroring content without it, so failing here names the fix.
pub(crate) fn ensure_screen_capture_access() -> Result<(), CaptureError> {
    if cg::screen_capture_access::preflight() {
        return Ok(());
    }
    tracing::info!("screen recording permission missing; requesting it");
    if cg::screen_capture_access::request() {
        return Ok(());
    }
    Err(CaptureError::Sck(
        "screen recording permission not granted; allow rewynd under System Settings → \
         Privacy & Security → Screen & System Audio Recording, then relaunch"
            .to_owned(),
    ))
}

/// Enumerate the shareable content (displays/windows/apps) SCK will capture from.
pub(crate) fn shareable_content() -> Result<arc::R<sc::ShareableContent>, CaptureError> {
    pollster::block_on(sc::ShareableContent::current())
        .map_err(|e| CaptureError::Sck(format!("shareable content: {e}")))
}

/// Pick the capture display: `index` is one-based into SCK's display list
/// (mirroring the Windows monitor convention); `None` prefers the main display.
pub(crate) fn select_display(
    content: &sc::ShareableContent,
    index: Option<usize>,
) -> Result<arc::R<sc::Display>, CaptureError> {
    let displays = content.displays();
    if displays.is_empty() {
        return Err(CaptureError::Sck("no shareable displays found".to_owned()));
    }
    let slot = match index {
        None => {
            let main = cg::DirectDisplayId::main();
            displays
                .iter()
                .position(|d| d.display_id() == main)
                .unwrap_or(0)
        }
        Some(0) => {
            return Err(CaptureError::Sck(
                "display index is one-based (got 0)".to_owned(),
            ));
        }
        Some(n) if n > displays.len() => {
            return Err(CaptureError::Sck(format!(
                "display index {n} out of range ({} displays)",
                displays.len()
            )));
        }
        Some(n) => n - 1,
    };
    displays
        .get(slot)
        .map_err(|e| CaptureError::Sck(format!("display {slot}: {e}")))
}

/// Park the calling thread until the session ends: callback `Break`, the stop
/// flag, a recorded stream failure, or (audio) an idle timeout with no samples.
pub(crate) fn watch_session(
    shared: &SessionShared,
    stop: Option<&Arc<AtomicBool>>,
    idle_timeout: Option<Duration>,
) {
    let start = Instant::now();
    loop {
        if shared.should_stop() {
            return;
        }
        if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            tracing::debug!("stop flag observed; ending SCK capture");
            shared.finish();
            return;
        }
        if let Some(limit) = idle_timeout
            && !shared.got_sample()
            && start.elapsed() > limit
        {
            shared.fail(format!(
                "no audio delivered within the idle timeout ({limit:?})"
            ));
            return;
        }
        std::thread::sleep(STOP_POLL);
    }
}

/// The blocking entry points' verdict once the watchdog returns and the stream
/// was stopped: `Ok` on a deliberate end, the recorded failure otherwise.
pub(crate) fn session_result(shared: &SessionShared) -> Result<(), CaptureError> {
    if shared.is_done() {
        Ok(())
    } else {
        Err(CaptureError::Sck(shared.take_error().unwrap_or_else(
            || "capture ended without a stop request".to_owned(),
        )))
    }
}
