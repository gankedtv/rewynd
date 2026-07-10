//! SCK display stream → IOSurface-backed NV12 `CVPixelBuffer`s (docs/adr/0015).
//!
//! Mirrors the shape of the other backends ([`crate::windows`]' WGC stream in
//! particular): a blocking call driving a per-frame callback with a cooperative
//! stop flag, parked in the shared watchdog while SCK delivers on its own serial
//! dispatch queue — hence the `Send` bound on the callback. SCK scales and
//! converts server-side, so frames arrive already at the requested size in
//! `420v` (NV12 video-range), ready for VideoToolbox without a conversion pass.

use std::ops::ControlFlow;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use cidre::sc::StreamOutput;
use cidre::{arc, cg, cm, cv, define_obj_type, dispatch, ns, objc, sc};

use super::{
    SessionShared, StreamErrDelegate, StreamErrDelegateInner, ensure_screen_capture_access,
    select_display, session_result, shareable_content, watch_session,
};
use crate::{CaptureError, StreamPrefs};

/// SCK's capture-surface pool size (valid 3–8). 5 is what Apple's 4K60 game
/// capture example uses: deep enough that the encoder consuming a frame doesn't
/// stall the pool, without holding excess IOSurface memory.
const QUEUE_DEPTH: isize = 5;

/// A single captured frame: a retained NV12 `CVPixelBuffer` from SCK's surface
/// pool.
///
/// The buffer comes from a fixed pool of [`QUEUE_DEPTH`] IOSurfaces; holding
/// retained frames starves the pool and SCK starts dropping new frames, so hand
/// it to the encoder and drop it promptly.
pub struct CapturedPixelBuf {
    /// The IOSurface-backed pixel buffer (NV12 video-range), retained.
    pub pixel_buf: arc::R<cv::PixelBuf>,
    pub width: u32,
    pub height: u32,
    /// Monotonic capture time relative to the stream epoch, strictly increasing
    /// (the VideoToolbox encoder rejects non-increasing timestamps).
    pub pts: Duration,
}

#[repr(C)]
struct VideoOutputInner {
    shared: Arc<SessionShared>,
    on_frame: Box<dyn FnMut(CapturedPixelBuf) -> ControlFlow<()> + Send>,
    epoch: Instant,
    last_pts: Option<Duration>,
    frames: u64,
}

impl VideoOutputInner {
    fn handle(&mut self, sample_buf: &mut cm::SampleBuf) {
        if self.shared.should_stop() {
            return;
        }
        // Only Complete buffers carry a new frame; SCK also emits status-only
        // buffers (Idle/Blank/Started/...) that must not reach the encoder.
        if frame_status(sample_buf) != Some(sc::FrameStatus::Complete as i64) {
            return;
        }
        let Some(image) = sample_buf.image_buf() else {
            return;
        };
        let width = image.width() as u32;
        let height = image.height() as u32;
        if width == 0 || height == 0 {
            return;
        }

        let mut pts = self.epoch.elapsed();
        if let Some(last) = self.last_pts
            && pts <= last
        {
            pts = last + Duration::from_micros(1);
        }
        self.last_pts = Some(pts);

        if self.frames == 0 {
            tracing::debug!(width, height, "first SCK frame delivered");
        }
        self.frames += 1;

        let frame = CapturedPixelBuf {
            pixel_buf: image.retained(),
            width,
            height,
            pts,
        };
        // The callback must not unwind across the ObjC/dispatch boundary.
        match catch_unwind(AssertUnwindSafe(|| (self.on_frame)(frame))) {
            Ok(ControlFlow::Continue(())) => {}
            Ok(ControlFlow::Break(())) => self.shared.finish(),
            Err(_) => self.shared.fail("frame callback panicked".to_owned()),
        }
    }
}

define_obj_type!(
    VideoOutput + sc::StreamOutputImpl,
    VideoOutputInner,
    REWYND_SCK_VIDEO_OUTPUT
);

impl StreamOutput for VideoOutput {}

#[objc::add_methods]
impl sc::StreamOutputImpl for VideoOutput {
    extern "C" fn impl_stream_did_output_sample_buf(
        &mut self,
        _cmd: Option<&objc::Sel>,
        _stream: &sc::Stream,
        sample_buf: &mut cm::SampleBuf,
        kind: sc::OutputType,
    ) {
        if kind == sc::OutputType::Screen {
            self.inner_mut().handle(sample_buf);
        }
    }
}

/// The `SCStreamFrameInfoStatus` attachment as its raw [`sc::FrameStatus`] value.
fn frame_status(sample_buf: &cm::SampleBuf) -> Option<i64> {
    let attaches = sample_buf.attaches(false)?;
    if attaches.is_empty() {
        return None;
    }
    attaches[0]
        .get(sc::FrameInfo::status().as_cf())?
        .try_as_number()?
        .to_i64()
}

/// Capture a continuous stream of frames from a display, handing each to
/// `on_frame` as a retained NV12 pixel buffer ([`CapturedPixelBuf`]).
///
/// `display_index` selects the display (one-based, per SCK's display list, the
/// Windows monitor convention); `None` captures the main display. SCK scales to
/// `prefs.width`/`height` server-side and `prefs.framerate` caps delivery via the
/// minimum frame interval — like the other backends it is damage-driven, so a
/// static screen delivers nothing.
///
/// `on_frame` returns [`ControlFlow::Continue`] to keep receiving frames or
/// [`ControlFlow::Break`] to stop the stream (a successful, deliberate end). It
/// runs on a serial dispatch queue owned by SCK, hence the `Send` bound.
///
/// Each frame's PTS is measured from `epoch`; pass the same epoch to the audio
/// capture so the tracks share one clock and the muxer can align them.
///
/// Blocks until `on_frame` breaks, the `stop` flag is set (both return `Ok`), or
/// the stream fails. The stop flag is watched off the sample path, so an idle
/// (frameless) screen can still be stopped.
pub fn capture_stream<F>(
    display_index: Option<usize>,
    epoch: Instant,
    prefs: StreamPrefs,
    stop: Option<Arc<AtomicBool>>,
    on_frame: F,
) -> Result<(), CaptureError>
where
    F: FnMut(CapturedPixelBuf) -> ControlFlow<()> + Send + 'static,
{
    // Pool for the ObjC temporaries, drained on return: the caller's thread has
    // none of its own (the delegate's dispatch queue is autorelease-pooled).
    let _pool = objc::AutoreleasePoolPage::push();
    ensure_screen_capture_access()?;
    let content = shareable_content()?;
    let display = select_display(&content, display_index)?;

    let mut cfg = sc::StreamCfg::new();
    // 420v (NV12 video-range): encode-ready, converted hardware-side by SCK.
    cfg.set_pixel_format(cv::PixelFormat::_420V);
    cfg.set_width(prefs.width as usize);
    cfg.set_height(prefs.height as usize);
    if prefs.framerate > 0 {
        cfg.set_minimum_frame_interval(cm::Time::new(1, prefs.framerate as i32));
    }
    cfg.set_queue_depth(QUEUE_DEPTH);
    cfg.set_shows_cursor(true);
    cfg.set_captures_audio(false);
    // BT.709 to match the H.264 stream's color metadata.
    cfg.set_color_matrix(cg::DisplayStreamYCbCrMatrix::itu_r_709_2());

    let shared = SessionShared::new();
    let delegate = StreamErrDelegate::with(StreamErrDelegateInner {
        shared: shared.clone(),
    });
    let output = VideoOutput::with(VideoOutputInner {
        shared: shared.clone(),
        on_frame: Box::new(on_frame),
        epoch,
        last_pts: None,
        frames: 0,
    });

    let windows = ns::Array::new();
    let filter = sc::ContentFilter::with_display_excluding_windows(&display, &windows);
    let stream = sc::Stream::with_delegate(&filter, &cfg, delegate.as_ref());
    let queue = dispatch::Queue::serial_with_ar_pool();
    stream
        .add_stream_output(output.as_ref(), sc::OutputType::Screen, Some(&queue))
        .map_err(|e| CaptureError::Sck(format!("add video output: {e}")))?;
    pollster::block_on(stream.start())
        .map_err(|e| CaptureError::Sck(format!("start stream: {e}")))?;
    tracing::info!(
        width = prefs.width,
        height = prefs.height,
        framerate = prefs.framerate,
        "SCK display capture started"
    );

    watch_session(&shared, stop.as_ref(), None);
    // A failed/already-stopped stream also errors its stop call; nothing to act on.
    if let Err(e) = pollster::block_on(stream.stop()) {
        tracing::debug!(error = %e, "SCK stream stop reported an error");
    }
    session_result(&shared)
}
