//! Single-instance guards (docs/adr/0008): advisory `flock` on per-user pid/lock files.

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use crate::paths::{ensure_instance_dir, instance_dir, recorder_pid_path, settings_lock_path};

/// A held single-instance lock (advisory `flock`). Keep it alive for the whole run: dropping it,
/// or the process exiting/crashing, releases the lock so the next instance can start. The kernel
/// drops the lock on process death, so there is no stale-lock to clean up.
#[cfg(unix)]
#[must_use = "the single-instance lock releases as soon as this guard is dropped"]
pub struct InstanceLock {
    _file: std::fs::File,
}

/// Open `path` and take a non-blocking exclusive advisory lock. `Ok(None)` means another process
/// already holds it; the file is created if absent (its contents are left untouched on lock failure).
/// The parent dir must already exist — see [`ensure_instance_dir`].
#[cfg(unix)]
fn lock_file(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Don't truncate on open: a failed lock must leave the holder's pid intact. We truncate
        // only after the lock is ours (in `acquire_pid_lock_at`).
        .truncate(false)
        // A pre-planted symlink must not redirect the lock file (the temp fallback dir is only
        // verified, not exclusive to us forever).
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    loop {
        // SAFETY: FFI call; `file` owns a valid open fd for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        let err = std::io::Error::last_os_error();
        match err.kind() {
            // LOCK_NB returns EWOULDBLOCK when another process holds the lock — not an error.
            std::io::ErrorKind::WouldBlock => return Ok(None),
            // A signal interrupted the call before it took effect; retry rather than treat the
            // lock as free (which would let a second instance through).
            std::io::ErrorKind::Interrupted => continue,
            _ => return Err(err),
        }
    }
}

/// Take the lock at `path` and (re)write our pid into it, so a peer can find this process.
#[cfg(unix)]
fn acquire_pid_lock_at(path: &Path) -> std::io::Result<Option<InstanceLock>> {
    use std::io::Write;
    let Some(mut file) = lock_file(path)? else {
        return Ok(None);
    };
    // Newline-framed and written before truncating: a concurrent unlocked reader never sees an
    // empty file, and by taking the first line it gets a clean pid even if a longer previous pid
    // briefly leaves a tail; the truncate then drops that tail.
    let line = format!("{}\n", std::process::id());
    file.write_all(line.as_bytes())?;
    file.set_len(line.len() as u64)?;
    Ok(Some(InstanceLock { _file: file }))
}

/// Acquire the recorder's single-instance lock (on [`recorder_pid_path`]), writing our pid for the
/// settings app's restart path. `Ok(None)` means another live recorder already holds it.
#[cfg(unix)]
pub fn acquire_recorder_lock() -> std::io::Result<Option<InstanceLock>> {
    ensure_instance_dir(&instance_dir())?;
    acquire_pid_lock_at(&recorder_pid_path())
}

/// Acquire the settings app's single-instance lock (on [`settings_lock_path`]). `Ok(None)` means a
/// settings window is already open.
#[cfg(unix)]
pub fn acquire_settings_lock() -> std::io::Result<Option<InstanceLock>> {
    ensure_instance_dir(&instance_dir())?;
    Ok(lock_file(&settings_lock_path())?.map(|file| InstanceLock { _file: file }))
}

// No `flock` off unix yet, so the guard is a no-op there (a Windows named-mutex equivalent lands
// with Windows parity). Stubs keep the public API total so callers need no `#[cfg]`.
#[cfg(not(unix))]
pub struct InstanceLock;

#[cfg(not(unix))]
pub fn acquire_recorder_lock() -> std::io::Result<Option<InstanceLock>> {
    Ok(Some(InstanceLock))
}

#[cfg(not(unix))]
pub fn acquire_settings_lock() -> std::io::Result<Option<InstanceLock>> {
    Ok(Some(InstanceLock))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn instance_lock_is_exclusive_and_releases_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.lock");
        let first = lock_file(&path).expect("io ok").expect("first acquires");
        assert!(
            lock_file(&path).expect("io ok").is_none(),
            "a second lock on the same file is refused"
        );
        drop(first);
        assert!(
            lock_file(&path).expect("io ok").is_some(),
            "the lock is free once the first holder drops it"
        );
    }

    #[test]
    fn pid_lock_writes_the_current_pid_and_is_exclusive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.pid");
        let lock = acquire_pid_lock_at(&path)
            .expect("io ok")
            .expect("acquires");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read pid"),
            format!("{}\n", std::process::id())
        );
        assert!(
            acquire_pid_lock_at(&path).expect("io ok").is_none(),
            "a peer is refused while the lock is held"
        );
        drop(lock);
    }

    #[test]
    fn pid_lock_overwrites_a_longer_stale_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.pid");
        // A leftover pid longer than ours must be fully replaced, not left with a trailing tail.
        std::fs::write(&path, "9999999999999").expect("seed stale pid");
        let lock = acquire_pid_lock_at(&path)
            .expect("io ok")
            .expect("acquires");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read pid"),
            format!("{}\n", std::process::id())
        );
        drop(lock);
    }

    #[test]
    fn lock_file_refuses_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        std::fs::write(&target, "").expect("target");
        let link = dir.path().join("link.lock");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(
            lock_file(&link).is_err(),
            "O_NOFOLLOW rejects a symlinked lock path"
        );
    }
}
