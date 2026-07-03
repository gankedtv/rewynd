//! In-app preview playback of a clip's kept trim range, decoded by a spawned `ffmpeg`.
//!
//! The bundled openh264 decoder only speaks Constrained Baseline, and the recorder's NVENC
//! stream is High profile: keyframes happen to decode (thumbnails, scrubbing) but delta frames
//! do not, so full playback needs a real decoder. ffmpeg is spawned per play with a raw RGBA
//! pipe; when it is not installed, the stream reports that and the UI points at the system
//! player instead. Video only. Dropping the stream (the subscription) cancels playback: the
//! dead channel stops the reader thread, which kills the child.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use iced::futures::Stream;
use iced::widget::image::Handle;

use crate::thumbs;

/// What the playback stream emits.
#[derive(Debug, Clone)]
pub enum Event {
    /// The next frame, with its position in the clip.
    Frame(Handle, Duration),
    /// No usable decoder on this machine (ffmpeg missing or broken).
    Unavailable,
    /// The end of the kept range (or a decode failure ended playback early).
    Ended,
}

/// Frames cross to the UI at this width (the preview pane is ~470 logical px; 2x for hidpi).
/// The scrub preview reuses it so paused and playing frames match.
pub const PREVIEW_WIDTH: u32 = 960;

/// The preview's fixed frame rate. The recorder's capture is damage-driven (variable rate, with
/// bursts far above the display rate), so ffmpeg resamples to constant-rate output; that also
/// makes frame index -> position exact.
const PREVIEW_FPS: u32 = 60;

/// A frame later than this against the clock re-anchors it instead of being shown late, so
/// playback slows down rather than freezing or drifting when decode can't keep up.
const LATE_BUDGET: Duration = Duration::from_millis(80);

/// Stream the `[start, end]` range of the clip at `path` as paced [`Event`]s.
pub fn stream(path: PathBuf, start: Duration, end: Duration) -> impl Stream<Item = Event> {
    iced::stream::channel(
        4,
        move |mut output: iced::futures::channel::mpsc::Sender<Event>| async move {
            use iced::futures::SinkExt;

            let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(2);
            std::thread::spawn(move || decode_loop(&path, start, end, &tx));
            while let Some(event) = rx.recv().await {
                if output.send(event).await.is_err() {
                    // The subscription was dropped; the dead channel stops the reader thread.
                    return;
                }
            }
        },
    )
}

/// Decode the range with a spawned ffmpeg and forward paced frames until the range ends or the
/// receiver goes away. Always terminates the child process before returning.
fn decode_loop(path: &Path, start: Duration, end: Duration, tx: &tokio::sync::mpsc::Sender<Event>) {
    let Ok(summary) = rewynd_mux::read::clip_summary(path) else {
        let _ = tx.blocking_send(Event::Ended);
        return;
    };
    let (width, height) = thumbs::scaled_dims(summary.width, summary.height, PREVIEW_WIDTH);
    // Raw RGBA frames carry no timestamps; the forced constant output rate defines them.
    let frame_time = Duration::from_secs(1) / PREVIEW_FPS;

    let mut child = match spawn_ffmpeg(path, start, end, width, height) {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!(error = %e, "ffmpeg unavailable; no in-app playback");
            let _ = tx.blocking_send(Event::Unavailable);
            return;
        }
    };
    if let Some(stdout) = child.stdout.take() {
        read_frames(stdout, start, end, width, height, frame_time, tx);
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = tx.blocking_send(Event::Ended);
}

/// ffmpeg decoding `[start, end]` of the clip to raw RGBA frames of `width`x`height` on stdout.
/// `-ss` before `-i` seeks to the enclosing keyframe, the same granularity as the trim itself.
fn spawn_ffmpeg(
    path: &Path,
    start: Duration,
    end: Duration,
    width: u32,
    height: u32,
) -> std::io::Result<Child> {
    Command::new("ffmpeg")
        .arg("-hide_banner")
        .args(["-loglevel", "error", "-nostdin"])
        .args(["-ss", &format!("{:.3}", start.as_secs_f64())])
        .arg("-i")
        .arg(path)
        .args([
            "-t",
            &format!("{:.3}", (end - start).as_secs_f64().max(0.0)),
        ])
        .args(["-vf", &format!("scale={width}:{height}")])
        .args(["-r", &PREVIEW_FPS.to_string()])
        .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// Read raw frames off the pipe and send them paced to their nominal times. Returns when the
/// pipe ends (range done or ffmpeg died) or the receiver is gone.
fn read_frames(
    mut stdout: impl Read,
    start: Duration,
    end: Duration,
    width: u32,
    height: u32,
    frame_time: Duration,
    tx: &tokio::sync::mpsc::Sender<Event>,
) {
    let frame_bytes = width as usize * height as usize * 4;
    let mut buffer = vec![0u8; frame_bytes];
    let mut clock: Option<(Instant, Duration)> = None;
    for index in 0u32.. {
        if stdout.read_exact(&mut buffer).is_err() {
            return;
        }
        let pts = (start + frame_time * index).min(end);
        let (epoch, base) = *clock.get_or_insert_with(|| (Instant::now(), pts));
        let due = epoch + (pts - base);
        let now = Instant::now();
        if now < due {
            std::thread::sleep(due - now);
        } else if now > due + LATE_BUDGET {
            // Falling behind: re-anchor so playback slows instead of skipping everything.
            clock = Some((now, pts));
        }
        let handle = Handle::from_rgba(width, height, buffer.clone());
        if tx.blocking_send(Event::Frame(handle, pts)).is_err() {
            return;
        }
    }
}
