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
        Some(pid) => stop_process(pid, "rewynd", term_wait, kill_wait),
        None => Ok(true),
    }
}

#[cfg(not(unix))]
pub fn stop_recorder(_term_wait: Duration, _kill_wait: Duration) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "recorder process control is unix-only",
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
