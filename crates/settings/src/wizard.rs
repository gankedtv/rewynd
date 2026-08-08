//! First-run onboarding: a short, skippable, Arena-styled wizard shown when no config file
//! exists yet (or rerun from Settings). It walks a non-technical user from a blank machine to a
//! working setup — screen-share permission, a hotkey, a replay length, and a proven test clip —
//! then writes the config once at the end. The step machine is pure and unit-tested; the side
//! effects (spawning the recorder, signalling a test save) run on blocking tasks.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iced::widget::{button, checkbox, column, container, row, slider, text};
use iced::{Background, Border, Element, Length, Task, Theme};

use rewynd_config::Config;

use crate::anim::{Cycle, Fade};
use crate::hotkey;
use crate::theme::{
    DISPLAY_BLACK, UI_BOLD, UI_SEMIBOLD, arena_check, arena_slider, aside, body, card, dot, hint,
    link_button, logo, palette, primary_button, secondary_button, tinted, value_row,
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
    /// Whether the hotkey field is armed and the next key press becomes the trigger.
    hotkey_recording: bool,
    buffer_seconds: u32,
    start_on_boot: bool,
    /// Whether to record the whole desktop instead of only the active game (the capture-mode step).
    capture_desktop: bool,
    /// A `StartRecording` task is in flight (spawning, then confirming the recorder is up).
    recording_starting: bool,
    /// Whether a recorder process was spawned at all, confirmed or not — it may be recording the
    /// desktop right now, so leaving onboarding still has to restart it into the real config.
    recorder_spawned: bool,
    /// The confirmed recorder's pid, so the test-clip step only reads that recorder's status.
    recorder_pid: Option<u32>,
    /// Whether the recorder confirmed it is capturing (so the test-clip step can proceed).
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
    /// Arm or disarm the hotkey capture field.
    HotkeyRecord(bool),
    /// A key press while the hotkey field is armed.
    HotkeyKey(iced::keyboard::Key, iced::keyboard::Modifiers),
    BufferChanged(u32),
    StartOnBoot(bool),
    CaptureDesktop(bool),
    StartRecording,
    RecordingStarted(RecorderStart),
    SaveTestClip,
    TestClipResult(Result<Option<(PathBuf, Option<String>)>, ClipFailure>),
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
            hotkey_recording: false,
            buffer_seconds: config
                .buffer_seconds()
                .clamp(u64::from(BUFFER_MIN_S), u64::from(BUFFER_MAX_S))
                as u32,
            start_on_boot: config.start_on_boot(),
            capture_desktop: config.capture_desktop(),
            recording_starting: false,
            recorder_spawned: false,
            recorder_pid: None,
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

    /// Whether the wizard launched a recorder (in desktop-capture mode for the test clip), so the
    /// app knows to restart it into the real config when onboarding ends. True even when the
    /// launch never confirmed: an unconfirmed recorder can still be capturing the desktop.
    pub fn recorder_spawned(&self) -> bool {
        self.recorder_spawned
    }

    /// Handle a wizard message. `Finish` and `SkipSetup` are intercepted by the app (they persist
    /// the config and leave onboarding), so they never reach here.
    pub fn update(&mut self, message: Message, config: &Config) -> Task<Message> {
        match message {
            Message::Next => {
                let before = self.step;
                self.step = self.step.next();
                if self.step != before {
                    // Leaving the step disarms the hotkey field; a stale armed state would
                    // capture again the moment the step reopens.
                    self.hotkey_recording = false;
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
                    self.hotkey_recording = false;
                    self.entrance = Some(Fade::new(ENTRANCE));
                }
            }
            Message::HotkeyRecord(armed) => self.hotkey_recording = armed,
            Message::HotkeyKey(key, modifiers) => match hotkey::capture(&key, modifiers) {
                hotkey::Capture::Done(trigger) => {
                    self.hotkey_recording = false;
                    self.hotkey = trigger;
                }
                hotkey::Capture::Cancel => self.hotkey_recording = false,
                hotkey::Capture::Pending => {}
            },
            Message::BufferChanged(s) => self.buffer_seconds = s.clamp(BUFFER_MIN_S, BUFFER_MAX_S),
            Message::StartOnBoot(on) => self.start_on_boot = on,
            Message::CaptureDesktop(on) => self.capture_desktop = on,
            Message::StartRecording => {
                // The button is hidden while a launch runs, but a queued message can still land
                // after it disappears.
                if self.recording_starting || self.recording_started {
                    return Task::none();
                }
                self.recording_starting = true;
                self.recording_error = None;
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(spawn_recorder_capturing_desktop)
                            .await
                            .unwrap_or_else(|e| RecorderStart::not_spawned(e.to_string()))
                    },
                    Message::RecordingStarted,
                );
            }
            Message::RecordingStarted(start) => {
                self.recording_starting = false;
                self.recorder_spawned |= start.spawned;
                match start.confirmed {
                    Ok(pid) => {
                        self.recorder_pid = Some(pid);
                        self.recording_started = true;
                        // The recorder start is async; the wash only makes sense on the step that
                        // shows the result, not wherever the user has navigated meanwhile.
                        if self.step == Step::ScreenShare {
                            self.pulse = Some(Fade::new(PULSE));
                        }
                    }
                    Err(e) => self.recording_error = Some(e),
                }
            }
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
                let pid = self.recorder_pid;
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || save_and_wait_for_clip(&dir, pid))
                            .await
                            .unwrap_or_else(|e| Err(ClipFailure::failed(e.to_string())))
                    },
                    Message::TestClipResult,
                );
            }
            Message::TestClipResult(Ok(Some((path, encoder)))) => {
                self.test = TestState::Saved { path, encoder };
                self.saving_dots = None;
                // Same rule as the recorder start: the wash belongs to the step that shows
                // the result, not wherever the user has navigated meanwhile.
                if self.step == Step::TestClip {
                    self.pulse = Some(Fade::new(PULSE));
                }
            }
            Message::TestClipResult(Ok(None)) => {
                self.saving_dots = None;
                self.test = TestState::Failed(
                    "No clip appeared yet. Give the recorder a moment to warm up, then try again."
                        .to_owned(),
                );
            }
            Message::TestClipResult(Err(f)) => {
                self.saving_dots = None;
                // Without this the screen-share step keeps claiming "Recording is running." with
                // no button, and the test step keeps telling the user to go back and start it.
                if f.recorder_gone {
                    self.recording_started = false;
                    self.recorder_pid = None;
                }
                self.test = TestState::Failed(f.message);
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
        let ticks = if self.animating() {
            iced::window::frames().map(Message::Tick)
        } else if matches!(self.test, TestState::Saving) && self.step == Step::TestClip {
            iced::time::every(SAVING_TICK).map(Message::Tick)
        } else {
            iced::Subscription::none()
        };
        // Only while the hotkey field is armed: every key press goes to the capture logic.
        let keys = if self.hotkey_recording && self.step == Step::Hotkey {
            iced::event::listen_with(|event, _status, _id| match event {
                iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key, modifiers, ..
                }) => Some(Message::HotkeyKey(key, modifiers)),
                _ => None,
            })
        } else {
            iced::Subscription::none()
        };
        iced::Subscription::batch([ticks, keys])
    }

    fn animating(&self) -> bool {
        self.entrance.is_some() || self.pulse.is_some()
    }

    /// Eased entrance progress, `1.0` once the step has settled (no fade running).
    fn entrance_progress(&self) -> f32 {
        self.entrance.as_ref().map_or(1.0, Fade::progress)
    }

    pub fn view(&self, config: &Config) -> Element<'_, Message> {
        // Disabled mid-launch: skipping then would read `recorder_spawned` before the spawn
        // reports back and leave a desktop-capturing recorder running for the session.
        let skip = button(text("Skip setup").size(12).font(UI_SEMIBOLD))
            .on_press_maybe((!self.recording_starting).then_some(Message::SkipSetup))
            .style(link_button)
            .padding(0);
        let mut header = row![
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
        ];
        // The launch outlives the screen-share step the user started it from, so the reason Skip
        // is dead has to travel with the header rather than live in that step's copy.
        if self.recording_starting {
            header = header.push(hint("Starting recording…"));
        }
        let header = header
            .push(skip)
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
        } else if self.recording_starting {
            row![body(
                "Starting recording… if your desktop asks what to share, pick your monitor."
            )]
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
                hotkey::field(
                    &self.hotkey,
                    self.hotkey_recording,
                    Message::HotkeyRecord(true),
                    Message::HotkeyRecord(false),
                ),
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
            hotkey::chips(&self.hotkey),
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

/// How long the wizard waits for a spawned recorder to report that it is capturing, and how often
/// it looks. Generous because the recorder can't confirm until the user has clicked through their
/// desktop's share picker; a recorder that dies instead is caught by its exit, not this.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
const CONFIRM_POLL: Duration = Duration::from_millis(200);

/// How long a test clip gets to appear, and how much of that is left once the recorder reports its
/// capture pipeline failed — it stays alive so already-buffered footage is still saveable, and
/// muxing a full buffer takes a moment.
const CLIP_TIMEOUT: Duration = Duration::from_secs(15);
const CLIP_FAILURE_GRACE: Duration = Duration::from_secs(5);
const CLIP_POLL: Duration = Duration::from_millis(400);

/// What came of the wizard's recorder launch.
#[derive(Debug, Clone)]
pub struct RecorderStart {
    /// A process was spawned, whether or not it went on to confirm.
    spawned: bool,
    /// The confirmed recorder's pid, or why the launch never confirmed.
    confirmed: Result<u32, String>,
}

impl RecorderStart {
    /// A launch that never got as far as a process.
    fn not_spawned(error: impl Into<String>) -> Self {
        Self {
            spawned: false,
            confirmed: Err(error.into()),
        }
    }
}

/// A test-clip attempt that produced no clip.
#[derive(Debug, Clone)]
pub struct ClipFailure {
    message: String,
    /// No recorder is running any more, so the wizard has to reopen its start button.
    recorder_gone: bool,
}

impl ClipFailure {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            recorder_gone: false,
        }
    }

    fn gone(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            recorder_gone: true,
        }
    }
}

/// Stop any running recorder and start one that captures the whole desktop, so the wizard's test
/// clip works while the user is on the desktop rather than in a game. The user's real capture mode
/// is applied when the app restarts the recorder after Finish.
fn spawn_recorder_capturing_desktop() -> RecorderStart {
    // A recorder that survived both signals still holds the single-instance lock, so spawning
    // would just produce a process that exits on sight. Say that instead of timing out on it.
    match rewynd_config::stop_recorder(Duration::from_secs(3), Duration::from_secs(2)) {
        Ok(true) => {}
        Ok(false) => {
            return RecorderStart::not_spawned(
                "The recorder that was already running wouldn't shut down. Log out and back in, then try again.",
            );
        }
        Err(e) => {
            return RecorderStart::not_spawned(format!("Could not stop the old recorder: {e}"));
        }
    }
    let Some(recorder) = rewynd_config::sibling_binary("rewynd-recorder") else {
        return RecorderStart::not_spawned("Could not locate the recorder binary.");
    };
    let mut child = match std::process::Command::new(&recorder)
        .env("REWYND_CAPTURE_DESKTOP", "1")
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return RecorderStart::not_spawned(format!("Could not start the recorder: {e}")),
    };
    let pid = child.id();
    let confirmed = wait_for_recorder_up(&mut child, pid, CONFIRM_TIMEOUT);
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    RecorderStart {
        spawned: true,
        confirmed,
    }
}

/// One status poll's read on whether the process we just spawned (`pid`) is capturing.
enum RecorderLaunch {
    /// Nothing from `pid` yet, or it is still coming up.
    Pending,
    /// `pid` reports its capture pipeline is live.
    Up,
    /// `pid` reports a failure.
    Failed(String),
}

/// `status.detail`, or `fallback` when the recorder failed without leaving one.
fn detail_or(status: &rewynd_config::RecorderStatus, fallback: &str) -> String {
    status.detail.clone().unwrap_or_else(|| fallback.to_owned())
}

/// The testable core of [`wait_for_recorder_up`]: classify one status read against the pid we're
/// waiting on. A `Starting` recorder is deliberately not `Up` — it publishes that before the
/// share picker and the encoder exist, so treating it as running is the false positive this
/// whole handshake is for.
fn recorder_launch_outcome(
    status: Option<&rewynd_config::RecorderStatus>,
    pid: u32,
) -> RecorderLaunch {
    use rewynd_config::RecorderState;
    match status {
        Some(s) if s.pid == pid => match s.state {
            RecorderState::Failed => {
                RecorderLaunch::Failed(detail_or(s, "the recorder failed to start"))
            }
            RecorderState::Starting => RecorderLaunch::Pending,
            RecorderState::Recording | RecorderState::Idle => RecorderLaunch::Up,
        },
        _ => RecorderLaunch::Pending,
    }
}

/// Why the recorder is gone when it exits before confirming. A clean exit is almost always the
/// single-instance lock, which it takes before it can publish anything at all.
fn exited_message(code: Option<i32>) -> String {
    match code {
        Some(0) => "The recorder stopped right after starting. Another copy may still be running; log out and back in, then try again.".to_owned(),
        Some(code) => format!("The recorder stopped right after starting (exit code {code})."),
        None => "The recorder was killed as it started.".to_owned(),
    }
}

/// Poll the recorder's published status until the freshly spawned `pid` reports that it is
/// capturing, so the wizard never declares "Recording is running." over a doomed launch (a
/// cancelled share picker, an encoder or capture init failure, a lock conflict). Watches the
/// child too: an immediate exit is answerable now rather than at the timeout.
fn wait_for_recorder_up(
    child: &mut std::process::Child,
    pid: u32,
    timeout: Duration,
) -> Result<u32, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match recorder_launch_outcome(rewynd_config::read_recorder_status().as_ref(), pid) {
            RecorderLaunch::Up => return Ok(pid),
            RecorderLaunch::Failed(detail) => return Err(detail),
            RecorderLaunch::Pending => {}
        }
        if let Ok(Some(exit)) = child.try_wait() {
            return Err(exited_message(exit.code()));
        }
        if Instant::now() >= deadline {
            return Err(
                "The recorder didn't start capturing in time. If your desktop asked you to pick a screen, pick one and try again.".to_owned(),
            );
        }
        std::thread::sleep(CONFIRM_POLL);
    }
}

/// The capture-pipeline failure `pid` reports, if any. Matching the pid matters: an autostart
/// recorder left in a failed state would otherwise be read as our own.
fn failed_detail(
    status: Option<&rewynd_config::RecorderStatus>,
    pid: Option<u32>,
) -> Option<String> {
    status
        .filter(|s| pid.is_none_or(|pid| s.pid == pid))
        .filter(|s| s.state == rewynd_config::RecorderState::Failed)
        .map(|s| detail_or(s, "the recorder's capture pipeline failed"))
}

/// The testable core of [`save_and_wait_for_clip`]'s poll loop: what one status read says about
/// the recorder we asked to save. A confirmed recorder that stopped publishing has exited, which
/// is a different (and more urgent) answer than a reported capture failure.
fn clip_wait_failure(
    status: Option<&rewynd_config::RecorderStatus>,
    pid: Option<u32>,
) -> Option<ClipFailure> {
    if pid.is_some() && status.is_none() {
        return Some(ClipFailure::gone(
            "The recorder stopped before the clip was saved. Go back a step and start recording.",
        ));
    }
    failed_detail(status, pid).map(ClipFailure::failed)
}

/// Ask the recorder to save a clip and wait for a new one to appear under `dir`. `Ok(None)` means
/// none showed up in time (the ring may still be filling); `Err` means the recorder isn't running,
/// or its capture pipeline failed and nothing landed afterwards.
fn save_and_wait_for_clip(
    dir: &Path,
    pid: Option<u32>,
) -> Result<Option<(PathBuf, Option<String>)>, ClipFailure> {
    let before = rewynd_config::newest_clip_in(dir);
    let requested =
        rewynd_config::request_recorder_save().map_err(|e| ClipFailure::gone(e.to_string()))?;
    if !requested {
        return Err(ClipFailure::gone(
            "The recorder isn't running. Go back a step and start recording.",
        ));
    }
    let mut deadline = Instant::now() + CLIP_TIMEOUT;
    let mut failure: Option<String> = None;
    while Instant::now() < deadline {
        std::thread::sleep(CLIP_POLL);
        let status = rewynd_config::read_recorder_status();
        if let Some(path) = rewynd_config::newest_clip_in(dir)
            && Some(&path) != before.as_ref()
        {
            return Ok(Some((path, status.map(|s| s.encoder))));
        }
        if let Some(f) = clip_wait_failure(status.as_ref(), pid) {
            // A gone recorder ends the wait: nothing is coming, and only saying so reopens the
            // start button. A reported failure might still be muxing the buffered footage we
            // asked for, so it shortens the wait rather than ending it.
            if f.recorder_gone {
                return Err(f);
            }
            if failure.is_none() {
                failure = Some(f.message);
                deadline = deadline.min(Instant::now() + CLIP_FAILURE_GRACE);
            }
        }
    }
    match failure {
        Some(detail) => Err(ClipFailure::failed(detail)),
        None => Ok(None),
    }
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
    fn start_recording_ignores_re_entry_while_already_starting() {
        let mut w = Wizard::new(&Config::default());
        let _ = w.update(Message::StartRecording, &Config::default());
        assert!(w.recording_starting, "first click begins starting");
        let _ = w.update(Message::StartRecording, &Config::default());
        assert!(w.recording_starting);
        assert!(!w.recording_started);
    }

    #[test]
    fn a_confirmed_launch_records_its_pid() {
        let mut w = Wizard::new(&Config::default());
        let _ = w.update(Message::StartRecording, &Config::default());
        let _ = w.update(
            Message::RecordingStarted(RecorderStart {
                spawned: true,
                confirmed: Ok(4242),
            }),
            &Config::default(),
        );
        assert!(w.recording_started);
        assert!(!w.recording_starting);
        assert!(w.recorder_spawned());
        assert_eq!(w.recorder_pid, Some(4242));
    }

    #[test]
    fn an_unconfirmed_launch_still_counts_as_spawned() {
        let mut w = Wizard::new(&Config::default());
        let _ = w.update(Message::StartRecording, &Config::default());
        // The process is up and capturing the desktop even though it never reported in, so
        // leaving onboarding must still restart it into the real config.
        let _ = w.update(
            Message::RecordingStarted(RecorderStart {
                spawned: true,
                confirmed: Err("timed out".to_owned()),
            }),
            &Config::default(),
        );
        assert!(!w.recording_starting);
        assert!(!w.recording_started);
        assert!(w.recorder_spawned());
        assert_eq!(w.recording_error.as_deref(), Some("timed out"));
    }

    #[test]
    fn a_launch_that_never_spawned_reports_only_the_error() {
        let mut w = Wizard::new(&Config::default());
        let _ = w.update(Message::StartRecording, &Config::default());
        let _ = w.update(
            Message::RecordingStarted(RecorderStart::not_spawned("boom")),
            &Config::default(),
        );
        assert!(!w.recorder_spawned());
        assert!(!w.recording_started);
        assert_eq!(w.recording_error.as_deref(), Some("boom"));
    }

    #[test]
    fn a_vanished_recorder_reopens_the_start_button() {
        let mut w = Wizard::new(&Config::default());
        let _ = w.update(
            Message::RecordingStarted(RecorderStart {
                spawned: true,
                confirmed: Ok(7),
            }),
            &Config::default(),
        );
        let _ = w.update(
            Message::TestClipResult(Err(ClipFailure::gone("gone"))),
            &Config::default(),
        );
        assert!(!w.recording_started, "otherwise onboarding dead-ends");
        assert_eq!(w.recorder_pid, None);
    }

    #[test]
    fn a_capture_failure_leaves_the_confirmed_recorder_alone() {
        let mut w = Wizard::new(&Config::default());
        let _ = w.update(
            Message::RecordingStarted(RecorderStart {
                spawned: true,
                confirmed: Ok(7),
            }),
            &Config::default(),
        );
        let _ = w.update(
            Message::TestClipResult(Err(ClipFailure::failed("no adapter"))),
            &Config::default(),
        );
        assert!(w.recording_started);
        assert_eq!(w.recorder_pid, Some(7));
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

    fn sample_status(
        pid: u32,
        state: rewynd_config::RecorderState,
    ) -> rewynd_config::RecorderStatus {
        rewynd_config::RecorderStatus {
            version: rewynd_config::RECORDER_STATUS_VERSION,
            pid,
            encoder: "cpu".to_owned(),
            state,
            game: None,
            detail: None,
            display_width: None,
            display_height: None,
        }
    }

    #[test]
    fn recorder_launch_outcome_is_pending_without_a_matching_status() {
        assert!(matches!(
            recorder_launch_outcome(None, 42),
            RecorderLaunch::Pending
        ));
        // A stale status from the process we just stopped (or a different pid entirely) must
        // never read as our freshly spawned recorder coming up.
        let other = sample_status(7, rewynd_config::RecorderState::Recording);
        assert!(matches!(
            recorder_launch_outcome(Some(&other), 42),
            RecorderLaunch::Pending
        ));
    }

    #[test]
    fn a_starting_recorder_is_not_yet_up() {
        // It publishes this before the share picker and the encoder exist; calling it up here is
        // exactly the false "Recording is running." the handshake has to prevent.
        let starting = sample_status(42, rewynd_config::RecorderState::Starting);
        assert!(matches!(
            recorder_launch_outcome(Some(&starting), 42),
            RecorderLaunch::Pending
        ));
    }

    #[test]
    fn an_immediate_exit_names_the_likely_cause() {
        assert!(exited_message(Some(0)).contains("Another copy"));
        assert!(exited_message(Some(3)).contains("exit code 3"));
        assert!(!exited_message(None).is_empty());
    }

    #[test]
    fn recorder_launch_outcome_reports_up_for_a_matching_pid() {
        let recording = sample_status(42, rewynd_config::RecorderState::Recording);
        assert!(matches!(
            recorder_launch_outcome(Some(&recording), 42),
            RecorderLaunch::Up
        ));
        let idle = sample_status(42, rewynd_config::RecorderState::Idle);
        assert!(matches!(
            recorder_launch_outcome(Some(&idle), 42),
            RecorderLaunch::Up
        ));
    }

    #[test]
    fn recorder_launch_outcome_surfaces_a_matching_failure() {
        let mut failed = sample_status(42, rewynd_config::RecorderState::Failed);
        failed.detail = Some("no GPU adapter found".to_owned());
        match recorder_launch_outcome(Some(&failed), 42) {
            RecorderLaunch::Failed(detail) => assert_eq!(detail, "no GPU adapter found"),
            _ => panic!("expected Failed"),
        }
        // No detail at all still yields a readable message rather than losing the failure.
        let bare = sample_status(42, rewynd_config::RecorderState::Failed);
        match recorder_launch_outcome(Some(&bare), 42) {
            RecorderLaunch::Failed(detail) => assert!(!detail.is_empty()),
            _ => panic!("expected Failed"),
        }
    }

    #[test]
    fn failed_detail_only_fires_on_a_failed_state() {
        assert_eq!(failed_detail(None, Some(1)), None);
        let recording = sample_status(1, rewynd_config::RecorderState::Recording);
        assert_eq!(failed_detail(Some(&recording), Some(1)), None);
        let mut failed = sample_status(1, rewynd_config::RecorderState::Failed);
        failed.detail = Some("WGC session lost".to_owned());
        assert_eq!(
            failed_detail(Some(&failed), Some(1)),
            Some("WGC session lost".to_owned())
        );
    }

    #[test]
    fn a_confirmed_recorder_that_stops_publishing_reads_as_gone() {
        let gone = clip_wait_failure(None, Some(42)).expect("a confirmed recorder must be missed");
        assert!(gone.recorder_gone);
        // Nothing was confirmed, so there is no recorder to have lost: keep waiting.
        assert!(clip_wait_failure(None, None).is_none());
    }

    #[test]
    fn a_reported_capture_failure_does_not_read_as_gone() {
        let mut failed = sample_status(42, rewynd_config::RecorderState::Failed);
        failed.detail = Some("WGC session lost".to_owned());
        let f = clip_wait_failure(Some(&failed), Some(42)).expect("the failure is reported");
        assert!(!f.recorder_gone, "the save may still be muxing");
        assert_eq!(f.message, "WGC session lost");
    }

    #[test]
    fn failed_detail_ignores_another_recorders_failure() {
        let mut failed = sample_status(9, rewynd_config::RecorderState::Failed);
        failed.detail = Some("an autostart recorder died earlier".to_owned());
        assert_eq!(failed_detail(Some(&failed), Some(1)), None);
        // With no confirmed pid there is nothing to compare against, so any failure counts.
        assert!(failed_detail(Some(&failed), None).is_some());
    }
}
