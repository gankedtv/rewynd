//! The clip store: where clips live, what they are called, and what is in there. Lives here
//! (not in the clip crate) so the settings app can browse clips without the saver's
//! ring-buffer/encoder tree behind it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

/// One saved clip found in the output directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipEntry {
    pub path: PathBuf,
    /// The per-game subfolder the clip sits in (`None` for the output root).
    pub game: Option<String>,
    /// When the clip was saved: the millisecond stamp in the file name, falling back to the
    /// file's mtime.
    pub saved_at: SystemTime,
    /// The file's mtime — the thumbnail cache key, which must notice a rewritten file even
    /// when the name (and thus `saved_at`) is unchanged.
    pub modified: SystemTime,
    pub size_bytes: u64,
}

/// Where clips live: `configured` if set, else the user's Videos folder, else a private
/// per-user temp directory. The single resolution the saver, the tray fallback, and the
/// library view all share — divergence here would make saved clips invisible somewhere.
#[must_use]
pub fn clips_dir(configured: Option<&Path>) -> PathBuf {
    configured
        .map(Path::to_path_buf)
        .or_else(crate::default_output_dir)
        .unwrap_or_else(private_temp_dir)
}

/// All `rewynd-*.mp4` clips under `dir` (plus one level of per-game subfolders),
/// newest first.
#[must_use]
pub fn list_clips(dir: &Path) -> Vec<ClipEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            let game = entry.file_name().to_str().map(str::to_owned);
            if let Ok(sub) = std::fs::read_dir(entry.path()) {
                out.extend(
                    sub.filter_map(Result::ok)
                        .filter(is_file_entry)
                        .filter_map(|e| clip_entry(&e, game.clone())),
                );
            }
        } else if is_file_entry(&entry) {
            out.extend(clip_entry(&entry, None));
        }
    }
    // Newest first; the name (which embeds the stamp) breaks same-instant ties stably.
    out.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then(b.path.cmp(&a.path)));
    out
}

/// Whether a directory entry counts as a clip candidate file: a regular file, or a symlink
/// whose target is one. Shared by [`list_clips`] and [`newest_clip_in`] so the two never
/// disagree about what is a clip.
fn is_file_entry(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|kind| {
        kind.is_file()
            || (kind.is_symlink() && std::fs::metadata(entry.path()).is_ok_and(|m| m.is_file()))
    })
}

/// Build a [`ClipEntry`] for a directory entry, or `None` when it isn't a clip.
fn clip_entry(entry: &std::fs::DirEntry, game: Option<String>) -> Option<ClipEntry> {
    let name = entry.file_name();
    let name = name.to_str()?;
    if !is_clip_name(name) {
        return None;
    }
    // Follows symlinks, so a linked clip carries its target's size/mtime.
    let meta = std::fs::metadata(entry.path()).ok()?;
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let saved_at = clip_stamp_millis(name)
        .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
        .unwrap_or(modified);
    Some(ClipEntry {
        path: entry.path(),
        game,
        saved_at,
        modified,
        size_bytes: meta.len(),
    })
}

/// Whether a file name looks like one of our clips.
fn is_clip_name(name: &str) -> bool {
    name.starts_with("rewynd-") && name.ends_with(".mp4")
}

/// The millisecond timestamp embedded in a `rewynd-<millis>-<seq>.mp4` name, if it parses.
fn clip_stamp_millis(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("rewynd-")?.strip_suffix(".mp4")?;
    let (millis, seq) = rest.split_once('-')?;
    seq.parse::<u64>().ok()?;
    millis.parse().ok()
}

/// The newest `rewynd-*.mp4` under `dir` by file name, looking one level into per-game
/// subfolders too (names embed a millisecond timestamp, so lexicographic max of the file
/// name is newest).
#[must_use]
pub fn newest_clip_in(dir: &Path) -> Option<PathBuf> {
    fn newest_in(dir: &Path, recurse: bool) -> Option<PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        entries
            .filter_map(|e| e.ok())
            .filter_map(|entry| {
                let kind = entry.file_type().ok()?;
                if kind.is_dir() {
                    return recurse.then(|| newest_in(&entry.path(), false)).flatten();
                }
                if !is_file_entry(&entry) {
                    return None;
                }
                let name = entry.file_name();
                let name = name.to_str()?;
                is_clip_name(name).then(|| entry.path())
            })
            .max_by(|a, b| a.file_name().cmp(&b.file_name()))
    }
    newest_in(dir, true)
}

/// A filesystem-safe folder name derived from a game name: path separators and
/// characters Windows forbids become spaces, whitespace collapses, and the result is
/// length-capped. `None` when nothing usable remains.
#[must_use]
pub fn folder_name(raw: &str) -> Option<String> {
    const MAX_LEN: usize = 80;
    let mapped: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let mut collapsed = String::with_capacity(mapped.len());
    for word in mapped.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
    }
    while collapsed.len() > MAX_LEN {
        collapsed.pop();
    }
    // Windows refuses names ending in dots; reserved device names (CON, NUL, ...) are
    // vanishingly unlikely as game titles and left to the OS to reject.
    let trimmed = collapsed.trim_end_matches(['.', ' ']).to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Where to write a saved clip: [`clips_dir`] plus the per-game subfolder when one is set,
/// with a millisecond-stamped, per-process-sequenced name. The sequence number disambiguates
/// two saves landing in the same millisecond.
#[must_use]
pub fn clip_output_path(output_dir: Option<&Path>, game_folder: Option<&str>) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let mut dir = clips_dir(output_dir);
    if let Some(game) = game_folder {
        dir.push(game);
    }
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("rewynd-{stamp}-{seq}.mp4"))
}

/// Last-resort clip directory: per-user and non-world-readable, since clips are screen + mic
/// recordings. The shared temp root is world-writable, so a pre-existing directory is only
/// trusted after an ownership check; a squatted name falls back to a home-scoped directory.
fn private_temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rewynd-clips-{}", euid()));
    if ensure_private_dir(&dir) {
        return dir;
    }
    tracing::warn!(dir = %dir.display(), "temp clip dir is not safely ours; using a home dir");
    if let Some(home) = dirs::home_dir() {
        let fallback = home.join(".rewynd-clips");
        // Same bar as the temp path: clips only land in a directory that is verifiably ours.
        if ensure_private_dir(&fallback) {
            return fallback;
        }
        tracing::error!(dir = %fallback.display(), "home clip dir is not safely ours either");
    }
    dir
}

/// Create `dir` 0700 if missing and verify it is a real directory owned by us. A dir we own but
/// left group/world-accessible by an older release is tightened to 0700 in place, not refused:
/// failing closed would disable the private store forever (mirrors the single-instance dir's
/// upgrade path). Only a symlink, a foreign owner, or a non-directory is refused. Shared by the
/// clip fallback dir and the settings app's thumbnail cache, which holds frames of the same
/// recordings.
#[cfg(unix)]
#[must_use]
pub fn ensure_private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let _ = std::fs::DirBuilder::new().mode(0o700).create(dir);
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if !meta.is_dir() || meta.uid() != euid() {
        return false;
    }
    if meta.mode() & 0o077 != 0 {
        return std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_ok();
    }
    true
}

#[cfg(not(unix))]
#[must_use]
pub fn ensure_private_dir(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok()
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

    #[test]
    fn folder_name_sanitizes() {
        assert_eq!(folder_name("Elden Ring"), Some("Elden Ring".to_owned()));
        assert_eq!(folder_name("a/b\\c"), Some("a b c".to_owned()));
        assert_eq!(
            folder_name("  spaced   out  "),
            Some("spaced out".to_owned())
        );
        assert_eq!(folder_name("..."), None);
        assert_eq!(folder_name("\u{0}\u{1}"), None);
        assert_eq!(folder_name(""), None);
        let long = "x".repeat(200);
        assert!(folder_name(&long).unwrap().len() <= 80);
    }

    #[test]
    fn clip_stamp_parses_only_well_formed_names() {
        assert_eq!(
            clip_stamp_millis("rewynd-1700000000123-0.mp4"),
            Some(1_700_000_000_123)
        );
        assert_eq!(clip_stamp_millis("rewynd-5-17.mp4"), Some(5));
        assert_eq!(clip_stamp_millis("rewynd-abc-0.mp4"), None);
        assert_eq!(clip_stamp_millis("rewynd-123-x.mp4"), None);
        assert_eq!(clip_stamp_millis("rewynd-123.mp4"), None);
        assert_eq!(clip_stamp_millis("other-123-0.mp4"), None);
    }

    #[test]
    fn clips_dir_prefers_the_configured_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(clips_dir(Some(dir.path())), dir.path());
        // Unset falls through to a real directory (Videos or the private temp dir) —
        // exercised without asserting which, since it depends on the machine.
        let fallback = clips_dir(None);
        assert!(!fallback.as_os_str().is_empty());
    }

    #[test]
    fn clip_output_path_stamps_and_sequences() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = clip_output_path(Some(dir.path()), None);
        let b = clip_output_path(Some(dir.path()), Some("Elden Ring"));
        assert_eq!(a.parent(), Some(dir.path()));
        assert_eq!(b.parent(), Some(dir.path().join("Elden Ring").as_path()));
        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_str().unwrap();
            assert!(is_clip_name(name), "{name}");
            assert!(clip_stamp_millis(name).is_some(), "{name}");
        }
        assert_ne!(a.file_name(), b.file_name(), "the sequence disambiguates");
    }

    #[test]
    fn lists_clips_newest_first_with_game_badges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("rewynd-100-0.mp4"), b"a").expect("write");
        std::fs::write(root.join("rewynd-300-0.mp4"), b"bcd").expect("write");
        std::fs::create_dir(root.join("Elden Ring")).expect("mkdir");
        std::fs::write(root.join("Elden Ring/rewynd-200-0.mp4"), b"xy").expect("write");
        // Decoys: wrong name, wrong extension, and a two-levels-deep clip (out of scope).
        std::fs::write(root.join("other.mp4"), b"x").expect("write");
        std::fs::write(root.join("rewynd-400-0.txt"), b"x").expect("write");
        std::fs::create_dir_all(root.join("Elden Ring/deep")).expect("mkdir");
        std::fs::write(root.join("Elden Ring/deep/rewynd-500-0.mp4"), b"x").expect("write");

        let clips = list_clips(root);
        assert_eq!(
            clips
                .iter()
                .map(|c| c.path.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["rewynd-300-0.mp4", "rewynd-200-0.mp4", "rewynd-100-0.mp4"]
        );
        assert_eq!(clips[0].game, None);
        assert_eq!(clips[1].game, Some("Elden Ring".to_owned()));
        assert_eq!(clips[0].size_bytes, 3);
        assert_eq!(
            clips[2].saved_at,
            SystemTime::UNIX_EPOCH + Duration::from_millis(100)
        );
    }

    #[test]
    fn unparseable_stamp_falls_back_to_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rewynd-notastamp-0.mp4");
        std::fs::write(&path, b"x").expect("write");
        let clips = list_clips(dir.path());
        assert_eq!(clips.len(), 1);
        let mtime = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        assert_eq!(clips[0].saved_at, mtime);
        assert_eq!(clips[0].modified, mtime);
    }

    #[test]
    fn missing_directory_lists_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(list_clips(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn newest_clip_wins_by_name_across_subfolders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        assert_eq!(newest_clip_in(root), None);
        std::fs::write(root.join("rewynd-100-0.mp4"), b"a").expect("write");
        std::fs::create_dir(root.join("Elden Ring")).expect("mkdir");
        std::fs::write(root.join("Elden Ring/rewynd-200-0.mp4"), b"b").expect("write");
        std::fs::write(root.join("other.mp4"), b"x").expect("write");
        assert_eq!(
            newest_clip_in(root),
            Some(root.join("Elden Ring/rewynd-200-0.mp4"))
        );
    }

    /// A symlink to a clip file counts everywhere; a dangling one and a symlink to a
    /// directory count nowhere. `list_clips` and `newest_clip_in` share the predicate.
    #[cfg(unix)]
    #[test]
    fn symlinked_clips_are_treated_consistently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let target = root.join("real").join("clip.mp4");
        std::fs::create_dir(root.join("real")).expect("mkdir");
        std::fs::write(&target, b"video").expect("write");
        std::os::unix::fs::symlink(&target, root.join("rewynd-100-0.mp4")).expect("symlink");
        std::os::unix::fs::symlink(root.join("gone.mp4"), root.join("rewynd-200-0.mp4"))
            .expect("symlink");
        std::os::unix::fs::symlink(root.join("real"), root.join("rewynd-300-0.mp4"))
            .expect("symlink");

        let clips = list_clips(root);
        assert_eq!(clips.len(), 1, "{clips:?}");
        assert_eq!(clips[0].path, root.join("rewynd-100-0.mp4"));
        assert_eq!(clips[0].size_bytes, 5, "size comes from the target");
        assert_eq!(newest_clip_in(root), Some(root.join("rewynd-100-0.mp4")));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_accepts_ours_and_rejects_loose_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let private = dir.path().join("private");
        assert!(ensure_private_dir(&private), "fresh 0700 dir is ours");
        assert!(ensure_private_dir(&private), "idempotent");

        // A dir we own but left too open by an older release is tightened in place, not refused.
        let loose = dir.path().join("loose");
        std::fs::create_dir(&loose).expect("mkdir");
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(
            ensure_private_dir(&loose),
            "a loose dir we own is tightened"
        );
        let mode = std::fs::metadata(&loose)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "tightened to owner-only");

        let file = dir.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        assert!(!ensure_private_dir(&file), "a non-directory is refused");

        // A symlink (even to a dir we own) is refused: it could be planted to redirect the store.
        let target = dir.path().join("target");
        std::fs::create_dir(&target).expect("mkdir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(!ensure_private_dir(&link), "a symlink is refused");
    }
}
