//! The ganked.tv "Arena" design system (docs/design/arena.md) as iced styles and small
//! widgets, shared by the library and settings views: a near-black surface ladder for depth
//! (base → raised → high; the system forbids shadows), one mint accent owning every
//! interactive state (no red — errors are the palette's one deliberate exception),
//! hairline borders.

use std::sync::LazyLock;

use iced::widget::{button, checkbox, column, container, pick_list, row, slider, text, text_input};
use iced::{Background, Border, Element, Font, Length, Theme, font};

use rewynd_config as config;

pub mod palette {
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

    // YouTube's brand red, for the upload panel when YouTube is the chosen destination. The one
    // place the app steps off its single mint accent, so the widget reads as "this goes to
    // YouTube". Softened from pure #FF0000 to sit on the dark surface; white ink on top.
    pub const YOUTUBE: Color = Color::from_rgb8(0xff, 0x33, 0x33);
    pub const YOUTUBE_HOVER: Color = Color::from_rgb8(0xff, 0x4d, 0x4d);
    pub const INK_ON_YOUTUBE: Color = Color::from_rgb8(0xff, 0xff, 0xff);
}

/// Display face for headings: Barlow Condensed, always uppercase per the design.
pub const DISPLAY_BLACK: Font = Font {
    family: font::Family::Name("Barlow Condensed"),
    weight: font::Weight::Black,
    ..Font::DEFAULT
};
/// UI face: Inter (Regular is the default font; these are the heavier cuts).
pub const UI_SEMIBOLD: Font = Font {
    family: font::Family::Name("Inter"),
    weight: font::Weight::Semibold,
    ..Font::DEFAULT
};
pub const UI_BOLD: Font = Font {
    family: font::Family::Name("Inter"),
    weight: font::Weight::Bold,
    ..Font::DEFAULT
};

/// The shipped brand-mark PNG nearest at or above `size` pixels (falling back to the largest),
/// from the ladder the config crate owns.
pub fn brand_png(size: u32) -> &'static [u8] {
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
pub fn logo<'a, M: 'a>(size: f32) -> Element<'a, M> {
    let handle = if size <= 24.0 {
        LOGO_SMALL.clone()
    } else {
        LOGO_LARGE.clone()
    };
    iced::widget::image(handle).width(size).height(size).into()
}

/// The window icon, decoded from the shipped PNG render of the mark (X11/Windows; see the
/// `window::Settings` note for Wayland).
pub fn window_icon() -> Option<iced::window::Icon> {
    let img = image::load_from_memory_with_format(brand_png(64), image::ImageFormat::Png).ok()?;
    let (width, height) = (img.width(), img.height());
    iced::window::icon::from_rgba(img.into_rgba8().into_vec(), width, height).ok()
}

/// A grouped card, Arena style: raised panel, hairline border, 8px radius, with the
/// title as a small uppercase eyebrow in the accent (mint by default).
pub fn card<'a, M: 'a>(title: &'a str, content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    card_accent(title, palette::ACCENT, content)
}

/// [`card`] with an explicit eyebrow accent, for the upload panel whose colour follows the
/// chosen destination (mint for ganked.tv, red for YouTube).
pub fn card_accent<'a, M: 'a>(
    title: &'a str,
    accent: iced::Color,
    content: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    let inner = column![
        text(title).size(10).font(UI_BOLD).style(tinted(accent)),
        content.into(),
    ]
    .spacing(14);
    container(inner)
        .width(Length::Fill)
        .padding(18)
        .style(card_style)
        .into()
}

/// The card container style alone, for content that is not a titled group.
pub fn card_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(palette::PANEL)),
        border: Border {
            color: palette::BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    }
}

/// Primary (mint) button per the Arena spec: filled accent, ink text, 8px radius.
pub fn primary_button(_theme: &Theme, status: button::Status) -> button::Style {
    accent_button_style(
        status,
        palette::ACCENT,
        palette::ACCENT_HOVER,
        palette::INK_ON_ACCENT,
    )
}

/// A filled primary button in an arbitrary brand accent (mint by default via `primary_button`, red
/// for the YouTube upload destination). Same shape as `primary_button`, just parameterized.
pub fn accent_button_style(
    status: button::Status,
    accent: iced::Color,
    accent_hover: iced::Color,
    ink: iced::Color,
) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => accent_hover,
        button::Status::Disabled => palette::HIGH,
        _ => accent,
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: if matches!(status, button::Status::Disabled) {
            palette::MUTED
        } else {
            ink
        },
        border: Border {
            radius: 8.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// OAuth sign-in shell: unlike `primary_button` the fill stays a dark well so the gradient
/// mark carries the brand; hover tints it mint.
pub fn oauth_button(_theme: &Theme, status: button::Status) -> button::Style {
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

/// Quiet link-style button (disclosures, back links): bare text, mint on hover.
pub fn link_button(_theme: &Theme, status: button::Status) -> button::Style {
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
pub fn secondary_button(_theme: &Theme, status: button::Status) -> button::Style {
    let (border_color, text_color) = match status {
        button::Status::Hovered | button::Status::Pressed => (palette::ACCENT, palette::ACCENT),
        button::Status::Disabled => (palette::BORDER, palette::MUTED),
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
pub fn arena_input(_theme: &Theme, status: text_input::Status) -> text_input::Style {
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
pub fn arena_pick(_theme: &Theme, status: pick_list::Status) -> pick_list::Style {
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
pub fn arena_check(_theme: &Theme, status: checkbox::Status) -> checkbox::Style {
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
pub fn arena_slider(_theme: &Theme, _status: slider::Status) -> slider::Style {
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
pub fn setting<'a, M: 'a>(
    label: &'a str,
    value: String,
    control: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    column![value_row(label, value), control.into()]
        .spacing(7)
        .into()
}

/// A label (left) and a value (right, accent) on one row — also used for read-only readouts.
pub fn value_row<'a, M: 'a>(label: &'a str, value: String) -> Element<'a, M> {
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
pub fn tinted(color: iced::Color) -> impl Fn(&Theme) -> text::Style {
    move |_| text::Style { color: Some(color) }
}

/// A labelled form field: [`field_label`] over the control. Returns the column so a caller can
/// `.push` a trailing [`hint`].
pub fn field<'a, M: 'a>(
    label: &'a str,
    control: impl Into<Element<'a, M>>,
) -> iced::widget::Column<'a, M> {
    column![field_label(label), control.into()].spacing(6)
}

/// Arena field label: small, bold, uppercase, secondary.
pub fn field_label<'a, M: 'a>(s: &str) -> Element<'a, M> {
    text(s.to_uppercase())
        .size(10)
        .font(UI_BOLD)
        .style(tinted(palette::TEXT_SECONDARY))
        .into()
}

/// Muted hint text.
pub fn hint<'a, M: 'a>(s: impl Into<String>) -> Element<'a, M> {
    text(s.into()).size(11).style(tinted(palette::MUTED)).into()
}

/// An accent chip (game tags, connected badges): mint text on the mint tint, 5px radius.
pub fn accent_chip<'a, M: 'a>(label: String) -> Element<'a, M> {
    container(
        text(label.to_uppercase())
            .size(9)
            .font(UI_BOLD)
            .style(tinted(palette::ACCENT)),
    )
    .padding([3, 7])
    .style(|_: &Theme| container::Style {
        background: Some(Background::Color(palette::ACCENT_BG)),
        border: Border {
            color: palette::ACCENT_BORDER,
            width: 1.0,
            radius: 5.0.into(),
        },
        ..container::Style::default()
    })
    .into()
}
