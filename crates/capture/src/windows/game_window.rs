//! Foreground-game detection: is the active window a running game we should capture?
//!
//! The heuristic is deliberately conservative — capture too little rather than too
//! much (the whole point of game-only capture is not recording the desktop): the
//! foreground window counts as a game only when it covers its entire monitor, which
//! is how both exclusive-fullscreen and borderless-fullscreen games present — or when
//! it is a known windowed game ([`WindowedGames`]: Minecraft built in, more from the
//! config), which qualifies at any size while it is visible. Other windowed-mode games
//! don't match; the desktop-capture opt-in covers those.
//!
//! Known non-game apps and decorated maximized windows are rejected too. A UWP app is
//! judged by the process hosted inside ApplicationFrameHost, not by the host.
//! [`Latch`] applies the same [`WindowState`] to the *captured* window, releasing one
//! that stops qualifying. Losing focus is not a state at all.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GWL_STYLE, GetWindowLongPtrW, IsIconic, IsWindowVisible, WS_CAPTION, WS_MAXIMIZE,
};
use windows::core::{PCWSTR, w};
use windows_capture::window::Window;

/// Shell processes that legitimately own monitor-sized windows, plus rewynd itself.
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

/// Apps that routinely cover the whole monitor but are never the game. Remote *play*
/// clients (Parsec, Moonlight) are deliberately absent: there the user is playing.
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

/// Kept apart so the probe can name which rule rejected a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exclusion {
    Shell,
    NonGame,
}

/// Screensavers run fullscreen over everything and always end in this.
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

/// UWP apps present through this host; the app itself owns the `CoreWindow` child.
const UWP_HOST: &str = "applicationframehost.exe";

/// JVM launchers: Minecraft Java is one of these with a "Minecraft" title.
const JAVA_PROCESSES: &[&str] = &["javaw.exe", "java.exe"];

/// Games commonly played in a window, matched by (lowercase) process name and title. The
/// `Some` is the name the clip folder gets, since the process name would not say it.
fn builtin_windowed_game(process: &str, title: &str) -> Option<&'static str> {
    let title = title.trim_start().to_lowercase();
    if JAVA_PROCESSES.contains(&process) && title.starts_with("minecraft") {
        return Some("Minecraft");
    }
    if process == "minecraft.windows.exe" {
        return Some("Minecraft");
    }
    None
}

/// Which windows count as a game at any size: the built-in rules plus the user's
/// `[capture] windowed_games` entries. An entry ending in `.exe` names a process; any other
/// entry is matched against the window title, as a case-insensitive substring.
#[derive(Debug, Clone, Default)]
pub struct WindowedGames {
    entries: Vec<String>,
}

/// How a window matched [`WindowedGames`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowedMatch {
    /// A built-in rule, carrying the game's name.
    Builtin(&'static str),
    /// A configured `.exe` entry: the user named this process, so it beats the exclusion lists.
    ConfiguredProcess,
    /// A configured title fragment. Titles are ambiguous ("Balatro - YouTube" in a browser,
    /// "Balatro on Steam" in the store), so the exclusion lists still apply.
    ConfiguredTitle,
}

impl WindowedMatch {
    fn overrides_exclusions(self) -> bool {
        self == Self::ConfiguredProcess
    }
}

impl WindowedGames {
    #[must_use]
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            entries: entries
                .into_iter()
                .map(|e| e.as_ref().trim().to_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
        }
    }

    fn matches(&self, process: &str, title: &str) -> Option<WindowedMatch> {
        let process = process.to_ascii_lowercase();
        if let Some(name) = builtin_windowed_game(&process, title) {
            return Some(WindowedMatch::Builtin(name));
        }
        let title = title.to_lowercase();
        let (exe_entries, title_entries): (Vec<_>, Vec<_>) =
            self.entries.iter().partition(|e| e.ends_with(".exe"));
        if exe_entries.iter().any(|entry| process == **entry) {
            return Some(WindowedMatch::ConfiguredProcess);
        }
        title_entries
            .iter()
            .any(|entry| title.contains(entry.as_str()))
            .then_some(WindowedMatch::ConfiguredTitle)
    }
}

/// A borderless window may hang a pixel over, so "covers" is `<=`/`>=`, not equality.
fn rect_covers(rect: RECT, bounds: RECT) -> bool {
    rect.left <= bounds.left
        && rect.top <= bounds.top
        && rect.right >= bounds.right
        && rect.bottom >= bounds.bottom
}

/// Every engine drops the caption in fullscreen and borderless, so caption+maximize is
/// a windowed app on a taskbar-less monitor. `WS_CAPTION` is two bits: match it whole.
fn is_fullscreen_style(style: u32) -> bool {
    !((style & WS_CAPTION.0) == WS_CAPTION.0 && (style & WS_MAXIMIZE.0) != 0)
}

/// Only [`WindowState::Capturable`] qualifies as a game; the rest say why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowState {
    /// Fullscreen, or a known windowed game at any size.
    Capturable,
    Minimized,
    /// Covering its monitor, but still drawing a title bar.
    Decorated,
    /// Windowed, hidden or gone.
    Lost,
}

/// Long enough to ride out a display-mode switch's flicker.
pub(crate) const RELEASE_GRACE: Duration = Duration::from_secs(1);

/// Longer: alt-tab minimizes an exclusive-fullscreen game. Bounded, so one left
/// minimized cannot hold the recorder while another runs.
pub(crate) const MINIMIZED_GRACE: Duration = Duration::from_secs(30);

/// Releases the captured window once it has not qualified for its earned grace.
#[derive(Debug, Default)]
pub(crate) struct Latch {
    away_since: Option<Instant>,
    grace: Duration,
}

impl Latch {
    /// `true` while the session should keep running.
    pub(crate) fn observe(&mut self, state: WindowState, now: Instant) -> bool {
        if state == WindowState::Capturable {
            self.away_since = None;
            self.grace = Duration::ZERO;
            return true;
        }
        // One clock per stretch, keeping the longest grace earned: minimizing forgives
        // nothing, and a window restoring from it can still re-enter fullscreen.
        let since = *self.away_since.get_or_insert(now);
        self.grace = self.grace.max(if state == WindowState::Minimized {
            MINIMIZED_GRACE
        } else {
            RELEASE_GRACE
        });
        now.duration_since(since) < self.grace
    }
}

/// Shared by the detector and the probe, so the diagnostic reports the real decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Game,
    /// A [`WindowedGames`] match, visible and not minimized.
    WindowedGame,
    Shell,
    NonGame,
    Minimized,
    Windowed,
    Decorated,
}

impl Verdict {
    fn is_game(self) -> bool {
        matches!(self, Self::Game | Self::WindowedGame)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Game => "YES",
            Self::WindowedGame => "YES (known windowed game, recorded at any size)",
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

/// The foreground window, found to be a running game.
pub(crate) struct DetectedGame {
    pub(crate) window: Window,
    /// Matched as a windowed game: the latch keeps it at any size.
    pub(crate) windowed: bool,
    /// The process name (the hosted app's for a UWP frame); empty when unreadable.
    pub(crate) process: String,
    /// A built-in rule's game name, which beats the process name for labelling.
    pub(crate) name: Option<&'static str>,
}

/// The foreground window when it looks like a running game.
pub(crate) fn game_window(windowed_games: &WindowedGames) -> Option<DetectedGame> {
    let window = Window::foreground().ok()?;
    if !window.is_valid() {
        return None;
    }
    let process = process_name(&window);
    let (verdict, matched) = classify(&window, &process, windowed_games);
    verdict.is_game().then(|| DetectedGame {
        window,
        windowed: matched.is_some(),
        process,
        name: match matched {
            Some(WindowedMatch::Builtin(name)) => Some(name),
            _ => None,
        },
    })
}

/// The window's process; for a UWP frame, the app hosted in it. Empty when unreadable:
/// anti-cheat games refuse OpenProcess, so that must NOT disqualify.
fn process_name(window: &Window) -> String {
    let Ok(name) = window.process_name() else {
        return String::new();
    };
    if name.eq_ignore_ascii_case(UWP_HOST)
        && let Some(hosted) = hosted_process(window)
    {
        return hosted;
    }
    name
}

fn hosted_process(window: &Window) -> Option<String> {
    // SAFETY: FFI; a stale parent HWND yields an error, not a crash.
    let child = unsafe {
        FindWindowExW(
            Some(HWND(window.as_raw_hwnd())),
            None,
            w!("Windows.UI.Core.CoreWindow"),
            PCWSTR::null(),
        )
    }
    .ok()?;
    Window::from_raw_hwnd(child.0).process_name().ok()
}

fn classify(
    window: &Window,
    process: &str,
    windowed_games: &WindowedGames,
) -> (Verdict, Option<WindowedMatch>) {
    // Compared only, never logged: titles carry documents, URLs and chat context.
    let title = window.title().unwrap_or_default();
    let matched = windowed_games.matches(process, &title);
    if !matched.is_some_and(WindowedMatch::overrides_exclusions) {
        match process_exclusion(process) {
            Some(Exclusion::Shell) => return (Verdict::Shell, None),
            Some(Exclusion::NonGame) => return (Verdict::NonGame, None),
            None => {}
        }
    }
    let verdict = match window_state(window, matched.is_some()) {
        WindowState::Capturable if matched.is_some() => Verdict::WindowedGame,
        WindowState::Capturable => Verdict::Game,
        WindowState::Minimized => Verdict::Minimized,
        WindowState::Decorated => Verdict::Decorated,
        WindowState::Lost => Verdict::Windowed,
    };
    (verdict, matched)
}

/// 0 when the bits can't be read, which never disqualifies.
fn window_style(window: &Window) -> u32 {
    // SAFETY: FFI; a stale HWND yields 0.
    let style = unsafe { GetWindowLongPtrW(HWND(window.as_raw_hwnd()), GWL_STYLE) };
    style as u32
}

/// Shared with the latch, so a window is never kept under a rule that would not latch it.
/// `windowed` is the detector's [`WindowedGames`] verdict: such a window qualifies at any
/// size and with any decoration.
pub(crate) fn window_state(window: &Window, windowed: bool) -> WindowState {
    let hwnd = HWND(window.as_raw_hwnd());
    // SAFETY: FFI; both tolerate a destroyed HWND (they report false).
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    // SAFETY: FFI.
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    window_state_from(
        visible,
        minimized,
        covers_its_monitor(window),
        window_style(window),
        windowed,
    )
}

fn window_state_from(
    visible: bool,
    minimized: bool,
    covers: bool,
    style: u32,
    windowed: bool,
) -> WindowState {
    if !visible {
        return WindowState::Lost;
    }
    // A minimized window reports a far-offscreen rect, so check this before geometry.
    if minimized {
        return WindowState::Minimized;
    }
    if windowed {
        return WindowState::Capturable;
    }
    if !covers {
        return WindowState::Lost;
    }
    if is_fullscreen_style(style) {
        WindowState::Capturable
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
    let process = process_name(&window);
    let rect = window
        .rect()
        .map(|r| format!("{},{} → {},{}", r.left, r.top, r.right, r.bottom))
        .unwrap_or_else(|e| format!("<unreadable: {e}>"));
    let style = window_style(&window);
    // Built-in windowed rules only: the probe has no config.
    let (verdict, matched) = classify(&window, &process, &WindowedGames::default());
    let state = window_state(&window, matched.is_some());
    // The anti-cheat case: unreadable process = still a game candidate.
    let process = if process.is_empty() {
        "<unreadable>"
    } else {
        process.as_str()
    };
    let verdict = verdict.label();
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

    /// The runtime comparison lowercases the process name only.
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
        assert!(is_fullscreen_style(
            WS_POPUP.0 | WS_BORDER.0 | WS_MAXIMIZE.0
        ));
        assert!(is_fullscreen_style(WS_OVERLAPPEDWINDOW.0 | WS_VISIBLE.0));
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
        assert!(latch.observe(WindowState::Capturable, t0));
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(200)));
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(800)));
        assert!(latch.observe(WindowState::Capturable, t0 + Duration::from_millis(1000)));
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(1200)));
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(2100)));
        assert!(!latch.observe(WindowState::Lost, t0 + Duration::from_millis(2200)));
    }

    #[test]
    fn latch_keeps_a_briefly_minimized_window() {
        let t0 = Instant::now();
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Capturable, t0));
        assert!(latch.observe(WindowState::Minimized, t0 + Duration::from_secs(2)));
        assert!(latch.observe(WindowState::Capturable, t0 + Duration::from_secs(10)));
        assert!(latch.observe(
            WindowState::Minimized,
            t0 + Duration::from_secs(10) + MINIMIZED_GRACE - Duration::from_secs(1)
        ));
    }

    #[test]
    fn latch_releases_a_window_left_minimized() {
        let t0 = Instant::now();
        let away = t0 + Duration::from_secs(1);
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Capturable, t0));
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
        // Minimizing widens the allowance, but does not forgive the time already away.
        assert!(latch.observe(WindowState::Minimized, t0 + Duration::from_millis(600)));
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(1400)));
        assert!(!latch.observe(WindowState::Lost, t0 + MINIMIZED_GRACE));
    }

    #[test]
    fn latch_lets_a_window_restoring_from_minimized_reach_fullscreen() {
        let t0 = Instant::now();
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Capturable, t0));
        assert!(latch.observe(WindowState::Minimized, t0 + Duration::from_secs(1)));
        assert!(latch.observe(WindowState::Minimized, t0 + Duration::from_secs(10)));
        // Restoring clears the minimized flag before the window covers its monitor.
        assert!(latch.observe(WindowState::Lost, t0 + Duration::from_millis(10_200)));
        assert!(latch.observe(WindowState::Capturable, t0 + Duration::from_millis(10_600)));
    }

    #[test]
    fn latch_releases_a_window_that_grew_a_title_bar() {
        let t0 = Instant::now();
        let mut latch = Latch::default();
        assert!(latch.observe(WindowState::Capturable, t0));
        assert!(latch.observe(WindowState::Decorated, t0 + Duration::from_millis(200)));
        assert!(!latch.observe(
            WindowState::Decorated,
            t0 + Duration::from_millis(200) + RELEASE_GRACE
        ));
    }

    #[test]
    fn window_state_reads_visibility_first_then_minimized() {
        assert_eq!(
            window_state_from(false, true, true, WS_POPUP.0, false),
            WindowState::Lost
        );
        assert_eq!(
            window_state_from(true, true, false, WS_POPUP.0, false),
            WindowState::Minimized
        );
    }

    #[test]
    fn window_state_separates_covering_from_decorated_and_windowed() {
        assert_eq!(
            window_state_from(true, false, true, WS_POPUP.0, false),
            WindowState::Capturable
        );
        assert_eq!(
            window_state_from(
                true,
                false,
                true,
                WS_OVERLAPPEDWINDOW.0 | WS_MAXIMIZE.0,
                false
            ),
            WindowState::Decorated
        );
        assert_eq!(
            window_state_from(true, false, false, WS_POPUP.0, false),
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
    fn builtin_rules_know_minecraft_java_and_bedrock() {
        let games = WindowedGames::default();
        assert_eq!(
            games.matches("javaw.exe", "Minecraft* 1.21.4 - Singleplayer"),
            Some(WindowedMatch::Builtin("Minecraft"))
        );
        assert_eq!(
            games.matches("Java.exe", "  minecraft 1.8.9"),
            Some(WindowedMatch::Builtin("Minecraft"))
        );
        assert_eq!(
            games.matches("Minecraft.Windows.exe", ""),
            Some(WindowedMatch::Builtin("Minecraft"))
        );
        // A JVM that is not Minecraft (an IDE, a launcher) stays a windowed app.
        assert_eq!(games.matches("javaw.exe", "IntelliJ IDEA"), None);
        assert_eq!(games.matches("eldenring.exe", "ELDEN RING"), None);
    }

    #[test]
    fn configured_entries_match_processes_by_exe_and_titles_by_substring() {
        let games = WindowedGames::new([" RobloxPlayerBeta.exe ", "balatro", ""]);
        assert_eq!(
            games.matches("robloxplayerbeta.exe", "Roblox"),
            Some(WindowedMatch::ConfiguredProcess)
        );
        assert_eq!(
            games.matches("balatro.exe", "Balatro"),
            Some(WindowedMatch::ConfiguredTitle)
        );
        // An exe entry never matches a title, and a title entry never a process name.
        assert_eq!(
            games.matches("chrome.exe", "RobloxPlayerBeta.exe - Downloads"),
            None
        );
        assert_eq!(games.matches("balatro", "Something else"), None);
        assert_eq!(
            WindowedGames::new(["", "  "]).matches("javaw.exe", "IntelliJ"),
            None
        );
    }

    #[test]
    fn only_a_named_process_overrides_the_exclusion_lists() {
        // "balatro" in a browser tab or the Steam store must stay excluded; naming the
        // process is the user's explicit word.
        assert!(!WindowedMatch::ConfiguredTitle.overrides_exclusions());
        assert!(!WindowedMatch::Builtin("Minecraft").overrides_exclusions());
        assert!(WindowedMatch::ConfiguredProcess.overrides_exclusions());
    }

    #[test]
    fn a_windowed_game_qualifies_at_any_size_unless_hidden_or_minimized() {
        let decorated = WS_OVERLAPPEDWINDOW.0 | WS_VISIBLE.0;
        assert_eq!(
            window_state_from(true, false, false, decorated, true),
            WindowState::Capturable
        );
        assert_eq!(
            window_state_from(true, false, true, decorated | WS_MAXIMIZE.0, true),
            WindowState::Capturable
        );
        assert_eq!(
            window_state_from(true, true, false, decorated, true),
            WindowState::Minimized
        );
        assert_eq!(
            window_state_from(false, false, false, decorated, true),
            WindowState::Lost
        );
        // The same window without the rule is just a windowed app.
        assert_eq!(
            window_state_from(true, false, false, decorated, false),
            WindowState::Lost
        );
    }

    #[test]
    fn a_windowed_game_verdict_reads_as_yes() {
        assert!(Verdict::WindowedGame.is_game());
        assert!(Verdict::WindowedGame.label().starts_with("YES"));
    }

    #[test]
    fn detector_never_matches_this_test_process() {
        // The foreground window while tests run is a terminal/IDE at best — never a
        // fullscreen game. Mostly asserts the FFI path doesn't crash or hang.
        let detected = game_window(&WindowedGames::default());
        if let Some(w) = detected {
            // A fullscreen video or similar could legitimately match on a dev box;
            // just prove the accessor path works.
            let _ = w.window.title();
        }
    }
}
