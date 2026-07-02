//! Local record of which clips have been uploaded where, so the library can show an "uploaded"
//! badge across restarts and refuse a duplicate upload. A small JSON file beside `config.toml`,
//! written 0600 — it holds remote ids and share links tied to the user's account.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Destination string stored for a ganked.tv upload.
pub const GANKED: &str = "ganked";
/// Destination string stored for a YouTube upload.
pub const YOUTUBE: &str = "youtube";

/// One recorded upload: a clip, where it went, and the id/link to reach it again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UploadRecord {
    pub file_name: String,
    pub size_bytes: u64,
    pub modified_millis: u64,
    /// [`GANKED`] or [`YOUTUBE`].
    pub destination: String,
    /// The remote id (ganked.tv clip id / YouTube video id), used to verify the copy still exists.
    pub remote_id: String,
    pub url: Option<String>,
    pub uploaded_millis: u64,
}

/// A clip's identity for matching history records: the file name plus size and mtime, so a clip
/// moved between game folders still matches while a rewrite (fresh mtime) does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipKey {
    pub file_name: String,
    pub size_bytes: u64,
    pub modified_millis: u64,
}

impl ClipKey {
    /// The key for the clip at `path` with the given size and mtime, or `None` for a nameless path.
    #[must_use]
    pub fn new(path: &Path, size_bytes: u64, modified: SystemTime) -> Option<Self> {
        Some(Self {
            file_name: path.file_name()?.to_str()?.to_owned(),
            size_bytes,
            modified_millis: to_millis(modified),
        })
    }

    /// Millisecond stamp for [`UploadRecord::uploaded_millis`].
    #[must_use]
    pub fn now_millis() -> u64 {
        to_millis(SystemTime::now())
    }

    fn matches(&self, record: &UploadRecord) -> bool {
        record.file_name == self.file_name
            && record.size_bytes == self.size_bytes
            && record.modified_millis == self.modified_millis
    }
}

fn to_millis(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The history file path, beside `config.toml`.
#[must_use]
pub fn history_path() -> Option<PathBuf> {
    Some(crate::config_path()?.with_file_name("upload-history.json"))
}

/// All records (empty when the file is missing or unreadable).
#[must_use]
pub fn load() -> Vec<UploadRecord> {
    history_path().map(|p| load_at(&p)).unwrap_or_default()
}

/// The record for `key` at `destination`, if the clip was uploaded there.
#[must_use]
pub fn find(key: &ClipKey, destination: &str) -> Option<UploadRecord> {
    find_in(&load(), key, destination)
}

/// Remember a successful upload, replacing any prior record for the same clip + destination.
pub fn record(entry: UploadRecord) -> std::io::Result<()> {
    let path = history_path().ok_or_else(no_path)?;
    let mut records = load_at(&path);
    upsert(&mut records, entry);
    save_at(&path, &records)
}

/// Forget the record for `key` at `destination` (e.g. after the remote copy was deleted), so the
/// duplicate guard lets the user upload again. A no-op when there's nothing to forget.
pub fn forget(key: &ClipKey, destination: &str) -> std::io::Result<()> {
    let path = history_path().ok_or_else(no_path)?;
    let mut records = load_at(&path);
    let before = records.len();
    records.retain(|r| !(key.matches(r) && r.destination == destination));
    if records.len() == before {
        return Ok(());
    }
    save_at(&path, &records)
}

fn no_path() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no config directory to store upload history",
    )
}

fn find_in(records: &[UploadRecord], key: &ClipKey, destination: &str) -> Option<UploadRecord> {
    records
        .iter()
        .find(|r| key.matches(r) && r.destination == destination)
        .cloned()
}

/// Replace any record with the same (clip, destination) identity, then append the new one — so the
/// file stays compacted to one entry per clip per destination rather than growing on re-upload.
fn upsert(records: &mut Vec<UploadRecord>, entry: UploadRecord) {
    records.retain(|r| {
        !(r.file_name == entry.file_name
            && r.size_bytes == entry.size_bytes
            && r.modified_millis == entry.modified_millis
            && r.destination == entry.destination)
    });
    records.push(entry);
}

fn load_at(path: &Path) -> Vec<UploadRecord> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_at(path: &Path, records: &[UploadRecord]) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(records)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_private_atomic(path, &bytes)
}

/// Write `bytes` to `path` atomically (temp + rename), owner-only: a crash can't leave a truncated
/// history, and the remote ids/links never become group/world readable.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> ClipKey {
        ClipKey {
            file_name: name.to_owned(),
            size_bytes: 100,
            modified_millis: 42,
        }
    }

    fn record_of(key: &ClipKey, dest: &str, id: &str) -> UploadRecord {
        UploadRecord {
            file_name: key.file_name.clone(),
            size_bytes: key.size_bytes,
            modified_millis: key.modified_millis,
            destination: dest.to_owned(),
            remote_id: id.to_owned(),
            url: Some(format!("https://x/{id}")),
            uploaded_millis: 1,
        }
    }

    #[test]
    fn clip_key_derives_from_the_file_name() {
        let k = ClipKey::new(
            Path::new("/clips/Elden Ring/rewynd-5-0.mp4"),
            2048,
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(7),
        )
        .expect("key");
        assert_eq!(k.file_name, "rewynd-5-0.mp4");
        assert_eq!(k.size_bytes, 2048);
        assert_eq!(k.modified_millis, 7);
    }

    #[test]
    fn upsert_keeps_one_record_per_clip_and_destination() {
        let k = key("rewynd-1-0.mp4");
        let mut records = Vec::new();
        upsert(&mut records, record_of(&k, GANKED, "old"));
        upsert(&mut records, record_of(&k, YOUTUBE, "yt"));
        // Re-upload to ganked replaces, doesn't duplicate.
        upsert(&mut records, record_of(&k, GANKED, "new"));
        assert_eq!(records.len(), 2);
        assert_eq!(find_in(&records, &k, GANKED).unwrap().remote_id, "new");
        assert_eq!(find_in(&records, &k, YOUTUBE).unwrap().remote_id, "yt");
    }

    #[test]
    fn find_in_matches_only_the_same_clip_and_destination() {
        let a = key("a.mp4");
        let b = key("b.mp4");
        let records = vec![record_of(&a, GANKED, "ida")];
        assert_eq!(find_in(&records, &a, GANKED).unwrap().remote_id, "ida");
        assert!(find_in(&records, &a, YOUTUBE).is_none());
        assert!(find_in(&records, &b, GANKED).is_none());
    }

    #[test]
    fn round_trips_through_the_file_and_forgets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("upload-history.json");
        let k = key("rewynd-9-0.mp4");

        save_at(
            &path,
            &[record_of(&k, GANKED, "gid"), record_of(&k, YOUTUBE, "yid")],
        )
        .expect("save");
        let loaded = load_at(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(find_in(&loaded, &k, GANKED).unwrap().remote_id, "gid");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "history is owner-only");
        }

        // Forget drops just that (clip, destination).
        let mut after = load_at(&path);
        after.retain(|r| !(k.matches(r) && r.destination == GANKED));
        save_at(&path, &after).expect("save");
        let reloaded = load_at(&path);
        assert!(find_in(&reloaded, &k, GANKED).is_none());
        assert!(find_in(&reloaded, &k, YOUTUBE).is_some());
    }

    #[test]
    fn missing_or_garbage_file_loads_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_at(&dir.path().join("nope.json")).is_empty());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"not json").expect("write");
        assert!(load_at(&bad).is_empty());
    }
}
