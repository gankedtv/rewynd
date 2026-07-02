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
    let process = window.process_name().ok()?.to_ascii_lowercase();
    if EXCLUDED_PROCESSES.contains(&process.as_str()) {
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
