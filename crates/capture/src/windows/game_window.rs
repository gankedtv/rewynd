//! Foreground-game detection: is the active window a running game we should capture?
//!
//! The heuristic is deliberately conservative — capture too little rather than too
//! much (the whole point of game-only capture is not recording the desktop): the
//! foreground window counts as a game only when it covers its entire monitor, which
//! is how both exclusive-fullscreen and borderless-fullscreen games present.
//! Windowed-mode games don't match; the desktop-capture opt-in covers those.
//!
//! Covering the monitor alone is not enough, because a fullscreen video, a stream
//! viewer or any maximized window on a monitor without a taskbar looks identical:
//! known non-game apps are rejected by process name, and a maximized window that
//! still shows its title bar is a windowed app rather than a fullscreen game.
//!
//! Detection is not the end of it. [`Latch`] re-checks the captured window while the
//! session runs, against the same [`WindowState`] the detector uses, and releases it
//! once it has not been fullscreen for a grace period — so a window that leaves
//! fullscreen hands the recorder back to the game it was blocking. A minimized window
//! gets a much longer grace (WGC delivers nothing while minimized, and a release
//! restarts the replay buffer) but not an unlimited one. Losing focus is not a state
//! at all: the latch never asks which window is in front.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetWindowLongPtrW, IsIconic, IsWindowVisible, WS_CAPTION, WS_MAXIMIZE,
};
use windows_capture::window::Window;

/// Shell/system processes that legitimately own monitor-sized foreground windows
/// (the desktop itself, the lock screen, task switching) and must never be latched
/// onto as "the game" — nor should rewynd capture itself.
const SHELL_PROCESSES: &[&str] = &[
    "explorer.exe",
    "searchhost.exe",
    "startmenuexperiencehost.exe",
    "shellexperiencehost.exe",
    "applicationframehost.exe",
    "lockapp.exe",
    "dwm.exe",
    "rewynd.exe",
    "rewynd-recorder.exe",
];

/// Apps that routinely present a window covering the whole monitor but are never the
/// game: browsers (a fullscreen video), media players, chat clients showing a stream,
/// storefronts and launchers, remote-desktop viewers and other recorders. Remote
/// *play* clients (Parsec, Moonlight) are deliberately absent — there the user is
/// playing a game.
const NON_GAME_PROCESSES: &[&str] = &[
    // Browsers
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
    "chromium.exe",
    "librewolf.exe",
    "waterfox.exe",
    "zen.exe",
    "floorp.exe",
    "arc.exe",
    // Media players
    "vlc.exe",
    "mpv.exe",
    "mpvnet.exe",
    "mpc-hc.exe",
    "mpc-hc64.exe",
    "mpc-be.exe",
    "mpc-be64.exe",
    "potplayermini.exe",
    "potplayermini64.exe",
    "wmplayer.exe",
    "kodi.exe",
    "plex.exe",
    "stremio.exe",
    "jellyfinmediaplayer.exe",
    // Chat / conferencing
    "discord.exe",
    "discordptb.exe",
    "discordcanary.exe",
    "zoom.exe",
    "teams.exe",
    "ms-teams.exe",
    "slack.exe",
    "skype.exe",
    "telegram.exe",
    // Storefronts and launchers
    "steam.exe",
    "steamwebhelper.exe",
    "epicgameslauncher.exe",
    "battle.net.exe",
    "galaxyclient.exe",
    "upc.exe",
    "ubisoftconnect.exe",
    "eadesktop.exe",
    "riotclientux.exe",
    "playnite.fullscreenapp.exe",
    "playnite.desktopapp.exe",
    // Remote desktop
    "mstsc.exe",
    "msrdc.exe",
    "anydesk.exe",
    "teamviewer.exe",
    "vncviewer.exe",
    // Other recorders
    "obs64.exe",
    "obs32.exe",
];

/// Why a process is disqualified. The two lists are kept apart so the probe can name
/// which rule rejected a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exclusion {
    Shell,
    NonGame,
}

/// Screensavers run fullscreen over everything and always end in this suffix.
const SCREENSAVER_SUFFIX: &str = ".scr";

fn process_exclusion(name: &str) -> Option<Exclusion> {
    let name = name.to_ascii_lowercase();
    if SHELL_PROCESSES.contains(&name.as_str()) {
        return Some(Exclusion::Shell);
    }
    if NON_GAME_PROCESSES.contains(&name.as_str()) || name.ends_with(SCREENSAVER_SUFFIX) {
        return Some(Exclusion::NonGame);
    }
    None
}

/// Whether `rect` covers all of `bounds` (a borderless window may hang a pixel over,
/// so "covers" is `<=`/`>=`, not equality).
fn rect_covers(rect: RECT, bounds: RECT) -> bool {
    rect.left <= bounds.left
        && rect.top <= bounds.top
        && rect.right >= bounds.right
        && rect.bottom >= bounds.bottom
}

/// A maximized window that still draws its title bar is a windowed app spread over a
/// monitor without a taskbar, not a fullscreen game: every engine's fullscreen and
/// borderless mode drops the caption. `WS_CAPTION` is two bits, so it is matched
/// whole — a `WS_POPUP | WS_BORDER` borderless window has no caption.
fn is_fullscreen_style(style: u32) -> bool {
    !((style & WS_CAPTION.0) == WS_CAPTION.0 && (style & WS_MAXIMIZE.0) != 0)
}

/// What a window is doing now. Only [`WindowState::Fullscreen`] qualifies as a game;
/// the rest say why not, which is what both the detector and the latch act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowState {
    /// Covering its monitor, with no title bar in the way.
    Fullscreen,
    /// Minimized: WGC delivers no frames, and the window is one restore away.
    Minimized,
    /// Covering its monitor, but as a maximized window that still draws a title bar.
    Decorated,
    /// Windowed, hidden or gone.
    Lost,
}

/// How long a window must stay out of fullscreen before its session is released.
/// Long enough to ride out the flicker of a display-mode switch, short enough that a
/// window left behind hands the recorder back quickly.
pub(crate) const RELEASE_GRACE: Duration = Duration::from_secs(1);

/// The same, for a window that is merely minimized. Generous, because an alt-tab out
/// of an exclusive-fullscreen game minimizes it and a release costs the replay
/// buffer's continuity — but still bounded, so a game left minimized cannot hold the
/// recorder while another one runs.
pub(crate) const MINIMIZED_GRACE: Duration = Duration::from_secs(30);

/// The hold on a captured window: once it has not been fullscreen for longer than the
/// grace its current state allows, the session ends and detection starts over.
#[derive(Debug, Default)]
pub(crate) struct Latch {
    away_since: Option<Instant>,
}

impl Latch {
    /// `true` while the session should keep running.
    pub(crate) fn observe(&mut self, state: WindowState, now: Instant) -> bool {
        if state == WindowState::Fullscreen {
            self.away_since = None;
            return true;
        }
        // One clock for the whole stretch away from fullscreen: minimizing a window
        // that had already left fullscreen buys the longer grace, not a fresh start.
        let since = *self.away_since.get_or_insert(now);
        let grace = if state == WindowState::Minimized {
            MINIMIZED_GRACE
        } else {
            RELEASE_GRACE
        };
        now.duration_since(since) < grace
    }
}

/// The ordered verdict on a foreground window, shared by the detector and the probe
/// so the diagnostic always reports the decision that was actually made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Game,
    Shell,
    NonGame,
    Minimized,
    Windowed,
    Decorated,
}

impl Verdict {
    fn is_game(self) -> bool {
        self == Self::Game
    }

    fn label(self) -> &'static str {
        match self {
            Self::Game => "YES",
            Self::Shell => "NO (shell process)",
            Self::NonGame => "NO (known non-game app: browser/media player/chat/launcher)",
            Self::Minimized => "NO (minimized)",
            Self::Windowed => "NO (not fullscreen: window does not cover its monitor)",
            Self::Decorated => {
                "NO (maximized window with a title bar: a windowed app, not a fullscreen game)"
            }
        }
    }
}

/// The foreground window when it looks like a running game.
pub(crate) fn fullscreen_game_window() -> Option<Window> {
    let window = Window::foreground().ok()?;
    if !window.is_valid() {
        return None;
    }
    classify(&window).is_game().then_some(window)
}

fn classify(window: &Window) -> Verdict {
    // Anti-cheat-protected games (Vanguard, EAC, ...) refuse OpenProcess, so a
    // failed name query must NOT disqualify — it is in fact a strong game signal.
    // The processes the lists guard against are always queryable.
    if let Ok(process) = window.process_name() {
        match process_exclusion(&process) {
            Some(Exclusion::Shell) => return Verdict::Shell,
            Some(Exclusion::NonGame) => return Verdict::NonGame,
            None => {}
        }
    }
    match window_state(window) {
        WindowState::Fullscreen => Verdict::Game,
        WindowState::Minimized => Verdict::Minimized,
        WindowState::Decorated => Verdict::Decorated,
        WindowState::Lost => Verdict::Windowed,
    }
}

/// The window's style bits, or 0 when they can't be read (which never disqualifies).
fn window_style(window: &Window) -> u32 {
    // SAFETY: FFI; a stale HWND yields 0, which reads as "no style bits".
    let style = unsafe { GetWindowLongPtrW(HWND(window.as_raw_hwnd()), GWL_STYLE) };
    style as u32
}

/// Where the window stands right now. The detector and the latch share it, so a
/// window can never be kept under a rule that would not have latched it.
pub(crate) fn window_state(window: &Window) -> WindowState {
    let hwnd = HWND(window.as_raw_hwnd());
    // SAFETY: FFI; both calls tolerate a destroyed HWND (they report false).
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    // SAFETY: FFI.
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    window_state_from(
        visible,
        minimized,
        covers_its_monitor(window),
        window_style(window),
    )
}

fn window_state_from(visible: bool, minimized: bool, covers: bool, style: u32) -> WindowState {
    if !visible {
        return WindowState::Lost;
    }
    // A minimized window reports a far-offscreen rect, so it must be recognised
    // before the geometry is consulted.
    if minimized {
        return WindowState::Minimized;
    }
    if !covers {
        return WindowState::Lost;
    }
    if is_fullscreen_style(style) {
        WindowState::Fullscreen
    } else {
        WindowState::Decorated
    }
}

/// Whether the window's rect covers its monitor's full bounds.
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
    rect_covers(rect, info.rcMonitor)
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
    let rect = window
        .rect()
        .map(|r| format!("{},{} → {},{}", r.left, r.top, r.right, r.bottom))
        .unwrap_or_else(|e| format!("<unreadable: {e}>"));
    let style = window_style(&window);
    let state = window_state(&window);
    let verdict = classify(&window).label();
    format!("process={process} style=0x{style:08x} rect={rect} state={state:?} → game: {verdict}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use windows::Win32::UI::WindowsAndMessaging::{
        WS_BORDER, WS_CAPTION, WS_MAXIMIZE, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
    };

    const MONITOR: RECT = RECT {
        left: 0,
        top: 0,
        right: 2560,
        bottom: 1440,
    };

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Keep the compiler honest about the lists staying lowercase — the runtime
    /// comparison lowercases the process name only.
    #[test]
    fn excluded_processes_are_lowercase() {
        for p in SHELL_PROCESSES.iter().chain(NON_GAME_PROCESSES) {
            assert_eq!(*p, p.to_ascii_lowercase(), "{p} must be stored lowercase");
        }
    }

    #[test]
    fn shell_processes_are_excluded_case_insensitively() {
        assert_eq!(process_exclusion("explorer.exe"), Some(Exclusion::Shell));
        assert_eq!(process_exclusion("Explorer.EXE"), Some(Exclusion::Shell));
    }

    #[test]
    fn known_non_game_apps_are_excluded() {
        for p in [
            "chrome.exe",
            "firefox.exe",
            "DiscordCanary.exe",
            "vlc.exe",
            "steamwebhelper.exe",
            "mstsc.exe",
        ] {
            assert_eq!(process_exclusion(p), Some(Exclusion::NonGame), "{p}");
        }
    }

    #[test]
    fn screensavers_are_excluded_by_suffix() {
        assert_eq!(process_exclusion("Bubbles.scr"), Some(Exclusion::NonGame));
        assert_eq!(process_exclusion("mystify.scr"), Some(Exclusion::NonGame));
    }

    #[test]
    fn games_and_unknown_processes_are_not_excluded() {
        for p in [
            "eldenring.exe",
            "cs2.exe",
            "VALORANT-Win64-Shipping.exe",
            "",
        ] {
            assert_eq!(process_exclusion(p), None, "{p}");
        }
    }

    #[test]
    fn rect_covers_accepts_fullscreen_and_borderless_overhang() {
        assert!(rect_covers(MONITOR, MONITOR));
        assert!(rect_covers(rect(-1, -1, 2561, 1441), MONITOR));
        // A maximized decorated window hangs its frame over the monitor: geometry
        // accepts it, the style rule is what rejects it.
        assert!(rect_covers(rect(-8, -8, 2568, 1448), MONITOR));
    }

    #[test]
    fn rect_covers_rejects_taskbar_minimized_and_other_monitors() {
        assert!(!rect_covers(rect(0, 0, 2560, 1392), MONITOR));
        assert!(!rect_covers(rect(-32000, -32000, -31840, -31970), MONITOR));
        assert!(!rect_covers(rect(2560, 0, 5120, 1440), MONITOR));
    }

    #[test]
    fn fullscreen_and_borderless_styles_are_accepted() {
        assert!(is_fullscreen_style(WS_POPUP.0 | WS_VISIBLE.0));
        // Chromium goes fullscreen by stripping the caption off a maximized window.
        assert!(is_fullscreen_style(
            WS_POPUP.0 | WS_VISIBLE.0 | WS_MAXIMIZE.0
        ));
        // A border alone is not a caption.
        assert!(is_fullscreen_style(
            WS_POPUP.0 | WS_BORDER.0 | WS_MAXIMIZE.0
        ));
        // Not maximized: geometry decides.
        assert!(is_fullscreen_style(WS_OVERLAPPEDWINDOW.0 | WS_VISIBLE.0));
        // Unreadable style must not disqualify.
        assert!(is_fullscreen_style(0));
    }

    #[test]
    fn decorated_maximized_windows_are_rejected() {
        assert!(!is_fullscreen_style(
            WS_OVERLAPPEDWINDOW.0 | WS_VISIBLE.0 | WS_MAXIMIZE.0
        ));
        assert!(!is_fullscreen_style(WS_CAPTION.0 | WS_MAXIMIZE.0));
    }

    #[test]
    fn latch_rides_out_a_brief_loss() {
        let t0 = Instant::now();
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Fullscreen, t0));
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(200)));
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(800)));
        assert!(latch.observe(WindowState::Fullscreen, t0 + Duration::from_millis(1000)));
        // The timer restarts from the new loss, not the old one.
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(1200)));
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(2100)));
        assert!(!latch.observe(WindowState::Lost, t0 + Duration::from_millis(2200)));
    }

    #[test]
    fn latch_keeps_a_briefly_minimized_window() {
        let t0 = Instant::now();
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Fullscreen, t0));
        assert!(latch.observe(WindowState::Minimized, t0 + Duration::from_secs(2)));
        assert!(latch.observe(WindowState::Fullscreen, t0 + Duration::from_secs(10)));
        // Coming back to fullscreen restarts the allowance.
        assert!(latch.observe(
            WindowState::Minimized,
            t0 + Duration::from_secs(10) + MINIMIZED_GRACE - Duration::from_secs(1)
        ));
    }

    #[test]
    fn latch_releases_a_window_left_minimized() {
        let t0 = Instant::now();
        // The allowance runs from the moment the window stopped being fullscreen.
        let away = t0 + Duration::from_secs(1);
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Fullscreen, t0));
        assert!(latch.observe(WindowState::Minimized, away));
        assert!(latch.observe(
            WindowState::Minimized,
            away + MINIMIZED_GRACE - Duration::from_millis(1)
        ));
        assert!(!latch.observe(WindowState::Minimized, away + MINIMIZED_GRACE));
    }

    #[test]
    fn latch_does_not_restart_the_timer_when_a_lost_window_is_minimized() {
        let t0 = Instant::now();
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Lost, t0));
        // Minimizing buys the longer allowance but does not forgive the time already
        // spent away from fullscreen.
        assert!(latch.observe(WindowState::Minimized, t0 + Duration::from_millis(600)));
        assert!(!latch.observe(WindowState::Lost, t0 + Duration::from_millis(1400)));
    }

    #[test]
    fn latch_releases_a_window_that_grew_a_title_bar() {
        let t0 = Instant::now();
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Fullscreen, t0));
        assert!(latch.observe(WindowState::Decorated, t0 + Duration::from_millis(200)));
        assert!(!latch.observe(
            WindowState::Decorated,
            t0 + Duration::from_millis(200) + RELEASE_GRACE
        ));
    }

    #[test]
    fn window_state_reads_visibility_first_then_minimized() {
        assert_eq!(
            window_state_from(false, true, true, WS_POPUP.0),
            WindowState::Lost
        );
        // A minimized window reports a far-offscreen rect, so it must be recognised
        // before the geometry is consulted.
        assert_eq!(
            window_state_from(true, true, false, WS_POPUP.0),
            WindowState::Minimized
        );
    }

    #[test]
    fn window_state_separates_covering_from_decorated_and_windowed() {
        assert_eq!(
            window_state_from(true, false, true, WS_POPUP.0),
            WindowState::Fullscreen
        );
        assert_eq!(
            window_state_from(true, false, true, WS_OVERLAPPEDWINDOW.0 | WS_MAXIMIZE.0),
            WindowState::Decorated
        );
        assert_eq!(
            window_state_from(true, false, false, WS_POPUP.0),
            WindowState::Lost
        );
    }

    #[test]
    fn latch_releases_once_the_grace_has_elapsed() {
        let t0 = Instant::now();
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Lost, t0));
        assert!(!latch.observe(WindowState::Lost, t0 + RELEASE_GRACE));
    }

    #[test]
    fn only_a_game_verdict_reads_as_yes() {
        assert!(Verdict::Game.is_game());
        assert_eq!(Verdict::Game.label(), "YES");
        for verdict in [
            Verdict::Shell,
            Verdict::NonGame,
            Verdict::Minimized,
            Verdict::Windowed,
            Verdict::Decorated,
        ] {
            assert!(!verdict.is_game(), "{verdict:?}");
            assert!(verdict.label().starts_with("NO ("), "{verdict:?}");
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
