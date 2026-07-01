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

use iced::theme::Palette;
use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, slider, text, text_input,
};
use iced::{Background, Border, Element, Length, Task, Theme};

use rewynd_config::{self as config, Config};

// Theme colours, kept in one place so restyling (or a future ganked.tv house style) is one edit.
mod palette {
    use iced::Color;
    pub const BACKGROUND: Color = Color::from_rgb8(0x0d, 0x11, 0x17);
    pub const TEXT: Color = Color::from_rgb8(0xe6, 0xed, 0xf3);
    pub const ACCENT: Color = Color::from_rgb8(0x22, 0xd3, 0xee);
    pub const PANEL: Color = Color::from_rgb8(0x16, 0x1b, 0x22);
    pub const BORDER: Color = Color::from_rgb8(0x2d, 0x33, 0x3b);
    pub const SUCCESS: Color = Color::from_rgb8(0x3f, 0xb9, 0x50);
    pub const WARNING: Color = Color::from_rgb8(0xf0, 0xb4, 0x29);
    pub const DANGER: Color = Color::from_rgb8(0xf8, 0x51, 0x49);
    pub const MUTED: Color = Color::from_rgb8(0x8b, 0x94, 0x9e);
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
const BITS_PER_MBIT: u32 = 1_000_000;

fn main() -> iced::Result {
    tracing_subscriber::fmt::init();

    // Single-instance guard: a second window edits the same file, where its save clobbers the
    // first's. Held until `run` returns; the kernel releases it when the process exits.
    let _instance: Option<config::InstanceLock> = match config::acquire_settings_lock() {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            tracing::info!("rewynd settings is already open; not opening a second window");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not acquire the settings lock; opening anyway");
            None
        }
    };

    iced::application(App::load, App::update, App::view)
        .title("rewynd settings")
        .theme(App::theme)
        .window_size((900.0, 900.0))
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
    /// Mirror of the ganked.tv API key.
    api_key: String,
    /// Mirror of the ganked.tv API base URL (empty = "use the default").
    api_url: String,
    /// Mirror of the share-link base URL (empty = "use the default").
    share_url: String,
    login: LoginState,
    status: Status,
}

/// Where the ganked.tv device login stands. "Connected" is not a state here — it is derived from
/// a non-empty API key.
#[derive(Debug, Clone)]
enum LoginState {
    Idle,
    Starting,
    /// Waiting for browser approval: the user code plus the server's verification page (shown as
    /// the fallback when the browser did not open — it may not be ganked.tv when self-hosting).
    Waiting {
        code: String,
        verification_uri: String,
    },
    Failed(String),
}

/// Upload visibility choices, mirroring what the ganked.tv API accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadVis {
    Public,
    Unlisted,
}

impl UploadVis {
    const ALL: [UploadVis; 2] = [UploadVis::Public, UploadVis::Unlisted];

    fn as_config(self) -> &'static str {
        match self {
            UploadVis::Public => "public",
            UploadVis::Unlisted => "unlisted",
        }
    }

    /// Fails closed like the uploader: only an explicit `public` maps to Public, so a hand-edited
    /// typo can't be rewritten into a wider visibility on save.
    fn from_config(s: &str) -> UploadVis {
        if s.trim().eq_ignore_ascii_case("public") {
            UploadVis::Public
        } else {
            UploadVis::Unlisted
        }
    }
}

impl fmt::Display for UploadVis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UploadVis::Public => "Public",
            UploadVis::Unlisted => "Unlisted",
        })
    }
}

#[derive(Debug, Clone)]
enum Message {
    MicGain(f32),
    SystemGain(f32),
    BufferSeconds(u32),
    ResolutionPicked(Resolution),
    FpsPicked(u32),
    BitrateMbps(u32),
    OutputDirEdited(String),
    BrowseDir,
    DirPicked(Option<String>),
    HotkeyEdited(String),
    AlwaysPrompt(bool),
    UploadEnabled(bool),
    ApiKeyEdited(String),
    ApiUrlEdited(String),
    ShareUrlEdited(String),
    VisibilityPicked(UploadVis),
    LoginPressed,
    LoginStarted(Result<rewynd_upload::DeviceLogin, String>),
    LoginDone(Result<String, String>),
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
        Self {
            output_dir: config.output_directory().unwrap_or_default().to_owned(),
            hotkey: config.hotkey_trigger().to_owned(),
            api_key: config.upload_api_key().to_owned(),
            api_url: config.upload_api_url().to_owned(),
            share_url: config.upload_share_url().to_owned(),
            config,
            login: LoginState::Idle,
            status: Status::Editing,
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
                success: palette::SUCCESS,
                warning: palette::WARNING,
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
            Message::BufferSeconds(s) => {
                self.config.set_buffer_seconds(u64::from(s));
                self.touch();
            }
            Message::ResolutionPicked(r) => {
                let (width, height) = r.dims();
                let mut v = self.config.video();
                v.width = width;
                v.height = height;
                self.config.set_video(v);
                self.touch();
            }
            Message::FpsPicked(fps) => {
                let mut v = self.config.video();
                v.framerate = fps;
                // Keep ~1 keyframe per second so the ring buffer's cut granularity tracks the
                // frame rate (the defaults couple these); the UI doesn't expose the GOP directly.
                v.idr_period = fps;
                self.config.set_video(v);
                self.touch();
            }
            Message::BitrateMbps(mbps) => {
                let mut v = self.config.video();
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
            Message::AlwaysPrompt(on) => {
                self.config.set_always_prompt(on);
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
                self.config.set_upload_visibility(v.as_config().to_owned());
                self.touch();
            }
            Message::LoginPressed => {
                self.login = LoginState::Starting;
                let base = self.effective_api_url();
                return Task::perform(
                    async move {
                        rewynd_upload::device_login_start(&base, "rewynd")
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::LoginStarted,
                );
            }
            Message::LoginStarted(Ok(login)) => {
                if let Err(e) = open::that_detached(&login.verification_uri_complete) {
                    tracing::warn!(error = %e, "could not open the browser for approval");
                }
                self.login = LoginState::Waiting {
                    code: login.user_code.clone(),
                    verification_uri: login.verification_uri.clone(),
                };
                // The login itself remembers which server issued it; no base to recompute.
                return Task::perform(
                    async move {
                        rewynd_upload::device_login_wait(&login)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::LoginDone,
                );
            }
            Message::LoginStarted(Err(e)) => self.login = LoginState::Failed(e),
            Message::LoginDone(Ok(key)) => {
                // Logging in states intent: switch uploads on and persist right away.
                self.api_key = key;
                self.config.set_upload_enabled(true);
                self.login = LoginState::Idle;
                self.save();
            }
            Message::LoginDone(Err(e)) => self.login = LoginState::Failed(e),
            Message::LogoutPressed => {
                self.api_key.clear();
                self.config.set_upload_enabled(false);
                self.login = LoginState::Idle;
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

    /// Mark the form as having unsaved edits.
    fn touch(&mut self) {
        self.status = Status::Editing;
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
                    Status::Saved
                }
                Err(e) => Status::Error(format!("could not write {}: {e}", path.display())),
            },
            None => Status::Error("no config path (set $HOME or $XDG_CONFIG_HOME)".to_owned()),
        };
    }

    fn view(&self) -> Element<'_, Message> {
        // `normalize` (on load) keeps these in range; clamp on the u64 before narrowing so a
        // pathological stored value can't wrap the cast.
        let v = self.config.video();
        let a = self.config.audio();
        let mbps = v.bitrate_bps / BITS_PER_MBIT;
        let secs = self.config.buffer_seconds().min(u64::from(BUFFER_MAX_S)) as u32;
        // Rough clip size from the target bitrate (video + audio) over the replay window,
        // rounded to the nearest MB.
        let est_bytes = u64::from(v.bitrate_bps)
            .saturating_add(u64::from(a.bitrate_bps))
            .saturating_mul(u64::from(secs))
            / 8;
        let est_mb = est_bytes.saturating_add(500_000) / 1_000_000;

        let audio = card(
            "AUDIO",
            column![
                setting(
                    "Microphone volume",
                    format!("{:.2}x", self.config.mic_gain()),
                    slider(0.0..=GAIN_MAX, self.config.mic_gain(), Message::MicGain).step(0.05),
                ),
                setting(
                    "System volume",
                    format!("{:.2}x", self.config.system_gain()),
                    slider(
                        0.0..=GAIN_MAX,
                        self.config.system_gain(),
                        Message::SystemGain
                    )
                    .step(0.05),
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
                    slider(BUFFER_MIN_S..=BUFFER_MAX_S, secs, Message::BufferSeconds),
                ),
                setting(
                    "Resolution",
                    format!("{}x{}", v.width, v.height),
                    pick_list(
                        &Resolution::ALL[..],
                        Resolution::from_dims(v.width, v.height),
                        Message::ResolutionPicked,
                    )
                    .width(Length::Fill),
                ),
                setting(
                    "Frame rate",
                    format!("{} fps", v.framerate),
                    pick_list(&FPS_OPTIONS[..], Some(v.framerate), Message::FpsPicked)
                        .width(Length::Fill),
                ),
                setting(
                    "Quality",
                    format!("{mbps} Mbps"),
                    slider(
                        BITRATE_MIN_MBPS..=BITRATE_MAX_MBPS,
                        mbps,
                        Message::BitrateMbps
                    ),
                ),
                value_row("Estimated clip size", format!("about {est_mb} MB")),
            ]
            .spacing(18),
        );

        let placeholder = config::default_output_dir().map_or_else(
            || "Leave empty for the system temp folder".to_owned(),
            |p| format!("Leave empty for {}", p.display()),
        );
        let output = card(
            "OUTPUT & CAPTURE",
            column![
                column![
                    text("Save clips to").size(14),
                    row![
                        text_input(&placeholder, &self.output_dir)
                            .on_input(Message::OutputDirEdited),
                        button("Browse").on_press(Message::BrowseDir),
                    ]
                    .spacing(8),
                ]
                .spacing(8),
                column![
                    text("Hotkey").size(14),
                    text_input("CTRL+ALT+R", &self.hotkey).on_input(Message::HotkeyEdited),
                    hint("Your desktop may let you rebind this in its shortcut settings."),
                ]
                .spacing(6),
                checkbox(self.config.always_prompt())
                    .label("Re-pick the monitor on next launch")
                    .on_toggle(Message::AlwaysPrompt),
            ]
            .spacing(18),
        );

        // Account area: a one-click browser login (device grant); the key it mints is stored
        // invisibly. Connectedness is simply "a key is present".
        let account: Element<Message> = if !self.api_key.trim().is_empty() {
            row![
                text("Connected to ganked.tv").size(14).width(Length::Fill),
                button(text("Log out").size(13))
                    .on_press(Message::LogoutPressed)
                    .style(button::secondary)
                    .padding([6, 14]),
            ]
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            match &self.login {
                LoginState::Idle => column![
                    button(text("Log in with ganked.tv").size(14))
                        .on_press(Message::LoginPressed)
                        .padding([10, 20]),
                    hint("Approve rewynd in your browser; no password is stored on this device."),
                ]
                .spacing(6)
                .into(),
                LoginState::Starting => text("Contacting ganked.tv...").size(14).into(),
                LoginState::Waiting {
                    code,
                    verification_uri,
                } => column![
                    text(format!("Approve in your browser with code {code}")).size(14),
                    hint(format!(
                        "Browser did not open? Go to {verification_uri} and enter the code."
                    )),
                ]
                .spacing(6)
                .into(),
                LoginState::Failed(e) => column![
                    button(text("Log in with ganked.tv").size(14))
                        .on_press(Message::LoginPressed)
                        .padding([10, 20]),
                    text(e.clone()).size(12).style(|_: &Theme| text::Style {
                        color: Some(palette::DANGER),
                    }),
                ]
                .spacing(6)
                .into(),
            }
        };

        let upload = card(
            "GANKED.TV",
            column![
                account,
                checkbox(self.config.upload_enabled())
                    .label("Enable uploads (tray: Upload last clip)")
                    .on_toggle(Message::UploadEnabled),
                setting(
                    "Visibility",
                    UploadVis::from_config(self.config.upload_visibility()).to_string(),
                    pick_list(
                        &UploadVis::ALL[..],
                        Some(UploadVis::from_config(self.config.upload_visibility())),
                        Message::VisibilityPicked,
                    )
                    .width(Length::Fill),
                ),
                column![
                    text("API server").size(14),
                    text_input(config::DEFAULT_UPLOAD_API_URL, &self.api_url)
                        .on_input(Message::ApiUrlEdited),
                ]
                .spacing(6),
                column![
                    text("Share links").size(14),
                    text_input(config::DEFAULT_UPLOAD_SHARE_URL, &self.share_url)
                        .on_input(Message::ShareUrlEdited),
                    hint("Leave both empty for ganked.tv. Self-hosting? An API key can be pasted below."),
                ]
                .spacing(6),
                column![
                    text("API key (advanced)").size(14),
                    text_input("gtv_...", &self.api_key)
                        .secure(true)
                        .on_input(Message::ApiKeyEdited),
                ]
                .spacing(6),
            ]
            .spacing(18),
        );

        let header = column![
            text("rewynd settings").size(28),
            hint("Tune how clips are captured and where they are saved. Changes take effect the next time you clip."),
        ]
        .spacing(6);

        let mut save_items: Vec<Element<Message>> = vec![
            button(text("Save settings").size(15))
                .on_press(Message::Save)
                .padding([12, 28])
                .into(),
            status_line(&self.status),
        ];
        // Offer a one-click restart once the file is saved (and let it retry after a failed one),
        // since changes only apply on restart. Hidden while a restart is in flight.
        if matches!(self.status, Status::Saved | Status::Error(_)) {
            save_items.push(
                button(text("Restart rewynd now").size(14))
                    .on_press(Message::Restart)
                    .padding([8, 22])
                    .style(button::secondary)
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

        // The scrollable is a safety net for very small windows; at the default size the content
        // fits, so it never actually scrolls (which keeps the software renderer smooth).
        container(scrollable(body))
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
    let mut v = c.video();
    let mbps = (v.bitrate_bps / BITS_PER_MBIT).clamp(BITRATE_MIN_MBPS, BITRATE_MAX_MBPS);
    v.bitrate_bps = mbps * BITS_PER_MBIT;
    c.set_video(v);
}

/// A grouped settings card: an accent title over its content, on a raised rounded panel.
fn card<'a>(title: &'a str, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let inner = column![
        text(title).size(13).style(|_: &Theme| text::Style {
            color: Some(palette::ACCENT),
        }),
        content.into(),
    ]
    .spacing(16);
    container(inner)
        .width(Length::Fill)
        .padding(20)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(palette::PANEL)),
            border: Border {
                color: palette::BORDER,
                width: 1.0,
                radius: 14.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// One setting: the label on the left, its current value on the right (accent), and the control
/// directly beneath spanning the full width.
fn setting<'a>(
    label: &'a str,
    value: String,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![value_row(label, value), control.into()]
        .spacing(8)
        .into()
}

/// A label (left) and a value (right, accent) on one row — also used for read-only readouts.
fn value_row<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    row![
        text(label).size(14).width(Length::Fill),
        text(value).size(14).style(|_: &Theme| text::Style {
            color: Some(palette::ACCENT),
        }),
    ]
    .into()
}

/// Muted hint text.
fn hint<'a>(s: impl Into<String>) -> Element<'a, Message> {
    text(s.into())
        .size(12)
        .style(|_: &Theme| text::Style {
            color: Some(palette::MUTED),
        })
        .into()
}

/// The save-status line under the Save button.
fn status_line(status: &Status) -> Element<'_, Message> {
    let (msg, color) = match status {
        Status::Editing => (String::new(), palette::MUTED),
        Status::Saved => (
            "Saved. Restart rewynd to apply the changes.".to_owned(),
            palette::SUCCESS,
        ),
        Status::Restarting => ("Restarting rewynd...".to_owned(), palette::MUTED),
        Status::Restarted => (
            "Restarted rewynd with the new settings.".to_owned(),
            palette::SUCCESS,
        ),
        Status::Error(e) => (e.clone(), palette::DANGER),
    };
    text(msg)
        .size(13)
        .style(move |_: &Theme| text::Style { color: Some(color) })
        .into()
}

/// Recorder binary, expected next to this settings binary.
const RECORDER_BIN: &str = if cfg!(windows) {
    "rewynd.exe"
} else {
    "rewynd"
};

/// Stop the running recorder (if any), wait for it to exit, then launch a fresh one so it picks
/// up the saved config. Blocking — runs on a `spawn_blocking` thread, not the UI thread.
fn restart_recorder() -> Result<(), String> {
    stop_running_recorder();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let recorder = exe
        .parent()
        .ok_or_else(|| "settings binary has no parent directory".to_owned())?
        .join(RECORDER_BIN);
    std::process::Command::new(&recorder)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not start {}: {e}", recorder.display()))
}

/// SIGTERM the running recorder (if its pid is still a rewynd process) and wait for it to exit, so
/// it has dropped the global hotkey and ScreenCast portal before the replacement starts.
#[cfg(unix)]
fn stop_running_recorder() {
    use std::time::Duration;
    let Ok(contents) = std::fs::read_to_string(config::recorder_pid_path()) else {
        return;
    };
    // The pid file is newline-framed; the first line is a clean pid even mid-rewrite.
    let pid = contents.lines().next().unwrap_or("").trim();
    if pid.is_empty() {
        return;
    }
    // Guard against a stale/reused pid: only signal it if it's still a rewynd process, and pin its
    // start-time identity so a pid reused mid-shutdown can't be mistaken for it (and SIGKILLed).
    if !std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "rewynd") {
        return;
    }
    let identity = proc_start_time(pid);
    let _ = std::process::Command::new("kill").arg(pid).status();
    // The recorder releases the portal/hotkey (and the single-instance lock) as it dies; wait
    // (bounded) for that before relaunch.
    if wait_for_exit(pid, &identity, Duration::from_secs(3)) {
        return;
    }
    // It outlived SIGTERM and is still the same process — escalate so the replacement isn't
    // refused by the lock the old process still holds.
    let _ = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(pid)
        .status();
    wait_for_exit(pid, &identity, Duration::from_secs(2));
}

/// `/proc/<pid>/stat` field 22 (start time in clock ticks): with the pid, a stable identity that
/// distinguishes the original process from a reused pid. `None` if unreadable.
#[cfg(unix)]
fn proc_start_time(pid: &str) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) is parenthesised and may itself contain spaces/parens, so resume after the
    // last ')': the remaining tokens start at field 3, putting start time at index 19.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
}

/// Whether the original recorder (pid + start-time identity) is still running.
#[cfg(unix)]
fn still_running(pid: &str, identity: &Option<String>) -> bool {
    match identity {
        Some(start) => proc_start_time(pid).as_deref() == Some(start),
        // No identity captured: fall back to bare pid existence.
        None => std::path::Path::new(&format!("/proc/{pid}")).exists(),
    }
}

/// Poll until the original recorder has exited (or its pid was reused) or the timeout elapses.
#[cfg(unix)]
fn wait_for_exit(pid: &str, identity: &Option<String>, timeout: std::time::Duration) -> bool {
    use std::time::Instant;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !still_running(pid, identity) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    !still_running(pid, identity)
}

#[cfg(not(unix))]
fn stop_running_recorder() {}

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

    #[test]
    fn upload_visibility_round_trips_and_fails_closed() {
        for v in UploadVis::ALL {
            assert_eq!(UploadVis::from_config(v.as_config()), v);
        }
        // A typo must never widen visibility.
        assert_eq!(UploadVis::from_config("garbage"), UploadVis::Unlisted);
    }
}
