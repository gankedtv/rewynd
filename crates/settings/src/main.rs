//! rewynd — the app window: the clip LIBRARY (default view) and the settings editor for the
//! config file (`$XDG_CONFIG_HOME/rewynd/config.toml`).
//!
//! Settings edits the same file the recorder reads (the file is the single source of truth, so
//! no IPC). Changes apply on the recorder's next clip / restart — the window says so after
//! saving; live-reload is a future refinement.
//!
//! Rendered with iced's wgpu backend (tiny-skia as the software fallback) and the Arena theme
//! (`theme`). The window needs a display to run, so there is no headless test of `run`; the
//! pure mapping helpers are unit-tested.

// A windowed GUI should never pop a console. Windows-only (cfg_attr leaves Linux a console app);
// `attach_parent_console` below reconnects stdout/stderr for terminal runs.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod anim;
#[cfg(target_os = "macos")]
mod dock;
mod library;
mod player;
mod scroll;
mod theme;
mod thumbs;
mod trimbar;
mod video;
mod wizard;

use std::fmt;

use iced::theme::Palette;
use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, slider, text, text_input,
};
use iced::{Background, Border, Element, Font, Length, Task, Theme, font};

use rewynd_config::{self as config, Config};

use crate::theme::{
    CONTENT_MAX_WIDTH, DISPLAY_BLACK, UI_BOLD, UI_SEMIBOLD, arena_check, arena_input, arena_pick,
    arena_slider, card, card_fixed, field, field_label, hint, link_button, logo, oauth_button,
    palette, primary_button, secondary_button, setting, status_pill, tinted, value_row,
    window_icon,
};

/// Whether a stored URL points somewhere other than the shipped default (empty means "use the
/// default" and an explicitly spelled-out default is still the default).
fn is_custom_url(stored: &str, default: &str) -> bool {
    let stored = stored.trim();
    !stored.is_empty() && stored != default
}

/// Cap a microphone label so a long PipeWire description doesn't run past the dropdown; the ellipsis
/// signals it was shortened. Counts by `char` so a multi-byte name is never split mid-codepoint.
fn truncate_mic_label(label: &str) -> String {
    const MAX_CHARS: usize = 38;
    if label.chars().count() <= MAX_CHARS {
        return label.to_owned();
    }
    let head: String = label.chars().take(MAX_CHARS - 1).collect();
    format!("{head}…")
}

/// The stored URL when it's a genuine custom endpoint, else empty — so the built-in default is
/// only ever a placeholder in the custom-connector fields, never a value the user seems to have
/// typed.
fn custom_url_or_empty(stored: &str, default: &str) -> String {
    if is_custom_url(stored, default) {
        stored.trim().to_owned()
    } else {
        String::new()
    }
}

/// The view to open on. First run (no config file) or an explicit `--onboarding` starts in the
/// onboarding wizard; otherwise the library.
fn initial_view() -> View {
    let requested = std::env::args().any(|arg| arg == "--onboarding");
    let no_config = config::config_path().is_none_or(|path| !path.exists());
    if requested || no_config {
        View::Onboarding
    } else {
        View::default()
    }
}

/// The raw `rewynd://clip/<name>` launch argument, if any — a clicked desktop "clip saved"
/// toast launches us with the link as an argument.
fn deeplink_arg() -> Option<String> {
    std::env::args().find(|arg| arg.starts_with(config::CLIP_URL_PREFIX))
}

/// The clip a `rewynd://clip/<name>` launch argument points at, resolved against the configured
/// output directory. `None` when no such argument is present (or it fails validation).
fn deeplink_clip(config: &Config) -> Option<std::path::PathBuf> {
    deeplink_arg().and_then(|arg| config::clip_from_deeplink(&arg, config.output_dir().as_deref()))
}

/// The clip a *forwarded* activation points at: a refused second instance (a toast clicked while
/// this window is open) hands its link over via the activation file. Consumes the pending file;
/// the link is re-validated here, exactly like a launch argument.
fn forwarded_clip(config: &Config) -> Option<std::path::PathBuf> {
    config::take_settings_activation()
        .and_then(|link| config::clip_from_deeplink(&link, config.output_dir().as_deref()))
}

/// How long to wait after the first filesystem event before refreshing, so a burst (one clip
/// write touches the `.part` file, the rename, and the directory) collapses into one rescan.
const WATCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// How long to wait before re-arming the clip-directory watch when the directory does not exist
/// yet (a fresh machine that has never saved a clip).
const WATCH_RETRY: std::time::Duration = std::time::Duration::from_secs(3);

/// How often to poll the recorder's status file for the top-right pill.
const STATUS_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Poll the recorder's status file about once a second, emitting a [`Message::RecorderStatus`]
/// only when it changes (plus once at startup). The read is a small file plus a pid liveness
/// check, run off the UI thread.
fn recorder_status_stream() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        4,
        |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            use iced::futures::SinkExt;
            let mut last: Option<config::RecorderStatus> = None;
            let mut first = true;
            loop {
                let current = tokio::task::spawn_blocking(config::read_recorder_status)
                    .await
                    .unwrap_or(None);
                if first || current != last {
                    first = false;
                    last = current.clone();
                    if output.send(Message::RecorderStatus(current)).await.is_err() {
                        break;
                    }
                }
                tokio::time::sleep(STATUS_POLL).await;
            }
        },
    )
}

/// A `notify` watcher bridged to a tokio channel: filesystem events that pass `filter` become
/// unit ticks on the receiver. The watcher must be kept alive (and pointed at a dir via
/// `watch`) for ticks to flow.
fn watch_channel(
    filter: impl Fn(&notify::Event) -> bool + Send + 'static,
) -> notify::Result<(
    notify::RecommendedWatcher,
    tokio::sync::mpsc::UnboundedReceiver<()>,
)> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && filter(&event)
        {
            let _ = tx.send(());
        }
    })?;
    Ok((watcher, rx))
}

/// Watch `dir` recursively and yield a debounced [`Message::ClipsChanged`] whenever clips there
/// appear or disappear. Recursive so a brand-new per-game subfolder (the first clip of a new
/// game) is covered without re-binding. The `notify` watcher is owned by the async block and
/// lives until the subscription is dropped.
fn clip_watch_stream(dir: std::path::PathBuf) -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        4,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            use iced::futures::SinkExt;
            use notify::{RecursiveMode, Watcher};

            let (mut watcher, mut rx) = match watch_channel(|_| true) {
                Ok(bridge) => bridge,
                Err(e) => {
                    tracing::warn!(error = %e, "clip-directory watcher unavailable; refresh is manual");
                    return;
                }
            };
            // The clip directory may not exist yet on a fresh machine (no clip saved). Re-arm on
            // a timer until it appears rather than giving up: the subscription is keyed by the
            // resolved path, so a one-shot failure would leave auto-refresh dead until the app
            // restarts. A dropped subscription cancels this future mid-sleep.
            let mut rearmed = false;
            loop {
                match watcher.watch(&dir, RecursiveMode::Recursive) {
                    Ok(()) => break,
                    Err(e) => {
                        tracing::debug!(error = %e, dir = %dir.display(), "clip directory not watchable yet; retrying");
                        rearmed = true;
                        tokio::time::sleep(WATCH_RETRY).await;
                    }
                }
            }
            // If the directory only just appeared, surface whatever it already holds.
            if rearmed && output.send(Message::ClipsChanged).await.is_err() {
                return;
            }

            while rx.recv().await.is_some() {
                // Collapse the rest of the burst before refreshing once.
                tokio::time::sleep(WATCH_DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
                if output.send(Message::ClipsChanged).await.is_err() {
                    break;
                }
            }
        },
    )
}

/// Watch the instance dir for a forwarded activation (a toast clicked while this window is
/// open — the refused second instance drops the link there) and yield [`Message::ClipForwarded`]
/// when the activation file lands. Events are filtered to that one file: the dir also holds the
/// recorder's pid/status files, whose churn must not wake the UI. No debounce — the file appears
/// in a single atomic rename, and consuming it is idempotent.
fn activation_watch_stream() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        4,
        |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            use iced::futures::SinkExt;
            use notify::{RecursiveMode, Watcher};

            let target = config::settings_activation_path();
            let Some(dir) = target.parent().map(std::path::Path::to_path_buf) else {
                return;
            };
            let file_name = target.file_name().map(std::ffi::OsStr::to_owned);
            let (mut watcher, mut rx) = match watch_channel(move |event| {
                event
                    .paths
                    .iter()
                    .any(|p| p.file_name() == file_name.as_deref())
            }) {
                Ok(bridge) => bridge,
                Err(e) => {
                    tracing::warn!(error = %e, "activation watcher unavailable; clip links from toasts need this window closed");
                    return;
                }
            };
            // The dir normally exists (taking the settings lock creates it), but that creation
            // is best-effort — retry rather than give up, the sender creates the dir too.
            // Non-recursive: only the activation file matters.
            loop {
                match watcher.watch(&dir, RecursiveMode::NonRecursive) {
                    Ok(()) => break,
                    Err(e) => {
                        tracing::debug!(error = %e, dir = %dir.display(), "instance dir not watchable yet; retrying");
                        tokio::time::sleep(WATCH_RETRY).await;
                    }
                }
            }
            // A hand-off that landed after load but before the watch armed produced no event;
            // one nudge covers it (consuming is idempotent — nothing pending is a no-op).
            if output.send(Message::ClipForwarded).await.is_err() {
                return;
            }
            while rx.recv().await.is_some() {
                if output.send(Message::ClipForwarded).await.is_err() {
                    break;
                }
            }
        },
    )
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
/// Height (logical px) the ganked.tv connector card is pinned to so it matches the YouTube card's
/// collapsed height beside it. Tuned by eye to the YouTube card (account row + hint + the Advanced
/// options toggle ganked.tv lacks); iced rows can't stretch a child to a sibling's height.
const CONNECTOR_CARD_HEIGHT: f32 = 176.0;
/// The microphone picker's "use the system default" row (stored as an empty value).
const MIC_DEFAULT: &str = "System default";
/// Width (logical px) of the fixed left navigation sidebar. Wide enough for the wordmark and the
/// nav labels; the content area fills the rest of the window.
const SIDEBAR_WIDTH: f32 = 232.0;
const BITS_PER_MBIT: u32 = 1_000_000;

fn main() -> iced::Result {
    // Must be first: Velopack's install/update hooks run here and may exit/restart the process.
    // `on_restarted` fires after an update relaunch — the update flow stopped the recorder before
    // applying, so bring it back on the new version. Inert for dev/cargo runs (no receipt).
    velopack::VelopackApp::build()
        .on_restarted(|_ver| spawn_recorder_detached())
        .run();

    // As a windows-subsystem exe we start with no console; reconnect to the launching one (if any)
    // so a terminal launch still shows tracing output and `--version`. A no-op elsewhere.
    config::attach_parent_console();

    if std::env::args().any(|arg| arg == "--version") {
        println!("rewynd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing_subscriber::fmt::init();

    // Single-instance guard: a second window edits the same file, where its save clobbers the
    // first's. Held until `run` returns; the kernel releases it when the process exits.
    let _instance: Option<config::InstanceLock> = match config::acquire_settings_lock() {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            tracing::info!("rewynd settings is already open; not opening a second window");
            // A clip link (a clicked "clip saved" toast) is handed to the running window
            // instead: it opens the clip itself. Only a link that actually resolves to a clip
            // is worth handing over — a bogus one, or a failed hand-off, falls through to the
            // "already open" notification like any other second launch.
            if let Some(link) = deeplink_arg()
                && deeplink_clip(&config::load_file()).is_some()
            {
                match config::send_settings_activation(&link) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        tracing::warn!(error = %e, "could not hand the clip link to the running window");
                    }
                }
            }
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
    #[cfg(target_os = "linux")]
    if let Err(e) = config::install_icons() {
        tracing::warn!(error = %e, "could not install app icons");
    }
    #[cfg(windows)]
    if let Err(e) = config::register_toast_identity() {
        tracing::warn!(error = %e, "could not register the toast identity");
    }
    // The launcher entry opens this GUI (the user-facing `rewynd`).
    #[cfg(target_os = "linux")]
    if let Ok(exe) = std::env::current_exe()
        && let Err(e) = config::install_launcher_entry(&exe)
    {
        tracing::warn!(error = %e, "could not write a desktop entry");
    }
    if let Some(recorder) = recorder_path().filter(|p| p.is_file()) {
        // Migrate a stale autostart entry (pre-icon, or the pre-rename recorder binary on Linux;
        // a moved binary on Windows or macOS) onto the current recorder.
        if let Err(e) = config::refresh_autostart(&recorder) {
            tracing::warn!(error = %e, "could not refresh the autostart entry");
        }
    }

    // Setup already done (a config exists, so not first-run onboarding): make sure the recorder is
    // up, so opening the app means it is buffering rather than silently doing nothing until the
    // user hits Restart. Idempotent — a duplicate exits immediately on the recorder's single-
    // instance lock. First run goes to onboarding, which starts the recorder itself at the end.
    if initial_view() != View::Onboarding {
        spawn_recorder_detached();
    }

    iced::application(App::load, App::update, App::view)
        .title("rewynd")
        .theme(App::theme)
        .subscription(App::subscription)
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
            // Landscape: the left sidebar takes a fixed column, leaving a wide content area for the
            // two-column settings and the four-across clip grid (wide enough that each thumbnail
            // renders crisp). Tall enough that the whole form (advanced collapsed) fits without
            // scrolling.
            size: iced::Size::new(1380.0, 880.0),
            min_size: Some(iced::Size::new(880.0, 560.0)),
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

/// One entry in the "Recording method" dropdown: the stored config value plus a display label.
#[derive(Clone, PartialEq, Eq)]
struct EncoderOption {
    value: String,
    label: String,
}

impl fmt::Display for EncoderOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// Build the recording-method options from the recorder's probe (just auto + CPU when the probe
/// is missing). Encode-incapable adapters are excluded; a pinned GPU that isn't in the probe
/// stays visible as "(unavailable)" so the selection never silently snaps to auto.
fn encoder_options(probe: Option<&config::EncoderProbe>, stored: &str) -> Vec<EncoderOption> {
    let mut options = vec![EncoderOption {
        value: "auto".to_owned(),
        label: "Automatic (recommended)".to_owned(),
    }];
    let mut saw_stored = stored == "auto" || stored == "cpu";
    if let Some(probe) = probe {
        for adapter in &probe.adapters {
            if !adapter.h264_encode {
                continue;
            }
            let value = format!("gpu:{}", adapter.name);
            saw_stored |= value == stored;
            options.push(EncoderOption {
                label: format!("GPU: {}", adapter.name),
                value,
            });
        }
    }
    if !saw_stored && let Some(name) = stored.strip_prefix("gpu:") {
        options.push(EncoderOption {
            value: stored.to_owned(),
            label: format!("{name} (unavailable)"),
        });
    }
    options.push(EncoderOption {
        value: "cpu".to_owned(),
        label: "CPU (not recommended)".to_owned(),
    });
    options
}

/// The top-right status pill's text and dot colour for a recorder status (`None` = not running).
fn status_pill_parts(status: Option<&config::RecorderStatus>) -> (String, iced::Color) {
    use config::RecorderState;
    match status {
        None => ("Not recording".to_owned(), palette::MUTED),
        Some(s) => match s.state {
            RecorderState::Recording => match &s.game {
                Some(game) => (
                    format!("Recording: {}", truncate_mic_label(game)),
                    palette::ACCENT,
                ),
                None => ("Recording: Desktop".to_owned(), palette::ACCENT),
            },
            RecorderState::Idle => ("Waiting for a game".to_owned(), palette::MUTED),
            RecorderState::Failed => ("Capture failed".to_owned(), palette::DANGER),
        },
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

/// Where the nav-bar "Check for updates" affordance stands. Only shown in a Velopack install.
#[derive(Default)]
enum UpdateState {
    #[default]
    Idle,
    /// Checking, downloading, or applying — the button is disabled meanwhile.
    Working,
    UpToDate,
    Failed(String),
}

/// The window's top-level pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum View {
    /// Saved clips with thumbnails — what a gamer opens rewynd for.
    #[default]
    Library,
    Settings,
    /// First-run setup (no config yet) or a rerun from Settings.
    Onboarding,
}

/// Editable application state: the loaded config plus text mirrors for the free-text fields.
struct App {
    view: View,
    wizard: wizard::Wizard,
    library: library::Library,
    config: Config,
    /// Mirror of the output directory for the text box (empty = "use the default").
    output_dir: String,
    /// Mirror of the hotkey trigger for the text box.
    hotkey: String,
    /// Active input devices for the microphone picker (Windows WASAPI endpoints, Linux PipeWire
    /// sources); empty when enumeration finds nothing, where the control is a free-text name.
    mic_options: Vec<config::AudioInput>,
    /// Whether the settings page's device discovery (audio inputs, the encoder probe) has been
    /// kicked off; it runs once, the first time that page opens.
    probes_started: bool,
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
    /// Whether the CUSTOM CONNECTOR card shows its fields (API server, share links, API key).
    /// UI-only state; collapsed by default because a normal user never needs it.
    advanced_open: bool,
    /// Whether the audio card shows its advanced options (the separate mic track). UI-only.
    audio_advanced_open: bool,
    /// Whether the capture card shows its advanced options (the per-start monitor prompt). UI-only.
    #[cfg(target_os = "linux")]
    capture_advanced_open: bool,
    login: LoginState,
    /// Mirror of the YouTube OAuth client id override (empty = the compiled-in default).
    yt_client_id: String,
    /// Mirror of the YouTube OAuth client secret override.
    yt_client_secret: String,
    /// Whether the YouTube card shows its OAuth-client override fields.
    yt_advanced_open: bool,
    yt_login: YtLoginState,
    status: Status,
    /// The recorder's encoder probe (adapters + capability), for the recording-method picker.
    /// `None` until the probe returns (or if it fails).
    encoder_probe: Option<config::EncoderProbe>,
    /// The recorder's live status (game/desktop/idle/failed), polled for the top-right pill.
    recorder_status: Option<config::RecorderStatus>,
    /// True in a packaged Velopack install; gates the "Check for updates" affordance so dev,
    /// cargo, and package-manager installs never show it.
    is_velopack: bool,
    update: UpdateState,
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

/// Where the YouTube loopback login stands. "Connected" is derived from a stored refresh token.
enum YtLoginState {
    Idle,
    Starting {
        abort: iced::task::Handle,
    },
    /// Waiting for the browser redirect: the consent URL (for a manual reopen) and the wait
    /// task's abort handle so Cancel actually stops the loopback listener.
    Waiting {
        auth_url: String,
        abort: iced::task::Handle,
    },
    Failed(String),
}

/// A started YouTube login headed through the message loop: the consent URL to open, plus the
/// login handed to the wait task. `YouTubeLogin` owns a socket so it cannot be cloned; the
/// message must be, hence the shared slot the wait task takes it from.
#[derive(Debug, Clone)]
struct YtStarted {
    auth_url: String,
    login: std::sync::Arc<std::sync::Mutex<Option<rewynd_upload::youtube::YouTubeLogin>>>,
}

#[derive(Debug, Clone)]
enum Message {
    Tab(View),
    Library(library::Message),
    Wizard(wizard::Message),
    RerunOnboarding,
    MicGain(f32),
    SystemGain(f32),
    MicEnabled(bool),
    SeparateMicTrack(bool),
    AudioAdvancedToggled,
    MicrophonePicked(String),
    BufferSeconds(u32),
    ResolutionPicked(Resolution),
    FpsPicked(u32),
    BitrateMbps(u32),
    EncoderPicked(String),
    /// The recorder's `--probe-encoders` output (for the recording-method picker).
    EncodersProbed(Result<config::EncoderProbe, String>),
    /// The machine's active audio inputs (for the microphone picker).
    MicsListed(Vec<config::AudioInput>),
    /// A poll of the recorder's status file (for the top-right pill).
    RecorderStatus(Option<config::RecorderStatus>),
    OutputDirEdited(String),
    BrowseDir,
    DirPicked(Option<String>),
    HotkeyEdited(String),
    #[cfg(target_os = "linux")]
    AlwaysPrompt(bool),
    #[cfg(target_os = "linux")]
    CaptureAdvancedToggled,
    CaptureDesktop(bool),
    GameFolders(bool),
    StartOnBoot(bool),
    ApiKeyEdited(String),
    ApiUrlEdited(String),
    ShareUrlEdited(String),
    AdvancedToggled,
    LoginPressed,
    LoginStarted(Result<rewynd_upload::DeviceLogin, String>),
    LoginDone(Result<String, String>),
    LoginCancelled,
    LogoutPressed,
    YtClientIdEdited(String),
    YtClientSecretEdited(String),
    YtAdvancedToggled,
    YtLoginPressed,
    YtLoginStarted(Result<YtStarted, String>),
    YtLoginDone(Result<String, String>),
    YtLoginCancelled,
    YtOpenAuthUrl,
    YtLogoutPressed,
    Save,
    Restart,
    Restarted(Result<(), String>),
    /// Nav-bar update button: check for, download, and apply an update.
    CheckForUpdates,
    UpdateFinished(Result<(), String>),
    /// The window regained focus; refresh the library so clips saved meanwhile show up.
    WindowFocused,
    /// The clip-directory watcher fired (debounced); refresh the library.
    ClipsChanged,
    /// A refused second instance forwarded a clip link (a toast clicked while this window is
    /// open); consume it, open the clip, and raise the window.
    ClipForwarded,
}

impl App {
    /// Build the initial state and the boot task (the library scan: it is the default view).
    /// Device discovery for the settings page (audio inputs, the encoder probe) waits until
    /// that page first opens, so the launch path pays for nothing the library doesn't need.
    fn load() -> (Self, Task<Message>) {
        let mut app = Self::new();
        let scan = app.library.refresh(&app.config).map(Message::Library);
        let mut tasks = vec![scan];
        // A rewynd://clip/<name> launch (a clicked desktop "clip saved" toast) opens that clip in
        // the library; the detail view fills in once the scan lands. A pending forwarded
        // activation is consumed unconditionally (`or`, not `or_else`) — even when the argument
        // wins, a leftover must not linger to replay later.
        let forwarded = forwarded_clip(&app.config);
        if let Some(clip) = deeplink_clip(&app.config).or(forwarded) {
            app.view = View::Library;
            tasks.push(Task::done(Message::Library(library::Message::Open(clip))));
        }
        (app, Task::batch(tasks))
    }

    fn new() -> Self {
        // Edit the file's own values (no env overrides — those are a runtime concern).
        let mut config = config::load_file();
        // Snap the stored values into the ranges the controls can represent, so what the window
        // shows is exactly what Save will write back (no slider that displays a clamped value
        // while the file keeps a different one). Resolution and the keyframe interval are left
        // alone — a custom resolution is shown verbatim, and the GOP is only retuned with the fps.
        normalize(&mut config);
        let wizard = wizard::Wizard::new(&config);
        Self {
            view: initial_view(),
            wizard,
            library: library::Library::new(),
            output_dir: config.output_directory().unwrap_or_default().to_owned(),
            hotkey: config.hotkey_trigger().to_owned(),
            mic_options: Vec::new(),
            probes_started: false,
            api_key: config.upload_api_key().to_owned(),
            // Show the custom-connector fields empty unless a genuinely custom endpoint is set:
            // the built-in default is a placeholder, never a pre-filled value the user didn't
            // type. An older config that stored the spelled-out default is blanked here and
            // rewritten empty on the next Save (`upload()` falls back to the default anyway).
            api_url: custom_url_or_empty(config.upload_api_url(), config::DEFAULT_UPLOAD_API_URL),
            share_url: custom_url_or_empty(
                config.upload_share_url(),
                config::DEFAULT_UPLOAD_SHARE_URL,
            ),
            applied_on_boot: config.start_on_boot(),
            // A saved self-hosting setup stays visible instead of hiding behind the disclosure
            // (a key alone is just "logged in" — the badge already shows that). URLs stored as
            // the ganked.tv defaults, spelled out, are not a custom setup.
            advanced_open: is_custom_url(config.upload_api_url(), config::DEFAULT_UPLOAD_API_URL)
                || is_custom_url(config.upload_share_url(), config::DEFAULT_UPLOAD_SHARE_URL),
            audio_advanced_open: false,
            #[cfg(target_os = "linux")]
            capture_advanced_open: false,
            yt_client_id: config.youtube_client_id().to_owned(),
            yt_client_secret: config.youtube_client_secret().to_owned(),
            // A stored OAuth-client override stays visible instead of hiding behind the
            // disclosure.
            yt_advanced_open: !config.youtube_client_id().trim().is_empty()
                || !config.youtube_client_secret().trim().is_empty(),
            config,
            login: LoginState::Idle,
            yt_login: YtLoginState::Idle,
            status: Status::Editing,
            last_save_ok: false,
            encoder_probe: None,
            recorder_status: None,
            is_velopack: is_velopack_install(),
            update: UpdateState::Idle,
        }
    }

    /// The OAuth client the YouTube login should use: the (unsaved) field values, or the
    /// compiled-in defaults.
    fn effective_yt_client(&self) -> (String, String) {
        (
            config::non_empty_or(
                &self.yt_client_id,
                rewynd_upload::youtube::DEFAULT_CLIENT_ID,
            )
            .to_owned(),
            config::non_empty_or(
                &self.yt_client_secret,
                rewynd_upload::youtube::DEFAULT_CLIENT_SECRET,
            )
            .to_owned(),
        )
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
            Message::Tab(view) => {
                self.view = view;
                // The Library tab / brand logo is a home action: leave any open clip's detail for
                // the grid, and refresh so clips saved while away appear.
                if view == View::Library {
                    self.library.show_grid();
                    return self.library.refresh(&self.config).map(Message::Library);
                }
                if view == View::Settings {
                    return self.start_settings_probes();
                }
            }
            Message::MicsListed(mics) => {
                self.mic_options = mics;
            }
            Message::Library(message) => {
                return self
                    .library
                    .update(message, &self.config)
                    .map(Message::Library);
            }
            Message::Wizard(wizard::Message::Finish) => return self.finish_onboarding(),
            Message::Wizard(wizard::Message::SkipSetup) => return self.skip_onboarding(),
            Message::Wizard(message) => {
                return self
                    .wizard
                    .update(message, &self.config)
                    .map(Message::Wizard);
            }
            Message::RerunOnboarding => {
                self.wizard = wizard::Wizard::new(&self.config);
                self.view = View::Onboarding;
            }
            Message::MicGain(v) => {
                self.config.set_mic_gain(v);
                self.touch();
            }
            Message::SystemGain(v) => {
                self.config.set_system_gain(v);
                self.touch();
            }
            Message::MicEnabled(on) => {
                self.config.set_mic_enabled(on);
                self.touch();
            }
            Message::SeparateMicTrack(on) => {
                self.config.set_separate_mic_track(on);
                self.touch();
            }
            Message::MicrophonePicked(mic) => {
                // The picker's "System default" row carries the empty id; free text is stored
                // verbatim. Either way the stored value is what the capture backend resolves.
                self.config.set_microphone(mic);
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
            Message::EncoderPicked(value) => {
                self.config.set_encoder(value);
                self.touch();
            }
            Message::EncodersProbed(Ok(probe)) => {
                self.encoder_probe = Some(probe);
            }
            Message::EncodersProbed(Err(e)) => {
                // Not fatal: the picker falls back to just Automatic + CPU.
                tracing::warn!(error = %e, "could not probe encoders; the picker lists auto + CPU only");
            }
            Message::RecorderStatus(status) => {
                self.recorder_status = status;
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
            Message::CaptureDesktop(on) => {
                self.config.set_capture_desktop(on);
                self.touch();
            }
            Message::GameFolders(on) => {
                self.config.set_game_folders(on);
                self.touch();
            }
            Message::StartOnBoot(on) => {
                self.config.set_start_on_boot(on);
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
            // No touch(): opening or closing the disclosure edits nothing. The scroll offset stays
            // put and clamps to the shorter form, rather than jumping the whole view to the top.
            Message::AdvancedToggled => self.advanced_open = !self.advanced_open,
            Message::AudioAdvancedToggled => self.audio_advanced_open = !self.audio_advanced_open,
            #[cfg(target_os = "linux")]
            Message::CaptureAdvancedToggled => {
                self.capture_advanced_open = !self.capture_advanced_open;
            }
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
            Message::YtClientIdEdited(s) => {
                self.yt_client_id = s;
                self.touch();
            }
            Message::YtClientSecretEdited(s) => {
                self.yt_client_secret = s;
                self.touch();
            }
            // No touch(): opening the disclosure edits nothing.
            Message::YtAdvancedToggled => self.yt_advanced_open = !self.yt_advanced_open,
            Message::YtLoginPressed => {
                let (client_id, client_secret) = self.effective_yt_client();
                // Google's installed-app token exchange needs both halves; catching a
                // missing secret here beats a cryptic failure after the consent screen.
                if client_id.is_empty() || client_secret.is_empty() {
                    self.yt_login = YtLoginState::Failed(
                        "No Google OAuth client is configured; add its id and secret under \
                         Advanced options."
                            .to_owned(),
                    );
                    return Task::none();
                }
                let (task, abort) = Task::perform(
                    async move {
                        rewynd_upload::youtube::youtube_login_start(&client_id, &client_secret)
                            .await
                            .map(|login| YtStarted {
                                auth_url: login.auth_url.clone(),
                                login: std::sync::Arc::new(std::sync::Mutex::new(Some(login))),
                            })
                            .map_err(|e| e.to_string())
                    },
                    Message::YtLoginStarted,
                )
                .abortable();
                self.yt_login = YtLoginState::Starting { abort };
                return task;
            }
            Message::YtLoginStarted(Ok(started)) => {
                // A cancelled/stale start must not begin listening.
                if !matches!(self.yt_login, YtLoginState::Starting { .. }) {
                    return Task::none();
                }
                if let Err(e) = open::that_detached(&started.auth_url) {
                    tracing::warn!(error = %e, "could not open the browser for Google consent");
                }
                let auth_url = started.auth_url.clone();
                let (task, abort) = Task::perform(
                    async move {
                        let login = started
                            .login
                            .lock()
                            .map_err(|_| "the login state was poisoned".to_owned())?
                            .take()
                            .ok_or_else(|| "the login was already consumed".to_owned())?;
                        rewynd_upload::youtube::youtube_login_wait(login)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::YtLoginDone,
                )
                .abortable();
                self.yt_login = YtLoginState::Waiting { auth_url, abort };
                return task;
            }
            Message::YtLoginStarted(Err(e)) => {
                if matches!(self.yt_login, YtLoginState::Starting { .. }) {
                    self.yt_login = YtLoginState::Failed(e);
                }
            }
            Message::YtLoginDone(result) => {
                // Ignore a result that arrives after Cancel/Logout switched the state away.
                if !matches!(self.yt_login, YtLoginState::Waiting { .. }) {
                    return Task::none();
                }
                match result {
                    Ok(refresh_token) => {
                        // Logging in states intent: switch YouTube uploads on and persist.
                        self.config.set_youtube_refresh_token(refresh_token);
                        self.config.set_youtube_enabled(true);
                        self.yt_login = YtLoginState::Idle;
                        self.save();
                    }
                    Err(e) => self.yt_login = YtLoginState::Failed(e),
                }
            }
            Message::YtLoginCancelled => self.abort_yt_login(),
            Message::YtOpenAuthUrl => {
                if let YtLoginState::Waiting { auth_url, .. } = &self.yt_login
                    && let Err(e) = open::that_detached(auth_url)
                {
                    tracing::warn!(error = %e, "could not reopen the Google consent page");
                }
            }
            Message::YtLogoutPressed => {
                self.config.set_youtube_refresh_token(String::new());
                self.config.set_youtube_enabled(false);
                // A pending login must not keep listening after an explicit logout.
                self.abort_yt_login();
                self.save();
            }
            Message::Save => self.save(),
            Message::Restart => {
                self.status = Status::Restarting;
                // Off the UI thread: stop the old recorder, wait for it to exit, then relaunch.
                return Self::restart_task();
            }
            Message::Restarted(result) => {
                self.status = match result {
                    Ok(()) => Status::Restarted,
                    Err(e) => Status::Error(e),
                };
            }
            Message::CheckForUpdates => {
                self.update = UpdateState::Working;
                // Off the UI thread: the check/download/apply calls block, and a successful apply
                // replaces this process (so the task only returns "up to date" or an error).
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(run_update_flow)
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::UpdateFinished,
                );
            }
            Message::UpdateFinished(result) => {
                self.update = match result {
                    Ok(()) => UpdateState::UpToDate,
                    Err(e) => UpdateState::Failed(e),
                };
            }
            // Auto-refresh only matters while the library is on screen; entering it already
            // rescans, so a Settings-side event has nothing to do.
            Message::WindowFocused | Message::ClipsChanged => {
                if self.view == View::Library {
                    return self.library.refresh(&self.config).map(Message::Library);
                }
            }
            Message::ClipForwarded => {
                // Mid-onboarding the hand-off is consumed but only raises the window: the
                // wizard's own test clip fires a toast, and clicking it must not yank the user
                // out of setup into the library.
                if self.view == View::Onboarding {
                    if config::take_settings_activation().is_some() {
                        return iced::window::latest().and_then(iced::window::gain_focus);
                    }
                } else if let Some(clip) = forwarded_clip(&self.config) {
                    tracing::info!(clip = %clip.display(), "opening a clip link handed over by a second launch");
                    self.view = View::Library;
                    return Task::batch([
                        // The user clicked a toast: bring the window forward, don't just
                        // change it in the background.
                        iced::window::latest().and_then(iced::window::gain_focus),
                        Task::done(Message::Library(library::Message::Open(clip))),
                    ]);
                }
            }
        }
        Task::none()
    }

    /// Device discovery the settings page needs, kicked off once when it first opens: the
    /// audio-input enumeration (PipeWire/WASAPI) and the encoder probe (a short-lived Vulkan
    /// probe process the recorder spawns). Neither belongs on the launch path — the library,
    /// the default view, needs neither.
    fn start_settings_probes(&mut self) -> Task<Message> {
        if self.probes_started {
            return Task::none();
        }
        self.probes_started = true;
        let mics = Task::perform(
            async {
                tokio::task::spawn_blocking(config::list_audio_inputs)
                    .await
                    .unwrap_or_default()
            },
            Message::MicsListed,
        );
        let probe = Task::perform(
            async {
                tokio::task::spawn_blocking(probe_encoders_via_recorder)
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            },
            Message::EncodersProbed,
        );
        Task::batch([mics, probe])
    }

    /// The stop-and-relaunch of the recorder as a task (off the UI thread), reported through
    /// [`Message::Restarted`].
    fn restart_task() -> Task<Message> {
        Task::perform(
            async {
                tokio::task::spawn_blocking(restart_recorder)
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            },
            Message::Restarted,
        )
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

    /// [`abort_login`](Self::abort_login)'s YouTube twin (aborting the wait task drops the
    /// loopback listener, closing the port).
    fn abort_yt_login(&mut self) {
        match std::mem::replace(&mut self.yt_login, YtLoginState::Idle) {
            YtLoginState::Starting { abort } | YtLoginState::Waiting { abort, .. } => abort.abort(),
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
        // Empty fields are stored empty: "use the compiled-in OAuth client" lives in one place
        // (the consumers), so a rebuilt default reaches everyone who didn't override it.
        self.config
            .set_youtube_client_id(self.yt_client_id.trim().to_owned());
        self.config
            .set_youtube_client_secret(self.yt_client_secret.trim().to_owned());

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
        // The Dock icon is set from here (a main-thread call after AppKit finished
        // launching, which resets any icon set earlier); it self-limits to one call.
        #[cfg(target_os = "macos")]
        dock::set_icon();

        // The wizard is full-screen (its own steps + Skip), with no nav bar to wander off into.
        if self.view == View::Onboarding {
            return self.wizard.view(&self.config).map(Message::Wizard);
        }
        let body: Element<Message> = match self.view {
            View::Library => self.library.view(&self.config).map(Message::Library),
            View::Settings => self.settings_view(),
            View::Onboarding => unreachable!("handled above"),
        };
        row![
            sidebar(
                self.view,
                self.recorder_status.as_ref(),
                self.is_velopack,
                &self.update,
            ),
            body
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    /// Apply the wizard's choices, persist, and open the library. The recorder is restarted so it
    /// captures with the real config (the wizard ran it in desktop-capture mode for the test clip).
    fn finish_onboarding(&mut self) -> Task<Message> {
        let trigger = self.wizard.hotkey().trim();
        self.hotkey = if trigger.is_empty() {
            config::DEFAULT_HOTKEY_TRIGGER.to_owned()
        } else {
            trigger.to_owned()
        };
        self.config
            .set_buffer_seconds(u64::from(self.wizard.buffer_seconds()));
        self.config.set_start_on_boot(self.wizard.start_on_boot());
        self.config
            .set_capture_desktop(self.wizard.capture_desktop());
        self.save();
        self.view = View::Library;
        let refresh = self.library.refresh(&self.config).map(Message::Library);
        Task::batch([refresh, Self::restart_task()])
    }

    /// Skip setup: persist the (default) config so the wizard doesn't reappear next launch, then
    /// open the library. If the wizard had started the recorder (in desktop-capture mode for the
    /// test clip), restart it so it applies the real config instead of continuing to record the
    /// desktop.
    fn skip_onboarding(&mut self) -> Task<Message> {
        let started = self.wizard.recording_started();
        self.save();
        self.view = View::Library;
        let refresh = self.library.refresh(&self.config).map(Message::Library);
        if !started {
            return refresh;
        }
        Task::batch([refresh, Self::restart_task()])
    }

    /// Live events that keep the library in step with what the recorder writes: the OS window
    /// regaining focus, and a debounced watch on the clip directory (and its per-game
    /// subfolders). The watch is keyed by the resolved directory, so it re-binds if the output
    /// directory changes.
    fn subscription(&self) -> iced::Subscription<Message> {
        let focus = iced::event::listen_with(|event, status, _id| match event {
            iced::Event::Window(iced::window::Event::Focused) => Some(Message::WindowFocused),
            // Escape leaves the fullscreen preview (a no-op otherwise), unless a widget
            // already claimed it (the trim bar captures Escape to drop keyboard focus).
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                ..
            }) if status == iced::event::Status::Ignored => {
                Some(Message::Library(library::Message::FullscreenExit))
            }
            _ => None,
        });
        let dir = config::clips_dir(self.config.output_dir().as_deref())
            .to_string_lossy()
            .into_owned();
        let clips = iced::Subscription::run_with(dir, |dir| {
            clip_watch_stream(std::path::PathBuf::from(dir))
        });
        // Poll the recorder's status file for the top-right pill.
        let status = iced::Subscription::run(recorder_status_stream);
        // Clip links forwarded by a refused second instance (a toast clicked while this window
        // is open).
        let activation = iced::Subscription::run(activation_watch_stream);
        // The library adds its own conditional subscriptions (accent-fade ticks, preview
        // playback); iced re-diffs after each update, so they vanish when idle and the software
        // renderer stops redrawing.
        // The wizard's ticks only exist while it animates, and only the onboarding view
        // renders it, so the stream is dropped everywhere else.
        let wizard = if matches!(self.view, View::Onboarding) {
            self.wizard.subscription().map(Message::Wizard)
        } else {
            iced::Subscription::none()
        };
        iced::Subscription::batch([
            focus,
            clips,
            status,
            activation,
            wizard,
            self.library.subscription().map(Message::Library),
        ])
    }

    fn settings_view(&self) -> Element<'_, Message> {
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

        // The microphone picker: a dropdown of the active input devices (Windows WASAPI
        // endpoints, Linux PipeWire sources), or a free-text device name when enumeration
        // found nothing. Stored value empty = the system default.
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
            // The default row is stored as the empty value.
            let default = config::AudioInput {
                id: String::new(),
                label: MIC_DEFAULT.to_owned(),
            };
            let mut options = vec![default.clone()];
            // Keep a configured-but-offline device visible instead of silently
            // snapping the selection to the default.
            if !mic_value.is_empty() && !self.mic_options.iter().any(|o| o.id == mic_value) {
                options.push(config::AudioInput {
                    id: mic_value.clone(),
                    label: mic_value.clone(),
                });
            }
            options.extend(self.mic_options.iter().cloned());
            // PipeWire descriptions can be long enough to overrun the dropdown; cap the visible
            // label. The stored id (used for matching and persistence) is left untouched.
            for o in &mut options {
                o.label = truncate_mic_label(&o.label);
            }
            let selected = options
                .iter()
                .find(|o| o.id == mic_value)
                .cloned()
                .unwrap_or(default);
            column![
                field_label("Microphone"),
                pick_list(options, Some(selected), |o: config::AudioInput| {
                    Message::MicrophonePicked(o.id)
                })
                .style(arena_pick)
                .width(Length::Fill),
            ]
            .spacing(8)
            .into()
        };
        // The mic controls only make sense while the mic is recording; when it's off they give
        // way to a hint (turning the mic off opens no mic stream at all — a privacy choice).
        let mic_enabled = self.config.mic_enabled();
        let mic_controls: Element<Message> = if mic_enabled {
            column![
                microphone,
                setting(
                    "Microphone volume",
                    format!("{:.2}x", self.config.mic_gain()),
                    slider(0.0..=GAIN_MAX, self.config.mic_gain(), Message::MicGain)
                        .step(0.05_f32)
                        .style(arena_slider),
                ),
            ]
            .spacing(18)
            .into()
        } else {
            hint("The microphone is off; turn it on to pick a device or set its level.")
        };
        let mut audio_items: Vec<Element<Message>> = vec![
            checkbox(mic_enabled)
                .label("Record microphone")
                .on_toggle(Message::MicEnabled)
                .style(arena_check)
                .into(),
            mic_controls,
            setting(
                "System volume",
                format!("{:.2}x", self.config.system_gain()),
                slider(
                    0.0..=GAIN_MAX,
                    self.config.system_gain(),
                    Message::SystemGain,
                )
                .step(0.05_f32)
                .style(arena_slider),
            ),
        ];
        // Advanced sits at the bottom of the card, and only while the mic is on (its one option,
        // the separate track, needs a recording mic to mean anything).
        if mic_enabled {
            audio_items.push(disclosure(
                self.audio_advanced_open,
                Message::AudioAdvancedToggled,
            ));
            if self.audio_advanced_open {
                audio_items.push(
                    column![
                        checkbox(self.config.separate_mic_track())
                            .label("Keep the microphone on a separate audio track")
                            .on_toggle(Message::SeparateMicTrack)
                            .style(arena_check),
                        hint(
                            "Records the mic as its own track so you can adjust or mute it later \
                             in an editor, instead of mixing it into the game audio.",
                        ),
                    ]
                    .spacing(6)
                    .into(),
                );
            }
        }
        let audio = card("AUDIO", column(audio_items).spacing(18));

        // Recording method: Automatic / one entry per encode-capable GPU / CPU. Populated from
        // the recorder's probe; a CPU pick gets a processor-cost hint.
        let stored_encoder = self.config.encoder_stored().to_owned();
        let encoder_opts = encoder_options(self.encoder_probe.as_ref(), &stored_encoder);
        let selected_encoder = encoder_opts
            .iter()
            .find(|o| o.value == stored_encoder)
            .cloned();
        let encoder_control = pick_list(encoder_opts, selected_encoder, |o: EncoderOption| {
            Message::EncoderPicked(o.value)
        })
        .style(arena_pick)
        .width(Length::Fill);
        let encoder_method: Element<Message> = if stored_encoder == "cpu" {
            column![
                setting("Recording method", String::new(), encoder_control),
                hint("The CPU encoder works everywhere but uses much more processor power at high resolutions."),
            ]
            .spacing(7)
            .into()
        } else {
            setting("Recording method", String::new(), encoder_control)
        };

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
                encoder_method,
                value_row("Estimated clip size", format!("about {est_mb} MB")),
            ]
            .spacing(18),
        );

        // The full default path is too long to read as a placeholder, so the box just says
        // "Leave empty for default" and the resolved location is spelled out on its own line.
        // With no Videos folder, clips fall back to a private rewynd-clips dir under the temp
        // dir (then ~/.rewynd-clips) — describe that rather than implying a shared temp folder.
        let default_location = config::default_output_dir().map_or_else(
            || "a private rewynd-clips folder under your temp directory".to_owned(),
            |p| p.display().to_string(),
        );
        let output_capture = column![
            column![
                field_label("Save clips to"),
                row![
                    text_input("Leave empty for default", &self.output_dir)
                        .on_input(Message::OutputDirEdited)
                        .style(arena_input),
                    button(text("Browse").size(12).font(UI_SEMIBOLD))
                        .on_press(Message::BrowseDir)
                        .style(secondary_button)
                        .padding([10, 16]),
                ]
                .spacing(8),
                hint(format!("Default location: {default_location}")),
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
        // Desktop capture is the primary choice; the per-start monitor prompt below is a Linux-only
        // ScreenCast-portal detail (Windows records the active game by default) that only applies
        // while desktop capture is on.
        let capture_desktop = self.config.capture_desktop();
        let output_capture = output_capture
            .push(
                column![
                    checkbox(capture_desktop)
                        .label("Record the whole desktop, not just the active game")
                        .on_toggle(Message::CaptureDesktop)
                        .style(arena_check),
                    hint(
                        "Off records only the game you're playing (fullscreen or \
                         borderless), keeping other windows out of your clips.",
                    ),
                ]
                .spacing(6),
            )
            .push(
                column![
                    checkbox(self.config.game_folders())
                        .label("Sort clips into a folder per game")
                        .on_toggle(Message::GameFolders)
                        .style(arena_check),
                    hint("Saved clips land in a subfolder named after the game, when known."),
                ]
                .spacing(6),
            )
            .push(
                checkbox(self.config.start_on_boot())
                    .label("Start rewynd when I log in")
                    .on_toggle(Message::StartOnBoot)
                    .style(arena_check),
            );
        // Advanced sits at the bottom of the card (the per-start monitor prompt is a Linux-only,
        // rarely-used detail).
        #[cfg(target_os = "linux")]
        let output_capture = {
            let mut oc = output_capture.push(disclosure(
                self.capture_advanced_open,
                Message::CaptureAdvancedToggled,
            ));
            if self.capture_advanced_open {
                // No on_toggle when desktop capture is off: the prompt has no effect there, so the
                // checkbox is greyed out and unclickable rather than silently doing nothing.
                let mut monitor = checkbox(self.config.always_prompt())
                    .label("Ask which monitor to record every time rewynd starts")
                    .style(arena_check);
                if capture_desktop {
                    monitor = monitor.on_toggle(Message::AlwaysPrompt);
                }
                oc = oc.push(
                    column![
                        monitor,
                        hint(
                            "Only applies to desktop capture: rewynd asks which monitor to grab \
                             each time it starts, instead of reusing your last pick.",
                        ),
                    ]
                    .spacing(6),
                );
            }
            oc
        };
        let output = card("OUTPUT & CAPTURE", output_capture);

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

        // The GANKED.TV card is just the account state now: login covers the common case and turns
        // uploads on by itself. Visibility is chosen per clip at upload time, and the self-hosting
        // fields moved to the generic CUSTOM CONNECTOR card at the bottom. Pinned to the YouTube
        // card's height so the two connectors sit symmetric side by side (ganked.tv has no advanced
        // settings, so it is otherwise the shorter of the pair).
        let upload = card_fixed(
            "GANKED.TV",
            CONNECTOR_CARD_HEIGHT,
            column![
                account,
                hint("Clips are sent from your library, with the visibility you pick per clip."),
            ]
            .spacing(18),
        );

        // YouTube account area, mirroring the ganked.tv login. Google grants no username with
        // the upload-only scope, so connectedness is simply "a refresh token is stored".
        let yt_account: Element<Message> = if !self.config.youtube_refresh_token().trim().is_empty()
        {
            row![
                container(text("CONNECTED TO YOUTUBE").size(10).font(UI_BOLD).style(
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
                    .on_press(Message::YtLogoutPressed)
                    .style(secondary_button)
                    .padding([7, 14]),
            ]
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            match &self.yt_login {
                YtLoginState::Idle => column![
                    yt_login_button(),
                    hint("Approve rewynd in your browser; only permission to upload is asked."),
                ]
                .spacing(6)
                .into(),
                YtLoginState::Starting { .. } => column![
                    text("Preparing the Google login...").size(13),
                    button(text("Cancel").size(11).font(UI_SEMIBOLD))
                        .on_press(Message::YtLoginCancelled)
                        .style(secondary_button)
                        .padding([6, 14]),
                ]
                .spacing(7)
                .into(),
                YtLoginState::Waiting { .. } => column![
                    text("Approve rewynd in the browser tab that just opened.").size(13),
                    row![
                        button(text("Open the page again").size(11).font(UI_SEMIBOLD))
                            .on_press(Message::YtOpenAuthUrl)
                            .style(link_button)
                            .padding(0),
                        button(text("Cancel").size(11).font(UI_SEMIBOLD))
                            .on_press(Message::YtLoginCancelled)
                            .style(secondary_button)
                            .padding([6, 14]),
                    ]
                    .spacing(14)
                    .align_y(iced::Alignment::Center),
                ]
                .spacing(7)
                .into(),
                YtLoginState::Failed(e) => column![
                    yt_login_button(),
                    text(e.clone()).size(12).style(tinted(palette::DANGER)),
                ]
                .spacing(6)
                .into(),
            }
        };
        // Login turns uploads on by itself, so the YouTube card is the account state plus the
        // Google-only OAuth advanced fields. Clips are sent from the library, per clip.
        let mut youtube_items: Vec<Element<Message>> = vec![
            yt_account,
            hint(
                "Clips are sent from your library, with the visibility you pick per clip. \
                 YouTube's built-in quota allows only a handful of uploads per day.",
            ),
            button(
                text(if self.yt_advanced_open {
                    "Hide advanced options"
                } else {
                    "Advanced options"
                })
                .size(11)
                .font(UI_SEMIBOLD),
            )
            .on_press(Message::YtAdvancedToggled)
            .style(link_button)
            .padding(0)
            .into(),
        ];
        if self.yt_advanced_open {
            youtube_items.extend([
                field(
                    "OAuth client ID",
                    text_input("...apps.googleusercontent.com", &self.yt_client_id)
                        .on_input(Message::YtClientIdEdited)
                        .style(arena_input),
                )
                .into(),
                field(
                    "OAuth client secret",
                    text_input("GOCSPX-...", &self.yt_client_secret)
                        .secure(true)
                        .on_input(Message::YtClientSecretEdited)
                        .style(arena_input),
                )
                .push(hint(
                    "Leave both empty for the built-in client. Bring your own Google Cloud \
                     OAuth client (Desktop app) for a private upload quota.",
                ))
                .into(),
            ]);
        }
        let youtube = card("YOUTUBE", column(youtube_items).spacing(18));

        // A generic escape hatch for pointing rewynd at your own upload server (a self-hosted or
        // otherwise compatible service) instead of the built-in one. Deliberately plain and
        // collapsed by default — a normal user never needs it, so it stays out of the way.
        let mut connector_items: Vec<Element<Message>> = vec![
            hint(
                "Advanced: send clips to your own upload server instead of the built-in one. \
                 Leave this alone unless you run a compatible service.",
            ),
            disclosure(self.advanced_open, Message::AdvancedToggled),
        ];
        if self.advanced_open {
            connector_items.extend([
                field(
                    "API server",
                    text_input("https://api.example.com", &self.api_url)
                        .on_input(Message::ApiUrlEdited)
                        .style(arena_input),
                )
                .into(),
                field(
                    "Share links",
                    text_input("https://example.com", &self.share_url)
                        .on_input(Message::ShareUrlEdited)
                        .style(arena_input),
                )
                .push(hint("Leave both empty to use the built-in server."))
                .into(),
                field(
                    "API key",
                    text_input("API key", &self.api_key)
                        .secure(true)
                        .on_input(Message::ApiKeyEdited)
                        .style(arena_input),
                )
                .push(hint("Authenticates rewynd with your server."))
                .into(),
            ]);
        }
        let connector = card("CUSTOM CONNECTOR", column(connector_items).spacing(18));

        let header = column![
            logo(46.0),
            text("SETTINGS").size(32).font(DISPLAY_BLACK),
            hint(
                "Most changes apply after Save + Restart rewynd now; the ganked.tv upload \
                 settings apply immediately.",
            ),
            button(text("Run setup again").size(12).font(UI_SEMIBOLD))
                .on_press(Message::RerunOnboarding)
                .style(link_button)
                .padding(0),
        ]
        .spacing(6)
        .align_x(iced::Alignment::Center)
        .width(Length::Fill);

        // Save and Restart side by side: stacked below the fold, the restart button fell
        // outside small windows' viewport and went unseen — with stale settings running on.
        let mut save_row = row![
            button(text("Save settings").size(13).font(UI_BOLD))
                .on_press(Message::Save)
                .style(primary_button)
                .padding([13, 28]),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);
        // Offer a one-click restart once a save actually landed (a failed save has nothing to
        // apply; a failed restart may be retried). Hidden while a restart is in flight and
        // once it succeeded — it reappears on the next save.
        if self.last_save_ok && !matches!(self.status, Status::Restarting | Status::Restarted) {
            save_row = save_row.push(
                button(text("Restart rewynd now").size(12).font(UI_SEMIBOLD))
                    .on_press(Message::Restart)
                    .padding([13, 22])
                    .style(secondary_button),
            );
        }
        let save = container(
            column![save_row, status_line(&self.status)]
                .spacing(10)
                .align_x(iced::Alignment::Center),
        )
        .center_x(Length::Fill);

        // Two columns so everything fits in a landscape window without scrolling: Recording and
        // Audio on the left, Output on the right.
        let columns = row![
            column![recording, audio].spacing(20).width(Length::Fill),
            column![output].spacing(20).width(Length::Fill),
        ]
        .spacing(20);

        // The two upload connectors sit side by side, each column filling half the width. GANKED.TV
        // is pinned to the YouTube card's collapsed height (see CONNECTOR_CARD_HEIGHT) so the pair
        // stays symmetric; opening YouTube's Advanced options grows only that card, which is fine.
        let connectors = row![
            column![upload].width(Length::Fill),
            column![youtube].width(Length::Fill),
        ]
        .spacing(20);

        let body = column![header, columns, connectors, connector, save]
            .spacing(20)
            .padding(28)
            .max_width(CONTENT_MAX_WIDTH);

        // The scrollable is a safety net for small windows and the opened Advanced disclosure;
        // the default window size is chosen (by eye — iced has no pre-layout measure) so the
        // collapsed form fits without it. The body caps at CONTENT_MAX_WIDTH, so on a wider window
        // center it rather than leaving the slack on one side. Collapsing a disclosure keeps the
        // current scroll offset (clamped to the shorter form) instead of jumping the view to the top.
        container(scroll::smooth(scrollable(
            container(body).center_x(Length::Fill),
        )))
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

/// An "Advanced options" disclosure toggle, link-styled and labelled by its open state.
fn disclosure(open: bool, msg: Message) -> Element<'static, Message> {
    button(
        text(if open {
            "Hide advanced options"
        } else {
            "Advanced options"
        })
        .size(11)
        .font(UI_SEMIBOLD),
    )
    .on_press(msg)
    .style(link_button)
    .padding(0)
    .into()
}

/// The brand mark + wordmark as a home button: clicking it returns to the library, the way a
/// logo doubles as "go to the front page" on most sites.
fn brand_home() -> Element<'static, Message> {
    button(
        row![logo(23.0), text("REWYND").size(17).font(DISPLAY_BLACK)]
            .spacing(10)
            .align_y(iced::Alignment::Center),
    )
    .on_press(Message::Tab(View::Library))
    .style(|_: &Theme, status| button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => palette::ACCENT,
            _ => palette::TEXT,
        },
        ..button::Style::default()
    })
    .padding(0)
    .into()
}

/// The left navigation sidebar, Arena style: the brand at the top, the Library/Settings nav items
/// below (accent-on-tint pill when active), and the recorder status + version pinned to the
/// bottom, on a raised panel with a hairline right edge dividing it from the content.
fn sidebar(
    active: View,
    recorder_status: Option<&config::RecorderStatus>,
    is_velopack: bool,
    update: &UpdateState,
) -> Element<'static, Message> {
    // Full-width nav items: the active one gets the accent pill, matching the old top-bar links.
    // The label fills the button so the text sits left, sidebar-style, and the pill spans the row.
    let item = |label: &'static str, view: View| {
        let is_active = view == active;
        button(text(label).size(13).font(UI_SEMIBOLD).width(Length::Fill))
            .on_press(Message::Tab(view))
            .width(Length::Fill)
            .style(move |_: &Theme, status| nav_link(is_active, status))
            .padding([9, 12])
    };
    let (pill_label, pill_dot) = status_pill_parts(recorder_status);
    let inner = column![
        brand_home(),
        column![
            item("Library", View::Library),
            item("Settings", View::Settings),
        ]
        .spacing(4),
        // Push the status + version block to the bottom of the sidebar.
        iced::widget::Space::new().height(Length::Fill),
        column![
            status_pill(pill_label, pill_dot),
            sidebar_updates(is_velopack, update),
        ]
        .spacing(12),
    ]
    .spacing(22)
    .height(Length::Fill);
    let panel = container(inner)
        .width(Length::Fixed(SIDEBAR_WIDTH))
        .height(Length::Fill)
        .padding([22, 18])
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(palette::PANEL)),
            ..container::Style::default()
        });
    // A hairline on the sidebar's right edge divides it from the content. A container border rings
    // all four sides, so the single edge is a 1px-wide filled column instead.
    let divider = container(iced::widget::Space::new().width(1))
        .height(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(palette::BORDER)),
            ..container::Style::default()
        });
    row![panel, divider].height(Length::Fill).into()
}

/// The sidebar's bottom block below the status pill: the running version, plus (only in a Velopack
/// install) a button to check for and apply an update, stacked for the narrow sidebar column.
fn sidebar_updates(is_velopack: bool, update: &UpdateState) -> Element<'static, Message> {
    let version = text(concat!("v", env!("CARGO_PKG_VERSION")))
        .size(11)
        .style(tinted(palette::TEXT_SECONDARY));
    if !is_velopack {
        return version.into();
    }
    let working = matches!(update, UpdateState::Working);
    let label = if working {
        "Checking…"
    } else {
        "Check for updates"
    };
    let mut check = button(text(label).size(11).font(UI_SEMIBOLD).width(Length::Fill))
        .padding([6, 10])
        .width(Length::Fill)
        .style(move |_: &Theme, status| nav_link(false, status));
    // No on_press while working leaves the button disabled (greyed, no re-entrancy).
    if !working {
        check = check.on_press(Message::CheckForUpdates);
    }
    let mut items = column![].spacing(8);
    match update {
        UpdateState::UpToDate => {
            items = items.push(text("Up to date").size(11).style(tinted(palette::ACCENT)));
        }
        UpdateState::Failed(e) => {
            items = items.push(text(e.clone()).size(11).style(tinted(palette::DANGER)));
        }
        _ => {}
    }
    items.push(check).push(version).into()
}

/// A nav link: mint text on the mint tint when active, quiet otherwise, 7px pill.
fn nav_link(active: bool, status: button::Status) -> button::Style {
    let (background, text_color) = if active {
        (Some(Background::Color(palette::ACCENT_BG)), palette::ACCENT)
    } else {
        match status {
            button::Status::Hovered | button::Status::Pressed => (None, palette::ACCENT),
            _ => (None, palette::TEXT_SECONDARY),
        }
    };
    button::Style {
        background,
        text_color,
        border: Border {
            radius: 7.0.into(),
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

/// The "Log in with YouTube" button: same OAuth sign-in shell, label only (no third-party
/// logo asset is shipped).
fn yt_login_button<'a>() -> Element<'a, Message> {
    button(text("Log in with YouTube").size(13).font(UI_SEMIBOLD))
        .on_press(Message::YtLoginPressed)
        .style(oauth_button)
        .padding([10, 22])
        .into()
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
    config::sibling_binary("rewynd-recorder")
}

/// Ask the (sibling) recorder to enumerate the machine's encoders (`--probe-encoders`). Blocking
/// — runs on a `spawn_blocking` thread. A non-zero exit or unparseable output is an error, and
/// the picker falls back to just Automatic + CPU.
fn probe_encoders_via_recorder() -> Result<config::EncoderProbe, String> {
    let recorder =
        recorder_path().ok_or_else(|| "could not locate the recorder binary".to_owned())?;
    let output = std::process::Command::new(&recorder)
        .arg("--probe-encoders")
        .output()
        .map_err(|e| format!("could not run the encoder probe: {e}"))?;
    if !output.status.success() {
        return Err(format!("the encoder probe exited with {}", output.status));
    }
    config::EncoderProbe::from_json(String::from_utf8_lossy(&output.stdout).trim())
}

/// The GitHub repo whose Releases are the update feed.
const UPDATE_REPO: &str = "https://github.com/gankedtv/rewynd";

/// The update feed. Prereleases are offered only to a prerelease build, so a `1.0.0-beta.N`
/// friend build tracks the beta line and later rolls into the `1.0.0` GA, while a stable build
/// ignores future betas. Anonymous (no token): the public GitHub API is 60 req/h/IP, and one
/// check per button press is nothing.
fn update_source() -> velopack::sources::GithubSource {
    velopack::sources::GithubSource::new(UPDATE_REPO, None, env!("CARGO_PKG_VERSION").contains('-'))
}

/// True only in a real Velopack install (a receipt exists). Dev/cargo runs and package-manager
/// installs return false — `UpdateManager::new` errors without a receipt — which hides the update
/// affordance so those installs never try to self-update.
fn is_velopack_install() -> bool {
    velopack::UpdateManager::new(update_source(), None, None).is_ok()
}

/// Check → download → stop the recorder → apply and restart. Blocking; runs on a spawn_blocking
/// thread. A successful apply replaces this process, so this only returns for "up to date" or an
/// error. The recorder is stopped first because Velopack force-kills any process in the install
/// dir mid-apply, and a recorder mid-MP4-write must never die that way.
fn run_update_flow() -> Result<(), String> {
    let um =
        velopack::UpdateManager::new(update_source(), None, None).map_err(|e| e.to_string())?;
    if let velopack::UpdateCheck::UpdateAvailable(info) =
        um.check_for_updates().map_err(|e| e.to_string())?
    {
        um.download_updates(&info, None)
            .map_err(|e| e.to_string())?;
        // Stop the recorder first and confirm it actually exited: Velopack force-kills any
        // process left in the install dir mid-apply, and a recorder mid-MP4-write must never die
        // that way. If it will not stop, abort rather than risk a corrupt clip.
        match config::stop_recorder(
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(2),
        ) {
            Ok(true) => {}
            Ok(false) => {
                return Err(
                    "the recorder is still running; close it and try updating again".to_owned(),
                );
            }
            Err(e) => return Err(format!("could not stop the recorder before updating: {e}")),
        }
        um.apply_updates_and_restart(&*info)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Launch a detached recorder — used to bring it back after an update relaunch. The child is
/// reaped on a background thread so it never lingers as a zombie that would confuse the next stop.
/// NOTE: inside an AppImage both binaries live in a per-invocation mount that is torn down when
/// this GUI exits; the recorder's own AppImage lifecycle is verified on the dev box (release plan).
fn spawn_recorder_detached() {
    if let Some(rec) = recorder_path().filter(|p| p.is_file())
        && let Ok(mut child) = std::process::Command::new(rec).spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
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

    fn probe(adapters: &[(&str, bool)]) -> config::EncoderProbe {
        config::EncoderProbe::new(
            adapters
                .iter()
                .map(|(name, h264)| config::ProbeAdapter {
                    name: (*name).to_owned(),
                    device_type: "discrete".to_owned(),
                    h264_encode: *h264,
                    max_width: 4096,
                    max_height: 4096,
                })
                .collect(),
        )
    }

    #[test]
    fn encoder_options_lists_auto_capable_gpus_and_cpu() {
        let p = probe(&[("RTX 3080 Ti", true), ("iGPU", false)]);
        let opts = encoder_options(Some(&p), "auto");
        let values: Vec<&str> = opts.iter().map(|o| o.value.as_str()).collect();
        // Auto first, the encode-capable GPU, then CPU; the incapable iGPU is excluded.
        assert_eq!(values, ["auto", "gpu:RTX 3080 Ti", "cpu"]);
    }

    #[test]
    fn encoder_options_without_probe_are_auto_and_cpu() {
        let opts = encoder_options(None, "auto");
        let values: Vec<&str> = opts.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(values, ["auto", "cpu"]);
    }

    #[test]
    fn encoder_options_keep_a_pinned_but_missing_gpu_visible() {
        let p = probe(&[("RTX 3080 Ti", true)]);
        let opts = encoder_options(Some(&p), "gpu:Old Card");
        let missing = opts
            .iter()
            .find(|o| o.value == "gpu:Old Card")
            .expect("stored-but-missing GPU stays selectable");
        assert!(missing.label.contains("unavailable"));
    }

    #[test]
    fn status_pill_reflects_state() {
        use config::{RecorderState, RecorderStatus};
        let base = RecorderStatus {
            version: config::RECORDER_STATUS_VERSION,
            pid: 1,
            encoder: "cpu".to_owned(),
            state: RecorderState::Recording,
            game: Some("Hades II".to_owned()),
            detail: None,
        };
        assert_eq!(status_pill_parts(None).0, "Not recording");
        assert_eq!(status_pill_parts(Some(&base)).0, "Recording: Hades II");
        let desktop = RecorderStatus {
            game: None,
            ..base.clone()
        };
        assert_eq!(status_pill_parts(Some(&desktop)).0, "Recording: Desktop");
        let idle = RecorderStatus {
            state: RecorderState::Idle,
            ..base.clone()
        };
        assert_eq!(status_pill_parts(Some(&idle)).0, "Waiting for a game");
        let failed = RecorderStatus {
            state: RecorderState::Failed,
            ..base
        };
        assert_eq!(status_pill_parts(Some(&failed)).0, "Capture failed");
    }
}
