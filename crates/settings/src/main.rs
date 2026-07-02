//! rewynd settings — a small, modern iced GUI to view and edit the config file
//! (`$XDG_CONFIG_HOME/rewynd/config.toml`).
//!
//! It edits the same file the recorder reads (the file is the single source of truth, so no
//! IPC). Changes apply on the recorder's next clip / restart — the window says so after saving;
//! live-reload is a future refinement.
//!
//! Rendered with iced's tiny-skia software backend (no GPU) and a custom dark theme. The window
//! needs a display to run, so there is no headless test of `run`; the pure mapping helpers are
//! unit-tested.

use std::fmt;
use std::sync::LazyLock;

use iced::theme::Palette;
use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, slider, text, text_input,
};
use iced::{Background, Border, Element, Font, Length, Task, Theme, font};

use rewynd_config::{self as config, Config};

// The ganked.tv "Arena" design system (docs/design/arena.md): a near-black
// surface ladder for depth (base → raised → high; the system forbids shadows), one mint accent
// owning every interactive state (no red — errors are mint too), hairline borders.
mod palette {
    use iced::Color;
    /// Window background (surface-base).
    pub const BACKGROUND: Color = Color::from_rgb8(0x0b, 0x0b, 0x0f);
    /// Cards and panels (surface-raised).
    pub const PANEL: Color = Color::from_rgb8(0x11, 0x11, 0x16);
    /// Inputs, wells, control tracks (surface-high).
    pub const HIGH: Color = Color::from_rgb8(0x18, 0x18, 0x1f);
    pub const TEXT: Color = Color::from_rgb8(0xf0, 0xf0, 0xf4);
    pub const TEXT_SECONDARY: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.50);
    pub const MUTED: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.28);
    pub const BORDER: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.07);
    pub const BORDER_STRONG: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.12);
    /// THE accent: mint. Primary buttons, active/focus states, values, links.
    pub const ACCENT: Color = Color::from_rgb8(0x00, 0xe5, 0xa0);
    /// Error text — the one deviation from the one-accent rule: a failure must
    /// never read as success-mint.
    pub const DANGER: Color = Color::from_rgb8(0xff, 0x5a, 0x5f);
    /// Primary-button hover (brightness(1.06) over the accent).
    pub const ACCENT_HOVER: Color = Color::from_rgb8(0x0d, 0xf3, 0xab);
    pub const ACCENT_BG: Color = Color::from_rgba(0.0, 0.898, 0.627, 0.08);
    pub const ACCENT_BORDER: Color = Color::from_rgba(0.0, 0.898, 0.627, 0.25);
    /// Text/icon color on mint-filled surfaces.
    pub const INK_ON_ACCENT: Color = Color::from_rgb8(0x08, 0x12, 0x0e);
}

/// Display face for headings: Barlow Condensed, always uppercase per the design.
const DISPLAY_BLACK: Font = Font {
    family: font::Family::Name("Barlow Condensed"),
    weight: font::Weight::Black,
    ..Font::DEFAULT
};
/// UI face: Inter (Regular is the default font; these are the heavier cuts).
const UI_SEMIBOLD: Font = Font {
    family: font::Family::Name("Inter"),
    weight: font::Weight::Semibold,
    ..Font::DEFAULT
};
const UI_BOLD: Font = Font {
    family: font::Family::Name("Inter"),
    weight: font::Weight::Bold,
    ..Font::DEFAULT
};

/// Whether a stored URL points somewhere other than the shipped default (empty means "use the
/// default" and an explicitly spelled-out default is still the default).
fn is_custom_url(stored: &str, default: &str) -> bool {
    let stored = stored.trim();
    !stored.is_empty() && stored != default
}

/// The shipped brand-mark PNG nearest at or above `size` pixels (falling back to the largest),
/// from the ladder the config crate owns.
fn brand_png(size: u32) -> &'static [u8] {
    config::BRAND_ICONS
        .iter()
        .find(|(s, _)| *s >= size)
        .or(config::BRAND_ICONS.last())
        .map(|(_, bytes)| *bytes)
        .expect("BRAND_ICONS is non-empty")
}

// The brand mark, decoded once per displayed size: a fresh handle every `view` call would miss
// the renderer's raster cache and re-decode each frame.
static LOGO_LARGE: LazyLock<iced::widget::image::Handle> =
    LazyLock::new(|| iced::widget::image::Handle::from_bytes(brand_png(64)));
static LOGO_SMALL: LazyLock<iced::widget::image::Handle> =
    LazyLock::new(|| iced::widget::image::Handle::from_bytes(brand_png(32)));

/// The brand mark at `size` logical pixels, from the PNG render nearest above it.
fn logo(size: f32) -> Element<'static, Message> {
    let handle = if size <= 24.0 {
        LOGO_SMALL.clone()
    } else {
        LOGO_LARGE.clone()
    };
    iced::widget::image(handle).width(size).height(size).into()
}

/// The window icon, decoded from the shipped PNG render of the mark (X11/Windows; see the
/// `window::Settings` note for Wayland).
fn window_icon() -> Option<iced::window::Icon> {
    let img = image::load_from_memory_with_format(brand_png(64), image::ImageFormat::Png).ok()?;
    let (width, height) = (img.width(), img.height());
    iced::window::icon::from_rgba(img.into_rgba8().into_vec(), width, height).ok()
}

/// Slider bounds (kept generous but sane).
const GAIN_MAX: f32 = 4.0;
const BUFFER_MIN_S: u32 = 5;
/// Slider ceiling for the replay length: the same cap the daemon enforces, so the slider and the
/// recorder agree (no value the slider shows differently from what's used). At this ceiling the
/// 30 s default sits about a quarter of the way along rather than pinned to the left.
const BUFFER_MAX_S: u32 = config::MAX_BUFFER_SECONDS as u32;
const BITRATE_MIN_MBPS: u32 = 1;
const BITRATE_MAX_MBPS: u32 = 50;
/// Frame-rate options offered in the dropdown.
const FPS_OPTIONS: [u32; 4] = [30, 60, 120, 144];
/// The microphone picker's "use the system default" row (stored as an empty value).
const MIC_DEFAULT: &str = "System default";
const BITS_PER_MBIT: u32 = 1_000_000;

fn main() -> iced::Result {
    tracing_subscriber::fmt::init();

    // Single-instance guard: a second window edits the same file, where its save clobbers the
    // first's. Held until `run` returns; the kernel releases it when the process exits.
    let _instance: Option<config::InstanceLock> = match config::acquire_settings_lock() {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            tracing::info!("rewynd settings is already open; not opening a second window");
            // Blocking show is fine: no async runtime is live yet. Without this, the tray's
            // "Open settings" appears to do nothing when a window is already open.
            let mut note = notify_rust::Notification::new();
            note.summary("rewynd settings is already open")
                .body("Look for the existing settings window.")
                .icon(config::APP_ID)
                .appname("rewynd");
            #[cfg(windows)]
            note.app_id(config::APP_ID);
            let _ = note.show();
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not acquire the settings lock; opening anyway");
            None
        }
    };

    // Best-effort desktop integration, so the taskbar and notification icons resolve even when
    // the settings window is the first rewynd binary this machine ever runs. The recorder does
    // the same at startup; both paths are cheap and idempotent.
    #[cfg(unix)]
    if let Err(e) = config::install_icons() {
        tracing::warn!(error = %e, "could not install app icons");
    }
    #[cfg(windows)]
    if let Err(e) = config::register_toast_identity() {
        tracing::warn!(error = %e, "could not register the toast identity");
    }
    if let Some(recorder) = recorder_path().filter(|p| p.is_file()) {
        #[cfg(unix)]
        if let Err(e) = config::install_launcher_entry(&recorder) {
            tracing::warn!(error = %e, "could not write a desktop entry");
        }
        // Migrate a stale autostart entry (pre-icon on Linux, a moved binary on Windows).
        if let Err(e) = config::refresh_autostart(&recorder) {
            tracing::warn!(error = %e, "could not refresh the autostart entry");
        }
    }

    iced::application(App::load, App::update, App::view)
        .title("rewynd settings")
        .theme(App::theme)
        // Bundled faces (both OFL, licenses beside the files): the Arena design is set in
        // Barlow Condensed (display) + Inter (UI); system fallbacks would break the look.
        .font(include_bytes!("../assets/fonts/BarlowCondensed-Black.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/Inter-Regular.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/Inter-SemiBold.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/Inter-Bold.ttf").as_slice())
        .default_font(Font {
            family: font::Family::Name("Inter"),
            ..Font::DEFAULT
        })
        .window(iced::window::Settings {
            // Tall enough that the whole form (advanced collapsed) fits without scrolling.
            size: iced::Size::new(960.0, 900.0),
            min_size: Some(iced::Size::new(720.0, 560.0)),
            // On Wayland this is a no-op until winit speaks xdg-toplevel-icon; there the
            // taskbar icon resolves through the app id's desktop entry + hicolor icons.
            icon: window_icon(),
            #[cfg(target_os = "linux")]
            platform_specific: iced::window::settings::PlatformSpecific {
                // Wayland app_id, so the compositor can match the window to our identity.
                application_id: config::APP_ID.to_owned(),
                ..Default::default()
            },
            ..Default::default()
        })
        .centered()
        .run()
}

/// A resolution preset, mapped to concrete width/height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    P720,
    P1080,
    P1440,
    P2160,
}

impl Resolution {
    const ALL: [Resolution; 4] = [
        Resolution::P720,
        Resolution::P1080,
        Resolution::P1440,
        Resolution::P2160,
    ];

    fn dims(self) -> (u32, u32) {
        match self {
            Resolution::P720 => (1280, 720),
            Resolution::P1080 => (1920, 1080),
            Resolution::P1440 => (2560, 1440),
            Resolution::P2160 => (3840, 2160),
        }
    }

    /// The preset matching exact dimensions, if any (a custom resolution maps to `None`).
    fn from_dims(width: u32, height: u32) -> Option<Resolution> {
        Resolution::ALL
            .into_iter()
            .find(|r| r.dims() == (width, height))
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The concrete W×H is shown next to the dropdown, so the option label stays clean.
        f.write_str(match self {
            Resolution::P2160 => "2160p (4K)",
            Resolution::P1440 => "1440p (QHD)",
            Resolution::P1080 => "1080p (Full HD)",
            Resolution::P720 => "720p (HD)",
        })
    }
}

#[derive(Debug, Clone)]
enum Status {
    Editing,
    Saved,
    Restarting,
    Restarted,
    Error(String),
}

/// Editable application state: the loaded config plus text mirrors for the free-text fields.
struct App {
    config: Config,
    /// Mirror of the output directory for the text box (empty = "use the default").
    output_dir: String,
    /// Mirror of the hotkey trigger for the text box.
    hotkey: String,
    /// Active input devices for the microphone picker (Windows enumerates them at
    /// startup; empty elsewhere, where the control is a free-text node name).
    mic_options: Vec<String>,
    /// Mirror of the ganked.tv API key.
    api_key: String,
    /// Mirror of the ganked.tv API base URL (empty = "use the default").
    api_url: String,
    /// Mirror of the share-link base URL (empty = "use the default").
    share_url: String,
    /// The start-on-boot state the autostart entry currently reflects. Save only touches the
    /// entry when the toggle moved away from this, so an entry the user manages through their
    /// desktop environment is never clobbered by unrelated saves.
    applied_on_boot: bool,
    /// Whether the last Save wrote the file — the Restart button only appears when restarting
    /// would actually apply something.
    last_save_ok: bool,
    /// Whether the ganked.tv card shows its self-hosting fields (API server, share links, API
    /// key). UI-only state; collapsed by default because login covers the common case.
    advanced_open: bool,
    login: LoginState,
    status: Status,
}

/// Where the ganked.tv device login stands. "Connected" is not a state here — it is derived from
/// a non-empty API key.
enum LoginState {
    Idle,
    Starting {
        abort: iced::task::Handle,
    },
    /// Waiting for browser approval: the user code plus the server's verification page (shown as
    /// the fallback when the browser did not open — it may not be ganked.tv when self-hosting),
    /// and the poll task's abort handle so Cancel actually stops the polling.
    Waiting {
        code: String,
        verification_uri: String,
        abort: iced::task::Handle,
    },
    Failed(String),
}

#[derive(Debug, Clone)]
enum Message {
    MicGain(f32),
    SystemGain(f32),
    MicrophonePicked(String),
    BufferSeconds(u32),
    ResolutionPicked(Resolution),
    FpsPicked(u32),
    BitrateMbps(u32),
    OutputDirEdited(String),
    BrowseDir,
    DirPicked(Option<String>),
    HotkeyEdited(String),
    #[cfg(target_os = "linux")]
    AlwaysPrompt(bool),
    #[cfg(target_os = "windows")]
    CaptureDesktop(bool),
    StartOnBoot(bool),
    UploadEnabled(bool),
    ApiKeyEdited(String),
    ApiUrlEdited(String),
    ShareUrlEdited(String),
    VisibilityPicked(rewynd_upload::Visibility),
    AdvancedToggled,
    LoginPressed,
    LoginStarted(Result<rewynd_upload::DeviceLogin, String>),
    LoginDone(Result<String, String>),
    LoginCancelled,
    LogoutPressed,
    Save,
    Restart,
    Restarted(Result<(), String>),
}

impl App {
    fn load() -> Self {
        // Edit the file's own values (no env overrides — those are a runtime concern).
        let mut config = config::load_file();
        // Snap the stored values into the ranges the controls can represent, so what the window
        // shows is exactly what Save will write back (no slider that displays a clamped value
        // while the file keeps a different one). Resolution and the keyframe interval are left
        // alone — a custom resolution is shown verbatim, and the GOP is only retuned with the fps.
        normalize(&mut config);
        #[cfg(target_os = "windows")]
        let mic_options = config::list_audio_inputs();
        #[cfg(not(target_os = "windows"))]
        let mic_options = Vec::new();
        Self {
            output_dir: config.output_directory().unwrap_or_default().to_owned(),
            hotkey: config.hotkey_trigger().to_owned(),
            mic_options,
            api_key: config.upload_api_key().to_owned(),
            api_url: config.upload_api_url().to_owned(),
            share_url: config.upload_share_url().to_owned(),
            applied_on_boot: config.start_on_boot(),
            // A saved self-hosting setup stays visible instead of hiding behind the disclosure
            // (a key alone is just "logged in" — the badge already shows that). URLs stored as
            // the ganked.tv defaults, spelled out, are not a custom setup.
            advanced_open: is_custom_url(config.upload_api_url(), config::DEFAULT_UPLOAD_API_URL)
                || is_custom_url(config.upload_share_url(), config::DEFAULT_UPLOAD_SHARE_URL),
            config,
            login: LoginState::Idle,
            status: Status::Editing,
            last_save_ok: false,
        }
    }

    /// The API base the login flow should talk to: the (unsaved) field value, or the default.
    fn effective_api_url(&self) -> String {
        let url = self.api_url.trim();
        if url.is_empty() {
            config::DEFAULT_UPLOAD_API_URL.to_owned()
        } else {
            url.to_owned()
        }
    }

    fn theme(&self) -> Theme {
        Theme::custom(
            "rewynd".to_owned(),
            Palette {
                background: palette::BACKGROUND,
                text: palette::TEXT,
                primary: palette::ACCENT,
                // One-accent rule for the happy states (no green/amber split; mint owns
                // them) — but errors get the palette's one red so they can't be misread.
                success: palette::ACCENT,
                warning: palette::ACCENT,
                danger: palette::DANGER,
            },
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        // Any edit invalidates a prior "Saved" state.
        match message {
            Message::MicGain(v) => {
                self.config.set_mic_gain(v);
                self.touch();
            }
            Message::SystemGain(v) => {
                self.config.set_system_gain(v);
                self.touch();
            }
            Message::MicrophonePicked(mic) => {
                // The picker's "System default" row maps to the empty stored value.
                self.config.set_microphone(if mic == MIC_DEFAULT {
                    String::new()
                } else {
                    mic
                });
                self.touch();
            }
            Message::BufferSeconds(s) => {
                self.config.set_buffer_seconds(u64::from(s));
                self.touch();
            }
            Message::ResolutionPicked(r) => {
                let (width, height) = r.dims();
                let mut v = self.config.video_stored();
                v.width = width;
                v.height = height;
                self.config.set_video(v);
                self.touch();
            }
            Message::FpsPicked(fps) => {
                let mut v = self.config.video_stored();
                v.framerate = fps;
                // Keep ~1 keyframe per second so the ring buffer's cut granularity tracks the
                // frame rate (the defaults couple these); the UI doesn't expose the GOP directly.
                v.idr_period = fps;
                self.config.set_video(v);
                self.touch();
            }
            Message::BitrateMbps(mbps) => {
                let mut v = self.config.video_stored();
                v.bitrate_bps = mbps.saturating_mul(BITS_PER_MBIT);
                self.config.set_video(v);
                self.touch();
            }
            Message::OutputDirEdited(s) => {
                self.output_dir = s;
                self.touch();
            }
            Message::BrowseDir => {
                return Task::future(rfd::AsyncFileDialog::new().pick_folder()).map(|handle| {
                    Message::DirPicked(handle.map(|h| h.path().to_string_lossy().into_owned()))
                });
            }
            Message::DirPicked(Some(path)) => {
                self.output_dir = path;
                self.touch();
            }
            Message::DirPicked(None) => {}
            Message::HotkeyEdited(s) => {
                self.hotkey = s;
                self.touch();
            }
            #[cfg(target_os = "linux")]
            Message::AlwaysPrompt(on) => {
                self.config.set_always_prompt(on);
                self.touch();
            }
            #[cfg(target_os = "windows")]
            Message::CaptureDesktop(on) => {
                self.config.set_capture_desktop(on);
                self.touch();
            }
            Message::StartOnBoot(on) => {
                self.config.set_start_on_boot(on);
                self.touch();
            }
            Message::UploadEnabled(on) => {
                self.config.set_upload_enabled(on);
                self.touch();
            }
            Message::ApiKeyEdited(s) => {
                self.api_key = s;
                self.touch();
            }
            Message::ApiUrlEdited(s) => {
                self.api_url = s;
                self.touch();
            }
            Message::ShareUrlEdited(s) => {
                self.share_url = s;
                self.touch();
            }
            Message::VisibilityPicked(v) => {
                self.config.set_upload_visibility(v.as_str().to_owned());
                self.touch();
            }
            // No touch(): opening the disclosure edits nothing.
            Message::AdvancedToggled => self.advanced_open = !self.advanced_open,
            Message::LoginPressed => {
                let base = self.effective_api_url();
                let (task, abort) = Task::perform(
                    async move {
                        rewynd_upload::device_login_start(&base, "rewynd")
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::LoginStarted,
                )
                .abortable();
                self.login = LoginState::Starting { abort };
                return task;
            }
            Message::LoginStarted(Ok(login)) => {
                // A cancelled/stale start must not begin polling.
                if !matches!(self.login, LoginState::Starting { .. }) {
                    return Task::none();
                }
                if let Err(e) = open::that_detached(&login.verification_uri_complete) {
                    tracing::warn!(error = %e, "could not open the browser for approval");
                }
                let code = login.user_code.clone();
                let verification_uri = login.verification_uri.clone();
                // The login itself remembers which server issued it; no base to recompute.
                let (task, abort) = Task::perform(
                    async move {
                        rewynd_upload::device_login_wait(&login)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::LoginDone,
                )
                .abortable();
                self.login = LoginState::Waiting {
                    code,
                    verification_uri,
                    abort,
                };
                return task;
            }
            Message::LoginStarted(Err(e)) => {
                if matches!(self.login, LoginState::Starting { .. }) {
                    self.login = LoginState::Failed(e);
                }
            }
            Message::LoginDone(result) => {
                // Ignore a result that arrives after Cancel/Logout switched the state away.
                if !matches!(self.login, LoginState::Waiting { .. }) {
                    return Task::none();
                }
                match result {
                    Ok(key) => {
                        // Logging in states intent: switch uploads on and persist right away.
                        self.api_key = key;
                        self.config.set_upload_enabled(true);
                        self.login = LoginState::Idle;
                        self.save();
                    }
                    Err(e) => self.login = LoginState::Failed(e),
                }
            }
            Message::LoginCancelled => self.abort_login(),
            Message::LogoutPressed => {
                self.api_key.clear();
                self.config.set_upload_enabled(false);
                // A pending device login must not keep polling after an explicit logout.
                self.abort_login();
                self.save();
            }
            Message::Save => self.save(),
            Message::Restart => {
                self.status = Status::Restarting;
                // Off the UI thread: stop the old recorder, wait for it to exit, then relaunch.
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(restart_recorder)
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::Restarted,
                );
            }
            Message::Restarted(result) => {
                self.status = match result {
                    Ok(()) => Status::Restarted,
                    Err(e) => Status::Error(e),
                };
            }
        }
        Task::none()
    }

    /// Mark the form as having unsaved edits (a restart would no longer apply what's on screen).
    fn touch(&mut self) {
        self.status = Status::Editing;
        self.last_save_ok = false;
    }

    /// Stop whichever login task is in flight (the handles are manual-abort, not
    /// abort-on-drop) and return to Idle.
    fn abort_login(&mut self) {
        match std::mem::replace(&mut self.login, LoginState::Idle) {
            LoginState::Starting { abort } | LoginState::Waiting { abort, .. } => abort.abort(),
            _ => {}
        }
    }

    /// Fold the free-text mirrors into the config and write it to disk.
    fn save(&mut self) {
        let dir = self.output_dir.trim();
        self.config
            .set_output_directory((!dir.is_empty()).then(|| dir.to_owned()));
        // An empty field means "use the default"; write the default trigger rather than an empty
        // string, so the file matches what the daemon will actually bind.
        let trigger = self.hotkey.trim();
        self.config.set_hotkey_trigger(if trigger.is_empty() {
            config::DEFAULT_HOTKEY_TRIGGER.to_owned()
        } else {
            trigger.to_owned()
        });
        self.config
            .set_upload_api_key(self.api_key.trim().to_owned());
        // Empty URL fields are stored empty: "use the default" lives in one place (the config
        // read side), so a future default change reaches everyone who didn't pick a custom URL.
        self.config
            .set_upload_api_url(self.api_url.trim().to_owned());
        self.config
            .set_upload_share_url(self.share_url.trim().to_owned());

        self.status = match config::config_path() {
            Some(path) => match self.config.save_to(&path) {
                Ok(()) => {
                    tracing::info!(path = %path.display(), "saved config");
                    self.last_save_ok = true;
                    self.apply_autostart()
                }
                Err(e) => {
                    self.last_save_ok = false;
                    Status::Error(format!("could not write {}: {e}", path.display()))
                }
            },
            None => {
                self.last_save_ok = false;
                Status::Error("no config path could be resolved on this system".to_owned())
            }
        };
    }

    /// Bring the login autostart entry in line with a just-saved toggle CHANGE. Touching the
    /// entry only on a change keeps a desktop-managed entry (same filename) safe from unrelated
    /// saves; removal needs no recorder path, installation refuses a missing binary out loud.
    fn apply_autostart(&mut self) -> Status {
        let on_boot = self.config.start_on_boot();
        if on_boot == self.applied_on_boot {
            return Status::Saved;
        }
        let result = if on_boot {
            match recorder_path() {
                Some(recorder) if recorder.is_file() => config::install_autostart(&recorder),
                Some(recorder) => Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("recorder not found at {}", recorder.display()),
                )),
                None => Err(std::io::Error::other(
                    "could not locate the recorder binary",
                )),
            }
        } else {
            config::remove_autostart()
        };
        match result {
            Ok(()) => {
                self.applied_on_boot = on_boot;
                Status::Saved
            }
            Err(e) => Status::Error(format!("saved, but autostart update failed: {e}")),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        // `normalize` (on load) keeps these in range; clamp on the u64 before narrowing so a
        // pathological stored value can't wrap the cast.
        let v = self.config.video_stored();
        let a = self.config.audio_stored();
        let mbps = v.bitrate_bps / BITS_PER_MBIT;
        let secs = self.config.buffer_seconds().min(u64::from(BUFFER_MAX_S)) as u32;
        // Rough clip size from the target bitrate (video + audio) over the replay window,
        // rounded to the nearest MB.
        let est_bytes = u64::from(v.bitrate_bps)
            .saturating_add(u64::from(a.bitrate_bps))
            .saturating_mul(u64::from(secs))
            / 8;
        let est_mb = est_bytes.saturating_add(500_000) / 1_000_000;

        // The microphone picker: a dropdown of the active input devices on Windows
        // (also usable when the OS sound settings are locked), a free-text PipeWire
        // node name elsewhere. Stored value empty = the system default.
        let mic_value = self.config.microphone().unwrap_or_default().to_owned();
        let microphone: Element<Message> = if self.mic_options.is_empty() {
            column![
                field_label("Microphone"),
                text_input(MIC_DEFAULT, &mic_value)
                    .on_input(Message::MicrophonePicked)
                    .style(arena_input),
                hint("Leave empty for the default; on Linux this is the PipeWire node name."),
            ]
            .spacing(8)
            .into()
        } else {
            let mut options = vec![MIC_DEFAULT.to_owned()];
            // Keep a configured-but-offline device visible instead of silently
            // snapping the selection to the default.
            if !mic_value.is_empty() && !self.mic_options.contains(&mic_value) {
                options.push(mic_value.clone());
            }
            options.extend(self.mic_options.iter().cloned());
            let selected = if mic_value.is_empty() {
                MIC_DEFAULT.to_owned()
            } else {
                mic_value
            };
            column![
                field_label("Microphone"),
                pick_list(options, Some(selected), Message::MicrophonePicked)
                    .style(arena_pick)
                    .width(Length::Fill),
            ]
            .spacing(8)
            .into()
        };
        let audio = card(
            "AUDIO",
            column![
                microphone,
                setting(
                    "Microphone volume",
                    format!("{:.2}x", self.config.mic_gain()),
                    slider(0.0..=GAIN_MAX, self.config.mic_gain(), Message::MicGain)
                        .step(0.05)
                        .style(arena_slider),
                ),
                setting(
                    "System volume",
                    format!("{:.2}x", self.config.system_gain()),
                    slider(
                        0.0..=GAIN_MAX,
                        self.config.system_gain(),
                        Message::SystemGain
                    )
                    .step(0.05)
                    .style(arena_slider),
                ),
            ]
            .spacing(18),
        );

        let recording = card(
            "RECORDING",
            column![
                setting(
                    "Replay length",
                    format!("{secs} s"),
                    slider(BUFFER_MIN_S..=BUFFER_MAX_S, secs, Message::BufferSeconds)
                        .style(arena_slider),
                ),
                setting(
                    "Resolution",
                    format!("{}x{}", v.width, v.height),
                    pick_list(
                        &Resolution::ALL[..],
                        Resolution::from_dims(v.width, v.height),
                        Message::ResolutionPicked,
                    )
                    .style(arena_pick)
                    .width(Length::Fill),
                ),
                setting(
                    "Frame rate",
                    format!("{} fps", v.framerate),
                    pick_list(&FPS_OPTIONS[..], Some(v.framerate), Message::FpsPicked)
                        .style(arena_pick)
                        .width(Length::Fill),
                ),
                setting(
                    "Quality",
                    format!("{mbps} Mbps"),
                    slider(
                        BITRATE_MIN_MBPS..=BITRATE_MAX_MBPS,
                        mbps,
                        Message::BitrateMbps
                    )
                    .style(arena_slider),
                ),
                value_row("Estimated clip size", format!("about {est_mb} MB")),
            ]
            .spacing(18),
        );

        let placeholder = config::default_output_dir().map_or_else(
            || "Leave empty for the system temp folder".to_owned(),
            |p| format!("Leave empty for {}", p.display()),
        );
        let output_capture = column![
            column![
                field_label("Save clips to"),
                row![
                    text_input(&placeholder, &self.output_dir)
                        .on_input(Message::OutputDirEdited)
                        .style(arena_input),
                    button(text("Browse").size(12).font(UI_SEMIBOLD))
                        .on_press(Message::BrowseDir)
                        .style(secondary_button)
                        .padding([10, 16]),
                ]
                .spacing(8),
            ]
            .spacing(8),
            field(
                "Hotkey",
                text_input("CTRL+ALT+R", &self.hotkey)
                    .on_input(Message::HotkeyEdited)
                    .style(arena_input),
            )
            .push(hint(
                "Your desktop may let you rebind this in its shortcut settings.",
            )),
        ]
        .spacing(18);
        // The monitor picker is the ScreenCast portal's (Linux); Windows records the
        // active game by default, with the whole desktop as the opt-in.
        #[cfg(target_os = "linux")]
        let output_capture = output_capture.push(
            checkbox(self.config.always_prompt())
                .label("Ask which monitor to record every time rewynd starts")
                .on_toggle(Message::AlwaysPrompt)
                .style(arena_check),
        );
        #[cfg(target_os = "windows")]
        let output_capture = output_capture.push(
            column![
                checkbox(self.config.capture_desktop())
                    .label("Record the whole desktop, not just the active game")
                    .on_toggle(Message::CaptureDesktop)
                    .style(arena_check),
                hint(
                    "Off records only the game you're playing (fullscreen or \
                     borderless), keeping other windows out of your clips.",
                ),
            ]
            .spacing(6),
        );
        let output = card(
            "OUTPUT & CAPTURE",
            output_capture.push(
                checkbox(self.config.start_on_boot())
                    .label("Start rewynd when I log in")
                    .on_toggle(Message::StartOnBoot)
                    .style(arena_check),
            ),
        );

        // Account area: a one-click browser login (device grant); the key it mints is stored
        // invisibly. Connectedness is simply "a key is present".
        let account: Element<Message> = if !self.api_key.trim().is_empty() {
            row![
                container(text("CONNECTED TO GANKED.TV").size(10).font(UI_BOLD).style(
                    |_: &Theme| text::Style {
                        color: Some(palette::ACCENT),
                    }
                ),)
                .padding([5, 10])
                .style(|_: &Theme| container::Style {
                    background: Some(Background::Color(palette::ACCENT_BG)),
                    border: Border {
                        color: palette::ACCENT_BORDER,
                        width: 1.0,
                        radius: 5.0.into(),
                    },
                    ..container::Style::default()
                }),
                iced::widget::Space::new().width(Length::Fill),
                button(text("Log out").size(11).font(UI_SEMIBOLD))
                    .on_press(Message::LogoutPressed)
                    .style(secondary_button)
                    .padding([7, 14]),
            ]
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            match &self.login {
                LoginState::Idle => column![
                    oauth_login_button(),
                    hint("Approve rewynd in your browser; no password is stored on this device."),
                ]
                .spacing(6)
                .into(),
                LoginState::Starting { .. } => column![
                    text("Contacting ganked.tv...").size(13),
                    button(text("Cancel").size(11).font(UI_SEMIBOLD))
                        .on_press(Message::LoginCancelled)
                        .style(secondary_button)
                        .padding([6, 14]),
                ]
                .spacing(7)
                .into(),
                LoginState::Waiting {
                    code,
                    verification_uri,
                    ..
                } => column![
                    text(format!("Approve in your browser with code {code}")).size(13),
                    hint(format!(
                        "Browser did not open? Go to {verification_uri} and enter the code."
                    )),
                    button(text("Cancel").size(11).font(UI_SEMIBOLD))
                        .on_press(Message::LoginCancelled)
                        .style(secondary_button)
                        .padding([6, 14]),
                ]
                .spacing(7)
                .into(),
                LoginState::Failed(e) => column![
                    oauth_login_button(),
                    text(e.clone()).size(12).style(tinted(palette::DANGER)),
                ]
                .spacing(6)
                .into(),
            }
        };

        // The self-hosting fields hide behind a disclosure: with the browser login covering the
        // common case, "API server" and "share links" only confuse an average user.
        let mut upload_items: Vec<Element<Message>> = vec![
            account,
            checkbox(self.config.upload_enabled())
                .label("Enable uploads (tray: Upload last clip)")
                .on_toggle(Message::UploadEnabled)
                .style(arena_check)
                .into(),
            setting(
                "Visibility",
                rewynd_upload::Visibility::parse(self.config.upload_visibility()).to_string(),
                pick_list(
                    &rewynd_upload::Visibility::ALL[..],
                    Some(rewynd_upload::Visibility::parse(
                        self.config.upload_visibility(),
                    )),
                    Message::VisibilityPicked,
                )
                .style(arena_pick)
                .width(Length::Fill),
            ),
            button(
                text(if self.advanced_open {
                    "Hide advanced options"
                } else {
                    "Advanced options"
                })
                .size(11)
                .font(UI_SEMIBOLD),
            )
            .on_press(Message::AdvancedToggled)
            .style(link_button)
            .padding(0)
            .into(),
        ];
        if self.advanced_open {
            upload_items.extend([
                field(
                    "API server",
                    text_input(config::DEFAULT_UPLOAD_API_URL, &self.api_url)
                        .on_input(Message::ApiUrlEdited)
                        .style(arena_input),
                )
                .into(),
                field(
                    "Share links",
                    text_input(config::DEFAULT_UPLOAD_SHARE_URL, &self.share_url)
                        .on_input(Message::ShareUrlEdited)
                        .style(arena_input),
                )
                .push(hint(
                    "Leave both empty for ganked.tv. Self-hosting? An API key can be \
                     pasted below.",
                ))
                .into(),
                field(
                    "API key",
                    text_input("gtv_...", &self.api_key)
                        .secure(true)
                        .on_input(Message::ApiKeyEdited)
                        .style(arena_input),
                )
                .into(),
            ]);
        }
        let upload = card("GANKED.TV", column(upload_items).spacing(18));

        let header = column![
            logo(46.0),
            text("SETTINGS").size(32).font(DISPLAY_BLACK),
            hint(
                "Most changes apply after Save + Restart rewynd now; the ganked.tv upload \
                 settings apply immediately.",
            ),
        ]
        .spacing(6)
        .align_x(iced::Alignment::Center)
        .width(Length::Fill);

        let mut save_items: Vec<Element<Message>> = vec![
            button(text("Save settings").size(13).font(UI_BOLD))
                .on_press(Message::Save)
                .style(primary_button)
                .padding([13, 28])
                .into(),
            status_line(&self.status),
        ];
        // Offer a one-click restart once a save actually landed (a failed save has nothing to
        // apply; a failed restart may be retried). Hidden while a restart is in flight and
        // once it succeeded — it reappears on the next save.
        if self.last_save_ok && !matches!(self.status, Status::Restarting | Status::Restarted) {
            save_items.push(
                button(text("Restart rewynd now").size(12).font(UI_SEMIBOLD))
                    .on_press(Message::Restart)
                    .padding([9, 22])
                    .style(secondary_button)
                    .into(),
            );
        }
        let save = container(
            column(save_items)
                .spacing(10)
                .align_x(iced::Alignment::Center),
        )
        .center_x(Length::Fill);

        // Two columns so everything fits in a landscape window without scrolling: Recording and
        // Audio on the left, Output and Upload on the right.
        let columns = row![
            column![recording, audio].spacing(20).width(Length::Fill),
            column![output, upload].spacing(20).width(Length::Fill),
        ]
        .spacing(20);

        let body = column![header, columns, save]
            .spacing(20)
            .padding(28)
            .max_width(880);

        // The scrollable is a safety net for small windows and the opened Advanced disclosure;
        // the default window size is chosen (by eye — iced has no pre-layout measure) so the
        // collapsed form fits without it. The body caps at 880, so on a wider window center it
        // rather than leaving the slack on one side.
        container(scrollable(container(body).center_x(Length::Fill)))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

/// Clamp the editable numeric settings into the ranges the controls can represent, so the view
/// (which reads these back) and Save (which writes them) agree — no control that shows a value
/// the file doesn't actually hold. Gains are sanitized and capped, the buffer window is clamped
/// to the daemon's own range, and the bitrate is snapped to whole Mbps. Resolution, frame rate,
/// and the keyframe interval are left as stored (a custom resolution stays custom).
fn normalize(c: &mut Config) {
    c.set_mic_gain(c.mic_gain().clamp(0.0, GAIN_MAX));
    c.set_system_gain(c.system_gain().clamp(0.0, GAIN_MAX));
    c.set_buffer_seconds(
        c.buffer_seconds()
            .clamp(u64::from(BUFFER_MIN_S), u64::from(BUFFER_MAX_S)),
    );
    let mut v = c.video_stored();
    let mbps = (v.bitrate_bps / BITS_PER_MBIT).clamp(BITRATE_MIN_MBPS, BITRATE_MAX_MBPS);
    v.bitrate_bps = mbps * BITS_PER_MBIT;
    c.set_video(v);
}

/// A grouped settings card, Arena style: raised panel, hairline border, 8px radius, with the
/// title as a small uppercase eyebrow (accent, like the design's active wizard cards).
fn card<'a>(title: &'a str, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let inner = column![
        text(title)
            .size(10)
            .font(UI_BOLD)
            .style(tinted(palette::ACCENT)),
        content.into(),
    ]
    .spacing(14);
    container(inner)
        .width(Length::Fill)
        .padding(18)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(palette::PANEL)),
            border: Border {
                color: palette::BORDER,
                width: 1.0,
                radius: 8.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// Primary (mint) button per the Arena spec: filled accent, ink text, 8px radius.
fn primary_button(_theme: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => palette::ACCENT_HOVER,
        _ => palette::ACCENT,
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: palette::INK_ON_ACCENT,
        border: Border {
            radius: 8.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// The "Log in with ganked.tv" button: the brand mark plus label, shaped like a familiar
/// third-party sign-in button.
fn oauth_login_button<'a>() -> Element<'a, Message> {
    button(
        row![
            logo(18.0),
            text("Log in with ganked.tv").size(13).font(UI_SEMIBOLD),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center),
    )
    .on_press(Message::LoginPressed)
    .style(oauth_button)
    .padding([10, 22])
    .into()
}

/// OAuth sign-in shell: unlike `primary_button` the fill stays a dark well so the gradient
/// mark carries the brand; hover tints it mint.
fn oauth_button(_theme: &Theme, status: button::Status) -> button::Style {
    let (background, border_color) = match status {
        button::Status::Hovered | button::Status::Pressed => {
            (palette::ACCENT_BG, palette::ACCENT_BORDER)
        }
        _ => (palette::HIGH, palette::BORDER_STRONG),
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: palette::TEXT,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..button::Style::default()
    }
}

/// Quiet link-style button (the Advanced disclosure): bare text, mint on hover.
fn link_button(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => palette::ACCENT,
            _ => palette::TEXT_SECONDARY,
        },
        ..button::Style::default()
    }
}

/// Secondary (outline) button: transparent, strong hairline; hover turns mint.
fn secondary_button(_theme: &Theme, status: button::Status) -> button::Style {
    let (border_color, text_color) = match status {
        button::Status::Hovered | button::Status::Pressed => (palette::ACCENT, palette::ACCENT),
        _ => (palette::BORDER_STRONG, palette::TEXT_SECONDARY),
    };
    button::Style {
        background: None,
        text_color,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..button::Style::default()
    }
}

/// Text-input shell: surface-high well, 6px radius, mint border when focused.
fn arena_input(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let border_color = match status {
        text_input::Status::Focused { .. } => palette::ACCENT,
        text_input::Status::Hovered => palette::BORDER_STRONG,
        _ => palette::BORDER,
    };
    text_input::Style {
        background: Background::Color(palette::HIGH),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 6.0.into(),
        },
        icon: palette::TEXT_SECONDARY,
        placeholder: palette::MUTED,
        value: palette::TEXT,
        selection: palette::ACCENT_BORDER,
    }
}

/// Dropdown shell, styled like an input.
fn arena_pick(_theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let border_color = match status {
        pick_list::Status::Hovered | pick_list::Status::Opened { .. } => palette::BORDER_STRONG,
        _ => palette::BORDER,
    };
    pick_list::Style {
        text_color: palette::TEXT,
        placeholder_color: palette::MUTED,
        handle_color: palette::TEXT_SECONDARY,
        background: Background::Color(palette::HIGH),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 6.0.into(),
        },
    }
}

/// Checkbox: surface-high box, mint fill + ink check when on.
fn arena_check(_theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let checked = matches!(
        status,
        checkbox::Status::Active { is_checked: true }
            | checkbox::Status::Hovered { is_checked: true }
            | checkbox::Status::Disabled { is_checked: true }
    );
    let hovered = matches!(status, checkbox::Status::Hovered { .. });
    checkbox::Style {
        background: Background::Color(if checked {
            palette::ACCENT
        } else {
            palette::HIGH
        }),
        icon_color: palette::INK_ON_ACCENT,
        border: Border {
            color: if checked {
                palette::ACCENT
            } else if hovered {
                palette::BORDER_STRONG
            } else {
                palette::BORDER
            },
            width: 1.0,
            radius: 4.0.into(),
        },
        text_color: Some(palette::TEXT),
    }
}

/// Slider: thin surface-high rail with a mint filled side and a mint round handle.
fn arena_slider(_theme: &Theme, _status: slider::Status) -> slider::Style {
    slider::Style {
        rail: slider::Rail {
            backgrounds: (
                Background::Color(palette::ACCENT),
                Background::Color(palette::HIGH),
            ),
            width: 6.0,
            border: Border {
                radius: 3.0.into(),
                ..Border::default()
            },
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius: 8.0 },
            background: Background::Color(palette::ACCENT),
            border_width: 0.0,
            border_color: palette::ACCENT,
        },
    }
}

/// One setting: the label on the left, its current value on the right (accent), and the control
/// directly beneath spanning the full width.
fn setting<'a>(
    label: &'a str,
    value: String,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![value_row(label, value), control.into()]
        .spacing(7)
        .into()
}

/// A label (left) and a value (right, accent) on one row — also used for read-only readouts.
fn value_row<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    row![
        text(label)
            .size(12)
            .font(UI_SEMIBOLD)
            .style(tinted(palette::TEXT_SECONDARY))
            .width(Length::Fill),
        text(value)
            .size(12)
            .font(UI_SEMIBOLD)
            .style(tinted(palette::ACCENT)),
    ]
    .into()
}

/// A text style closure for one fixed color.
fn tinted(color: iced::Color) -> impl Fn(&Theme) -> text::Style {
    move |_| text::Style { color: Some(color) }
}

/// A labelled form field: [`field_label`] over the control. Returns the column so a caller can
/// `.push` a trailing [`hint`].
fn field<'a>(
    label: &'a str,
    control: impl Into<Element<'a, Message>>,
) -> iced::widget::Column<'a, Message> {
    column![field_label(label), control.into()].spacing(6)
}

/// Arena field label: small, bold, uppercase, secondary.
fn field_label<'a>(s: &str) -> Element<'a, Message> {
    text(s.to_uppercase())
        .size(10)
        .font(UI_BOLD)
        .style(tinted(palette::TEXT_SECONDARY))
        .into()
}

/// Muted hint text.
fn hint<'a>(s: impl Into<String>) -> Element<'a, Message> {
    text(s.into()).size(11).style(tinted(palette::MUTED)).into()
}

/// The save-status line under the Save button.
fn status_line(status: &Status) -> Element<'_, Message> {
    let (msg, color) = match status {
        Status::Editing => (String::new(), palette::MUTED),
        Status::Saved => (
            "Saved. Restart rewynd to apply the changes.".to_owned(),
            palette::ACCENT,
        ),
        Status::Restarting => ("Restarting rewynd...".to_owned(), palette::MUTED),
        Status::Restarted => (
            "Restarted rewynd with the new settings.".to_owned(),
            palette::ACCENT,
        ),
        Status::Error(e) => (e.clone(), palette::DANGER),
    };
    text(msg).size(12).style(tinted(color)).into()
}

/// The recorder binary, expected as a sibling of this settings binary.
fn recorder_path() -> Option<std::path::PathBuf> {
    config::sibling_binary("rewynd")
}

/// Stop the running recorder (if any), wait for it to exit, then launch a fresh one so it picks
/// up the saved config. Blocking — runs on a `spawn_blocking` thread, not the UI thread.
/// The stop half (pid verification, SIGTERM→SIGKILL escalation, start-time identity guard)
/// lives in rewynd-config next to the pid file's writer.
fn restart_recorder() -> Result<(), String> {
    match config::stop_recorder(
        std::time::Duration::from_secs(3),
        std::time::Duration::from_secs(2),
    ) {
        Ok(true) => {}
        Ok(false) => {
            // Spawning now would just bounce off the old process's single-instance lock.
            return Err("the running recorder did not stop; try again".to_owned());
        }
        Err(e) => tracing::warn!(error = %e, "could not stop the running recorder"),
    }
    let recorder =
        recorder_path().ok_or_else(|| "could not locate the recorder binary".to_owned())?;
    std::process::Command::new(&recorder)
        .spawn()
        .map(|mut child| {
            // Reap in the background: an unreaped child stays a zombie whose /proc identity
            // makes the NEXT restart's stop wait think the old recorder never exited.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        })
        .map_err(|e| format!("could not start {}: {e}", recorder.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_dims_round_trip() {
        for r in Resolution::ALL {
            let (w, h) = r.dims();
            assert_eq!(Resolution::from_dims(w, h), Some(r));
        }
        assert_eq!(Resolution::from_dims(800, 600), None, "non-preset → None");
    }

    #[test]
    fn bitrate_mbps_conversion() {
        // The view shows bps/1_000_000; a message multiplies back.
        assert_eq!(12_000_000 / BITS_PER_MBIT, 12);
        assert_eq!(25u32.saturating_mul(BITS_PER_MBIT), 25_000_000);
    }
}
