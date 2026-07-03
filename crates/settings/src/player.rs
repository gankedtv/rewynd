//! In-app preview playback of a clip's kept trim range: decode on a dedicated blocking thread
//! (from the enclosing keyframe, exactly like the lossless cut), pace frames to their
//! presentation times, and hand downscaled frames to the UI as a stream. Video only; the hint in
//! the UI points at a real player for sound. Dropping the stream (the subscription) cancels the
//! decode thread through its dead channel.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iced::futures::Stream;
use iced::widget::image::Handle;

use crate::thumbs;

/// What the playback stream emits: a frame with its position in the clip, or the end of the
/// range.
#[derive(Debug, Clone)]
pub enum Event {
    Frame(Handle, Duration),
    Ended,
}

/// Frames cross to the UI at this width (the preview pane is ~470 logical px; 2x for hidpi).
/// The scrub preview reuses it so paused and playing frames match.
pub const PREVIEW_WIDTH: u32 = 960;

/// A frame later than this against the clock is dropped instead of shown, so playback skips
/// rather than drifts when decode can't keep up.
const LATE_BUDGET: Duration = Duration::from_millis(50);

/// Stream the `[start, end]` range of the clip at `path` as paced [`Event`]s.
pub fn stream(path: PathBuf, start: Duration, end: Duration) -> impl Stream<Item = Event> {
    iced::stream::channel(
        4,
        move |mut output: iced::futures::channel::mpsc::Sender<Event>| async move {
            use iced::futures::SinkExt;

            let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(2);
            std::thread::spawn(move || {
                decode_loop(&path, start, end, &tx);
                let _ = tx.blocking_send(Event::Ended);
            });
            while let Some(event) = rx.recv().await {
                if output.send(event).await.is_err() {
                    // The subscription was dropped; the dead channel stops the decode thread.
                    return;
                }
            }
        },
    )
}

/// Decode from the keyframe at/before `start` through `end`, sending paced frames until the
/// range ends or the receiver goes away.
fn decode_loop(path: &Path, start: Duration, end: Duration, tx: &tokio::sync::mpsc::Sender<Event>) {
    let Ok(frames) = rewynd_mux::read::video_frames_from(path, start) else {
        return;
    };
    let Ok(mut decoder) = openh264::decoder::Decoder::new() else {
        return;
    };
    // openh264 may buffer a feed before emitting; pair outputs with their timestamps in order
    // (the recorder writes no B-frames, so decode order is presentation order).
    let mut pending: VecDeque<Duration> = VecDeque::new();
    let mut clock: Option<(Instant, Duration)> = None;
    for frame in frames {
        if frame.pts > end {
            break;
        }
        pending.push_back(frame.pts);
        match decoder.decode(&frame.annexb) {
            Ok(Some(yuv)) => {
                let Some(pts) = pending.pop_front() else {
                    break;
                };
                if !emit(&yuv, pts, start, &mut clock, tx) {
                    return;
                }
            }
            Ok(None) => {}
            Err(_) => return,
        }
    }
    let Ok(remaining) = decoder.flush_remaining() else {
        return;
    };
    for yuv in &remaining {
        let Some(pts) = pending.pop_front() else {
            break;
        };
        if pts > end {
            break;
        }
        if !emit(yuv, pts, start, &mut clock, tx) {
            return;
        }
    }
}

/// Show one decoded frame: skip lead-in frames before `start` (decoded only for the reference
/// chain), sleep until the frame is due (the clock anchors on the first shown frame), drop it
/// when hopelessly late, else downscale and send. Returns `false` when the receiver is gone.
fn emit(
    yuv: &openh264::decoder::DecodedYUV<'_>,
    pts: Duration,
    start: Duration,
    clock: &mut Option<(Instant, Duration)>,
    tx: &tokio::sync::mpsc::Sender<Event>,
) -> bool {
    if pts < start {
        return true;
    }
    let (epoch, base) = *clock.get_or_insert_with(|| (Instant::now(), pts));
    let due = epoch + (pts - base);
    let now = Instant::now();
    if now < due {
        std::thread::sleep(due - now);
    } else if now > due + LATE_BUDGET {
        return true;
    }
    let (width, height, rgb) = thumbs::rgb_of(yuv);
    let Some(image) = image::RgbImage::from_raw(width, height, rgb) else {
        return true;
    };
    let (tw, th) = thumbs::scaled_dims(width, height, PREVIEW_WIDTH);
    let small = image::imageops::thumbnail(&image, tw, th);
    let rgba = image::DynamicImage::ImageRgb8(small).into_rgba8();
    tx.blocking_send(Event::Frame(
        Handle::from_rgba(tw, th, rgba.into_raw()),
        pts,
    ))
    .is_ok()
}
