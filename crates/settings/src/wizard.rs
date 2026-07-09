//! First-run onboarding: a short, skippable, Arena-styled wizard shown when no config file
//! exists yet (or rerun from Settings). It walks a non-technical user from a blank machine to a
//! working setup — screen-share permission, a hotkey, a replay length, and a proven test clip —
//! then writes the config once at the end. The step machine is pure and unit-tested; the side
//! effects (spawning the recorder, signalling a test save) run on blocking tasks.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iced::widget::{button, checkbox, column, container, row, slider, text, text_input};
use iced::{Background, Border, Element, Length, Task, Theme};

use rewynd_config::Config;

use crate::anim::{Cycle, Fade};
use crate::theme::{
    DISPLAY_BLACK, UI_BOLD, UI_SEMIBOLD, arena_check, arena_input, arena_slider, aside, body, card,
    dot, hint, kbd_chip, link_button, logo, palette, primary_button, secondary_button, tinted,
    value_row,
};

/// Replay-length slider bounds, matching the settings editor's.
const BUFFER_MIN_S: u32 = 5;
const BUFFER_MAX_S: u32 = rewynd_config::MAX_BUFFER_SECONDS as u32;
const BITS_PER_BYTE: u64 = 8;

/// Step-change slide/fade, success wash, and saving-ellipsis timings. The ellipsis only has
/// three states per period, so it ticks on a timer instead of the frame clock.
const ENTRANCE: Duration = Duration::from_millis(180);
const PULSE: Duration = Duration::from_millis(600);
const SAVING_PERIOD: Duration = Duration::from_millis(900);
const SAVING_TICK: Duration = Duration::from_millis(300);

/// The suggested hotkey combo: the input placeholder and the keycap preview fallback.
const DEFAULT_HOTKEY: &str = "CTRL+ALT+R";

/// One shows up under the welcome copy, picked per wizard run.
const QUIPS: [&str; 4] = [
    "For the plays nobody would believe without proof.",
    "The best moments happen when you are not recording. Fixed.",
    "Press the button after it happens. That is the whole trick.",
    "Your future highlight reel says thanks.",
];

/// The wizard's ordered steps. Kept a plain enum with an explicit order so `next`/`back` are
/// trivially testable and reordering is a one-line change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    ScreenShare,
    Hotkey,
    ReplayLength,
    TestClip,
    CaptureMode,
    Finish,
}

impl Step {
    const ORDER: [Step; 7] = [
        Step::Welcome,
        Step::ScreenShare,
        Step::Hotkey,
        Step::ReplayLength,
        Step::TestClip,
        Step::CaptureMode,
        Step::Finish,
    ];

    fn index(self) -> usize {
        Self::ORDER.iter().position(|&s| s == self).unwrap_or(0)
    }

    fn next(self) -> Step {
        Self::ORDER
            .get(self.index() + 1)
            .copied()
            .unwrap_or(Step::Finish)
    }

    fn back(self) -> Step {
        self.index()
            .checked_sub(1)
            .and_then(|i| Self::ORDER.get(i).copied())
            .unwrap_or(Step::Welcome)
    }

    fn is_last(self) -> bool {
        self == Step::Finish
    }
}

/// Where the test-clip step stands.
enum TestState {
    Idle,
    Saving,
    /// A clip was saved; `encoder` is the recorder's active backend (`"cpu"` / `"gpu:<name>"`)
    /// when known, so the step can flag the CPU fallback.
    Saved {
        path: PathBuf,
        encoder: Option<String>,
    },
    Failed(String),
}

pub struct Wizard {
    step: Step,
    hotkey: String,
    buffer_seconds: u32,
    start_on_boot: bool,
    /// Whether to record the whole desktop instead of only the active game (the capture-mode step).
    capture_desktop: bool,
    /// Whether the recorder has been asked to start (so the test-clip step can proceed).
    recording_started: bool,
    recording_error: Option<String>,
    test: TestState,
    /// Slide/fade of the current step's card, alive only while a step change settles.
    entrance: Option<Fade>,
    /// One-shot mint wash behind the card on a success (recording up, clip saved, finish).
    pulse: Option<Fade>,
    /// Drives the saving ellipsis, alive only while a test save runs.
    saving_dots: Option<Cycle>,
    /// A failed "Open folder" on the test-clip step.
    open_error: Option<String>,
    /// Picked once at construction so redraws don't reshuffle the line.
    quip: &'static str,
}

#[derive(Debug, Clone)]
pub enum Message {
    Next,
    Back,
    SkipSetup,
    HotkeyEdited(String),
    BufferChanged(u32),
    StartOnBoot(bool),
    CaptureDesktop(bool),
    StartRecording,
    RecordingStarted(Result<(), String>),
    SaveTestClip,
    TestClipResult(Result<Option<(PathBuf, Option<String>)>, String>),
    OpenClipFolder,
    Tick(std::time::Instant),
    Finish,
}

impl Wizard {
    /// A fresh wizard seeded from the current (default or existing) config.
    pub fn new(config: &Config) -> Self {
        Self {
            step: Step::Welcome,
            hotkey: config.hotkey_trigger().to_owned(),
            buffer_seconds: config
                .buffer_seconds()
                .clamp(u64::from(BUFFER_MIN_S), u64::from(BUFFER_MAX_S))
                as u32,
            start_on_boot: config.start_on_boot(),
            capture_desktop: config.capture_desktop(),
            recording_started: false,
            recording_error: None,
            test: TestState::Idle,
            entrance: None,
            pulse: None,
            saving_dots: None,
            open_error: None,
            quip: QUIPS[std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos() as usize)
                % QUIPS.len()],
        }
    }

    /// The edited values, applied by the app when the wizard finishes.
    pub fn hotkey(&self) -> &str {
        &self.hotkey
    }
    pub fn buffer_seconds(&self) -> u32 {
        self.buffer_seconds
    }
    pub fn start_on_boot(&self) -> bool {
        self.start_on_boot
    }
    pub fn capture_desktop(&self) -> bool {
        self.capture_desktop
    }

    /// Whether the wizard started the recorder (in desktop-capture mode for the test clip), so the
    /// app knows to restart it into the real config when onboarding ends.
    pub fn recording_started(&self) -> bool {
        self.recording_started
    }

    /// Handle a wizard message. `Finish` and `SkipSetup` are intercepted by the app (they persist
    /// the config and leave onboarding), so they never reach here.
    pub fn update(&mut self, message: Message, config: &Config) -> Task<Message> {
        match message {
            Message::Next => {
                let before = self.step;
                self.step = self.step.next();
                if self.step != before {
                    self.entrance = Some(Fade::new(ENTRANCE));
                    if self.step == Step::Finish {
                        self.pulse = Some(Fade::new(PULSE));
                    }
                }
            }
            Message::Back => {
                let before = self.step;
                self.step = self.step.back();
                if self.step != before {
                    self.entrance = Some(Fade::new(ENTRANCE));
                }
            }
            Message::HotkeyEdited(s) => self.hotkey = s,
            Message::BufferChanged(s) => self.buffer_seconds = s.clamp(BUFFER_MIN_S, BUFFER_MAX_S),
            Message::StartOnBoot(on) => self.start_on_boot = on,
            Message::CaptureDesktop(on) => self.capture_desktop = on,
            Message::StartRecording => {
                self.recording_error = None;
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(spawn_recorder_capturing_desktop)
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::RecordingStarted,
                );
            }
            Message::RecordingStarted(Ok(())) => {
                self.recording_started = true;
                // The recorder start is async; the wash only makes sense on the step that
                // shows the result, not wherever the user has navigated meanwhile.
                if self.step == Step::ScreenShare {
                    self.pulse = Some(Fade::new(PULSE));
                }
            }
            Message::RecordingStarted(Err(e)) => self.recording_error = Some(e),
            Message::OpenClipFolder => {
                self.open_error = None;
                if let TestState::Saved { path, .. } = &self.test
                    && let Some(dir) = path.parent()
                    && let Err(e) = open::that_detached(dir)
                {
                    self.open_error = Some(format!("Could not open the folder: {e}"));
                }
            }
            Message::SaveTestClip => {
                self.test = TestState::Saving;
                self.saving_dots = Some(Cycle::new(SAVING_PERIOD));
                self.open_error = None;
                let dir = rewynd_config::clips_dir(config.output_dir().as_deref());
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || save_and_wait_for_clip(&dir))
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::TestClipResult,
                );
            }
            Message::TestClipResult(Ok(Some((path, encoder)))) => {
                self.test = TestState::Saved { path, encoder };
                self.saving_dots = None;
                self.pulse = Some(Fade::new(PULSE));
            }
            Message::TestClipResult(Ok(None)) => {
                self.saving_dots = None;
                self.test = TestState::Failed(
                    "No clip appeared yet. Give the recorder a moment to warm up, then try again."
                        .to_owned(),
                );
            }
            Message::TestClipResult(Err(e)) => {
                self.saving_dots = None;
                self.test = TestState::Failed(e);
            }
            Message::Tick(now) => {
                if let Some(fade) = &mut self.entrance
                    && fade.advance(now)
                {
                    self.entrance = None;
                }
                if let Some(fade) = &mut self.pulse
                    && fade.advance(now)
                {
                    self.pulse = None;
                }
                if let Some(cycle) = &mut self.saving_dots {
                    cycle.advance(now);
                }
            }
            // Intercepted by the app.
            Message::SkipSetup | Message::Finish => {}
        }
        Task::none()
    }

    /// Frame ticks while the short fades run; a slow timer while the saving ellipsis is
    /// actually visible (it has three states per period, so the frame clock would be ~40x
    /// overkill); nothing when idle.
    pub fn subscription(&self) -> iced::Subscription<Message> {
        if self.animating() {
            iced::window::frames().map(Message::Tick)
        } else if matches!(self.test, TestState::Saving) && self.step == Step::TestClip {
            iced::time::every(SAVING_TICK).map(Message::Tick)
        } else {
            iced::Subscription::none()
        }
    }

    fn animating(&self) -> bool {
        self.entrance.is_some() || self.pulse.is_some()
    }

    /// Eased entrance progress, `1.0` once the step has settled (no fade running).
    fn entrance_progress(&self) -> f32 {
        self.entrance.as_ref().map_or(1.0, Fade::progress)
    }

    pub fn view(&self, config: &Config) -> Element<'_, Message> {
        let skip = button(text("Skip setup").size(12).font(UI_SEMIBOLD))
            .on_press(Message::SkipSetup)
            .style(link_button)
            .padding(0);
        let header = row![
            self.stepper(),
            text(format!(
                "STEP {} OF {}",
                self.step.index() + 1,
                Step::ORDER.len()
            ))
            .size(12)
            .font(UI_SEMIBOLD)
            .style(tinted(palette::MUTED)),
            iced::widget::Space::new().width(Length::Fill),
            skip,
        ]
        .spacing(14)
        .align_y(iced::Alignment::Center);

        let step = match self.step {
            Step::Welcome => self.welcome(),
            Step::ScreenShare => self.screen_share(),
            Step::Hotkey => self.hotkey_step(),
            Step::ReplayLength => self.replay_length(config),
            Step::TestClip => self.test_clip(),
            Step::CaptureMode => self.capture_mode(),
            Step::Finish => self.finish(),
        };

        // The card surface is opaque, so the success wash needs its own rim around the card;
        // the wrapper is always there (background only while pulsing) to keep layout stable.
        let wash = self.pulse.as_ref().map(|f| 1.0 - f.progress());
        let washed = container(step)
            .padding(4)
            .style(move |_: &Theme| container::Style {
                background: wash.map(|a| Background::Color(palette::ACCENT_BG.scale_alpha(a))),
                border: Border {
                    radius: 8.0.into(),
                    ..Border::default()
                },
                ..container::Style::default()
            });
        // The slide trades top padding for bottom padding so the wrapper's height is constant
        // and the nav row below never moves while the card settles.
        let slide = 14.0 * (1.0 - self.entrance_progress());
        let animated = container(washed).padding(iced::Padding {
            top: slide,
            bottom: 14.0 - slide,
            ..iced::Padding::ZERO
        });

        let content = container(
            column![header, animated, self.nav()]
                .spacing(32)
                .padding(40)
                .max_width(720)
                .width(Length::Fill),
        )
        .center_x(Length::Fill);
        container(crate::scroll::smooth(iced::widget::scrollable(content)))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// One dot per step: filled mint for done and current (the current one sits in a faint
    /// mint well), outlined for what's still ahead.
    fn stepper(&self) -> Element<'_, Message> {
        let current = self.step.index();
        let mut dots = row![].spacing(10).align_y(iced::Alignment::Center);
        for i in 0..Step::ORDER.len() {
            dots = dots.push(step_dot(i <= current, i == current));
        }
        dots.into()
    }

    /// The Back / Next (or Finish) buttons under every step.
    fn nav(&self) -> Element<'_, Message> {
        let mut nav = row![].spacing(12).align_y(iced::Alignment::Center);
        if self.step != Step::Welcome {
            nav = nav.push(
                button(text("Back").size(12).font(UI_SEMIBOLD))
                    .on_press(Message::Back)
                    .style(secondary_button)
                    .padding([12, 24]),
            );
        }
        nav = nav.push(iced::widget::Space::new().width(Length::Fill));
        let (label, msg) = if self.step.is_last() {
            ("Finish", Message::Finish)
        } else {
            ("Next", Message::Next)
        };
        nav.push(cta(label, msg)).into()
    }

    fn welcome(&self) -> Element<'_, Message> {
        column![
            container(logo(72.0)).center_x(Length::Fill),
            self.step_card(
                "Welcome to rewynd",
                column![
                    body(
                        "rewynd keeps the last few minutes of your game on standby and saves a \
                         clip the moment you hit your hotkey, so you never miss the play."
                    ),
                    body(
                        "It records only the game you're playing. Nothing leaves your machine \
                         unless you choose to upload a clip."
                    ),
                    aside(self.quip),
                ]
                .spacing(12),
            ),
        ]
        .spacing(24)
        .into()
    }

    fn screen_share(&self) -> Element<'_, Message> {
        let action = if self.recording_started {
            row![
                text("Recording is running.")
                    .size(13)
                    .style(tinted(palette::ACCENT))
            ]
        } else {
            row![cta("Start recording", Message::StartRecording)]
        };
        let mut col = column![
            body(
                "rewynd captures your screen through the system's screen-sharing permission. When \
                 you start recording, your desktop will ask you to pick what to share. Choose \
                 your monitor. It only asks once; the choice is remembered."
            ),
            action,
        ]
        .spacing(16);
        if let Some(e) = &self.recording_error {
            col = col.push(text(e.clone()).size(12).style(tinted(palette::DANGER)));
        }
        self.step_card("Allow screen recording", col)
    }

    fn hotkey_step(&self) -> Element<'_, Message> {
        self.step_card(
            "Choose your hotkey",
            column![
                body("This is the key you press to save the last few minutes as a clip."),
                text_input(DEFAULT_HOTKEY, &self.hotkey)
                    .on_input(Message::HotkeyEdited)
                    .size(14)
                    .padding(12)
                    .style(arena_input),
                hotkey_chips(&self.hotkey),
                hint(
                    "On KDE, your desktop may open its shortcuts dialog the first time so you can \
                     assign the key to rewynd. Assign it there and it sticks."
                ),
            ]
            .spacing(12),
        )
    }

    fn replay_length(&self, config: &Config) -> Element<'_, Message> {
        let est_mb = estimated_clip_mb(config, self.buffer_seconds);
        self.step_card(
            "How much to keep",
            column![
                body(
                    "How many seconds of gameplay a clip captures, counting back from your hotkey."
                ),
                value_row("Replay length", format!("{} seconds", self.buffer_seconds)),
                slider(
                    BUFFER_MIN_S..=BUFFER_MAX_S,
                    self.buffer_seconds,
                    Message::BufferChanged
                )
                .style(arena_slider),
                aside(replay_flavor(self.buffer_seconds)),
                value_row("Estimated clip size", format!("about {est_mb} MB")),
            ]
            .spacing(12),
        )
    }

    fn test_clip(&self) -> Element<'_, Message> {
        let action: Element<Message> = match &self.test {
            TestState::Saving => {
                let phase = self.saving_dots.as_ref().map_or(0.0, Cycle::phase);
                body(format!(
                    "Saving a test clip{}",
                    ".".repeat(1 + (phase * 3.0) as usize)
                ))
            }
            TestState::Saved { path, encoder } => {
                let mut saved = column![
                    text("Clip secured.")
                        .size(13)
                        .style(tinted(palette::ACCENT)),
                    aside("That one is a keeper."),
                    hint(path.display().to_string()),
                    button(text("Open folder").size(12).font(UI_SEMIBOLD))
                        .on_press(Message::OpenClipFolder)
                        .style(secondary_button)
                        .padding([6, 14]),
                ]
                .spacing(8);
                if encoder.as_deref() == Some("cpu") {
                    saved = saved.push(body(
                        "Your GPU can't encode video, so rewynd used its CPU encoder. Clips still \
                         work, at the cost of more processor power.",
                    ));
                }
                if let Some(e) = &self.open_error {
                    saved = saved.push(text(e.clone()).size(12).style(tinted(palette::DANGER)));
                }
                saved.into()
            }
            _ => cta("Save a test clip now", Message::SaveTestClip),
        };
        let mut col = column![
            body(
                "Let's make sure it works. This saves a clip right now, the same as pressing your \
                 hotkey would."
            ),
            body(
                "For this test rewynd records your whole desktop, so it works even with no game \
                 open. While you're playing, it records just the game."
            ),
            action,
        ]
        .spacing(16);
        if let TestState::Failed(e) = &self.test {
            col = col.push(text(e.clone()).size(12).style(tinted(palette::DANGER)));
        }
        self.step_card("Save a test clip", col)
    }

    fn capture_mode(&self) -> Element<'_, Message> {
        self.step_card(
            "What to record",
            column![
                body(
                    "By default rewynd records only the game you're playing (fullscreen or \
                     borderless), so other windows stay out of your clips. Turn this on to record \
                     your whole desktop instead."
                ),
                checkbox(self.capture_desktop)
                    .label("Record my whole desktop, not just the active game")
                    .on_toggle(Message::CaptureDesktop)
                    .style(arena_check),
                hint("You can change this any time under Settings."),
            ]
            .spacing(14),
        )
    }

    fn finish(&self) -> Element<'_, Message> {
        let hotkey_line = row![
            hotkey_chips(&self.hotkey),
            body("while playing, and the moment is yours."),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        self.step_card(
            "You're set",
            column![
                hotkey_line,
                checkbox(self.start_on_boot)
                    .label("Start rewynd automatically when I log in")
                    .on_toggle(Message::StartOnBoot)
                    .style(arena_check),
                hint("Want to share clips? Connect ganked.tv or YouTube any time under Settings."),
                aside("glhf."),
            ]
            .spacing(14),
        )
    }

    /// A titled card for a step's content; the title is display-face (so uppercase, per the
    /// design) and fades in with the entrance.
    fn step_card<'a>(
        &self,
        title: &'a str,
        content: impl Into<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        let title_color = palette::TEXT.scale_alpha(self.entrance_progress());
        let inner = column![
            text(title.to_uppercase())
                .size(32)
                .font(DISPLAY_BLACK)
                .style(tinted(title_color)),
            content.into(),
        ]
        .spacing(18);
        card("SETUP", inner)
    }
}

/// The estimated clip size in MB for `seconds` of the config's video + audio bitrate.
fn estimated_clip_mb(config: &Config, seconds: u32) -> u64 {
    let v = config.video_stored();
    let a = config.audio_stored();
    let bytes = u64::from(v.bitrate_bps)
        .saturating_add(u64::from(a.bitrate_bps))
        .saturating_mul(u64::from(seconds))
        / BITS_PER_BYTE;
    bytes.saturating_add(500_000) / 1_000_000
}

/// The mint call-to-action every step shares.
fn cta(label: &str, msg: Message) -> Element<'_, Message> {
    button(text(label).size(13).font(UI_BOLD))
        .on_press(msg)
        .style(primary_button)
        .padding([13, 30])
        .into()
}

/// What `seconds` of replay feels like, for the length slider.
fn replay_flavor(seconds: u32) -> &'static str {
    match seconds {
        ..=30 => "Just the kill.",
        31..=90 => "The kill and the setup.",
        91..=180 => "The whole teamfight, start to finish.",
        _ => "The full story arc, hero included.",
    }
}

/// The hotkey string split on `+` into uppercase keycap labels, empties dropped.
fn hotkey_parts(s: &str) -> Vec<String> {
    s.split('+')
        .map(|p| p.trim().to_uppercase())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The hotkey as keycap chips joined by muted plus signs; the placeholder combo when the
/// input has no usable parts.
fn hotkey_chips<'a>(hotkey: &str) -> Element<'a, Message> {
    let mut parts = hotkey_parts(hotkey);
    if parts.is_empty() {
        parts = hotkey_parts(DEFAULT_HOTKEY);
    }
    let mut chips = row![].spacing(6).align_y(iced::Alignment::Center);
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            chips = chips.push(text("+").size(11).style(tinted(palette::MUTED)));
        }
        chips = chips.push(kbd_chip(part));
    }
    chips.into()
}

/// One stepper dot: filled mint when reached, outlined when ahead; the current one gets a
/// faint mint well (10px dot + 3px padding = a 16px round well).
fn step_dot<'a>(done: bool, current: bool) -> Element<'a, Message> {
    let mark: Element<'a, Message> = if done {
        dot(10.0, palette::ACCENT)
    } else {
        container(iced::widget::Space::new().width(10).height(10))
            .style(|_: &Theme| container::Style {
                border: Border {
                    color: palette::BORDER_STRONG,
                    width: 1.0,
                    radius: 5.0.into(),
                },
                ..container::Style::default()
            })
            .into()
    };
    if current {
        container(mark)
            .padding(3)
            .style(|_: &Theme| container::Style {
                background: Some(Background::Color(palette::ACCENT_BG)),
                border: Border {
                    radius: 8.0.into(),
                    ..Border::default()
                },
                ..container::Style::default()
            })
            .into()
    } else {
        mark
    }
}

/// Stop any running recorder and start one that captures the whole desktop, so the wizard's test
/// clip works while the user is on the desktop rather than in a game. The user's real capture mode
/// is applied when the app restarts the recorder after Finish.
fn spawn_recorder_capturing_desktop() -> Result<(), String> {
    let _ = rewynd_config::stop_recorder(Duration::from_secs(3), Duration::from_secs(2));
    let recorder = rewynd_config::sibling_binary("rewynd-recorder")
        .ok_or_else(|| "could not locate the recorder binary".to_owned())?;
    std::process::Command::new(&recorder)
        .env("REWYND_CAPTURE_DESKTOP", "1")
        .spawn()
        .map(|mut child| {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        })
        .map_err(|e| format!("could not start the recorder: {e}"))
}

/// Ask the recorder to save a clip and wait for a new one to appear under `dir`. `Ok(None)` means
/// none showed up in time (the ring may still be filling); `Err` means the recorder isn't running.
fn save_and_wait_for_clip(dir: &Path) -> Result<Option<(PathBuf, Option<String>)>, String> {
    let before = rewynd_config::newest_clip_in(dir);
    let requested = rewynd_config::request_recorder_save().map_err(|e| e.to_string())?;
    if !requested {
        return Err(
            "The recorder isn't running yet. Go back a step and start recording.".to_owned(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(400));
        let now = rewynd_config::newest_clip_in(dir);
        if let Some(path) = now
            && Some(&path) != before.as_ref()
        {
            let encoder = rewynd_config::read_recorder_status().map(|s| s.encoder);
            return Ok(Some((path, encoder)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_advance_and_retreat_without_running_off_the_ends() {
        assert_eq!(Step::Welcome.back(), Step::Welcome, "clamped at the start");
        let mut s = Step::Welcome;
        for expected in [
            Step::ScreenShare,
            Step::Hotkey,
            Step::ReplayLength,
            Step::TestClip,
            Step::CaptureMode,
            Step::Finish,
        ] {
            s = s.next();
            assert_eq!(s, expected);
        }
        assert_eq!(s.next(), Step::Finish, "clamped at the end");
        assert!(s.is_last());
        assert_eq!(Step::Finish.back(), Step::CaptureMode);
    }

    #[test]
    fn buffer_edit_clamps_to_the_slider_range() {
        let mut w = Wizard::new(&Config::default());
        let _ = w.update(
            Message::BufferChanged(BUFFER_MAX_S + 1000),
            &Config::default(),
        );
        assert_eq!(w.buffer_seconds(), BUFFER_MAX_S);
        let _ = w.update(Message::BufferChanged(0), &Config::default());
        assert_eq!(w.buffer_seconds(), BUFFER_MIN_S);
    }

    #[test]
    fn next_message_walks_the_steps() {
        let mut w = Wizard::new(&Config::default());
        assert_eq!(w.step, Step::Welcome);
        let _ = w.update(Message::Next, &Config::default());
        assert_eq!(w.step, Step::ScreenShare);
        let _ = w.update(Message::Back, &Config::default());
        assert_eq!(w.step, Step::Welcome);
    }

    #[test]
    fn estimate_scales_with_length() {
        let config = Config::default();
        let short = estimated_clip_mb(&config, 10);
        let long = estimated_clip_mb(&config, 60);
        assert!(long > short, "{long} !> {short}");
    }

    #[test]
    fn replay_flavor_boundaries() {
        assert_eq!(replay_flavor(BUFFER_MIN_S), "Just the kill.");
        assert_eq!(replay_flavor(30), "Just the kill.");
        assert_eq!(replay_flavor(31), "The kill and the setup.");
        assert_eq!(replay_flavor(90), "The kill and the setup.");
        assert_eq!(replay_flavor(91), "The whole teamfight, start to finish.");
        assert_eq!(replay_flavor(180), "The whole teamfight, start to finish.");
        assert_eq!(replay_flavor(181), "The full story arc, hero included.");
    }

    #[test]
    fn hotkey_parts_uppercase_trim_and_drop_empties() {
        assert_eq!(hotkey_parts("ctrl + alt+r"), vec!["CTRL", "ALT", "R"]);
        assert_eq!(hotkey_parts(" shift +"), vec!["SHIFT"]);
        assert!(hotkey_parts("").is_empty());
        assert!(hotkey_parts(" + ").is_empty());
    }

    #[test]
    fn entrance_fade_drives_animating_until_it_completes() {
        let mut w = Wizard::new(&Config::default());
        assert!(!w.animating(), "idle wizard needs no frame ticks");
        let _ = w.update(Message::Next, &Config::default());
        assert!(w.animating(), "a step change starts the entrance fade");
        let t0 = Instant::now();
        let _ = w.update(Message::Tick(t0), &Config::default());
        assert!(w.animating(), "still fading right after the anchor tick");
        let _ = w.update(
            Message::Tick(t0 + Duration::from_millis(200)),
            &Config::default(),
        );
        assert!(!w.animating(), "the finished fade is dropped");
    }
}
