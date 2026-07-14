//! The recorder's live status, published to a small JSON file the GUI polls.
//!
//! The recorder and GUI are separate processes with no IPC channel (the config file is their
//! only shared state). To show a live "Recording: <game>" indicator, the recorder writes its
//! current state next to its pid file whenever it changes; the GUI reads it on a timer. The
//! reader rejects a stale file left by a crashed recorder by checking the pid is still a live
//! recorder process.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::{ensure_instance_dir, instance_dir};
use crate::process::recorder_alive;

/// Bumped when the status JSON shape changes incompatibly; the reader rejects other versions.
pub const RECORDER_STATUS_VERSION: u32 = 1;

/// What the recorder is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderState {
    /// Actively buffering (a game, or the whole desktop — see [`RecorderStatus::game`]).
    Recording,
    /// Running but waiting for a game to focus (game-only capture, nothing detected yet).
    Idle,
    /// The capture pipeline failed; see [`RecorderStatus::detail`].
    Failed,
}

/// The recorder's published status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderStatus {
    /// Schema version; see [`RECORDER_STATUS_VERSION`].
    pub version: u32,
    /// The writing recorder's pid, so a reader can reject a stale file.
    pub pid: u32,
    /// The active backend (`EncoderChoice::label()`: `"gpu:<name>"` / `"cpu"`).
    pub encoder: String,
    /// The current state.
    pub state: RecorderState,
    /// The game being recorded; `None` while [`Recording`](RecorderState::Recording) means the
    /// whole desktop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
    /// Extra detail (e.g. a failure message) when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Path to the status file, next to `recorder.pid`.
#[must_use]
pub fn recorder_status_path() -> PathBuf {
    instance_dir().path.join("status.json")
}

/// Atomically publish `status` (write a temp file, then rename). Best-effort: the caller logs
/// failures rather than aborting capture.
///
/// # Errors
/// Filesystem errors creating the instance dir or writing/renaming the file.
pub fn write_recorder_status(status: &RecorderStatus) -> std::io::Result<()> {
    let dir = instance_dir();
    ensure_instance_dir(&dir)?;
    let json = serde_json::to_string(status).map_err(std::io::Error::other)?;
    crate::paths::write_file_atomic(&dir.path.join("status.json"), json.as_bytes())
}

/// Remove the status file (recorder shutdown). Best-effort.
pub fn clear_recorder_status() {
    let _ = std::fs::remove_file(recorder_status_path());
}

/// Read the recorder's status, or `None` if absent, unparseable, the wrong version, or written
/// by a recorder that is no longer running.
#[must_use]
pub fn read_recorder_status() -> Option<RecorderStatus> {
    read_status_from(&recorder_status_path(), recorder_alive)
}

/// Testable core of [`read_recorder_status`] with the path and liveness check injected.
fn read_status_from(path: &Path, alive: impl Fn(u32) -> bool) -> Option<RecorderStatus> {
    let contents = std::fs::read_to_string(path).ok()?;
    let status: RecorderStatus = serde_json::from_str(&contents).ok()?;
    (status.version == RECORDER_STATUS_VERSION && alive(status.pid)).then_some(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32) -> RecorderStatus {
        RecorderStatus {
            version: RECORDER_STATUS_VERSION,
            pid,
            encoder: "cpu".to_owned(),
            state: RecorderState::Recording,
            game: Some("Hades II".to_owned()),
            detail: None,
        }
    }

    #[test]
    fn round_trips_through_json_when_pid_is_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("status.json");
        let status = sample(4242);
        std::fs::write(&path, serde_json::to_string(&status).unwrap()).unwrap();
        let read = read_status_from(&path, |pid| pid == 4242).expect("reads");
        assert_eq!(read, status);
    }

    #[test]
    fn rejects_dead_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("status.json");
        std::fs::write(&path, serde_json::to_string(&sample(4242)).unwrap()).unwrap();
        assert!(read_status_from(&path, |_| false).is_none());
    }

    #[test]
    fn rejects_wrong_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("status.json");
        std::fs::write(
            &path,
            r#"{"version":999,"pid":1,"encoder":"cpu","state":"recording"}"#,
        )
        .unwrap();
        assert!(read_status_from(&path, |_| true).is_none());
    }

    #[test]
    fn missing_file_is_none() {
        assert!(read_status_from(Path::new("/nonexistent/status.json"), |_| true).is_none());
    }

    #[test]
    fn desktop_recording_omits_game_field() {
        let status = RecorderStatus {
            game: None,
            ..sample(1)
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("game"),
            "None game should be skipped: {json}"
        );
    }
}
