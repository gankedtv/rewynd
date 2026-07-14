//! Driving the real Velopack Setup.exe: stage the payload, run it silently, then launch the
//! installed app — Setup.exe skips its own end-of-install launch in silent mode.

use std::path::{Path, PathBuf};

/// The Setup.exe bytes packed into this binary at release time (`REWYND_SETUP_EXE` in
/// `build.rs`); empty in dev builds, which fall back to a Setup.exe beside the installer.
static PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/setup-payload.bin"));

/// The published Setup.exe asset name, used for the dev-build sibling fallback.
const SETUP_FILE: &str = "rewynd-win-Setup.exe";

/// How much of the Setup.exe log tail to surface on a failure.
const LOG_TAIL_CHARS: usize = 600;

/// Run the whole install: stage Setup.exe, run it with `--silent`, and start the installed
/// app. Blocking — run it off the UI thread. The error string is shown to the user verbatim.
pub fn run() -> Result<(), String> {
    let work = std::env::temp_dir().join(format!("rewynd-installer-{}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| format!("could not create a work folder: {e}"))?;
    let result = run_in(&work);
    // The staged Setup.exe and log are only diagnostics once we're done.
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// The install steps against an existing work dir, so cleanup lives in one place above.
fn run_in(work: &Path) -> Result<(), String> {
    let setup = resolve_setup(work).map_err(|e| format!("no Setup.exe to run: {e}"))?;
    let log = work.join("install.log");
    let status = std::process::Command::new(&setup)
        .arg("--silent")
        .arg("--log")
        .arg(&log)
        .status()
        .map_err(|e| format!("could not start Setup.exe: {e}"))?;
    if !status.success() {
        return Err(format!("the install did not finish\n{}", log_tail(&log)));
    }
    launch_installed()
}

/// The Setup.exe to run: the embedded payload staged into `work`, or — when the payload is
/// empty (a dev build) — one sitting beside our own exe.
fn resolve_setup(work: &Path) -> std::io::Result<PathBuf> {
    if let Some(staged) = staged_payload(work, PAYLOAD)? {
        return Ok(staged);
    }
    std::env::current_exe()?
        .parent()
        .and_then(sibling_setup)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("this build embeds no payload and no {SETUP_FILE} sits beside it"),
            )
        })
}

/// Write a non-empty embedded payload into `work`; `None` when this is a payload-less dev
/// build.
fn staged_payload(work: &Path, payload: &[u8]) -> std::io::Result<Option<PathBuf>> {
    if payload.is_empty() {
        return Ok(None);
    }
    let staged = work.join(SETUP_FILE);
    std::fs::write(&staged, payload)?;
    Ok(Some(staged))
}

/// A real Setup.exe in `dir`, if one is there.
fn sibling_setup(dir: &Path) -> Option<PathBuf> {
    Some(dir.join(SETUP_FILE)).filter(|p| p.is_file())
}

/// The exe Velopack laid down: `%LocalAppData%\rewynd\current\rewynd.exe`. Split out so the
/// path shape is testable without an install.
fn installed_app(local_app_data: &Path) -> PathBuf {
    local_app_data
        .join("rewynd")
        .join("current")
        .join("rewynd.exe")
}

/// Start the freshly installed app, detached; the first-run wizard takes over from here.
fn launch_installed() -> Result<(), String> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA is not set".to_owned())?;
    let app = installed_app(&local);
    if !app.is_file() {
        return Err(format!("installed but {} is missing", app.display()));
    }
    std::process::Command::new(&app)
        .current_dir(app.parent().expect("exe path has a parent"))
        // Fully detached: inherited pipes would tie the app's lifetime to whoever ran us.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("installed, but rewynd would not start: {e}"))?;
    Ok(())
}

/// The last chunk of the Setup.exe log, for the failure screen; empty when unreadable.
fn log_tail(log: &Path) -> String {
    let Ok(content) = std::fs::read_to_string(log) else {
        return String::new();
    };
    let mut tail_start = content.len().saturating_sub(LOG_TAIL_CHARS);
    // Cut on a character, then on a line.
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail = &content[tail_start..];
    match tail.split_once('\n') {
        Some((_, rest)) if tail_start > 0 => rest.trim().to_owned(),
        _ => tail.trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_empty_payload_is_staged_into_the_work_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = staged_payload(dir.path(), b"payload-bytes")
            .expect("stage")
            .expect("non-empty stages");
        assert_eq!(staged, dir.path().join(SETUP_FILE));
        assert_eq!(std::fs::read(&staged).expect("read"), b"payload-bytes");
    }

    #[test]
    fn an_empty_payload_stages_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(staged_payload(dir.path(), b"").expect("io ok"), None);
    }

    #[test]
    fn the_sibling_fallback_needs_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(sibling_setup(dir.path()), None);
        std::fs::write(dir.path().join(SETUP_FILE), "exe").expect("plant");
        assert_eq!(sibling_setup(dir.path()), Some(dir.path().join(SETUP_FILE)));
    }

    #[test]
    fn installed_app_follows_the_velopack_layout() {
        let app = installed_app(Path::new("local"));
        assert_eq!(
            app,
            Path::new("local")
                .join("rewynd")
                .join("current")
                .join("rewynd.exe")
        );
    }

    /// The Install button's exact code path, live: stages the sibling Setup.exe, runs the
    /// real per-user install, and starts the installed app. Needs `rewynd-win-Setup.exe`
    /// beside the test binary, so it stays ignored in CI like the other live-environment
    /// tests.
    #[test]
    #[ignore = "runs a real Velopack install; needs rewynd-win-Setup.exe beside the test exe"]
    fn real_install_end_to_end() {
        run().expect("the full silent install succeeds");
    }

    #[test]
    fn log_tail_returns_the_end_of_the_log_on_line_boundaries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("install.log");
        let long_line = "x".repeat(LOG_TAIL_CHARS);
        std::fs::write(&log, format!("first line\n{long_line}\nlast line")).expect("write");
        let tail = log_tail(&log);
        assert!(tail.ends_with("last line"));
        assert!(!tail.contains("first line"), "the head is dropped");

        assert_eq!(log_tail(&dir.path().join("missing.log")), "");
    }
}
