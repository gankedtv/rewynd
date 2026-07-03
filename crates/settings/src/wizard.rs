//! First-run onboarding: a short, skippable, Arena-styled wizard shown when no config file
//! exists yet (or rerun from Settings). It walks a non-technical user from a blank machine to a
//! working setup — screen-share permission, a hotkey, a replay length, and a proven test clip —
//! then writes the config once at the end. The step machine is pure and unit-tested; the side
//! effects (spawning the recorder, signalling a test save) run on blocking tasks.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iced::widget::{button, checkbox, column, container, row, slider, text, text_input};
use iced::{Element, Length, Task};

use rewynd_config::Config;

use crate::theme::{
    DISPLAY_BLACK, UI_BOLD, UI_SEMIBOLD, arena_check, arena_input, arena_slider, card, hint,
    link_button, palette, primary_button, secondary_button, tinted, value_row,
};

/// Replay-length slider bounds, matching the settings editor's.
const BUFFER_MIN_S: u32 = 5;
const BUFFER_MAX_S: u32 = rewynd_config::MAX_BUFFER_SECONDS as u32;
const BITS_PER_BYTE: u64 = 8;

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
            Message::Next => self.step = self.step.next(),
            Message::Back => self.step = self.step.back(),
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
            Message::RecordingStarted(Ok(())) => self.recording_started = true,
            Message::RecordingStarted(Err(e)) => self.recording_error = Some(e),
            Message::SaveTestClip => {
                self.test = TestState::Saving;
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
            }
            Message::TestClipResult(Ok(None)) => {
                self.test = TestState::Failed(
                    "No clip appeared yet. Give the recorder a moment to warm up, then try again."
                        .to_owned(),
                );
            }
            Message::TestClipResult(Err(e)) => self.test = TestState::Failed(e),
            // Intercepted by the app.
            Message::SkipSetup | Message::Finish => {}
        }
        Task::none()
    }

    pub fn view(&self, config: &Config) -> Element<'_, Message> {
        let skip = button(text("Skip setup").size(12).font(UI_SEMIBOLD))
            .on_press(Message::SkipSetup)
            .style(link_button)
            .padding(0);
        let header = row![
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
        .align_y(iced::Alignment::Center);

        let body = match self.step {
            Step::Welcome => self.welcome(),
            Step::ScreenShare => self.screen_share(),
            Step::Hotkey => self.hotkey_step(),
            Step::ReplayLength => self.replay_length(config),
            Step::TestClip => self.test_clip(),
            Step::CaptureMode => self.capture_mode(),
            Step::Finish => self.finish(),
        };

        let content = container(
            column![header, body, self.nav()]
                .spacing(28)
                .padding(32)
                .max_width(640)
                .width(Length::Fill),
        )
        .center_x(Length::Fill);
        container(iced::widget::scrollable(content))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// The Back / Next (or Finish) buttons under every step.
    fn nav(&self) -> Element<'_, Message> {
        let mut nav = row![].spacing(12).align_y(iced::Alignment::Center);
        if self.step != Step::Welcome {
            nav = nav.push(
                button(text("Back").size(12).font(UI_SEMIBOLD))
                    .on_press(Message::Back)
                    .style(secondary_button)
                    .padding([10, 20]),
            );
        }
        nav = nav.push(iced::widget::Space::new().width(Length::Fill));
        let (label, msg) = if self.step.is_last() {
            ("Finish", Message::Finish)
        } else {
            ("Next", Message::Next)
        };
        nav.push(
            button(text(label).size(13).font(UI_BOLD))
                .on_press(msg)
                .style(primary_button)
                .padding([11, 28]),
        )
        .into()
    }

    fn welcome(&self) -> Element<'_, Message> {
        step_card(
            "Welcome to rewynd",
            column![
                hint(
                    "rewynd keeps the last few minutes of your game on standby and saves a clip \
                     the moment you hit your hotkey, so you never miss the play."
                ),
                hint(
                    "It records only the game you're playing. Nothing leaves your machine unless \
                     you choose to upload a clip."
                ),
            ]
            .spacing(12),
        )
    }

    fn screen_share(&self) -> Element<'_, Message> {
        let action = if self.recording_started {
            row![
                text("Recording is running.")
                    .size(13)
                    .style(tinted(palette::ACCENT))
            ]
        } else {
            row![
                button(text("Start recording").size(13).font(UI_BOLD))
                    .on_press(Message::StartRecording)
                    .style(primary_button)
                    .padding([11, 24]),
            ]
        };
        let mut col = column![
            hint(
                "rewynd captures your screen through the system's screen-sharing permission. When \
                 you start recording, your desktop will ask you to pick what to share — choose \
                 your monitor. It only asks once; the choice is remembered."
            ),
            action,
        ]
        .spacing(16);
        if let Some(e) = &self.recording_error {
            col = col.push(text(e.clone()).size(12).style(tinted(palette::DANGER)));
        }
        step_card("Allow screen recording", col)
    }

    fn hotkey_step(&self) -> Element<'_, Message> {
        step_card(
            "Choose your hotkey",
            column![
                hint("This is the key you press to save the last few minutes as a clip."),
                text_input("CTRL+ALT+R", &self.hotkey)
                    .on_input(Message::HotkeyEdited)
                    .style(arena_input),
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
        step_card(
            "How much to keep",
            column![
                hint(
                    "How many seconds of gameplay a clip captures, counting back from your hotkey."
                ),
                value_row("Replay length", format!("{} seconds", self.buffer_seconds)),
                slider(
                    BUFFER_MIN_S..=BUFFER_MAX_S,
                    self.buffer_seconds,
                    Message::BufferChanged
                )
                .style(arena_slider),
                value_row("Estimated clip size", format!("about {est_mb} MB")),
            ]
            .spacing(12),
        )
    }

    fn test_clip(&self) -> Element<'_, Message> {
        let action: Element<Message> = match &self.test {
            TestState::Saving => hint("Saving a test clip..."),
            TestState::Saved { path, encoder } => {
                let mut saved = column![
                    text("Saved a test clip.")
                        .size(13)
                        .style(tinted(palette::ACCENT)),
                    hint(path.display().to_string()),
                ]
                .spacing(8);
                if encoder.as_deref() == Some("cpu") {
                    saved = saved.push(hint(
                        "Your GPU can't encode video, so rewynd used its CPU encoder. Clips still \
                         work, at the cost of more processor power.",
                    ));
                }
                saved.into()
            }
            _ => button(text("Save a test clip now").size(13).font(UI_BOLD))
                .on_press(Message::SaveTestClip)
                .style(primary_button)
                .padding([11, 24])
                .into(),
        };
        let mut col = column![
            hint(
                "Let's make sure it works. This saves a clip right now, the same as pressing your \
                 hotkey would."
            ),
            hint(
                "For this test rewynd records your whole desktop, so it works even with no game \
                 open. While you're playing, it records just the game."
            ),
            action,
        ]
        .spacing(16);
        if let TestState::Failed(e) = &self.test {
            col = col.push(text(e.clone()).size(12).style(tinted(palette::DANGER)));
        }
        step_card("Save a test clip", col)
    }

    fn capture_mode(&self) -> Element<'_, Message> {
        step_card(
            "What to record",
            column![
                hint(
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
        step_card(
            "You're set",
            column![
                hint(
                    "That's it — press your hotkey while playing and rewynd saves the clip to your \
                     library."
                ),
                checkbox(self.start_on_boot)
                    .label("Start rewynd automatically when I log in")
                    .on_toggle(Message::StartOnBoot)
                    .style(arena_check),
                hint("Want to share clips? Connect ganked.tv or YouTube any time under Settings."),
            ]
            .spacing(14),
        )
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

/// A titled card for a step's content.
fn step_card<'a>(title: &'a str, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let inner = column![text(title).size(26).font(DISPLAY_BLACK), content.into(),].spacing(18);
    card("SETUP", inner)
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
}
