#![cfg_attr(windows, windows_subsystem = "windows")]
//! The branded rewynd installer (docs/adr/0017): one borderless Arena-styled window — logo,
//! version, a single Install button — driving the embedded Velopack Setup.exe silently.
//! Everything on disk is what Setup.exe itself lays down, so the update contract
//! (Update.exe, deltas, the in-app one-click update) is untouched.

mod setup;

use std::sync::LazyLock;

use iced::widget::{
    Space, button, column, container, mouse_area, progress_bar, row, scrollable, text,
};
use iced::{Background, Border, Element, Font, Length, Subscription, Task, Theme, font};
use rewynd_config as config;

/// The Arena values this window uses — a hand-kept mirror of `crates/settings/src/theme.rs`
/// (docs/design/arena.md); a shared theme crate for one extra window isn't worth the split.
mod palette {
    use iced::Color;
    pub const BACKGROUND: Color = Color::from_rgb8(0x0b, 0x0b, 0x0f);
    pub const HIGH: Color = Color::from_rgb8(0x18, 0x18, 0x1f);
    pub const TEXT: Color = Color::from_rgb8(0xf0, 0xf0, 0xf4);
    pub const TEXT_SECONDARY: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.50);
    pub const MUTED: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.28);
    pub const BORDER: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.07);
    pub const ACCENT: Color = Color::from_rgb8(0x00, 0xe5, 0xa0);
    pub const ACCENT_HOVER: Color = Color::from_rgb8(0x0d, 0xf3, 0xab);
    pub const INK_ON_ACCENT: Color = Color::from_rgb8(0x08, 0x12, 0x0e);
    pub const DANGER: Color = Color::from_rgb8(0xff, 0x5a, 0x5f);
}

const DISPLAY_BLACK: Font = Font {
    family: font::Family::Name("Barlow Condensed"),
    weight: font::Weight::Black,
    ..Font::DEFAULT
};
const UI_SEMIBOLD: Font = Font {
    family: font::Family::Name("Inter"),
    weight: font::Weight::Semibold,
    ..Font::DEFAULT
};

const WINDOW_WIDTH: f32 = 440.0;
const WINDOW_HEIGHT: f32 = 380.0;
/// One sweep of the indeterminate bar per ~1.6 s at the 30 Hz tick.
const SWEEP_STEP: f32 = 0.02;
/// How long the "done" state stays on screen before the window closes itself.
const DONE_LINGER: std::time::Duration = std::time::Duration::from_millis(1600);

fn main() -> iced::Result {
    tracing_subscriber::fmt::init();
    iced::application(Installer::new, Installer::update, Installer::view)
        .title("rewynd installer")
        .theme(Installer::theme)
        .subscription(Installer::subscription)
        // The Arena faces, same OFL-licensed files the app ships.
        .font(include_bytes!("../../settings/assets/fonts/BarlowCondensed-Black.ttf").as_slice())
        .font(include_bytes!("../../settings/assets/fonts/Inter-Regular.ttf").as_slice())
        .font(include_bytes!("../../settings/assets/fonts/Inter-SemiBold.ttf").as_slice())
        .default_font(Font {
            family: font::Family::Name("Inter"),
            ..Font::DEFAULT
        })
        .window(iced::window::Settings {
            size: iced::Size::new(WINDOW_WIDTH, WINDOW_HEIGHT),
            position: iced::window::Position::Centered,
            resizable: false,
            // Borderless launcher look; the window is its own chrome (drag anywhere, × to
            // close).
            decorations: false,
            icon: window_icon(),
            // Close requests go through `update`, which refuses them mid-install: exiting
            // then would abandon a running Setup.exe and skip the app launch.
            exit_on_close_request: false,
            ..iced::window::Settings::default()
        })
        .run()
}

/// The window icon, from the shipped brand-mark PNGs.
fn window_icon() -> Option<iced::window::Icon> {
    let img =
        image::load_from_memory_with_format(config::brand_png(64), image::ImageFormat::Png).ok()?;
    let (width, height) = (img.width(), img.height());
    iced::window::icon::from_rgba(img.into_rgba8().into_vec(), width, height).ok()
}

// Decoded once: a fresh handle every `view` call would miss the renderer's raster cache and
// re-decode the PNG each frame — and the install phase redraws at the tick rate.
static LOGO: LazyLock<iced::widget::image::Handle> =
    LazyLock::new(|| iced::widget::image::Handle::from_bytes(config::brand_png(128)));

#[derive(Default)]
enum Phase {
    #[default]
    Ready,
    Installing,
    Done,
    Failed(String),
}

#[derive(Default)]
struct Installer {
    phase: Phase,
    /// Monotone tick count while installing; the bar position is the derived triangle wave.
    sweep: f32,
}

#[derive(Debug, Clone)]
enum Message {
    Install,
    Tick,
    Finished(Result<(), String>),
    Close,
    Drag,
}

impl Installer {
    fn new() -> (Self, Task<Message>) {
        (Self::default(), Task::none())
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Install => {
                self.phase = Phase::Installing;
                self.sweep = 0.0;
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(setup::run)
                            .await
                            .unwrap_or_else(|e| Err(format!("the install task died: {e}")))
                    },
                    Message::Finished,
                );
            }
            Message::Tick => self.sweep += SWEEP_STEP,
            Message::Finished(Ok(())) => {
                self.phase = Phase::Done;
                return Task::perform(tokio::time::sleep(DONE_LINGER), |()| Message::Close);
            }
            Message::Finished(Err(e)) => self.phase = Phase::Failed(e),
            Message::Close => {
                // Mid-install there is nothing sane to cancel: Setup.exe would keep running
                // detached and the launch/cleanup would be skipped. Refuse; the button is
                // hidden then anyway, this also catches Alt+F4.
                if !matches!(self.phase, Phase::Installing) {
                    return iced::exit();
                }
            }
            Message::Drag => {
                return iced::window::latest().and_then(iced::window::drag);
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        let ticks = match self.phase {
            Phase::Installing => {
                iced::time::every(std::time::Duration::from_millis(33)).map(|_| Message::Tick)
            }
            _ => Subscription::none(),
        };
        // OS close requests (Alt+F4) arrive here because `exit_on_close_request` is off;
        // `update` decides whether closing is allowed right now.
        let close = iced::window::close_requests().map(|_| Message::Close);
        Subscription::batch([ticks, close])
    }

    fn view(&self) -> Element<'_, Message> {
        let close = button(text("×").size(16).style(tinted(palette::MUTED)))
            .style(|_: &Theme, status: button::Status| button::Style {
                background: None,
                text_color: match status {
                    button::Status::Hovered | button::Status::Pressed => palette::TEXT,
                    _ => palette::MUTED,
                },
                ..button::Style::default()
            })
            .padding([2, 8])
            .on_press(Message::Close);

        let mark = iced::widget::image(LOGO.clone()).width(64.0).height(64.0);

        let body: Element<'_, Message> = match &self.phase {
            Phase::Ready => column![
                primary("INSTALL", Message::Install),
                text("Installs just for you, no admin needed.")
                    .size(11)
                    .style(tinted(palette::MUTED)),
            ]
            .spacing(12)
            .align_x(iced::Alignment::Center)
            .into(),
            Phase::Installing => column![
                progress_bar(0.0..=1.0, triangle(self.sweep))
                    .girth(6.0)
                    .length(Length::Fixed(240.0))
                    .style(|_: &Theme| progress_bar::Style {
                        background: Background::Color(palette::HIGH),
                        bar: Background::Color(palette::ACCENT),
                        border: Border {
                            radius: 3.0.into(),
                            ..Border::default()
                        },
                    }),
                text("Installing...")
                    .size(12)
                    .font(UI_SEMIBOLD)
                    .style(tinted(palette::TEXT_SECONDARY)),
            ]
            .spacing(14)
            .align_x(iced::Alignment::Center)
            .into(),
            Phase::Done => column![
                text("Done. rewynd is starting.")
                    .size(13)
                    .font(UI_SEMIBOLD)
                    .style(tinted(palette::ACCENT)),
            ]
            .align_x(iced::Alignment::Center)
            .into(),
            // The log tail can be long; a capped scrollable keeps the CLOSE button on
            // screen inside the fixed window.
            Phase::Failed(why) => column![
                scrollable(text(why.as_str()).size(11).style(tinted(palette::DANGER)))
                    .height(Length::Fixed(96.0)),
                primary("CLOSE", Message::Close),
            ]
            .spacing(14)
            .align_x(iced::Alignment::Center)
            .into(),
        };

        // No close affordance mid-install: there is no cancel, only abandonment.
        let top_right: Element<'_, Message> = if matches!(self.phase, Phase::Installing) {
            Space::new().height(20.0).into()
        } else {
            close.into()
        };
        let content = column![
            row![Space::new().width(Length::Fill), top_right],
            Space::new().height(Length::Fill),
            mark,
            text("REWYND")
                .size(36)
                .font(DISPLAY_BLACK)
                .style(tinted(palette::TEXT)),
            text(format!("version {}", env!("CARGO_PKG_VERSION")))
                .size(11)
                .style(tinted(palette::TEXT_SECONDARY)),
            Space::new().height(22.0),
            body,
            Space::new().height(Length::Fill),
        ]
        .align_x(iced::Alignment::Center)
        .width(Length::Fill);

        // The whole surface is the drag handle; interactive children (buttons) win the click.
        mouse_area(
            container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding([10, 24])
                .style(|_: &Theme| container::Style {
                    background: Some(Background::Color(palette::BACKGROUND)),
                    border: Border {
                        color: palette::BORDER,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..container::Style::default()
                }),
        )
        .on_press(Message::Drag)
        .into()
    }
}

/// The primary (mint) Arena button, wide enough to be the window's one obvious action.
fn primary(label: &str, on_press: Message) -> Element<'_, Message> {
    button(
        text(label)
            .size(13)
            .font(UI_SEMIBOLD)
            .width(Length::Fill)
            .align_x(iced::Alignment::Center),
    )
    .width(Length::Fixed(240.0))
    .padding([12, 16])
    .style(|_: &Theme, status: button::Status| button::Style {
        background: Some(Background::Color(match status {
            button::Status::Hovered | button::Status::Pressed => palette::ACCENT_HOVER,
            button::Status::Disabled => palette::HIGH,
            _ => palette::ACCENT,
        })),
        text_color: palette::INK_ON_ACCENT,
        border: Border {
            radius: 8.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    })
    .on_press(on_press)
    .into()
}

/// A text style closure for one fixed color.
fn tinted(color: iced::Color) -> impl Fn(&Theme) -> iced::widget::text::Style {
    move |_| iced::widget::text::Style { color: Some(color) }
}

/// A 0..1 triangle wave over a monotone phase: the indeterminate bar's bounce.
fn triangle(phase: f32) -> f32 {
    let t = phase % 2.0;
    if t > 1.0 { 2.0 - t } else { t }
}
