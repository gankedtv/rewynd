//! Single-instance guards (docs/adr/0008): advisory `flock` on per-user pid/lock files
//! on unix; per-session named kernel mutexes on Windows. Both release on process death,
//! so there is no stale lock to clean up.

#[cfg(any(unix, windows))]
use std::path::Path;

#[cfg(unix)]
use crate::paths::settings_lock_path;
#[cfg(any(unix, windows))]
use crate::paths::{ensure_instance_dir, instance_dir, recorder_pid_path};

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
        // Owner-only at creation, matching the 0600/0700 posture of everything else here.
        .mode(0o600)
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

/// Whether the settings lock at `path` is held (briefly takes it). Core of [`settings_running`].
#[cfg(unix)]
fn settings_running_at(path: &Path) -> bool {
    !matches!(lock_file(path), Ok(Some(_)))
}

/// Whether a settings window is open. Errors count as open: callers must not disturb a live one.
#[cfg(unix)]
#[must_use]
pub fn settings_running() -> bool {
    if ensure_instance_dir(&instance_dir()).is_err() {
        return true;
    }
    settings_running_at(&settings_lock_path())
}

/// A held single-instance lock (a named kernel mutex in the per-session `Local\`
/// namespace). Keep it alive for the whole run: dropping it closes the handle, and the
/// kernel destroys the named object once the last handle (including a crashed holder's)
/// closes, so the next instance can start.
#[cfg(windows)]
#[must_use = "the single-instance lock releases as soon as this guard is dropped"]
pub struct InstanceLock {
    _mutex: std::os::windows::io::OwnedHandle,
}

/// Create (or open) the named mutex for `name`. `Ok(None)` means the object already
/// existed — another live process holds the guard. Existence is the whole signal:
/// ownership (`WaitForSingleObject`) is never taken, so an abandoned mutex can't occur.
#[cfg(windows)]
fn create_instance_mutex(name: &str) -> std::io::Result<Option<std::os::windows::io::OwnedHandle>> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: FFI; `wide` is NUL-terminated and outlives the call.
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) };
    // Snapshot last-error before anything else can touch it. Documented contract: on
    // success it is ERROR_ALREADY_EXISTS iff the named object pre-existed.
    // SAFETY: trivially safe FFI.
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let handle = handle.map_err(std::io::Error::other)?;
    // SAFETY: `CreateMutexW` succeeded, so `handle` is a valid handle we own.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    // Our fresh handle drops here, leaving the holder's object alone.
    if already_exists {
        return Ok(None);
    }
    Ok(Some(handle))
}

/// The per-session mutex name for one of our guards, derived from the app id so it
/// can't collide with other software.
#[cfg(windows)]
fn mutex_name(suffix: &str) -> String {
    format!("Local\\{}.{suffix}", crate::paths::APP_ID)
}

/// The testable core of the Windows [`acquire_recorder_lock`]: take the named mutex,
/// then (re)write our pid to `pid_path` so a peer can find this process.
#[cfg(windows)]
fn acquire_recorder_lock_at(name: &str, pid_path: &Path) -> std::io::Result<Option<InstanceLock>> {
    let Some(mutex) = create_instance_mutex(name)? else {
        return Ok(None);
    };
    std::fs::write(pid_path, format!("{}\n", std::process::id()))?;
    Ok(Some(InstanceLock { _mutex: mutex }))
}

/// Acquire the recorder's single-instance lock (a named mutex), writing our pid to
/// [`recorder_pid_path`] for the settings app's restart path. `Ok(None)` means another
/// live recorder already holds it.
#[cfg(windows)]
pub fn acquire_recorder_lock() -> std::io::Result<Option<InstanceLock>> {
    ensure_instance_dir(&instance_dir())?;
    acquire_recorder_lock_at(&mutex_name("recorder"), &recorder_pid_path())
}

/// Acquire the settings app's single-instance lock (a named mutex). `Ok(None)` means a
/// settings window is already open. The holder watches the instance dir for forwarded
/// activations, so the dir is created here too — but only best-effort, after the mutex: a
/// filesystem failure must degrade the forwarding, never the guard itself.
#[cfg(windows)]
pub fn acquire_settings_lock() -> std::io::Result<Option<InstanceLock>> {
    let Some(mutex) = create_instance_mutex(&mutex_name("settings"))? else {
        return Ok(None);
    };
    if let Err(e) = ensure_instance_dir(&instance_dir()) {
        tracing::warn!(error = %e, "could not create the instance dir; activation forwarding unavailable");
    }
    Ok(Some(InstanceLock { _mutex: mutex }))
}

/// Whether the settings mutex `name` is held (briefly creates it). Core of [`settings_running`].
#[cfg(windows)]
fn settings_running_named(name: &str) -> bool {
    !matches!(create_instance_mutex(name), Ok(Some(_)))
}

/// Whether a settings window is open. Errors count as open: callers must not disturb a live one.
#[cfg(windows)]
#[must_use]
pub fn settings_running() -> bool {
    settings_running_named(&mutex_name("settings"))
}

// No guard on other targets; stubs keep the public API total so callers need no `#[cfg]`.
#[cfg(not(any(unix, windows)))]
pub struct InstanceLock;

#[cfg(not(any(unix, windows)))]
pub fn acquire_recorder_lock() -> std::io::Result<Option<InstanceLock>> {
    Ok(Some(InstanceLock))
}

#[cfg(not(any(unix, windows)))]
pub fn acquire_settings_lock() -> std::io::Result<Option<InstanceLock>> {
    Ok(Some(InstanceLock))
}

#[cfg(not(any(unix, windows)))]
pub fn settings_running() -> bool {
    false
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// A mutex name unique to one test, so parallel tests (and reruns against a
    /// crashed run's leftovers) never collide.
    fn unique_name(tag: &str) -> String {
        format!("Local\\rewynd-test-{}-{tag}", std::process::id())
    }

    #[test]
    fn instance_mutex_is_exclusive_and_releases_on_drop() {
        let name = unique_name("exclusive");
        let first = create_instance_mutex(&name)
            .expect("io ok")
            .expect("first acquires");
        assert!(
            create_instance_mutex(&name).expect("io ok").is_none(),
            "a second mutex with the same name is refused"
        );
        drop(first);
        assert!(
            create_instance_mutex(&name).expect("io ok").is_some(),
            "the guard is free once the first holder drops it"
        );
    }

    #[test]
    fn recorder_lock_writes_the_current_pid_and_is_exclusive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.pid");
        let name = unique_name("recorder");
        let lock = acquire_recorder_lock_at(&name, &path)
            .expect("io ok")
            .expect("acquires");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read pid"),
            format!("{}\n", std::process::id())
        );
        assert!(
            acquire_recorder_lock_at(&name, &path)
                .expect("io ok")
                .is_none(),
            "a peer is refused while the lock is held"
        );
        drop(lock);
    }

    #[test]
    fn settings_probe_tracks_the_mutex_holder() {
        let name = unique_name("settings-probe");
        assert!(
            !settings_running_named(&name),
            "a free mutex reads as not running"
        );
        let held = create_instance_mutex(&name)
            .expect("io ok")
            .expect("acquires");
        assert!(
            settings_running_named(&name),
            "a held mutex reads as running"
        );
        drop(held);
        assert!(
            !settings_running_named(&name),
            "a released mutex reads as not running"
        );
    }

    #[test]
    fn distinct_names_do_not_contend() {
        let a = create_instance_mutex(&unique_name("a"))
            .expect("io ok")
            .expect("acquires");
        let b = create_instance_mutex(&unique_name("b"))
            .expect("io ok")
            .expect("acquires independently");
        drop((a, b));
    }
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
    fn settings_probe_tracks_the_lock_holder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.lock");
        assert!(
            !settings_running_at(&path),
            "free lock reads as not running"
        );
        let held = lock_file(&path).expect("io ok").expect("acquires");
        assert!(settings_running_at(&path), "held lock reads as running");
        drop(held);
        assert!(
            !settings_running_at(&path),
            "released lock reads as not running"
        );
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
