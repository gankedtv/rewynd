//! Hand-off of a `rewynd://clip/<name>` launch to an already-running settings window
//! (docs/adr/0016): the refused second instance drops the link into a file in the per-user
//! instance dir, atomically (write-aside + rename), and the running window's directory watch
//! picks it up. The receiver re-validates the link before acting on it, so the file's content
//! is never trusted beyond being a candidate link.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::paths::{ensure_instance_dir, instance_dir, settings_activation_path};

/// How long a pending activation stays honored. Generous against watcher latency and a slow
/// machine, short enough that a link left behind by a crashed window is not replayed as a
/// surprise "the library opens some old clip" on the next launch (the launch consumes the file
/// either way; stale ones are just dropped).
const MAX_AGE: Duration = Duration::from_secs(30);

/// Ceiling on the activation file size. A valid link is well under this; anything larger is not
/// ours and is discarded unread.
const MAX_LEN: u64 = 4096;

/// Hand `link` to the running settings window by (re)placing the activation file. The rename is
/// atomic on both unix and Windows, so the watcher on the other side never sees a partial write;
/// a second hand-off before the first is consumed replaces it (last click wins).
pub fn send_settings_activation(link: &str) -> std::io::Result<()> {
    let dir = instance_dir();
    ensure_instance_dir(&dir)?;
    send_activation_at(&settings_activation_path(), link)
}

/// The testable core of [`send_settings_activation`]: write beside `path`, then rename over it.
fn send_activation_at(path: &Path, link: &str) -> std::io::Result<()> {
    let staged = path.with_extension("part");
    std::fs::write(&staged, link)?;
    std::fs::rename(&staged, path)
}

/// Consume the pending activation, if there is a fresh one: the link is returned and the file
/// removed. `None` when there is nothing pending, or the pending file is stale, oversized, or
/// not text — those are removed too, so nothing lingers to be replayed later.
#[must_use]
pub fn take_settings_activation() -> Option<String> {
    take_activation_at(&settings_activation_path(), SystemTime::now())
}

/// The testable core of [`take_settings_activation`], with the freshness reference injected.
fn take_activation_at(path: &Path, now: SystemTime) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    // Consume-first: whatever the verdict below, a pending file never outlives one look at it.
    let content = if meta.len() <= MAX_LEN {
        std::fs::read(path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(path);
    // An mtime ahead of `now` (clock step between writer and reader) counts as fresh — age zero,
    // not an error.
    let age = now
        .duration_since(meta.modified().ok()?)
        .unwrap_or(Duration::ZERO);
    if age > MAX_AGE {
        return None;
    }
    let link = String::from_utf8(content?).ok()?;
    let link = link.trim();
    (!link.is_empty()).then(|| link.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_in(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("settings.activate")
    }

    #[test]
    fn send_then_take_round_trips_and_consumes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = path_in(&dir);
        send_activation_at(&path, "rewynd://clip/rewynd-2026-01-01.mp4").expect("send");
        assert_eq!(
            take_activation_at(&path, SystemTime::now()).as_deref(),
            Some("rewynd://clip/rewynd-2026-01-01.mp4")
        );
        assert!(!path.exists(), "taking the activation removes the file");
        assert_eq!(
            take_activation_at(&path, SystemTime::now()),
            None,
            "a second take finds nothing"
        );
    }

    #[test]
    fn a_second_send_replaces_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = path_in(&dir);
        send_activation_at(&path, "rewynd://clip/first.mp4").expect("send");
        send_activation_at(&path, "rewynd://clip/second.mp4").expect("resend");
        assert_eq!(
            take_activation_at(&path, SystemTime::now()).as_deref(),
            Some("rewynd://clip/second.mp4")
        );
    }

    #[test]
    fn a_stale_activation_is_dropped_and_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = path_in(&dir);
        send_activation_at(&path, "rewynd://clip/old.mp4").expect("send");
        let later = SystemTime::now() + MAX_AGE + Duration::from_secs(1);
        assert_eq!(take_activation_at(&path, later), None);
        assert!(!path.exists(), "a stale file is still consumed");
    }

    #[test]
    fn a_future_mtime_counts_as_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = path_in(&dir);
        send_activation_at(&path, "rewynd://clip/skewed.mp4").expect("send");
        let earlier = SystemTime::now() - Duration::from_secs(600);
        assert_eq!(
            take_activation_at(&path, earlier).as_deref(),
            Some("rewynd://clip/skewed.mp4")
        );
    }

    #[test]
    fn oversized_or_binary_or_blank_content_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");

        let oversized = path_in(&dir);
        std::fs::write(&oversized, vec![b'a'; (MAX_LEN + 1) as usize]).expect("write");
        assert_eq!(take_activation_at(&oversized, SystemTime::now()), None);
        assert!(!oversized.exists(), "an oversized file is removed unread");

        let binary = dir.path().join("binary.activate");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).expect("write");
        assert_eq!(take_activation_at(&binary, SystemTime::now()), None);
        assert!(!binary.exists());

        let blank = dir.path().join("blank.activate");
        std::fs::write(&blank, "  \n").expect("write");
        assert_eq!(take_activation_at(&blank, SystemTime::now()), None);
        assert!(!blank.exists());
    }

    #[test]
    fn taking_from_an_absent_path_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(take_activation_at(&path_in(&dir), SystemTime::now()), None);
    }
}
