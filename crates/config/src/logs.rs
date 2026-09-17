//! A size-capped log file set for the installed recorder, which has no console.
//!
//! [`RotatingLog`] appends to `<name>.log` and, once a write would take that file past
//! its cap, shifts it to `<name>.1.log` (and that to `.2.log`, …), dropping the oldest,
//! so the whole set is bounded by `max_bytes × keep` no matter how long the recorder
//! runs or how noisy a bad day gets. Rotation happens between writes, never inside one,
//! so a log line is never split across files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Where the recorder's log files go: the platform's local data dir
/// (`%LOCALAPPDATA%\rewynd\logs`, `~/Library/Application Support/rewynd/logs`,
/// `$XDG_DATA_HOME/rewynd/logs`). `None` when the platform has no such dir.
#[must_use]
pub fn log_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("rewynd").join("logs"))
}

/// An append-only log file with a byte cap and a bounded number of rotated predecessors.
#[derive(Debug)]
pub struct RotatingLog {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    keep: usize,
}

impl RotatingLog {
    /// Open `dir/<name>.log` for appending, creating the directory. `max_bytes` caps each
    /// file; `keep` is the number of files kept in total, the live one included (so `1`
    /// means the live file is simply truncated when full).
    pub fn open(dir: &Path, name: &str, max_bytes: u64, keep: usize) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{name}.log"));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            written,
            max_bytes: max_bytes.max(1),
            keep: keep.max(1),
        })
    }

    /// The live file's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `<name>.<index>.log` next to the live file.
    fn rotated(&self, index: usize) -> PathBuf {
        let stem = self
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.path.with_file_name(format!("{stem}.{index}.log"))
    }

    /// Shift every file up one index (the oldest falls off) and start a fresh live file.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        for index in (1..self.keep).rev() {
            let from = if index == 1 {
                self.path.clone()
            } else {
                self.rotated(index - 1)
            };
            // A missing predecessor (a young set) is simply skipped.
            if from.exists() {
                fs::rename(&from, self.rotated(index))?;
            }
        }
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // A single write larger than the cap still lands whole, in a fresh file.
        if self.written > 0 && self.written + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes(dir: &Path, name: &str, keep: usize) -> Vec<Option<u64>> {
        let mut out = vec![
            fs::metadata(dir.join(format!("{name}.log")))
                .ok()
                .map(|m| m.len()),
        ];
        for index in 1..=keep {
            out.push(
                fs::metadata(dir.join(format!("{name}.{index}.log")))
                    .ok()
                    .map(|m| m.len()),
            );
        }
        out
    }

    #[test]
    fn rotates_at_the_cap_and_keeps_a_bounded_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = RotatingLog::open(dir.path(), "t", 100, 3).expect("open");
        // 40-byte lines: two fit, the third rotates. Twelve lines = six rotations.
        for i in 0..12 {
            writeln!(log, "line {i:02} {}", "x".repeat(31)).expect("write");
        }
        log.flush().expect("flush");
        let sizes = sizes(dir.path(), "t", 4);
        // Live + .1 + .2 exist, each within the cap; .3 never appears, .4 neither.
        assert!(sizes[0].is_some_and(|n| n > 0 && n <= 100), "{sizes:?}");
        assert!(sizes[1].is_some_and(|n| n > 0 && n <= 100), "{sizes:?}");
        assert!(sizes[2].is_some_and(|n| n > 0 && n <= 100), "{sizes:?}");
        assert_eq!(sizes[3], None, "{sizes:?}");
        assert_eq!(sizes[4], None, "{sizes:?}");
        // Newest lines live in the live file, older ones behind it.
        let live = fs::read_to_string(dir.path().join("t.log")).expect("read");
        let older = fs::read_to_string(dir.path().join("t.1.log")).expect("read");
        assert!(live.contains("line 11"), "{live}");
        assert!(
            older.contains("line 09") || older.contains("line 08"),
            "{older}"
        );
    }

    #[test]
    fn keep_one_truncates_the_live_file_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = RotatingLog::open(dir.path(), "solo", 50, 1).expect("open");
        for _ in 0..10 {
            writeln!(log, "{}", "y".repeat(19)).expect("write");
        }
        let sizes = sizes(dir.path(), "solo", 2);
        assert!(sizes[0].is_some_and(|n| n <= 50), "{sizes:?}");
        assert_eq!(&sizes[1..], [None, None], "{sizes:?}");
    }

    #[test]
    fn reopening_appends_and_counts_what_is_already_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut log = RotatingLog::open(dir.path(), "again", 60, 2).expect("open");
            write!(log, "{}", "a".repeat(40)).expect("write");
        }
        let mut log = RotatingLog::open(dir.path(), "again", 60, 2).expect("reopen");
        assert_eq!(log.written, 40);
        // 40 + 30 > 60: the reopened log rotates rather than overshooting its cap.
        write!(log, "{}", "b".repeat(30)).expect("write");
        let sizes = sizes(dir.path(), "again", 1);
        assert_eq!(sizes, [Some(30), Some(40)]);
    }

    #[test]
    fn an_oversized_write_lands_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = RotatingLog::open(dir.path(), "big", 10, 2).expect("open");
        write!(log, "{}", "c".repeat(25)).expect("write");
        write!(log, "d").expect("write");
        assert_eq!(sizes(dir.path(), "big", 1), [Some(1), Some(25)]);
    }

    #[test]
    fn log_dir_sits_under_the_platform_data_dir() {
        if let Some(dir) = log_dir() {
            assert!(
                dir.ends_with(Path::new("rewynd").join("logs")),
                "{}",
                dir.display()
            );
        }
    }
}
