//! Game identity, shared by the per-platform detectors: what the focused window told
//! us, whether it should count as a game, and how to name a per-game clip folder.

/// The focused fullscreen application, as reported by the platform's window layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameInfo {
    /// The toplevel's app id (Wayland `app_id` / X11 `WM_CLASS`); Proton games report
    /// `steam_app_<appid>`. May be empty when the compositor doesn't know.
    pub app_id: String,
    /// The window title. Only a naming fallback: titles carry documents/URLs/chat
    /// context, so they must never be logged.
    pub title: String,
    /// The owning process, when the protocol exposes it (plasma-window-management does).
    pub pid: Option<u32>,
}

impl GameInfo {
    /// The Steam appid encoded in the app id, for `steam_app_<appid>` windows
    /// (how Proton labels every game window).
    #[must_use]
    pub fn steam_appid(&self) -> Option<u32> {
        self.app_id.strip_prefix("steam_app_")?.parse().ok()
    }

    /// A human name for the game: the installed Steam title when the appid resolves,
    /// otherwise a cleaned-up app id, otherwise the window title.
    #[must_use]
    pub fn display_name(&self) -> String {
        if let Some(appid) = self.steam_appid()
            && let Some(name) = steam_app_name(appid)
        {
            return name;
        }
        let cleaned = clean_app_id(&self.app_id);
        if !cleaned.is_empty() {
            return cleaned;
        }
        self.title.trim().to_owned()
    }
}

/// Shell/desktop app ids that legitimately go fullscreen (lock screens, launchers,
/// splashes) or are rewynd itself, and must never be latched onto as "the game".
/// The Windows detector keeps the equivalent process list in `windows::game_window`.
const EXCLUDED_APP_IDS: &[&str] = &[
    // KDE Plasma
    "plasmashell",
    "org.kde.plasmashell",
    "krunner",
    "org.kde.krunner",
    "kscreenlocker_greet",
    "org.kde.kscreenlocker_greet",
    "ksmserver-logout-greeter",
    "org.kde.ksplashqml",
    "ksplashqml",
    // wlroots lockers/launchers
    "swaylock",
    "hyprlock",
    "wofi",
    "rofi",
    // rewynd's own windows
    "tv.ganked.rewynd",
    "rewynd",
    "rewynd-settings",
];

/// Whether `app_id` belongs to the desktop shell (or rewynd) rather than a game.
#[must_use]
pub fn is_shell_app_id(app_id: &str) -> bool {
    let lowered = app_id.to_ascii_lowercase();
    EXCLUDED_APP_IDS.contains(&lowered.as_str())
}

/// The installed name for a Steam appid, from the local library's app manifest.
/// Purely local (no network); `None` when Steam or the app isn't found.
#[must_use]
pub fn steam_app_name(appid: u32) -> Option<String> {
    let steam = steamlocate::SteamDir::locate().ok()?;
    let (app, _library) = steam.find_app(appid).ok()??;
    app.name
}

/// A readable name from a raw app id: `eldenring.exe` → `eldenring`,
/// `net.lutris.dishonored` → `dishonored`. Deliberately light-touch — the id is
/// already the best stable key we have.
fn clean_app_id(app_id: &str) -> String {
    let base = app_id.trim();
    let base = base.strip_suffix(".exe").unwrap_or(base);
    // Reverse-DNS desktop ids: keep the final segment, which names the app.
    let base = base.rsplit('.').next().unwrap_or(base);
    base.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(app_id: &str, title: &str) -> GameInfo {
        GameInfo {
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            pid: None,
        }
    }

    #[test]
    fn steam_appid_parses_proton_class() {
        assert_eq!(info("steam_app_1245620", "").steam_appid(), Some(1245620));
        assert_eq!(info("steam_app_", "").steam_appid(), None);
        assert_eq!(info("factorio", "").steam_appid(), None);
        assert_eq!(info("steam_app_x", "").steam_appid(), None);
    }

    #[test]
    fn excluded_ids_are_lowercase_and_matched_case_insensitively() {
        for id in EXCLUDED_APP_IDS {
            assert_eq!(
                *id,
                id.to_ascii_lowercase(),
                "{id} must be stored lowercase"
            );
        }
        assert!(is_shell_app_id("PlasmaShell"));
        assert!(is_shell_app_id("org.kde.plasmashell"));
        assert!(!is_shell_app_id("factorio"));
    }

    #[test]
    fn display_name_falls_back_from_app_id_to_title() {
        // No Steam appid involved: the cleaned app id wins over the title.
        assert_eq!(
            info("eldenring.exe", "ELDEN RING").display_name(),
            "eldenring"
        );
        assert_eq!(
            info("net.lutris.dishonored", "Dishonored").display_name(),
            "dishonored"
        );
        assert_eq!(
            info("", "Some Window Title").display_name(),
            "Some Window Title"
        );
    }
}
