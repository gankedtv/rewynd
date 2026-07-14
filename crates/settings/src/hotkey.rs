//! The hotkey field: a capture control, not a text box. Click it, press the combo, done.
//! Shared by the settings page and the onboarding wizard; the pure key-to-trigger mapping
//! lives here so both route their captured presses through the same rules.

use iced::keyboard::key::Named;
use iced::keyboard::{Key, Modifiers};
use iced::widget::{button, row, text};
use iced::{Border, Element};

use crate::theme::{UI_SEMIBOLD, kbd_chip, palette, tinted};

/// What a key press means to a recording hotkey field.
pub enum Capture {
    /// A complete combo: the new trigger, in the `CTRL+ALT+R` form every backend parses.
    Done(String),
    /// Escape: stop recording and keep the old trigger.
    Cancel,
    /// A bare modifier, or a key no backend can bind: keep listening.
    Pending,
}

/// Fold a pressed key and the live modifiers into a trigger string. Keys are limited to what
/// every hotkey backend parses (A-Z, 0-9, F1-F24); letters and digits need at least one
/// modifier, or the combo would fire on plain typing — a bare F-key is fine.
pub fn capture(key: &Key, modifiers: Modifiers) -> Capture {
    if matches!(key, Key::Named(Named::Escape)) {
        return Capture::Cancel;
    }
    let Some(token) = key_token(key) else {
        return Capture::Pending;
    };
    let mut parts: Vec<&str> = Vec::new();
    if modifiers.control() {
        parts.push("CTRL");
    }
    if modifiers.alt() {
        parts.push("ALT");
    }
    if modifiers.shift() {
        parts.push("SHIFT");
    }
    if modifiers.logo() {
        parts.push("SUPER");
    }
    if parts.is_empty() && !token.starts_with('F') {
        return Capture::Pending;
    }
    parts.push(&token);
    Capture::Done(parts.join("+"))
}

/// The trigger token for a key, if it is one the recorder can bind everywhere.
fn key_token(key: &Key) -> Option<String> {
    match key {
        // `key` is the layout-resolved key WITHOUT modifiers applied, so SHIFT+2 arrives as
        // "2", not "@".
        Key::Character(c) => {
            let mut chars = c.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else {
                return None;
            };
            let up = ch.to_ascii_uppercase();
            (up.is_ascii_uppercase() || up.is_ascii_digit()).then(|| up.to_string())
        }
        Key::Named(named) => f_key_token(*named),
        _ => None,
    }
}

fn f_key_token(named: Named) -> Option<String> {
    let n = match named {
        Named::F1 => 1,
        Named::F2 => 2,
        Named::F3 => 3,
        Named::F4 => 4,
        Named::F5 => 5,
        Named::F6 => 6,
        Named::F7 => 7,
        Named::F8 => 8,
        Named::F9 => 9,
        Named::F10 => 10,
        Named::F11 => 11,
        Named::F12 => 12,
        Named::F13 => 13,
        Named::F14 => 14,
        Named::F15 => 15,
        Named::F16 => 16,
        Named::F17 => 17,
        Named::F18 => 18,
        Named::F19 => 19,
        Named::F20 => 20,
        Named::F21 => 21,
        Named::F22 => 22,
        Named::F23 => 23,
        Named::F24 => 24,
        _ => return None,
    };
    Some(format!("F{n}"))
}

/// The hotkey string split on `+` into uppercase keycap labels, empties dropped.
pub fn parts(s: &str) -> Vec<String> {
    s.split('+')
        .map(|p| p.trim().to_uppercase())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The hotkey as keycap chips joined by muted plus signs; the default combo when the value has
/// no usable parts.
pub fn chips<'a, M: 'a>(hotkey: &str) -> Element<'a, M> {
    let mut split = parts(hotkey);
    if split.is_empty() {
        split = parts(rewynd_config::DEFAULT_HOTKEY_TRIGGER);
    }
    let mut chips = row![].spacing(6).align_y(iced::Alignment::Center);
    for (i, part) in split.into_iter().enumerate() {
        if i > 0 {
            chips = chips.push(text("+").size(11).style(tinted(palette::MUTED)));
        }
        chips = chips.push(kbd_chip(part));
    }
    chips.into()
}

/// The field itself: an input-styled button showing the combo as keycaps. A click arms it
/// (`on_arm`), a second click disarms (`on_disarm`); while armed it invites the press and shows
/// the Escape way out. The key events themselves arrive through the app's subscription, not
/// this widget.
pub fn field<'a, M: Clone + 'a>(
    trigger: &str,
    recording: bool,
    on_arm: M,
    on_disarm: M,
) -> Element<'a, M> {
    let content: Element<'a, M> = if recording {
        row![
            text("Press your shortcut...")
                .size(13)
                .style(tinted(palette::ACCENT)),
            iced::widget::Space::new().width(iced::Length::Fill),
            kbd_chip("esc"),
            text("cancel").size(11).style(tinted(palette::MUTED)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into()
    } else {
        row![
            chips(trigger),
            iced::widget::Space::new().width(iced::Length::Fill),
            text("Click to change")
                .size(11)
                .font(UI_SEMIBOLD)
                .style(tinted(palette::MUTED)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into()
    };
    button(content)
        .on_press(if recording { on_disarm } else { on_arm })
        .width(iced::Length::Fill)
        .padding([10, 12])
        .style(move |_: &iced::Theme, status| field_style(recording, status))
        .into()
}

/// Input-well look for the field button: surface-high, hairline border, mint while armed or
/// focused — the same silhouette as [`crate::theme::arena_input`].
fn field_style(
    recording: bool,
    status: iced::widget::button::Status,
) -> iced::widget::button::Style {
    let border_color = if recording {
        palette::ACCENT
    } else {
        match status {
            iced::widget::button::Status::Hovered | iced::widget::button::Status::Pressed => {
                palette::BORDER_STRONG
            }
            _ => palette::BORDER,
        }
    };
    iced::widget::button::Style {
        background: Some(iced::Background::Color(palette::HIGH)),
        text_color: palette::TEXT,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..iced::widget::button::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool, logo: bool) -> Modifiers {
        let mut m = Modifiers::empty();
        m.set(Modifiers::CTRL, ctrl);
        m.set(Modifiers::ALT, alt);
        m.set(Modifiers::SHIFT, shift);
        m.set(Modifiers::LOGO, logo);
        m
    }

    #[test]
    fn letters_and_digits_need_a_modifier() {
        let key = Key::Character("r".into());
        assert!(matches!(
            capture(&key, mods(false, false, false, false)),
            Capture::Pending
        ));
        match capture(&key, mods(true, true, false, false)) {
            Capture::Done(s) => assert_eq!(s, "CTRL+ALT+R"),
            _ => panic!("expected a combo"),
        }
        match capture(&Key::Character("7".into()), mods(false, false, true, true)) {
            Capture::Done(s) => assert_eq!(s, "SHIFT+SUPER+7"),
            _ => panic!("expected a combo"),
        }
    }

    #[test]
    fn function_keys_bind_bare_or_modified() {
        match capture(&Key::Named(Named::F9), Modifiers::empty()) {
            Capture::Done(s) => assert_eq!(s, "F9"),
            _ => panic!("expected a combo"),
        }
        match capture(&Key::Named(Named::F24), mods(true, false, false, false)) {
            Capture::Done(s) => assert_eq!(s, "CTRL+F24"),
            _ => panic!("expected a combo"),
        }
    }

    #[test]
    fn escape_cancels_and_unbindables_wait() {
        assert!(matches!(
            capture(&Key::Named(Named::Escape), Modifiers::empty()),
            Capture::Cancel
        ));
        // A bare modifier press arrives as a Named key: not a combo yet.
        assert!(matches!(
            capture(&Key::Named(Named::Control), mods(true, false, false, false)),
            Capture::Pending
        ));
        // Layout keys outside A-Z/0-9 (umlauts, punctuation) have no portable VK mapping.
        assert!(matches!(
            capture(&Key::Character("é".into()), mods(true, false, false, false)),
            Capture::Pending
        ));
        assert!(matches!(
            capture(&Key::Character(";".into()), mods(true, false, false, false)),
            Capture::Pending
        ));
    }

    #[test]
    fn parts_splits_and_uppercases() {
        assert_eq!(parts("ctrl+alt+r"), vec!["CTRL", "ALT", "R"]);
        assert_eq!(parts(" F9 "), vec!["F9"]);
        assert!(parts(" + ").is_empty());
    }
}
