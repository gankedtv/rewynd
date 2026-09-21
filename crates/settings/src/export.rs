//! Writing a cropped copy of a clip with the same spawned `ffmpeg` the preview uses.
//!
//! A crop cannot be a stream copy, so this is the one path in the app that re-encodes video:
//! ffmpeg cuts the kept range, crops, and encodes with libx264 (the bundled build is GPL, so
//! it has one). Audio is copied through untouched — and mapped optionally, because a clip
//! recorded with everything muted has no audio track at all and must still export.
//!
//! [`run`] blocks; [`stream`] is the same work as UI events, cancelled by dropping the stream
//! (the dead channel stops the worker, which kills ffmpeg) exactly like the preview's decode
//! loop.

use std::ffi::OsString;
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::time::Duration;

use iced::futures::Stream;

use crate::crop::PixelRect;
use crate::player;

/// Smallest crop ffmpeg will take: yuv420p subsamples chroma 2x2, so a side below this has no
/// chroma sample to encode.
const MIN_CROP: u32 = 2;

/// How much of ffmpeg's stderr a failure carries back to the UI.
const TAIL_LINES: usize = 3;
const TAIL_CHARS: usize = 400;

/// Encoder settings for a crop export. The single place these live, so a config field can slot
/// in later without touching the argv builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoQuality {
    /// libx264's constant-rate factor: lower is better quality and a bigger file.
    pub crf: u8,
    /// libx264 speed/efficiency preset.
    pub preset: &'static str,
    /// Forced H.264 profile, or `None` for libx264's own pick (High).
    pub profile: Option<&'static str>,
}

impl VideoQuality {
    /// Visually lossless for game footage at a preset fast enough not to hold the UI. The
    /// profile is left to libx264: our openh264 thumbnail decoder reads its High-profile
    /// keyframes, the same way it reads the recorder's NVENC ones.
    pub const DEFAULT: Self = Self {
        crf: 18,
        preset: "veryfast",
        profile: None,
    };
}

impl Default for VideoQuality {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One export: `src`'s `range` (all of it when `None`), cropped to `crop`, written to `dst`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub range: Option<(Duration, Duration)>,
    pub crop: PixelRect,
    pub video: VideoQuality,
}

/// What [`stream`] emits: progress, then exactly one terminal [`Event::Done`].
#[derive(Debug)]
pub enum Event {
    /// How far the export has got, `0.0..=1.0`.
    Progress(f32),
    Done(Result<(), Error>),
}

/// Why an export could not be made.
#[derive(Debug)]
pub enum Error {
    /// No ffmpeg to spawn (not bundled, not on `PATH`).
    NoFfmpeg,
    /// The crop rectangle is degenerate; nothing to encode.
    InvalidCrop,
    /// ffmpeg ran and gave up.
    Failed {
        status: Option<i32>,
        stderr_tail: String,
    },
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFfmpeg => f.write_str("Cropping needs ffmpeg installed."),
            Self::InvalidCrop => f.write_str("The crop is too small to export."),
            Self::Failed {
                status: Some(code),
                stderr_tail,
            } => write!(f, "ffmpeg failed (exit {code}): {stderr_tail}"),
            Self::Failed {
                status: None,
                stderr_tail,
            } => write!(f, "ffmpeg failed: {stderr_tail}"),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Whether a crop export can run on this machine. Playback degrades to the system player when
/// ffmpeg is missing, but a crop has no fallback, so the UI asks before offering one. Probed
/// once per run: the answer cannot change while the app is open, and the probe spawns a process.
pub fn available() -> bool {
    static PROBED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PROBED.get_or_init(player::available)
}

/// The ffmpeg argv for `spec` (no program name).
pub fn args(spec: &Spec) -> Result<Vec<OsString>, Error> {
    argv(spec, false)
}

/// Run `spec` to completion. Blocking; a failed export leaves no half-written `dst` behind.
pub fn run(spec: &Spec) -> Result<(), Error> {
    let argv = args(spec)?;
    let mut child = spawn(&argv, Stdio::null())?;
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    let status = child.wait().map_err(Error::Io)?;
    finish(status, &stderr, &spec.dst)
}

/// Run `spec` as UI events: [`Event::Progress`] against `total` (the exported length), then one
/// [`Event::Done`]. Dropping the stream cancels the export and removes the partial file.
pub fn stream(spec: Spec, total: Duration) -> impl Stream<Item = Event> + Send {
    iced::stream::channel(
        4,
        move |mut output: iced::futures::channel::mpsc::Sender<Event>| async move {
            use iced::futures::SinkExt;

            let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(4);
            std::thread::spawn(move || {
                // A panic mid-export must still produce the terminal event: without one the UI
                // would sit on its progress bar forever.
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    export_loop(&spec, total, &tx);
                }));
                if run.is_err() {
                    tracing::error!("crop export panicked");
                    let _ = tx.blocking_send(Event::Done(Err(Error::Failed {
                        status: None,
                        stderr_tail: "the export crashed".to_owned(),
                    })));
                }
            });
            while let Some(event) = rx.recv().await {
                if output.send(event).await.is_err() {
                    // The task was dropped; the dead channel stops the worker, which kills
                    // ffmpeg and clears the partial file.
                    return;
                }
            }
        },
    )
}

/// The argv, with ffmpeg's machine-readable progress on stdout when `progress`.
fn argv(spec: &Spec, progress: bool) -> Result<Vec<OsString>, Error> {
    let crop = spec.crop;
    if crop.width < MIN_CROP || crop.height < MIN_CROP {
        return Err(Error::InvalidCrop);
    }
    let mut argv = owned(&["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    if let Some((start, _)) = spec.range {
        // Before `-i`: re-encoding makes this an accurate seek rather than a keyframe-granular
        // one, so the cropped copy starts exactly where the trim says.
        argv.extend(owned(&["-ss", &seconds(start)]));
    }
    argv.push("-i".into());
    argv.push(spec.src.clone().into_os_string());
    if let Some((start, end)) = spec.range {
        // After `-i`, matching the preview's range: a duration relative to the seek.
        argv.extend(owned(&["-t", &seconds(end.saturating_sub(start))]));
    }
    argv.extend(owned(&[
        "-vf",
        &format!("crop={}:{}:{}:{}", crop.width, crop.height, crop.x, crop.y),
        // First video track, and audio only if the clip has any.
        "-map",
        "0:v:0",
        "-map",
        "0:a?",
        "-c:v",
        "libx264",
        "-crf",
        &spec.video.crf.to_string(),
        "-preset",
        spec.video.preset,
    ]));
    if let Some(profile) = spec.video.profile {
        argv.extend(owned(&["-profile:v", profile]));
    }
    argv.extend(owned(&[
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "copy",
        "-movflags",
        "+faststart",
        // Temps are written as `.tmp`, so the muxer cannot be guessed from the name.
        "-f",
        "mp4",
    ]));
    if progress {
        argv.extend(owned(&["-progress", "pipe:1", "-nostats"]));
    }
    argv.push(spec.dst.clone().into_os_string());
    Ok(argv)
}

fn owned(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn seconds(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64())
}

/// Spawn ffmpeg with `argv`: no stdin, stderr piped for the failure tail, stdout as asked.
fn spawn(argv: &[OsString], stdout: Stdio) -> Result<Child, Error> {
    let mut command = player::ffmpeg_command();
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::piped());
    player::hide_console(&mut command);
    command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::NoFfmpeg,
        _ => Error::Io(e),
    })
}

/// Spawn, pump progress, and report. Emits exactly one [`Event::Done`] unless the receiver went
/// away, which means the export was cancelled and nobody is left to tell.
fn export_loop(spec: &Spec, total: Duration, tx: &tokio::sync::mpsc::Sender<Event>) {
    let argv = match argv(spec, true) {
        Ok(argv) => argv,
        Err(e) => {
            let _ = tx.blocking_send(Event::Done(Err(e)));
            return;
        }
    };
    let mut child = match spawn(&argv, Stdio::piped()) {
        Ok(child) => child,
        Err(e) => {
            let _ = tx.blocking_send(Event::Done(Err(e)));
            return;
        }
    };
    // stderr drains on its own thread: a failing ffmpeg can fill that pipe and deadlock while
    // this thread is still waiting on progress lines.
    let pipe = child.stderr.take();
    let drain = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    });

    let live = match child.stdout.take() {
        Some(stdout) => pump_progress(stdout, total, tx),
        None => true,
    };
    if !live {
        let _ = child.kill();
    }
    let status = child.wait();
    let stderr = drain.join().unwrap_or_default();
    if !live {
        let _ = std::fs::remove_file(&spec.dst);
        return;
    }
    let result = match status {
        Ok(status) => finish(status, &stderr, &spec.dst),
        Err(e) => Err(Error::Io(e)),
    };
    let _ = tx.blocking_send(Event::Done(result));
}

/// Forward rising progress until ffmpeg's stdout ends. `false` means the receiver is gone (the
/// export was cancelled).
fn pump_progress(
    stdout: impl Read,
    total: Duration,
    tx: &tokio::sync::mpsc::Sender<Event>,
) -> bool {
    let mut last = 0.0;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Some(fraction) = progress_of(&line, total) else {
            continue;
        };
        if fraction <= last {
            continue;
        }
        last = fraction;
        if tx.blocking_send(Event::Progress(fraction)).is_err() {
            return false;
        }
    }
    true
}

/// The fraction of `total` one `-progress` line reports, or `None` when it carries no position.
/// ffmpeg writes `out_time_ms` in microseconds too (long-standing, and kept for compatibility),
/// so both keys are read the same way.
fn progress_of(line: &str, total: Duration) -> Option<f32> {
    let micros: u128 = line
        .strip_prefix("out_time_us=")
        .or_else(|| line.strip_prefix("out_time_ms="))?
        .trim()
        .parse()
        .ok()?;
    let total = total.as_micros();
    if total == 0 {
        return None;
    }
    Some((micros as f64 / total as f64).clamp(0.0, 1.0) as f32)
}

/// Turn ffmpeg's exit into a result, clearing a half-written `dst` when it failed.
fn finish(status: ExitStatus, stderr: &[u8], dst: &Path) -> Result<(), Error> {
    if status.success() {
        return Ok(());
    }
    let _ = std::fs::remove_file(dst);
    Err(Error::Failed {
        status: status.code(),
        stderr_tail: stderr_tail(stderr),
    })
}

/// The last few non-empty lines of ffmpeg's stderr, short enough for a UI message.
fn stderr_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("; ");
    let overflow = tail.chars().count().saturating_sub(TAIL_CHARS);
    if overflow == 0 {
        return tail;
    }
    // Keep the end: ffmpeg's last words are the ones that say what went wrong.
    format!("…{}", tail.chars().skip(overflow).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            src: PathBuf::from("/clips/in.mp4"),
            dst: PathBuf::from("/clips/out.tmp"),
            range: Some((Duration::from_millis(1_500), Duration::from_millis(4_250))),
            crop: PixelRect {
                x: 160,
                y: 90,
                width: 320,
                height: 180,
            },
            video: VideoQuality::DEFAULT,
        }
    }

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn index(argv: &[String], needle: &str) -> usize {
        argv.iter()
            .position(|a| a == needle)
            .unwrap_or_else(|| panic!("{needle} is in the argv"))
    }

    fn value_after(argv: &[String], flag: &str) -> String {
        argv[index(argv, flag) + 1].clone()
    }

    #[test]
    fn args_are_the_documented_invocation() {
        assert_eq!(
            strings(&args(&spec()).expect("valid crop")),
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-y",
                "-ss",
                "1.500",
                "-i",
                "/clips/in.mp4",
                "-t",
                "2.750",
                "-vf",
                "crop=320:180:160:90",
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-c:v",
                "libx264",
                "-crf",
                "18",
                "-preset",
                "veryfast",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "copy",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                "/clips/out.tmp",
            ]
        );
    }

    #[test]
    fn a_forced_profile_is_passed_to_the_encoder() {
        let spec = Spec {
            video: VideoQuality {
                profile: Some("baseline"),
                ..VideoQuality::DEFAULT
            },
            ..spec()
        };
        let argv = strings(&args(&spec).expect("valid crop"));
        assert_eq!(value_after(&argv, "-profile:v"), "baseline");
        assert!(
            index(&argv, "-profile:v") > index(&argv, "-preset"),
            "the profile rides with the rest of the encoder settings"
        );
    }

    #[test]
    fn without_a_range_the_whole_clip_is_exported() {
        let spec = Spec {
            range: None,
            ..spec()
        };
        let argv = strings(&args(&spec).expect("valid crop"));
        assert!(!argv.contains(&"-ss".to_owned()), "no seek without a range");
        assert!(
            !argv.contains(&"-t".to_owned()),
            "no duration without a range"
        );
        assert!(argv.contains(&"crop=320:180:160:90".to_owned()));
    }

    #[test]
    fn the_seek_precedes_the_input_and_the_duration_follows_it() {
        let argv = strings(&args(&spec()).expect("valid crop"));
        assert!(index(&argv, "-ss") < index(&argv, "-i"));
        assert!(index(&argv, "-i") < index(&argv, "-t"));
    }

    #[test]
    fn the_muxer_is_forced_right_before_the_destination() {
        let argv = strings(&args(&spec()).expect("valid crop"));
        assert_eq!(argv[argv.len() - 3..], ["-f", "mp4", "/clips/out.tmp"]);
    }

    #[test]
    fn audio_is_copied_and_the_moov_atom_comes_first() {
        let argv = strings(&args(&spec()).expect("valid crop"));
        assert_eq!(value_after(&argv, "-c:a"), "copy");
        assert_eq!(value_after(&argv, "-movflags"), "+faststart");
    }

    #[test]
    fn progress_reporting_is_only_asked_for_by_the_stream() {
        let plain = strings(&argv(&spec(), false).expect("valid crop"));
        assert!(!plain.contains(&"-progress".to_owned()));
        assert!(!plain.contains(&"-nostats".to_owned()));

        let streamed = strings(&argv(&spec(), true).expect("valid crop"));
        let at = index(&streamed, "-progress");
        assert_eq!(streamed[at + 1], "pipe:1");
        assert_eq!(streamed[at + 2], "-nostats");
        assert!(
            at > index(&streamed, "mp4"),
            "progress is asked for after the muxer, and the destination still comes last"
        );
        assert_eq!(streamed.last().expect("a destination"), "/clips/out.tmp");
    }

    #[test]
    fn a_degenerate_crop_is_refused() {
        for crop in [
            PixelRect {
                x: 0,
                y: 0,
                width: 1,
                height: 180,
            },
            PixelRect {
                x: 0,
                y: 0,
                width: 320,
                height: 0,
            },
        ] {
            let spec = Spec { crop, ..spec() };
            assert!(
                matches!(args(&spec), Err(Error::InvalidCrop)),
                "{crop:?} has no pixels to encode"
            );
        }
    }

    #[test]
    fn progress_is_read_from_either_out_time_key() {
        let total = Duration::from_secs(3);
        assert_eq!(progress_of("out_time_us=1500000", total), Some(0.5));
        assert_eq!(progress_of("out_time_ms=1500000", total), Some(0.5));
        assert_eq!(
            progress_of("out_time_us=9000000", total),
            Some(1.0),
            "a seek-padded tail clamps instead of overshooting"
        );
        assert_eq!(progress_of("out_time_us=0", total), Some(0.0));
    }

    #[test]
    fn other_progress_lines_are_ignored() {
        let total = Duration::from_secs(3);
        for line in [
            "frame=42",
            "progress=continue",
            "out_time=00:00:01.500000",
            "out_time_us=N/A",
            "",
        ] {
            assert_eq!(
                progress_of(line, total),
                None,
                "{line:?} carries no position"
            );
        }
        assert_eq!(
            progress_of("out_time_us=1500000", Duration::ZERO),
            None,
            "an unknown length has no fraction to report"
        );
    }

    #[test]
    fn the_failure_tail_keeps_the_last_lines_within_a_budget() {
        let tail = stderr_tail(b"first\n\nsecond\n  third  \nfourth\n");
        assert_eq!(tail, "second; third; fourth");
        assert_eq!(stderr_tail(b""), "");

        let long = stderr_tail(format!("{}\n", "x".repeat(2_000)).as_bytes());
        assert_eq!(
            long.chars().count(),
            TAIL_CHARS + 1,
            "capped, with an ellipsis"
        );
        assert!(long.starts_with('…'));
        assert!(long.ends_with('x'));
    }

    #[test]
    fn errors_read_as_sentences_the_ui_can_show() {
        assert_eq!(
            Error::NoFfmpeg.to_string(),
            "Cropping needs ffmpeg installed."
        );
        assert_eq!(
            Error::InvalidCrop.to_string(),
            "The crop is too small to export."
        );
        assert_eq!(
            Error::Failed {
                status: Some(1),
                stderr_tail: "no such file".to_owned(),
            }
            .to_string(),
            "ffmpeg failed (exit 1): no such file"
        );
        assert_eq!(
            Error::Failed {
                status: None,
                stderr_tail: "killed".to_owned(),
            }
            .to_string(),
            "ffmpeg failed: killed"
        );
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(Error::Io(io).to_string(), "denied");
    }

    /// The real thing, against a synthesised clip: crop + trim + audio passthrough, and the
    /// result read back by the same MP4 reader and openh264 decoder the library uses.
    #[test]
    #[ignore = "needs ffmpeg on PATH"]
    fn a_cropped_export_is_readable_by_the_library() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("in.mp4");
        synthesise(&src);

        let dst = dir.path().join("out.tmp");
        let spec = Spec {
            src,
            dst: dst.clone(),
            range: Some((Duration::from_millis(500), Duration::from_millis(1_500))),
            crop: PixelRect {
                x: 160,
                y: 90,
                width: 320,
                height: 180,
            },
            video: VideoQuality::DEFAULT,
        };
        run(&spec).expect("the export runs");

        let summary = rewynd_mux::read::clip_summary(&dst).expect("a readable MP4");
        assert_eq!((summary.width, summary.height), (320, 180));
        let seconds = summary.duration.as_secs_f64();
        assert!(
            (seconds - 1.0).abs() < 0.1,
            "exported {seconds}s of the range"
        );

        crate::thumbs::load_at(&dst, 0.0, 320).expect("openh264 decodes the first keyframe");

        assert!(
            ffmpeg(&[
                "-v",
                "error",
                "-i",
                &dst.to_string_lossy(),
                "-map",
                "0:a:0",
                "-f",
                "null",
                "-"
            ]),
            "the Opus track was copied through"
        );
    }

    /// A 2 s colour-bars clip with an Opus tone, i.e. a stand-in for a recorded clip.
    fn synthesise(dst: &Path) {
        let ok = ffmpeg(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "libopus",
            "-strict",
            "-2",
            "-shortest",
            &dst.to_string_lossy(),
        ]);
        assert!(ok, "ffmpeg synthesises the source clip");
    }

    fn ffmpeg(args: &[&str]) -> bool {
        let mut command = player::ffmpeg_command();
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
            .spawn()
            .and_then(|mut child| child.wait())
            .is_ok_and(|status| status.success())
    }
}
