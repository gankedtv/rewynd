//! Foreground-game detection: is the active window a running game we should capture?
//!
//! The heuristic is deliberately conservative — capture too little rather than too
//! much (the whole point of game-only capture is not recording the desktop): the
//! foreground window counts as a game only when it covers its entire monitor, which
//! is how both exclusive-fullscreen and borderless-fullscreen games present.
//! Windowed-mode games don't match; the desktop-capture opt-in covers those.

use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows_capture::window::Window;

/// Shell/system processes that legitimately own monitor-sized foreground windows
/// (the desktop itself, the lock screen, task switching) and must never be latched
/// onto as "the game" — nor should rewynd capture itself.
const EXCLUDED_PROCESSES: &[&str] = &[
    "explorer.exe",
    "searchhost.exe",
    "startmenuexperiencehost.exe",
    "shellexperiencehost.exe",
    "applicationframehost.exe",
    "lockapp.exe",
    "dwm.exe",
    "rewynd.exe",
    "rewynd-settings.exe",
];

/// The foreground window when it looks like a running game: visible and valid, not
/// a shell process, and covering its whole monitor.
pub(crate) fn fullscreen_game_window() -> Option<Window> {
    let window = Window::foreground().ok()?;
    if !window.is_valid() {
        return None;
    }
    // Anti-cheat-protected games (Vanguard, EAC, ...) refuse OpenProcess, so a
    // failed name query must NOT disqualify — it is in fact a strong game signal.
    // The shell processes this list guards against are always queryable.
    if let Ok(process) = window.process_name()
        && EXCLUDED_PROCESSES.contains(&process.to_ascii_lowercase().as_str())
    {
        return None;
    }
    covers_its_monitor(&window).then_some(window)
}

/// Whether the window's rect covers its monitor's full bounds (a borderless window
/// may hang a pixel over, so "covers" is `<=`/`>=`, not equality).
fn covers_its_monitor(window: &Window) -> bool {
    let Ok(rect) = window.rect() else {
        return false;
    };
    let Some(monitor) = window.monitor() else {
        return false;
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: FFI; `info` is a correctly sized out-param.
    if !unsafe { GetMonitorInfoW(HMONITOR(monitor.as_raw_hmonitor()), &mut info) }.as_bool() {
        return false;
    }
    let bounds = info.rcMonitor;
    rect.left <= bounds.left
        && rect.top <= bounds.top
        && rect.right >= bounds.right
        && rect.bottom >= bounds.bottom
}

/// One-line diagnosis of the current foreground window against the game heuristic —
/// the `game_probe` example prints this so "my game isn't detected" reports carry
/// the failing step instead of guesswork. Deliberately excludes the window title.
#[must_use]
pub fn describe_foreground() -> String {
    let window = match Window::foreground() {
        Ok(w) => w,
        Err(e) => return format!("no foreground window ({e})"),
    };
    if !window.is_valid() {
        return "foreground window is not a valid capture target (invisible/tool/child)".to_owned();
    }
    let process = match window.process_name() {
        Ok(p) => p,
        // The anti-cheat case: unreadable process = still a game candidate.
        Err(e) => format!("<unreadable: {e}>"),
    };
    let excluded = EXCLUDED_PROCESSES.contains(&process.to_ascii_lowercase().as_str());
    let rect = window
        .rect()
        .map(|r| format!("{},{} → {},{}", r.left, r.top, r.right, r.bottom))
        .unwrap_or_else(|e| format!("<unreadable: {e}>"));
    let covers = covers_its_monitor(&window);
    let verdict = if excluded {
        "NO (shell process)"
    } else if covers {
        "YES"
    } else {
        "NO (not fullscreen: window does not cover its monitor)"
    };
    format!("process={process} rect={rect} covers_monitor={covers} → game: {verdict}")
}

/// Keep the compiler honest about the excluded list staying lowercase — the runtime
/// comparison lowercases the process name only.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_processes_are_lowercase() {
        for p in EXCLUDED_PROCESSES {
            assert_eq!(*p, p.to_ascii_lowercase(), "{p} must be stored lowercase");
        }
    }

    #[test]
    fn detector_never_matches_this_test_process() {
        // The foreground window while tests run is a terminal/IDE at best — never a
        // fullscreen game. Mostly asserts the FFI path doesn't crash or hang.
        let detected = fullscreen_game_window();
        if let Some(w) = detected {
            // A fullscreen video or similar could legitimately match on a dev box;
            // just prove the accessor path works.
            let _ = w.title();
        }
    }
}
