//! The clip save path: cut the video + audio rings, pick an output path, mux to MP4.
//!
//! Extracted from the recorder binary so the logic every trigger shares (hotkey, tray, dev
//! flush hook) exists once, holds its own state, and is CI-testable. All methods are blocking;
//! async callers run [`ClipSaver::save`] on a blocking thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rewynd_buffer::{AudioRingBuffer, RingBuffer};
use rewynd_encode::{AudioEncodeParams, EncodeParams};
use rewynd_mux::{AudioTrack, Mp4Muxer};
use thiserror::Error;

/// Shared, mutable video ring: the capture thread pushes, [`ClipSaver::save`] cuts.
pub type SharedBuffer = Arc<Mutex<RingBuffer>>;
/// Shared audio ring: the mixer thread pushes, [`ClipSaver::save`] cuts.
pub type SharedAudioBuffer = Arc<Mutex<AudioRingBuffer>>;

/// How long [`ClipSaver::save`] waits for the mixer to drain in-flight audio after signalling
/// `audio_drain_now`, so a clip's audio reaches as close to the cut as the mixer can deliver.
const AUDIO_DRAIN_WAIT: Duration = Duration::from_millis(60);

/// Lock a mutex, recovering a poisoned one: the rings must stay usable even if some holder
/// panicked, and a panic across the PipeWire callback boundary would be undefined behaviour.
pub fn lock_unpoisoned<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Why a save produced no clip.
#[derive(Debug, Error)]
pub enum SaveError {
    /// The ring has nothing cuttable yet (no keyframe in the window).
    #[error("nothing to save yet: {0}")]
    Empty(String),
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        source: rewynd_mux::MuxError,
    },
}

/// Everything one clip save needs, bundled once so the hotkey loop, tray menu, and dev flush
/// hook share a single handle instead of threading six parameters around.
pub struct ClipSaver {
    buffer: SharedBuffer,
    audio: SharedAudioBuffer,
    params: EncodeParams,
    audio_params: AudioEncodeParams,
    window: Duration,
    output_dir: Option<PathBuf>,
    /// Set to ask the mixer for an immediate drain before the audio cut; the mixer clears it.
    audio_drain_now: Option<Arc<AtomicBool>>,
    last_clip: Mutex<Option<PathBuf>>,
}

impl ClipSaver {
    #[must_use]
    pub fn new(
        buffer: SharedBuffer,
        audio: SharedAudioBuffer,
        params: EncodeParams,
        audio_params: AudioEncodeParams,
        window: Duration,
        output_dir: Option<PathBuf>,
        audio_drain_now: Option<Arc<AtomicBool>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            buffer,
            audio,
            params,
            audio_params,
            window,
            output_dir,
            audio_drain_now,
            last_clip: Mutex::new(None),
        })
    }

    /// Cut the most recent window from both rings and write it to an MP4. Blocking: the mux +
    /// file write run inline (callers use a blocking thread). On success the path is remembered
    /// for [`last_clip`](Self::last_clip).
    pub fn save(&self) -> Result<PathBuf, SaveError> {
        // Let the mixer flush what it is holding (settle window + encoder sub-frame) so the
        // audio track ends as close to "now" as possible, then give it one beat to drain.
        if let Some(drain) = &self.audio_drain_now {
            drain.store(true, Ordering::SeqCst);
            let waited = std::time::Instant::now();
            while drain.load(Ordering::SeqCst) && waited.elapsed() < AUDIO_DRAIN_WAIT {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        // Hold each lock only for the cut (which clones ref-counts, not payloads).
        let chunks = lock_unpoisoned(&self.buffer)
            .flush_last(self.window)
            .map_err(|e| SaveError::Empty(e.to_string()))?;

        // The clip starts at its first (keyframe) chunk; take the audio from that instant on —
        // both PTS share the capture epoch, so this keeps the tracks aligned.
        let clip_base = chunks.first().map_or(Duration::ZERO, |c| c.pts);
        let audio_chunks = lock_unpoisoned(&self.audio).flush_from(clip_base);

        let path = clip_output_path(self.output_dir.as_deref());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let muxer = Mp4Muxer::new(self.params.width, self.params.height, self.params.framerate);
        let result = if audio_chunks.is_empty() {
            muxer.write_mp4(&chunks, &path)
        } else {
            let audio = AudioTrack {
                chunks: &audio_chunks,
                channels: self.audio_params.channels as u8,
                sample_rate: self.audio_params.sample_rate,
                // Mid-stream cut: the encoder startup priming isn't present at the clip's
                // first packet, so don't trim.
                pre_skip: 0,
            };
            muxer.write_mp4_with_audio(&chunks, &audio, &path)
        };

        match result {
            Ok(()) => {
                let span = match (chunks.first(), chunks.last()) {
                    (Some(first), Some(last)) => last.pts.saturating_sub(first.pts),
                    _ => Duration::ZERO,
                };
                tracing::info!(
                    path = %path.display(),
                    frames = chunks.len(),
                    audio_packets = audio_chunks.len(),
                    span_s = span.as_secs_f64(),
                    "saved clip"
                );
                *lock_unpoisoned(&self.last_clip) = Some(path.clone());
                Ok(path)
            }
            Err(source) => Err(SaveError::Write { path, source }),
        }
    }

    /// The replay window this saver cuts.
    #[must_use]
    pub fn window(&self) -> Duration {
        self.window
    }

    /// The most recently saved clip. Falls back to the newest `rewynd-*.mp4` in the output
    /// directory, so "upload last clip" still works right after a recorder restart.
    #[must_use]
    pub fn last_clip(&self) -> Option<PathBuf> {
        if let Some(path) = lock_unpoisoned(&self.last_clip).clone() {
            return Some(path);
        }
        let dir = self
            .output_dir
            .clone()
            .or_else(rewynd_config::default_output_dir)?;
        newest_clip_in(&dir)
    }
}

/// The newest `rewynd-*.mp4` in `dir` by file name (names embed a millisecond timestamp, so
/// lexicographic max is newest; no metadata calls needed).
fn newest_clip_in(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "mp4")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rewynd-"))
        })
        .max()
}

/// Where to write a saved clip: `output_dir` if configured, else the user's Videos folder, else
/// a private per-user temp directory — with a millisecond-stamped, per-process-sequenced name.
/// The sequence number disambiguates two saves landing in the same millisecond.
fn clip_output_path(output_dir: Option<&Path>) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = output_dir
        .map(Path::to_path_buf)
        .or_else(rewynd_config::default_output_dir)
        .unwrap_or_else(private_temp_dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("rewynd-{stamp}-{seq}.mp4"))
}

/// Last-resort clip directory: per-user and non-world-readable, since clips are screen + mic
/// recordings and the shared temp root is world-readable.
fn private_temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rewynd-clips-{}", euid()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    }
    #[cfg(not(unix))]
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: geteuid is infallible and takes no arguments.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn euid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rewynd_buffer::{EncodedAudioChunk, EncodedChunk};

    /// Minimal Annex-B H.264 payload the muxer accepts as a clip start: SPS + PPS + IDR.
    fn keyframe_bytes() -> Arc<[u8]> {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f, 0x8c, 0x8d, 0x40]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33, 0xff]);
        data.into()
    }

    fn delta_bytes() -> Arc<[u8]> {
        vec![0, 0, 0, 1, 0x41, 0x9a, 0x24, 0x6c, 0x41, 0x4f].into()
    }

    fn saver_with(dir: &Path, window_s: u64) -> (Arc<ClipSaver>, SharedBuffer, SharedAudioBuffer) {
        let window = Duration::from_secs(window_s);
        let buffer: SharedBuffer = Arc::new(Mutex::new(RingBuffer::new(window)));
        let audio: SharedAudioBuffer = Arc::new(Mutex::new(AudioRingBuffer::new(window)));
        let saver = ClipSaver::new(
            buffer.clone(),
            audio.clone(),
            EncodeParams::default(),
            AudioEncodeParams::default(),
            window,
            Some(dir.to_path_buf()),
            None,
        );
        (saver, buffer, audio)
    }

    #[test]
    fn empty_ring_reports_nothing_to_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (saver, _, _) = saver_with(dir.path(), 5);
        match saver.save() {
            Err(SaveError::Empty(_)) => {}
            other => panic!("expected Empty, got {other:?}"),
        }
        assert_eq!(saver.last_clip(), None, "no clip and no fallback file");
    }

    #[test]
    fn saves_a_video_only_clip_and_remembers_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (saver, buffer, _) = saver_with(dir.path(), 5);
        lock_unpoisoned(&buffer).push(EncodedChunk {
            bytes: keyframe_bytes(),
            is_keyframe: true,
            pts: Duration::ZERO,
        });
        lock_unpoisoned(&buffer).push(EncodedChunk {
            bytes: delta_bytes(),
            is_keyframe: false,
            pts: Duration::from_millis(16),
        });

        let path = saver.save().expect("saves");
        assert!(path.exists());
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rewynd-") && n.ends_with(".mp4"))
        );
        assert_eq!(saver.last_clip(), Some(path));
    }

    #[test]
    fn saves_an_av_clip_when_audio_is_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (saver, buffer, audio) = saver_with(dir.path(), 5);
        lock_unpoisoned(&buffer).push(EncodedChunk {
            bytes: keyframe_bytes(),
            is_keyframe: true,
            pts: Duration::ZERO,
        });
        lock_unpoisoned(&audio).push(EncodedAudioChunk {
            bytes: vec![0xfc, 0xff, 0xfe].into(),
            frames: 960,
            pts: Duration::ZERO,
        });

        let path = saver.save().expect("saves with audio");
        assert!(path.exists());
        assert!(std::fs::metadata(&path).expect("stat").len() > 0);
    }

    #[test]
    fn drain_signal_is_raised_and_wait_is_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let window = Duration::from_secs(5);
        let buffer: SharedBuffer = Arc::new(Mutex::new(RingBuffer::new(window)));
        let audio: SharedAudioBuffer = Arc::new(Mutex::new(AudioRingBuffer::new(window)));
        let drain = Arc::new(AtomicBool::new(false));
        let saver = ClipSaver::new(
            buffer.clone(),
            audio,
            EncodeParams::default(),
            AudioEncodeParams::default(),
            window,
            Some(dir.path().to_path_buf()),
            Some(drain.clone()),
        );
        lock_unpoisoned(&buffer).push(EncodedChunk {
            bytes: keyframe_bytes(),
            is_keyframe: true,
            pts: Duration::ZERO,
        });

        // No mixer clears the flag here: save must still finish after its bounded wait.
        let started = std::time::Instant::now();
        saver.save().expect("saves");
        assert!(drain.load(Ordering::SeqCst), "drain was signalled");
        assert!(started.elapsed() >= AUDIO_DRAIN_WAIT);
    }

    #[test]
    fn last_clip_falls_back_to_newest_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("rewynd-100-0.mp4"), b"old").expect("write");
        std::fs::write(dir.path().join("rewynd-200-0.mp4"), b"new").expect("write");
        std::fs::write(dir.path().join("other.mp4"), b"x").expect("write");
        std::fs::write(dir.path().join("rewynd-300-0.txt"), b"x").expect("write");

        let (saver, _, _) = saver_with(dir.path(), 5);
        assert_eq!(
            saver.last_clip(),
            Some(dir.path().join("rewynd-200-0.mp4")),
            "newest rewynd-*.mp4 wins; other names/extensions are ignored"
        );
    }
}
