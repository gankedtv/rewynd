//! [`ClipSaver`]: cut the most recent window from both rings and write it to an MP4.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rewynd_buffer::{AudioRingBuffer, RingBuffer};
use rewynd_encode::{AudioEncodeParams, EncodeParams};
use rewynd_mux::{AudioTrack, Mp4Muxer};
use thiserror::Error;

use crate::lock_unpoisoned;
use crate::store::{clip_output_path, clips_dir, folder_name, newest_clip_in};

/// Shared, mutable video ring: the capture thread pushes, [`ClipSaver::save`] cuts.
pub type SharedBuffer = Arc<Mutex<RingBuffer>>;
/// Shared audio ring: the mixer thread pushes, [`ClipSaver::save`] cuts.
pub type SharedAudioBuffer = Arc<Mutex<AudioRingBuffer>>;

/// How long [`ClipSaver::save`] waits for the mixer to drain in-flight audio after signalling
/// `audio_drain_now`, so a clip's audio reaches as close to the cut as the mixer can deliver.
const AUDIO_DRAIN_WAIT: Duration = Duration::from_millis(60);

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
    /// The per-game subfolder the next save lands in (ShadowPlay-style), when game
    /// detection knows what the buffer holds. The platform wiring keeps it current.
    game_folder: Mutex<Option<String>>,
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
            game_folder: Mutex::new(None),
        })
    }

    /// Set (or clear) the per-game subfolder for subsequent saves. The name is
    /// sanitized here so callers can pass a raw game name.
    pub fn set_game_folder(&self, game: Option<&str>) {
        *lock_unpoisoned(&self.game_folder) = game.and_then(folder_name);
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

        let game_folder = lock_unpoisoned(&self.game_folder).clone();
        let path = clip_output_path(self.output_dir.as_deref(), game_folder.as_deref());
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
        // Mirror clip_output_path's directory resolution exactly, or clips saved to a
        // fallback dir would be invisible after a restart.
        newest_clip_in(&clips_dir(self.output_dir.as_deref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rewynd_buffer::{EncodedAudioChunk, EncodedChunk};
    use std::path::Path;

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
    fn saves_into_the_game_subfolder_and_finds_it_after_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (saver, buffer, _) = saver_with(dir.path(), 5);
        saver.set_game_folder(Some("Half-Life 2: Episode Two"));
        lock_unpoisoned(&buffer).push(EncodedChunk {
            bytes: keyframe_bytes(),
            is_keyframe: true,
            pts: Duration::ZERO,
        });

        let path = saver.save().expect("saves");
        assert_eq!(
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some("Half-Life 2 Episode Two"),
            "clip lands in the sanitized per-game folder"
        );

        // A fresh saver (post-restart) must find the clip inside the subfolder.
        let (fresh, _, _) = saver_with(dir.path(), 5);
        assert_eq!(fresh.last_clip(), Some(path));

        // Clearing the folder (or an unusable name) returns saves to the root.
        saver.set_game_folder(Some("..."));
        lock_unpoisoned(&buffer).push(EncodedChunk {
            bytes: keyframe_bytes(),
            is_keyframe: true,
            pts: Duration::from_millis(32),
        });
        let root = saver.save().expect("saves");
        assert_eq!(root.parent(), Some(dir.path()));
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
