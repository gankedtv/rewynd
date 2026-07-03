//! In-app preview playback of a clip's kept trim range, decoded by a spawned `ffmpeg`.
//!
//! The bundled openh264 decoder only speaks Constrained Baseline, and the recorder's NVENC
//! stream is High profile: keyframes happen to decode (thumbnails, scrubbing) but delta frames
//! do not, so full playback needs a real decoder. ffmpeg is spawned per play with a raw RGBA
//! pipe for video and an f32 PCM pipe for audio (played through rodio); when it is not
//! installed, the stream reports that and the UI points at the system player instead. Dropping
//! the stream (the subscription) cancels playback: the dead channel stops the reader thread,
//! which kills the children.

use std::io::Read;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Frames cross to the UI at this width in the normal detail pane (~470 logical px; 2x for
/// hidpi). The scrub preview reuses it so paused and playing frames match.
pub const PREVIEW_WIDTH: u32 = 960;

/// Decode width while the preview is fullscreen.
pub const FULLSCREEN_WIDTH: u32 = 1920;

/// The preview's fixed frame rate. The recorder's capture is damage-driven (variable rate, with
/// bursts far above the display rate), so ffmpeg resamples to constant-rate output; that also
/// makes frame index -> position exact.
const PREVIEW_FPS: u32 = 60;

/// A frame later than this against the clock re-anchors it instead of being shown late, so
/// playback slows down rather than freezing or drifting when decode can't keep up.
const LATE_BUDGET: Duration = Duration::from_millis(80);

/// Preview audio format: what ffmpeg is asked to emit and what the sink is fed.
const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u16 = 2;

/// How far ahead of the wall clock audio may be queued into the sink. Small enough that a
/// pause stops sound promptly-ish while the queue drains... it does not: pause tears the sink
/// down, so this only bounds memory.
const AUDIO_LEAD: Duration = Duration::from_secs(1);

/// Stream the `[start, end]` range of the clip at `path` as paced [`Event`]s, decoding video
/// at most `width` px wide.
pub fn stream(
    path: PathBuf,
    start: Duration,
    end: Duration,
    width: u32,
) -> impl Stream<Item = Event> {
    iced::stream::channel(
        4,
        move |mut output: iced::futures::channel::mpsc::Sender<Event>| async move {
            use iced::futures::SinkExt;

            let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(2);
            std::thread::spawn(move || {
                // A panic mid-decode must still end the playback state machine: without a
                // terminal event the UI would stay in "playing" forever.
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decode_loop(&path, start, end, width, &tx);
                }));
                if run.is_err() {
                    tracing::error!("preview decode panicked; ending playback");
                    let _ = tx.blocking_send(Event::Ended);
                }
            });
            while let Some(event) = rx.recv().await {
                if output.send(event).await.is_err() {
                    // The subscription was dropped; the dead channel stops the reader thread.
                    return;
                }
            }
        },
    )
}

/// Decode the range with spawned ffmpeg processes (video paced here, audio on its own thread)
/// and forward frames until the range ends or the receiver goes away. Always terminates the
/// children before returning.
fn decode_loop(
    path: &Path,
    start: Duration,
    end: Duration,
    width: u32,
    tx: &tokio::sync::mpsc::Sender<Event>,
) {
    let Ok(summary) = rewynd_mux::read::clip_summary(path) else {
        let _ = tx.blocking_send(Event::Ended);
        return;
    };
    let (width, height) = thumbs::scaled_dims(summary.width, summary.height, width);
    // Raw RGBA frames carry no timestamps; the forced constant output rate defines them.
    let frame_time = Duration::from_secs(1) / PREVIEW_FPS;

    let mut child = match spawn_video_ffmpeg(path, start, end, width, height) {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!(error = %e, "ffmpeg unavailable; no in-app playback");
            let _ = tx.blocking_send(Event::Unavailable);
            return;
        }
    };
    // Audio rides on its own thread and clock; the stop flag ends it when video ends or the
    // subscription is dropped. A clip without an audio track just plays silent.
    let stop = Arc::new(AtomicBool::new(false));
    let audio = std::thread::spawn({
        let path = path.to_path_buf();
        let stop = Arc::clone(&stop);
        move || audio_loop(&path, start, end, &stop)
    });

    let mut natural_end = false;
    if let Some(stdout) = child.stdout.take() {
        natural_end = read_frames(stdout, start, end, width, height, frame_time, tx);
    }
    let _ = child.kill();
    let _ = child.wait();
    // On a natural end the audio thread drains its queued tail before joining; a cancelled
    // playback (pause, subscription dropped) cuts it immediately.
    if !natural_end {
        stop.store(true, Ordering::Relaxed);
    }
    let _ = audio.join();
    let _ = tx.blocking_send(Event::Ended);
}

/// ffmpeg decoding `[start, end]` of the clip to raw RGBA frames of `width`x`height` on stdout.
/// `-ss` before `-i` seeks to the enclosing keyframe, the same granularity as the trim itself.
fn spawn_video_ffmpeg(
    path: &Path,
    start: Duration,
    end: Duration,
    width: u32,
    height: u32,
) -> std::io::Result<Child> {
    let mut command = ffmpeg_range(path, start, end);
    command
        .args(["-vf", &format!("scale={width}:{height}")])
        .args(["-r", &PREVIEW_FPS.to_string()])
        .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg("pipe:1");
    command.spawn()
}

/// ffmpeg decoding the same range's default audio track (the recorder's mix) to interleaved
/// f32 PCM on stdout.
fn spawn_audio_ffmpeg(path: &Path, start: Duration, end: Duration) -> std::io::Result<Child> {
    let mut command = ffmpeg_range(path, start, end);
    command
        .arg("-vn")
        .args(["-f", "f32le"])
        .args(["-ac", &AUDIO_CHANNELS.to_string()])
        .args(["-ar", &AUDIO_RATE.to_string()])
        .arg("pipe:1");
    command.spawn()
}

/// The shared ffmpeg invocation prefix for one `[start, end]` range of a clip.
fn ffmpeg_range(path: &Path, start: Duration, end: Duration) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .args(["-loglevel", "error", "-nostdin"])
        .args(["-ss", &format!("{:.3}", start.as_secs_f64())])
        .arg("-i")
        .arg(path)
        .args([
            "-t",
            &format!("{:.3}", (end - start).as_secs_f64().max(0.0)),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

/// Read raw frames off the pipe and send them paced to their nominal times. Returns `true`
/// when the pipe ended naturally (range done or ffmpeg died), `false` when the receiver is
/// gone (playback was cancelled).
fn read_frames(
    mut stdout: impl Read,
    start: Duration,
    end: Duration,
    width: u32,
    height: u32,
    frame_time: Duration,
    tx: &tokio::sync::mpsc::Sender<Event>,
) -> bool {
    let frame_bytes = width as usize * height as usize * 4;
    let mut buffer = vec![0u8; frame_bytes];
    let mut clock: Option<(Instant, Duration)> = None;
    for index in 0u32.. {
        if stdout.read_exact(&mut buffer).is_err() {
            return true;
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
            return false;
        }
    }
    true
}

/// Decode the range's audio and feed it to the default output device until the pipe ends or
/// `stop` is raised. Queues at most [`AUDIO_LEAD`] ahead of the wall clock; rodio's own clock
/// plays it out. Any failure (no audio track, no output device) just means a silent preview.
fn audio_loop(path: &Path, start: Duration, end: Duration, stop: &AtomicBool) {
    let Ok(mut child) = spawn_audio_ffmpeg(path, start, end) else {
        return;
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let (channels, rate) = (
        NonZero::new(AUDIO_CHANNELS).expect("nonzero"),
        NonZero::new(AUDIO_RATE).expect("nonzero"),
    );
    // The sink owns the device stream; it must outlive the player. Failure to open one (no
    // sound server) silently skips audio.
    let Ok(mut sink) = rodio::DeviceSinkBuilder::open_default_sink() else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    sink.log_on_drop(false);
    let player = rodio::Player::connect_new(sink.mixer());

    // The clock anchors on the first appended chunk, so decoder startup does not count as
    // played time.
    let mut started: Option<Instant> = None;
    let mut queued = Duration::ZERO;
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 32 * 1024];
    while !stop.load(Ordering::Relaxed) {
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&chunk[..n]);
        // Whole interleaved sample frames only; a partial one carries to the next read.
        let usable = pending.len() - pending.len() % (4 * AUDIO_CHANNELS as usize);
        if usable == 0 {
            continue;
        }
        let samples: Vec<f32> = pending[..usable]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        pending.drain(..usable);
        queued += Duration::from_secs_f64(
            samples.len() as f64 / f64::from(AUDIO_CHANNELS) / f64::from(AUDIO_RATE),
        );
        player.append(rodio::buffer::SamplesBuffer::new(channels, rate, samples));
        let epoch = *started.get_or_insert_with(Instant::now);
        while !stop.load(Ordering::Relaxed) && queued > epoch.elapsed() + AUDIO_LEAD {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    // Natural end of the pipe: the sink still holds up to [`AUDIO_LEAD`] of unplayed sound
    // (decode runs ahead of the clock), so let it drain instead of chopping the tail. A stop
    // request (pause, subscription dropped) still cuts immediately.
    if let Some(epoch) = started {
        while !stop.load(Ordering::Relaxed) && epoch.elapsed() < queued {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    player.stop();
    let _ = child.kill();
    let _ = child.wait();
}

/// Per-bucket audio peaks (`0.0..=1.0`) across the whole clip, decoded by ffmpeg at a low
/// mono rate, for the timeline's waveform lane. `None` when there is no audio (or no ffmpeg).
pub fn waveform(path: &Path, buckets: usize) -> Option<Vec<f32>> {
    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .args(["-loglevel", "error", "-nostdin"])
        .arg("-i")
        .arg(path)
        .arg("-vn")
        .args(["-f", "f32le", "-ac", "1", "-ar", "8000"])
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()?
        .read_to_end(&mut bytes)
        .map(|_| ())
        .ok();
    let _ = child.wait();
    read?;

    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs())
        .collect();
    if samples.is_empty() || buckets == 0 {
        return None;
    }
    let per = samples.len().div_ceil(buckets);
    let peaks: Vec<f32> = samples
        .chunks(per)
        .map(|bucket| bucket.iter().copied().fold(0.0, f32::max))
        .collect();
    let max = peaks.iter().copied().fold(0.0, f32::max);
    if max <= f32::EPSILON {
        return None;
    }
    Some(peaks.iter().map(|p| p / max).collect())
}
