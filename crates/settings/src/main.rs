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

/// Theme colours — a modern charcoal/indigo palette. Tweak here to restyle the whole window.
mod palette {
    use iced::Color;
    /// Deep charcoal window background (not hard black).
    pub const BACKGROUND: Color = Color::from_rgb8(0x15, 0x17, 0x1e);
    /// Soft off-white text.
    pub const TEXT: Color = Color::from_rgb8(0xe7, 0xe9, 0xf0);
    /// Vibrant indigo/violet accent for buttons, slider rails, active controls.
    pub const ACCENT: Color = Color::from_rgb8(0x7c, 0x5c, 0xff);
    /// Raised card surface, a step lighter than the window background.
    pub const PANEL: Color = Color::from_rgb8(0x1d, 0x20, 0x2b);
    /// Hairline border around cards.
    pub const BORDER: Color = Color::from_rgb8(0x2c, 0x30, 0x3e);
    /// Green for the "saved" confirmation.
    pub const SUCCESS: Color = Color::from_rgb8(0x40, 0xd0, 0x8b);
    /// Amber for the restart hint.
    pub const WARNING: Color = Color::from_rgb8(0xf5, 0xc2, 0x4b);
    /// Red for errors.
    pub const DANGER: Color = Color::from_rgb8(0xff, 0x5c, 0x5c);
    /// Muted text for hints/sublabels.
    pub const MUTED: Color = Color::from_rgb8(0x9a, 0x9f, 0xb3);
}

/// Slider bounds (kept generous but sane).
const GAIN_MAX: f32 = 4.0;
const BUFFER_MIN_S: u32 = 5;
/// Slider ceiling for the replay length: the same cap the daemon enforces, so the slider and the
/// recorder agree (no value the slider shows differently from what's used). At this ceiling the
/// common ~60 s default sits about a quarter of the way along rather than pinned to the left.
const BUFFER_MAX_S: u32 = config::MAX_BUFFER_SECONDS as u32;
const BITRATE_MIN_MBPS: u32 = 1;
const BITRATE_MAX_MBPS: u32 = 50;
/// Frame-rate options offered in the dropdown.
const FPS_OPTIONS: [u32; 4] = [30, 60, 120, 144];
const BITS_PER_MBIT: u32 = 1_000_000;

fn main() -> iced::Result {
    tracing_subscriber::fmt::init();
    iced::application(App::load, App::update, App::view)
        .title("rewynd settings")
        .theme(App::theme)
        .window_size((540.0, 700.0))
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

/// Save feedback shown under the Save button.
#[derive(Debug, Clone)]
enum Status {
    /// Unsaved edits (or a fresh load).
    Editing,
    /// Written to disk; the recorder must restart to pick it up.
    Saved,
    /// Writing failed.
    Error(String),
}

/// Editable application state: the loaded config plus text mirrors for the free-text fields.
struct App {
    config: Config,
    /// Mirror of the output directory for the text box (empty = "use the default").
    output_dir: String,
    /// Mirror of the hotkey trigger for the text box.
    hotkey: String,
    status: Status,
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
    Save,
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
            config,
            status: Status::Editing,
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
            Message::Save => self.save(),
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

        let header = column![
            text("rewynd settings").size(28),
            hint("Tune how clips are captured and where they are saved. Changes take effect the next time you clip."),
        ]
        .spacing(6);

        let save = column![
            button(text("Save settings").size(15))
                .on_press(Message::Save)
                .padding([12, 28]),
            status_line(&self.status),
        ]
        .spacing(10);

        let body = column![header, audio, recording, output, save]
            .spacing(20)
            .padding(28)
            .max_width(600);

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
fn hint(s: &str) -> Element<'_, Message> {
    text(s.to_owned())
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
        Status::Error(e) => (e.clone(), palette::DANGER),
    };
    text(msg)
        .size(13)
        .style(move |_: &Theme| text::Style { color: Some(color) })
        .into()
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
