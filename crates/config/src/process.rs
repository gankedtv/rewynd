//! Recorder process control: read the pid file and stop a running recorder. The comm and
//! start-time identity checks ensure a stale or reused pid is never signalled.

use std::time::Duration;

use crate::paths::recorder_pid_path;

/// The recorder's pid from the pid file's first line, if readable and parseable.
#[must_use]
pub fn read_recorder_pid() -> Option<u32> {
    parse_pid(&std::fs::read_to_string(recorder_pid_path()).ok()?)
}

/// First line of a pid file, trimmed and parsed. The file is newline-framed, so the first line
/// is a clean pid even mid-rewrite.
fn parse_pid(contents: &str) -> Option<u32> {
    contents.lines().next()?.trim().parse().ok()
}

/// Field 22 (start time in clock ticks) of `/proc/<pid>/stat` content: with the pid, a stable
/// identity that distinguishes the original process from a reused pid.
#[cfg(unix)]
fn parse_start_time(stat: &str) -> Option<String> {
    // comm (field 2) is parenthesised and may itself contain spaces/parens, so resume after the
    // last ')': the remaining tokens start at field 3, putting start time at index 19.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
}

#[cfg(unix)]
fn proc_start_time(pid: u32) -> Option<String> {
    parse_start_time(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Whether the original process (pid + start-time identity) is still running.
#[cfg(unix)]
fn still_running(pid: u32, identity: Option<&str>) -> bool {
    match identity {
        Some(start) => proc_start_time(pid).as_deref() == Some(start),
        // No identity captured: fall back to bare pid existence.
        None => std::path::Path::new(&format!("/proc/{pid}")).exists(),
    }
}

/// Poll until the process has exited (or its pid was reused) or the timeout elapses.
#[cfg(unix)]
fn wait_for_exit(pid: u32, identity: Option<&str>, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !still_running(pid, identity) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    !still_running(pid, identity)
}

/// `kill(2)` a single process: `Ok(true)` signal sent, `Ok(false)` already gone (ESRCH); other
/// failures (e.g. EPERM) are errors.
#[cfg(unix)]
fn send_signal(pid: libc::pid_t, signal: libc::c_int) -> std::io::Result<bool> {
    // SAFETY: plain syscall; the caller guarantees `pid` is positive (a single process).
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(err)
}

/// Stop the running recorder, if any: SIGTERM it and wait up to `term_wait` for it to exit (so
/// it drops the global hotkey, ScreenCast portal, and single-instance lock), escalating to
/// SIGKILL with a further `kill_wait`. `Ok(true)` when it is gone (or none was running);
/// `Ok(false)` when it survived both signals.
#[cfg(unix)]
pub fn stop_recorder(term_wait: Duration, kill_wait: Duration) -> std::io::Result<bool> {
    match read_recorder_pid() {
        Some(pid) => stop_process(pid, "rewynd-recorder", term_wait, kill_wait),
        None => Ok(true),
    }
}

/// Ask the running recorder to save a clip now, via SIGUSR1 — used by the onboarding wizard's
/// test-clip step so the user need not press the just-configured hotkey. Verifies the pid is
/// actually the recorder (its `/proc/<pid>/comm`) before signalling, so a reused pid is never hit.
/// `Ok(true)` when a save was requested, `Ok(false)` when no recorder is running.
#[cfg(unix)]
pub fn request_recorder_save() -> std::io::Result<bool> {
    let Some(pid) = read_recorder_pid() else {
        return Ok(false);
    };
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return Ok(false);
    };
    if raw <= 0 || !recorder_alive(pid) {
        return Ok(false);
    }
    send_signal(raw, libc::SIGUSR1)
}

/// No per-process save signal on Windows; the wizard falls back to the hotkey there.
#[cfg(not(unix))]
pub fn request_recorder_save() -> std::io::Result<bool> {
    Ok(false)
}

/// Whether `pid` is a live recorder process. The identity check (a reused pid must never read
/// as alive) mirrors [`stop_process`]: `/proc/<pid>/comm` on unix, the image file name on
/// Windows. Used by the status reader to reject a stale `status.json` after a crash.
#[cfg(unix)]
pub(crate) fn recorder_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|comm| comm.trim() == "rewynd-recorder")
}

/// The file name of `handle`'s process image (the Windows analog of `/proc/<pid>/comm`), or
/// `None` if the query fails. Shared by [`recorder_alive`] and [`stop_process`] so the identity
/// FFI lives in one place.
#[cfg(windows)]
fn process_image_file_name(handle: windows::Win32::Foundation::HANDLE) -> Option<String> {
    use windows::Win32::System::Threading::{PROCESS_NAME_WIN32, QueryFullProcessImageNameW};
    use windows::core::PWSTR;

    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: FFI; `buf`/`len` describe a valid buffer and `handle` is a live process handle.
    if unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_err()
    {
        return None;
    }
    let full = String::from_utf16_lossy(&buf[..len as usize]);
    std::path::Path::new(&full)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(windows)]
pub(crate) fn recorder_alive(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: FFI; a failed open (process gone / no access) reads as not-alive.
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    // SAFETY: `OpenProcess` succeeded, so `handle` is a valid handle we own.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let raw = HANDLE(handle.as_raw_handle());
    process_image_file_name(raw)
        .is_some_and(|name| name.eq_ignore_ascii_case("rewynd-recorder.exe"))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn recorder_alive(_pid: u32) -> bool {
    false
}

/// Name of the per-session stop event the recorder waits on — the Windows stand-in for
/// SIGTERM. Signaling it asks the (single-instance) recorder to shut down cleanly.
#[cfg(windows)]
fn stop_event_name() -> String {
    format!("Local\\{}.stop", crate::paths::APP_ID)
}

#[cfg(windows)]
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The recorder's stop event, created at startup and waited on for the settings app's
/// restart request. Manual-reset, so a signal that lands early is never lost.
#[cfg(windows)]
pub struct RecorderStopEvent {
    handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl RecorderStopEvent {
    /// Create (or open) the named stop event. Call once at recorder startup, before
    /// threads spawn, so a restart request can never race past an unarmed event.
    pub fn create() -> std::io::Result<Self> {
        Self::create_named(&stop_event_name())
    }

    /// The testable core of [`create`](Self::create).
    fn create_named(name: &str) -> std::io::Result<Self> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows::Win32::System::Threading::CreateEventW;
        use windows::core::PCWSTR;

        let name = wide(name);
        // SAFETY: FFI; `name` is NUL-terminated and outlives the call.
        let handle = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }
            .map_err(std::io::Error::other)?;
        // SAFETY: `CreateEventW` succeeded, so `handle` is a valid handle we own.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        Ok(Self { handle })
    }

    /// Block until the event is signaled (a stop request arrives).
    pub fn wait(&self) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};

        // SAFETY: FFI; the handle is valid for `self`'s lifetime.
        let _ = unsafe { WaitForSingleObject(HANDLE(self.handle.as_raw_handle()), INFINITE) };
    }
}

/// Signal the stop event named `name`, if some process is keeping it alive (an absent
/// event means no recorder is running — not an error).
#[cfg(windows)]
fn signal_stop_event_named(name: &str) -> std::io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
    use windows::core::PCWSTR;

    let name = wide(name);
    // SAFETY: FFI; `name` is NUL-terminated and outlives the call.
    let handle = match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) } {
        Ok(handle) => handle,
        // No event object → no recorder waiting on it.
        Err(_) => return Ok(()),
    };
    // SAFETY: `OpenEventW` succeeded, so `handle` is a valid handle we now own.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    // SAFETY: FFI; the owned handle is valid.
    unsafe { SetEvent(HANDLE(handle.as_raw_handle())) }.map_err(std::io::Error::other)
}

/// Stop the running recorder, if any: signal its stop event and wait up to `term_wait`
/// for the process to exit, escalating to `TerminateProcess` with a further `kill_wait`.
/// `Ok(true)` when it is gone (or none was running); `Ok(false)` when it survived both.
#[cfg(windows)]
pub fn stop_recorder(term_wait: Duration, kill_wait: Duration) -> std::io::Result<bool> {
    signal_stop_event_named(&stop_event_name())?;
    match read_recorder_pid() {
        Some(pid) => stop_process(pid, "rewynd-recorder.exe", term_wait, kill_wait),
        None => Ok(true),
    }
}

/// The testable core of the Windows [`stop_recorder`]: only touches `pid` while it is an
/// `expected_image` process (the image-name check guards against a reused pid).
#[cfg(windows)]
fn stop_process(
    pid: u32,
    expected_image: &str,
    term_wait: Duration,
    kill_wait: Duration,
) -> std::io::Result<bool> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        TerminateProcess, WaitForSingleObject,
    };

    // SAFETY: FFI; a failed open (process gone, or a reused pid we lack access to)
    // means there is nothing of ours to stop.
    let Ok(handle) = (unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            false,
            pid,
        )
    }) else {
        return Ok(true);
    };
    // SAFETY: `OpenProcess` succeeded, so `handle` is a valid handle we own.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let raw = HANDLE(handle.as_raw_handle());

    // Identity check: a reused pid must never be signalled. The image's file name is
    // the Windows analog of /proc/<pid>/comm.
    let is_ours =
        process_image_file_name(raw).is_some_and(|name| name.eq_ignore_ascii_case(expected_image));
    if !is_ours {
        return Ok(true);
    }

    // SAFETY: FFI; waits on our own valid handle.
    if unsafe { WaitForSingleObject(raw, wait_millis(term_wait)) } == WAIT_OBJECT_0 {
        return Ok(true);
    }
    // Same process outlived the stop request — escalate so a replacement isn't refused
    // by the single-instance mutex the old process still holds.
    // SAFETY: FFI; terminating the verified-ours process.
    let _ = unsafe { TerminateProcess(raw, 1) };
    // SAFETY: FFI; waits on our own valid handle.
    Ok(unsafe { WaitForSingleObject(raw, wait_millis(kill_wait)) } == WAIT_OBJECT_0)
}

/// A `Duration` as the millisecond timeout `WaitForSingleObject` takes, saturating.
#[cfg(windows)]
fn wait_millis(wait: Duration) -> u32 {
    u32::try_from(wait.as_millis()).unwrap_or(u32::MAX)
}

#[cfg(not(any(unix, windows)))]
pub fn stop_recorder(_term_wait: Duration, _kill_wait: Duration) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "recorder process control is not supported on this platform",
    ))
}

/// The testable core of [`stop_recorder`]: only signals `pid` while it is an `expected_comm`
/// process, pinning its start-time identity so a pid reused mid-shutdown is never hit.
#[cfg(unix)]
fn stop_process(
    pid: u32,
    expected_comm: &str,
    term_wait: Duration,
    kill_wait: Duration,
) -> std::io::Result<bool> {
    // kill(0)/kill(-1) target whole process groups; a corrupt pid must never reach them.
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return Ok(true);
    };
    if raw <= 0 {
        return Ok(true);
    }
    if !std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|c| c.trim() == expected_comm)
    {
        return Ok(true);
    }
    let identity = proc_start_time(pid);
    if !send_signal(raw, libc::SIGTERM)? {
        return Ok(true);
    }
    if wait_for_exit(pid, identity.as_deref(), term_wait) {
        return Ok(true);
    }
    // Same identity outlived SIGTERM — escalate so a replacement isn't refused by the lock the
    // old process still holds.
    if !send_signal(raw, libc::SIGKILL)? {
        return Ok(true);
    }
    Ok(wait_for_exit(pid, identity.as_deref(), kill_wait))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn unique_event(tag: &str) -> String {
        format!("Local\\rewynd-test-{}-{tag}", std::process::id())
    }

    #[test]
    fn stop_event_signals_a_waiting_holder() {
        let name = unique_event("signal");
        let event = RecorderStopEvent::create_named(&name).expect("create");
        signal_stop_event_named(&name).expect("signal");
        // Manual-reset + already signaled: returns immediately instead of blocking.
        event.wait();
    }

    #[test]
    fn signaling_an_absent_event_is_fine() {
        signal_stop_event_named(&unique_event("absent")).expect("no recorder → Ok");
    }

    /// Spawn a long-running `ping` child (a stable, always-present system binary).
    fn spawn_ping() -> std::process::Child {
        std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping")
    }

    #[test]
    fn stop_process_terminates_a_matching_child() {
        let mut child = spawn_ping();
        let pid = child.id();
        // `ping` never listens to our stop event, so the graceful wait must elapse and
        // the terminate escalation must end it.
        let gone = stop_process(
            pid,
            "ping.exe",
            Duration::from_millis(200),
            Duration::from_secs(5),
        )
        .expect("stop ok");
        assert!(gone, "terminate escalation ends the child");
        child.wait().expect("reap");
    }

    #[test]
    fn stop_process_refuses_an_image_mismatch() {
        let mut child = spawn_ping();
        let gone = stop_process(
            child.id(),
            "rewynd.exe",
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .expect("no signal sent");
        assert!(gone, "a mismatched image reports done without terminating");
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "the child was not terminated"
        );
        child.kill().expect("cleanup");
        child.wait().expect("reap");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pid_takes_the_first_line() {
        assert_eq!(parse_pid("1234\n"), Some(1234));
        assert_eq!(parse_pid(" 42 \nleftover-tail"), Some(42));
        assert_eq!(parse_pid(""), None);
        assert_eq!(parse_pid("not-a-pid\n"), None);
        assert_eq!(parse_pid("-5\n"), None, "negative pids don't parse as u32");
    }

    #[cfg(unix)]
    #[test]
    fn parse_start_time_survives_hostile_comm() {
        // comm may hold spaces and parens; fields resume after the LAST ')'.
        let stat = "123 ((a) b(c)) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 START 23 24 25";
        assert_eq!(parse_start_time(stat).as_deref(), Some("START"));
        assert_eq!(
            parse_start_time("123 (short) S 1 2"),
            None,
            "too few fields"
        );
        assert_eq!(parse_start_time("no parens at all"), None);
    }

    #[cfg(unix)]
    #[test]
    fn parse_start_time_matches_our_own_stat() {
        let pid = std::process::id();
        let start = proc_start_time(pid).expect("own stat parses");
        assert!(
            start.chars().all(|c| c.is_ascii_digit()),
            "start time is numeric: {start}"
        );
    }

    /// Spawn `sleep 30` and wait until `/proc/<pid>/comm` reads `sleep` (post-exec).
    #[cfg(unix)]
    fn spawn_sleep() -> std::process::Child {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|c| c.trim() == "sleep")
        {
            assert!(
                std::time::Instant::now() < deadline,
                "child never exec'd sleep"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    #[cfg(unix)]
    #[test]
    fn stop_process_terminates_a_matching_child() {
        let mut child = spawn_sleep();
        let pid = child.id();
        // Reap concurrently: an unreaped zombie keeps its /proc entry and would read as running.
        let reaper = std::thread::spawn(move || child.wait());
        let gone = stop_process(pid, "sleep", Duration::from_secs(5), Duration::from_secs(2))
            .expect("signal ok");
        assert!(gone, "SIGTERM ends the child");
        reaper.join().expect("join").expect("reap");
    }

    #[cfg(unix)]
    #[test]
    fn stop_process_refuses_a_comm_mismatch() {
        let mut child = spawn_sleep();
        let gone = stop_process(
            child.id(),
            "rewynd",
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .expect("no signal sent");
        assert!(gone, "a mismatched comm reports done without signalling");
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "the child was not signalled"
        );
        child.kill().expect("cleanup");
        child.wait().expect("reap");
    }
}
