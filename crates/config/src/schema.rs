//! The config schema: section structs and defaults, the [`Config`] accessors/setters, and the
//! load/save plumbing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::paths::config_path;

/// Built-in video defaults (must match `rewynd_encode::EncodeParams::default`; the app guards it).
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
const DEFAULT_FRAMERATE: u32 = 60;
const DEFAULT_VIDEO_BITRATE_BPS: u32 = 12_000_000;
const DEFAULT_IDR_PERIOD: u32 = 60;
/// Built-in audio defaults (must match `rewynd_encode::AudioEncodeParams::default`).
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_CHANNELS: u32 = 2;
const DEFAULT_AUDIO_BITRATE_BPS: u32 = 128_000;
/// Default retention window in seconds. 30 s suits most clips; configurable up to the cap below.
const DEFAULT_BUFFER_SECONDS: u64 = 30;
/// Upper bound on the retention window (two minutes). The ring buffer holds encoded frames in
/// memory, so the window is capped: two minutes is already a generous instant-replay buffer, and a
/// bound keeps a fat-fingered `seconds` from growing it without limit. The settings UI offers the
/// same ceiling, so the slider and the daemon agree (and the 30 s default sits about a quarter of
/// the way along the slider).
pub const MAX_BUFFER_SECONDS: u64 = 120;
/// Default preferred global-shortcut trigger; the compositor may rebind it.
pub const DEFAULT_HOTKEY_TRIGGER: &str = "CTRL+ALT+R";
/// Default ganked.tv API base for uploads.
pub const DEFAULT_UPLOAD_API_URL: &str = "https://api.ganked.tv";
/// Default base for share links (`<share>/c/<code>`).
pub const DEFAULT_UPLOAD_SHARE_URL: &str = "https://ganked.tv";

/// The sample rates libopus accepts as input.
const OPUS_SAMPLE_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

/// Plain video encode settings (the consumer maps these onto its encoder param type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_bps: u32,
    pub idr_period: u32,
}

/// Plain audio encode settings (the consumer maps these onto its encoder param type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSettings {
    pub sample_rate: u32,
    pub channels: u32,
    pub bitrate_bps: u32,
}

/// Plain ganked.tv upload settings (the consumer maps these onto its client).
#[derive(Clone, PartialEq, Eq)]
pub struct UploadSettings {
    /// Only true when uploads are switched on AND an API key is set.
    pub enabled: bool,
    pub api_url: String,
    pub share_url: String,
    pub api_key: String,
    /// `"public"`, `"unlisted"` or `"private"`. Consumers fail closed: anything else is treated
    /// as private, so a typo can never widen a clip's visibility.
    pub visibility: String,
}

// Manual Debug: the API key must never reach logs through an innocent `{:?}`.
impl std::fmt::Debug for UploadSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadSettings")
            .field("enabled", &self.enabled)
            .field("api_url", &self.api_url)
            .field("share_url", &self.share_url)
            .field("api_key", &"gtv_***")
            .field("visibility", &self.visibility)
            .finish()
    }
}

/// Plain YouTube upload settings (the consumer maps these onto its client).
#[derive(Clone, PartialEq, Eq)]
pub struct YouTubeSettings {
    /// Only true when YouTube uploads are switched on AND a refresh token is stored.
    pub enabled: bool,
    /// OAuth client id override; empty = the compiled-in default.
    pub client_id: String,
    /// OAuth client secret override; empty = the compiled-in default.
    pub client_secret: String,
    pub refresh_token: String,
    /// `"public"` or `"unlisted"`; empty falls back to the `[upload]` visibility. Consumers
    /// fail closed: anything unrecognized is treated as unlisted.
    pub visibility: String,
}

// Manual Debug: the refresh token (and the nominally-secret client secret) must never reach
// logs through an innocent `{:?}`.
impl std::fmt::Debug for YouTubeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YouTubeSettings")
            .field("enabled", &self.enabled)
            .field("client_id", &self.client_id)
            .field("client_secret", &"***")
            .field("refresh_token", &"***")
            .field("visibility", &self.visibility)
            .finish()
    }
}

/// Video encode settings as parsed from TOML, defaulting to the built-ins.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct VideoConfig {
    width: u32,
    height: u32,
    framerate: u32,
    bitrate_bps: u32,
    idr_period: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            framerate: DEFAULT_FRAMERATE,
            bitrate_bps: DEFAULT_VIDEO_BITRATE_BPS,
            idr_period: DEFAULT_IDR_PERIOD,
        }
    }
}

/// Audio settings: the Opus encode params plus per-source linear mix gains.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AudioConfig {
    sample_rate: u32,
    channels: u32,
    bitrate_bps: u32,
    /// Linear gain applied to the microphone before mixing (raise for a quiet mic).
    mic_gain: f32,
    /// Linear gain applied to system audio before mixing.
    system_gain: f32,
    /// Capture this microphone instead of the system default. Empty = default. Matched
    /// case-insensitively against the device's name (a substring is enough on Windows;
    /// the PipeWire node name on Linux).
    microphone: String,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
            bitrate_bps: DEFAULT_AUDIO_BITRATE_BPS,
            mic_gain: 1.0,
            system_gain: 1.0,
            microphone: String::new(),
        }
    }
}

/// Ring-buffer retention.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BufferConfig {
    seconds: u64,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            seconds: DEFAULT_BUFFER_SECONDS,
        }
    }
}

/// Where saved clips are written (`None` → the caller's default, e.g. the temp dir).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OutputConfig {
    // Omitted from the serialized file when unset (TOML has no null).
    #[serde(skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
    /// Sort clips into a per-game subfolder (ShadowPlay-style) when game detection
    /// knows which game the buffer holds.
    game_folders: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            directory: None,
            game_folders: true,
        }
    }
}

/// Global-shortcut preference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HotkeyConfig {
    trigger: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            trigger: DEFAULT_HOTKEY_TRIGGER.to_owned(),
        }
    }
}

/// Capture options.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CaptureConfig {
    /// Re-show the ScreenCast monitor picker each launch (ignore the saved restore token),
    /// so a different monitor can be chosen; `false` reuses the saved selection.
    always_prompt: bool,
    /// Capture the whole desktop instead of only the active game. Off by default:
    /// recording everything can catch private content, and game-only is what an
    /// instant-replay tool is expected to do. Windows targets the game window itself;
    /// Linux keeps the portal's monitor stream but only fills the buffer while a
    /// fullscreen game is focused.
    desktop: bool,
}

/// Desktop-session startup behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StartupConfig {
    /// Start the recorder automatically at login (an XDG autostart entry manages this).
    on_boot: bool,
}

/// ganked.tv upload settings. `api_key` is a secret — `save_to` tightens the file mode for it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct UploadConfig {
    enabled: bool,
    api_url: String,
    share_url: String,
    api_key: String,
    visibility: String,
}

// Manual Debug (also covering Config's derived Debug): the key must never reach logs.
impl std::fmt::Debug for UploadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadConfig")
            .field("enabled", &self.enabled)
            .field("api_url", &self.api_url)
            .field("share_url", &self.share_url)
            .field("api_key", &"gtv_***")
            .field("visibility", &self.visibility)
            .finish()
    }
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_url: DEFAULT_UPLOAD_API_URL.to_owned(),
            share_url: DEFAULT_UPLOAD_SHARE_URL.to_owned(),
            api_key: String::new(),
            visibility: "unlisted".to_owned(),
        }
    }
}

/// YouTube upload settings. `refresh_token` and `client_secret` are secrets — `save_to`
/// tightens the file mode for them (as it already does for the ganked.tv key).
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct YouTubeConfig {
    enabled: bool,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    visibility: String,
}

// Manual Debug (also covering Config's derived Debug): the secrets must never reach logs.
impl std::fmt::Debug for YouTubeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YouTubeConfig")
            .field("enabled", &self.enabled)
            .field("client_id", &self.client_id)
            .field("client_secret", &"***")
            .field("refresh_token", &"***")
            .field("visibility", &self.visibility)
            .finish()
    }
}

/// The parsed, layered configuration. Build it with [`load`]; read it through the accessors.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    video: VideoConfig,
    audio: AudioConfig,
    buffer: BufferConfig,
    output: OutputConfig,
    hotkey: HotkeyConfig,
    capture: CaptureConfig,
    startup: StartupConfig,
    upload: UploadConfig,
    youtube: YouTubeConfig,
}

impl Config {
    /// Parse a config from TOML text; missing fields fall back to the built-in defaults.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Apply `REWYND_*` environment overrides (highest precedence). A non-positive or
    /// unparseable numeric override is ignored, falling back to the config/default value.
    fn apply_env_overrides(&mut self, get: impl Fn(&str) -> Option<String>) {
        let u32_of = |key: &str| {
            get(key)
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|&v| v > 0)
        };
        if let Some(v) = u32_of("REWYND_WIDTH") {
            self.video.width = v;
        }
        if let Some(v) = u32_of("REWYND_HEIGHT") {
            self.video.height = v;
        }
        if let Some(v) = u32_of("REWYND_FPS") {
            self.video.framerate = v;
        }
        if let Some(v) = u32_of("REWYND_BITRATE_BPS") {
            self.video.bitrate_bps = v;
        }
        if let Some(v) = u32_of("REWYND_IDR_PERIOD") {
            self.video.idr_period = v;
        }
        if let Some(v) = u32_of("REWYND_AUDIO_BITRATE_BPS") {
            self.audio.bitrate_bps = v;
        }
        if let Some(mic) = get("REWYND_MICROPHONE") {
            self.audio.microphone = mic;
        }
        if let Some(dir) = get("REWYND_OUTPUT_DIR").filter(|s| !s.is_empty()) {
            self.output.directory = Some(dir);
        }
    }

    /// The video encode settings, sanitized to encodable values: dimensions clamped to
    /// `[16, 7680]` and rounded down to even (4:2:0 subsampling), framerate `[1, 240]`, bitrate
    /// `[100 kbps, 100 Mbps]`, IDR period `[1, 1000]`. The stored values stay untouched, so an
    /// editor round-trip doesn't rewrite the file.
    #[must_use]
    pub fn video(&self) -> VideoSettings {
        let raw = VideoSettings {
            width: self.video.width,
            height: self.video.height,
            framerate: self.video.framerate,
            bitrate_bps: self.video.bitrate_bps,
            idr_period: self.video.idr_period,
        };
        let sanitized = VideoSettings {
            width: raw.width.clamp(16, 7680) & !1,
            height: raw.height.clamp(16, 7680) & !1,
            framerate: raw.framerate.clamp(1, 240),
            bitrate_bps: raw.bitrate_bps.clamp(100_000, 100_000_000),
            idr_period: raw.idr_period.clamp(1, 1000),
        };
        if sanitized != raw {
            tracing::warn!(
                ?raw,
                ?sanitized,
                "video settings adjusted to encodable values"
            );
        }
        sanitized
    }

    /// The audio encode settings, sanitized to what Opus accepts: sample rate snapped to the
    /// nearest supported rate, channels clamped to `[1, 2]`, bitrate to `[6, 510] kbps`. The
    /// stored values stay untouched, so an editor round-trip doesn't rewrite the file.
    #[must_use]
    pub fn audio(&self) -> AudioSettings {
        let raw = AudioSettings {
            sample_rate: self.audio.sample_rate,
            channels: self.audio.channels,
            bitrate_bps: self.audio.bitrate_bps,
        };
        let sanitized = AudioSettings {
            sample_rate: OPUS_SAMPLE_RATES
                .into_iter()
                .min_by_key(|r| r.abs_diff(raw.sample_rate))
                .unwrap_or(DEFAULT_SAMPLE_RATE),
            channels: raw.channels.clamp(1, 2),
            bitrate_bps: raw.bitrate_bps.clamp(6_000, 510_000),
        };
        if sanitized != raw {
            tracing::warn!(
                ?raw,
                ?sanitized,
                "audio settings adjusted to Opus-valid values"
            );
        }
        sanitized
    }

    /// The video settings exactly as stored (no sanitizing) — for the settings editor, so an
    /// edit-and-save round-trip never rewrites values the daemon merely clamps at use.
    #[must_use]
    pub fn video_stored(&self) -> VideoSettings {
        VideoSettings {
            width: self.video.width,
            height: self.video.height,
            framerate: self.video.framerate,
            bitrate_bps: self.video.bitrate_bps,
            idr_period: self.video.idr_period,
        }
    }

    /// The audio settings exactly as stored (no sanitizing) — the editor-side twin of
    /// [`video_stored`](Self::video_stored).
    #[must_use]
    pub fn audio_stored(&self) -> AudioSettings {
        AudioSettings {
            sample_rate: self.audio.sample_rate,
            channels: self.audio.channels,
            bitrate_bps: self.audio.bitrate_bps,
        }
    }

    /// Linear gain for the microphone before mixing (sanitized to a finite, non-negative value).
    #[must_use]
    pub fn mic_gain(&self) -> f32 {
        sanitize_gain(self.audio.mic_gain)
    }

    /// Linear gain for system audio before mixing.
    #[must_use]
    pub fn system_gain(&self) -> f32 {
        sanitize_gain(self.audio.system_gain)
    }

    /// The microphone to capture instead of the system default, if one is configured
    /// (trimmed; empty = default).
    #[must_use]
    pub fn microphone(&self) -> Option<&str> {
        let mic = self.audio.microphone.trim();
        (!mic.is_empty()).then_some(mic)
    }

    /// Retention window, clamped to `[1, MAX_BUFFER_SECONDS]`: a zero window would keep
    /// nothing, and an unbounded one would grow the in-memory ring buffer until it OOMs.
    #[must_use]
    pub fn buffer_window(&self) -> Duration {
        Duration::from_secs(self.buffer.seconds.clamp(1, MAX_BUFFER_SECONDS))
    }

    /// The configured output directory, if any (else the caller picks a default).
    #[must_use]
    pub fn output_dir(&self) -> Option<PathBuf> {
        self.output
            .directory
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    }

    /// The preferred global-shortcut trigger hint (trimmed; never empty).
    #[must_use]
    pub fn hotkey_trigger(&self) -> &str {
        non_empty_or(&self.hotkey.trigger, DEFAULT_HOTKEY_TRIGGER)
    }

    /// Whether to re-show the ScreenCast monitor picker each launch.
    #[must_use]
    pub fn always_prompt(&self) -> bool {
        self.capture.always_prompt
    }

    /// Whether to capture the whole desktop instead of only the active game.
    #[must_use]
    pub fn capture_desktop(&self) -> bool {
        self.capture.desktop
    }

    /// Whether the recorder should start automatically at login.
    #[must_use]
    pub fn start_on_boot(&self) -> bool {
        self.startup.on_boot
    }

    /// Set whether the recorder starts automatically at login.
    pub fn set_start_on_boot(&mut self, on_boot: bool) {
        self.startup.on_boot = on_boot;
    }

    /// The validated upload settings: `enabled` requires an API key (fail closed), and empty
    /// URLs fall back to the ganked.tv defaults.
    #[must_use]
    pub fn upload(&self) -> UploadSettings {
        let key = self.upload.api_key.trim();
        UploadSettings {
            enabled: self.upload.enabled && !key.is_empty(),
            api_url: non_empty_or(&self.upload.api_url, DEFAULT_UPLOAD_API_URL).to_owned(),
            share_url: non_empty_or(&self.upload.share_url, DEFAULT_UPLOAD_SHARE_URL).to_owned(),
            api_key: key.to_owned(),
            visibility: self.upload.visibility.clone(),
        }
    }

    /// The validated YouTube settings: `enabled` requires a refresh token (fail closed), and an
    /// empty visibility falls back to the shared `[upload]` default.
    #[must_use]
    pub fn youtube(&self) -> YouTubeSettings {
        let token = self.youtube.refresh_token.trim();
        YouTubeSettings {
            enabled: self.youtube.enabled && !token.is_empty(),
            client_id: self.youtube.client_id.trim().to_owned(),
            client_secret: self.youtube.client_secret.trim().to_owned(),
            refresh_token: token.to_owned(),
            visibility: non_empty_or(&self.youtube.visibility, &self.upload.visibility).to_owned(),
        }
    }

    // --- Raw getters + setters for an editor (the settings UI). The getters return the
    // configured value as stored (unclamped/unfiltered) so a round-trip through the editor
    // doesn't silently rewrite it; the daemon should keep using the validating accessors above.

    /// The configured retention window in seconds, as stored (before [`buffer_window`]'s clamp).
    #[must_use]
    pub fn buffer_seconds(&self) -> u64 {
        self.buffer.seconds
    }

    /// The configured output directory string, as stored (before [`output_dir`]'s empty filter).
    #[must_use]
    pub fn output_directory(&self) -> Option<&str> {
        self.output.directory.as_deref()
    }

    /// Whether clips sort into per-game subfolders when the running game is known.
    #[must_use]
    pub fn game_folders(&self) -> bool {
        self.output.game_folders
    }

    /// The upload toggle as stored (before [`upload`]'s has-a-key requirement).
    #[must_use]
    pub fn upload_enabled(&self) -> bool {
        self.upload.enabled
    }

    /// The API key as stored.
    #[must_use]
    pub fn upload_api_key(&self) -> &str {
        &self.upload.api_key
    }

    /// The API base URL as stored (before [`upload`]'s empty fallback).
    #[must_use]
    pub fn upload_api_url(&self) -> &str {
        &self.upload.api_url
    }

    /// The share-link base URL as stored (before [`upload`]'s empty fallback).
    #[must_use]
    pub fn upload_share_url(&self) -> &str {
        &self.upload.share_url
    }

    /// The visibility string as stored.
    #[must_use]
    pub fn upload_visibility(&self) -> &str {
        &self.upload.visibility
    }

    /// The YouTube toggle as stored (before [`youtube`](Self::youtube)'s has-a-token requirement).
    #[must_use]
    pub fn youtube_enabled(&self) -> bool {
        self.youtube.enabled
    }

    /// The YouTube OAuth client id as stored (before the trim; empty = compiled-in default).
    #[must_use]
    pub fn youtube_client_id(&self) -> &str {
        &self.youtube.client_id
    }

    /// The YouTube OAuth client secret as stored.
    #[must_use]
    pub fn youtube_client_secret(&self) -> &str {
        &self.youtube.client_secret
    }

    /// The YouTube refresh token as stored.
    #[must_use]
    pub fn youtube_refresh_token(&self) -> &str {
        &self.youtube.refresh_token
    }

    /// The YouTube visibility string as stored (before the `[upload]` fallback).
    #[must_use]
    pub fn youtube_visibility(&self) -> &str {
        &self.youtube.visibility
    }

    /// Replace the video settings.
    pub fn set_video(&mut self, v: VideoSettings) {
        self.video = VideoConfig {
            width: v.width,
            height: v.height,
            framerate: v.framerate,
            bitrate_bps: v.bitrate_bps,
            idr_period: v.idr_period,
        };
    }

    /// Set the microphone mix gain.
    pub fn set_mic_gain(&mut self, gain: f32) {
        self.audio.mic_gain = gain;
    }

    /// Set the system-audio mix gain.
    pub fn set_system_gain(&mut self, gain: f32) {
        self.audio.system_gain = gain;
    }

    /// Set the microphone to capture (empty = the system default).
    pub fn set_microphone(&mut self, microphone: String) {
        self.audio.microphone = microphone;
    }

    /// Set the retention window in seconds (stored as-is; clamped on read by [`buffer_window`]).
    pub fn set_buffer_seconds(&mut self, seconds: u64) {
        self.buffer.seconds = seconds;
    }

    /// Set the output directory; an empty string clears it (back to the caller's default).
    pub fn set_output_directory(&mut self, dir: Option<String>) {
        self.output.directory = dir.filter(|s| !s.is_empty());
    }

    /// Set whether clips sort into per-game subfolders.
    pub fn set_game_folders(&mut self, on: bool) {
        self.output.game_folders = on;
    }

    /// Set the preferred global-shortcut trigger.
    pub fn set_hotkey_trigger(&mut self, trigger: String) {
        self.hotkey.trigger = trigger;
    }

    /// Set whether to re-show the monitor picker each launch.
    pub fn set_always_prompt(&mut self, always_prompt: bool) {
        self.capture.always_prompt = always_prompt;
    }

    /// Set whether to capture the whole desktop instead of only the active game.
    pub fn set_capture_desktop(&mut self, desktop: bool) {
        self.capture.desktop = desktop;
    }

    /// Switch uploads on/off (takes effect only once a key is set — see [`upload`]).
    pub fn set_upload_enabled(&mut self, enabled: bool) {
        self.upload.enabled = enabled;
    }

    /// Set the ganked.tv API key.
    pub fn set_upload_api_key(&mut self, key: String) {
        self.upload.api_key = key;
    }

    /// Set the API base URL; an empty string means "use the default".
    pub fn set_upload_api_url(&mut self, url: String) {
        self.upload.api_url = url;
    }

    /// Set the share-link base URL; an empty string means "use the default".
    pub fn set_upload_share_url(&mut self, url: String) {
        self.upload.share_url = url;
    }

    /// Set the upload visibility string.
    pub fn set_upload_visibility(&mut self, visibility: String) {
        self.upload.visibility = visibility;
    }

    /// Switch YouTube uploads on/off (takes effect only once a refresh token is stored).
    pub fn set_youtube_enabled(&mut self, enabled: bool) {
        self.youtube.enabled = enabled;
    }

    /// Set the YouTube OAuth client id; an empty string means "use the compiled-in default".
    pub fn set_youtube_client_id(&mut self, id: String) {
        self.youtube.client_id = id;
    }

    /// Set the YouTube OAuth client secret; an empty string means "use the compiled-in default".
    pub fn set_youtube_client_secret(&mut self, secret: String) {
        self.youtube.client_secret = secret;
    }

    /// Set the YouTube refresh token; an empty string logs out.
    pub fn set_youtube_refresh_token(&mut self, token: String) {
        self.youtube.refresh_token = token;
    }

    /// Set the YouTube visibility string; empty falls back to the `[upload]` visibility.
    pub fn set_youtube_visibility(&mut self, visibility: String) {
        self.youtube.visibility = visibility;
    }

    /// Serialize to a TOML string (the editor writes this back to the config file).
    pub fn to_toml_string(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Serialize and write the config to `path`, creating parent directories. The write is
    /// atomic (temp file + rename), so a crash or full disk can't leave a truncated config; the
    /// file may hold the upload API key, so on unix the temp is created 0600 and the rename
    /// carries that mode over any looser pre-existing file.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let toml = self
            .to_toml_string()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        create_parent_dirs(path)?;
        let tmp = path.with_extension("toml.tmp");
        // Drop any stale temp from a crashed save: `mode` below only applies at creation.
        let _ = std::fs::remove_file(&tmp);
        let result = secret_file_options()
            .open(&tmp)
            .and_then(|mut file| file.write_all(toml.as_bytes()))
            .and_then(|()| std::fs::rename(&tmp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// `value` trimmed, or `default` when the trim leaves nothing.
#[must_use]
pub fn non_empty_or<'a>(value: &'a str, default: &'a str) -> &'a str {
    let v = value.trim();
    if v.is_empty() { default } else { v }
}

/// Clamp a gain to a usable linear multiplier: non-finite or negative values fall back to unity.
fn sanitize_gain(g: f32) -> f32 {
    if g.is_finite() && g >= 0.0 { g } else { 1.0 }
}

/// Write-create-truncate options that create the file 0600 on unix (it may hold the API key).
fn secret_file_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Create `path`'s parent directories, 0700 on unix (the dir holds the key-bearing config).
fn create_parent_dirs(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    // A bare relative filename has an empty parent; creating "" would error.
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)
}

/// Read the config file at `path` (if any) and layer `REWYND_*` overrides via `get_env`. A
/// missing file falls back to the built-in defaults; a malformed one keeps whatever sections
/// still parse (logging why). The testable core of [`load`].
fn load_from(path: Option<&Path>, get_env: impl Fn(&str) -> Option<String>) -> Config {
    let mut config = match path {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => match Config::from_toml_str(&text) {
                Ok(c) => {
                    tracing::info!(path = %path.display(), "loaded config");
                    c
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "invalid config; salvaging valid sections");
                    salvage_sections(&text)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %path.display(), "no config file; using defaults");
                Config::default()
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not read config; using defaults");
                Config::default()
            }
        },
        None => Config::default(),
    };
    config.apply_env_overrides(get_env);
    config
}

/// Per-section salvage for a file that fails the strict parse: each known section that still
/// deserializes overrides the defaults; bad or unknown sections are logged and skipped. A file
/// that isn't even valid TOML degrades to all defaults.
fn salvage_sections(text: &str) -> Config {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Config::default();
    };
    let mut config = Config::default();
    for (name, value) in table {
        let result = match name.as_str() {
            "video" => value.try_into().map(|s| config.video = s),
            "audio" => value.try_into().map(|s| config.audio = s),
            "buffer" => value.try_into().map(|s| config.buffer = s),
            "output" => value.try_into().map(|s| config.output = s),
            "hotkey" => value.try_into().map(|s| config.hotkey = s),
            "capture" => value.try_into().map(|s| config.capture = s),
            "startup" => value.try_into().map(|s| config.startup = s),
            "upload" => value.try_into().map(|s| config.upload = s),
            "youtube" => value.try_into().map(|s| config.youtube = s),
            _ => {
                tracing::warn!(section = %name, "unknown config section ignored");
                continue;
            }
        };
        if let Err(e) = result {
            tracing::warn!(section = %name, error = %e, "invalid config section; using its defaults");
        }
    }
    config
}

/// Load configuration: read the config file, then apply `REWYND_*` environment overrides.
/// Never fails — a missing or bad config degrades to defaults rather than blocking startup.
#[must_use]
pub fn load() -> Config {
    load_from(config_path().as_deref(), |k| std::env::var(k).ok())
}

/// Load the config file's values **without** applying environment overrides — for an editor
/// (the settings UI) that reads, edits, and writes the file itself; env overrides are a runtime
/// concern that must not be baked back into the saved file.
#[must_use]
pub fn load_file() -> Config {
    load_from(config_path().as_deref(), |_| None)
}

/// A commented `config.toml` matching the built-in defaults, written on first run for
/// discoverability. Kept in sync with the defaults by the `default_template_matches_defaults` test.
pub const DEFAULT_TEMPLATE: &str = "\
# rewynd configuration. Values shown are the defaults; uncomment and edit to change.
# Precedence: these settings override the built-in defaults, and REWYND_* environment
# variables override the video/audio/output settings.

[video]
width = 1920
height = 1080
framerate = 60
bitrate_bps = 12000000
idr_period = 60

[audio]
sample_rate = 48000
channels = 2
bitrate_bps = 128000
# Linear gain applied before mixing. 1.0 = unchanged, 2.0 = +6 dB. Raise mic_gain if your
# microphone is quiet; the mix is clamped so it can't overflow.
mic_gain = 1.0
system_gain = 1.0
# Capture a specific microphone instead of the system default. Case-insensitive; on
# Windows a part of the device name is enough, on Linux use the PipeWire node name.
microphone = \"\"

[buffer]
# How many seconds of footage to keep for a clip.
seconds = 30

[output]
# Directory for saved clips. Unset = your Videos folder (or a private temp dir).
# directory = \"/home/you/Videos/rewynd\"
# Sort clips into a folder per game (like ShadowPlay) when the game is known.
game_folders = true

[hotkey]
# Preferred trigger; the desktop may let you rebind it in its shortcut settings.
trigger = \"CTRL+ALT+R\"

[capture]
# Re-show the monitor picker each launch (so you can pick a different screen).
always_prompt = false
# Record the whole desktop instead of only the active fullscreen game.
# Off keeps private windows out of clips.
desktop = false

[startup]
# Start rewynd automatically when you log in.
on_boot = false

[upload]
# Upload saved clips to ganked.tv from the tray (\"Upload last clip\"). Create an API key at
# ganked.tv/settings/api-keys and paste it here.
enabled = false
api_url = \"https://api.ganked.tv\"
share_url = \"https://ganked.tv\"
api_key = \"\"
# \"public\" (in feeds), \"unlisted\" (link only) or \"private\" (only you)
visibility = \"unlisted\"

[youtube]
# Upload saved clips to YouTube from the tray (\"Upload last clip to YouTube\"). The easy way
# is the settings window's \"Log in with YouTube\", which fills refresh_token for you.
enabled = false
# Override the built-in Google OAuth client (advanced; e.g. your own Google Cloud project).
client_id = \"\"
client_secret = \"\"
refresh_token = \"\"
# \"public\", \"unlisted\" or \"private\"; empty uses the [upload] visibility above.
visibility = \"\"
";

/// Write [`DEFAULT_TEMPLATE`] to `path`, creating parent directories (0700 on unix). The file is
/// created 0600 on unix: the template will later hold the API key once edited. The testable core
/// of [`ensure_default_file`].
fn write_default_file_at(path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    create_parent_dirs(path)?;
    secret_file_options()
        .open(path)?
        .write_all(DEFAULT_TEMPLATE.as_bytes())
}

/// Write [`DEFAULT_TEMPLATE`] to the config path if no file exists yet (best-effort; logs on
/// failure). Lets a first-time user discover the settings by opening the generated file.
pub fn ensure_default_file() {
    let Some(path) = config_path() else {
        return;
    };
    if path.exists() {
        return;
    }
    match write_default_file_at(&path) {
        Ok(()) => tracing::info!(path = %path.display(), "wrote default config"),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "could not write default config")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_is_all_defaults() {
        let c = Config::from_toml_str("").expect("empty parses");
        assert_eq!(c, Config::default());
        let v = c.video();
        assert_eq!((v.width, v.height, v.framerate), (1920, 1080, 60));
        assert_eq!((v.bitrate_bps, v.idr_period), (12_000_000, 60));
        let a = c.audio();
        assert_eq!(
            (a.sample_rate, a.channels, a.bitrate_bps),
            (48_000, 2, 128_000)
        );
        assert_eq!(c.buffer_window(), Duration::from_secs(30));
        assert_eq!(c.hotkey_trigger(), "CTRL+ALT+R");
        assert!(c.output_dir().is_none());
        assert!(!c.always_prompt());
        assert_eq!((c.mic_gain(), c.system_gain()), (1.0, 1.0));
    }

    #[test]
    fn default_template_matches_defaults() {
        let c = Config::from_toml_str(DEFAULT_TEMPLATE).expect("template parses");
        assert_eq!(
            c,
            Config::default(),
            "DEFAULT_TEMPLATE drifted from the defaults"
        );
    }

    #[test]
    fn partial_file_fills_missing_with_defaults() {
        let c = Config::from_toml_str("[video]\nwidth = 1280\nframerate = 30\n").expect("parses");
        let v = c.video();
        assert_eq!(v.width, 1280); // set
        assert_eq!(v.framerate, 30); // set
        assert_eq!(v.height, 1080); // default
        assert_eq!(v.bitrate_bps, 12_000_000); // default
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = Config::from_toml_str("[video]\nwidht = 1280\n").expect_err("typo rejected");
        assert!(
            err.to_string().contains("widht"),
            "error should name the bad key: {err}"
        );
    }

    #[test]
    fn reads_audio_gains_and_capture_and_output() {
        let c = Config::from_toml_str(
            "[audio]\nmic_gain = 2.5\nsystem_gain = 0.5\n\
             [output]\ndirectory = \"/tmp/clips\"\n\
             [capture]\nalways_prompt = true\n\
             [hotkey]\ntrigger = \"CTRL+ALT+K\"\n\
             [buffer]\nseconds = 30\n",
        )
        .expect("parses");
        assert_eq!(c.mic_gain(), 2.5);
        assert_eq!(c.system_gain(), 0.5);
        assert_eq!(c.output_dir(), Some(PathBuf::from("/tmp/clips")));
        assert!(c.always_prompt());
        assert_eq!(c.hotkey_trigger(), "CTRL+ALT+K");
        assert_eq!(c.buffer_window(), Duration::from_secs(30));
    }

    #[test]
    fn game_folders_defaults_on_and_round_trips() {
        let mut c = Config::default();
        assert!(c.game_folders(), "per-game folders default on");
        c.set_game_folders(false);
        let back = Config::from_toml_str(&c.to_toml_string().expect("serialize")).expect("reparse");
        assert!(!back.game_folders());
    }

    #[test]
    fn start_on_boot_round_trips() {
        let mut c = Config::default();
        assert!(!c.start_on_boot(), "off by default");
        c.set_start_on_boot(true);
        let back = Config::from_toml_str(&c.to_toml_string().expect("serialize")).expect("reparse");
        assert!(back.start_on_boot());
    }

    #[test]
    fn video_is_sanitized_to_encodable_values() {
        let c = Config::from_toml_str(
            "[video]\nwidth = 1921\nheight = 4\nframerate = 0\nbitrate_bps = 1\nidr_period = 100000\n",
        )
        .expect("parses");
        let v = c.video();
        assert_eq!(v.width, 1920, "odd width rounds down to even");
        assert_eq!(v.height, 16, "tiny height clamps up (and stays even)");
        assert_eq!(v.framerate, 1);
        assert_eq!(v.bitrate_bps, 100_000);
        assert_eq!(v.idr_period, 1000);
        // The stored values survive for the editor round-trip.
        let toml = c.to_toml_string().expect("serialize");
        assert!(toml.contains("width = 1921"));
        assert!(toml.contains("framerate = 0"));
    }

    #[test]
    fn audio_is_sanitized_to_opus_values() {
        let c = Config::from_toml_str(
            "[audio]\nsample_rate = 44100\nchannels = 6\nbitrate_bps = 1000000\n",
        )
        .expect("parses");
        let a = c.audio();
        assert_eq!(
            a.sample_rate, 48_000,
            "44.1 kHz snaps to the nearest Opus rate"
        );
        assert_eq!(a.channels, 2);
        assert_eq!(a.bitrate_bps, 510_000);
        assert!(
            c.to_toml_string()
                .expect("serialize")
                .contains("sample_rate = 44100"),
            "the stored rate survives for the editor round-trip"
        );

        let low =
            Config::from_toml_str("[audio]\nsample_rate = 4000\nchannels = 0\n").expect("parses");
        assert_eq!(
            low.audio().sample_rate,
            8_000,
            "snaps up to the lowest rate"
        );
        assert_eq!(low.audio().channels, 1);
    }

    #[test]
    fn non_empty_or_trims_and_falls_back() {
        assert_eq!(non_empty_or("  x  ", "d"), "x");
        assert_eq!(non_empty_or("   ", "d"), "d");
        assert_eq!(non_empty_or("", "d"), "d");
    }

    #[test]
    fn upload_defaults_and_key_requirement() {
        let c = Config::from_toml_str("").expect("parses");
        let u = c.upload();
        assert!(!u.enabled);
        assert_eq!(u.api_url, DEFAULT_UPLOAD_API_URL);
        assert_eq!(u.share_url, DEFAULT_UPLOAD_SHARE_URL);
        assert_eq!(u.visibility, "unlisted");

        // Enabled without a key stays disabled; empty URL falls back to the default.
        let c =
            Config::from_toml_str("[upload]\nenabled = true\napi_url = \"\"\n").expect("parses");
        assert!(!c.upload().enabled, "no key → not enabled");
        assert_eq!(c.upload().api_url, DEFAULT_UPLOAD_API_URL);

        let c = Config::from_toml_str(
            "[upload]\nenabled = true\napi_key = \" gtv_k \"\napi_url = \"http://localhost:5050/\"\nvisibility = \"unlisted\"\n",
        )
        .expect("parses");
        let u = c.upload();
        assert!(u.enabled);
        assert_eq!(u.api_key, "gtv_k", "key is trimmed");
        assert_eq!(u.api_url, "http://localhost:5050/");
        assert_eq!(u.visibility, "unlisted");
    }

    #[test]
    fn youtube_defaults_token_requirement_and_visibility_fallback() {
        let c = Config::from_toml_str("").expect("parses");
        let y = c.youtube();
        assert!(!y.enabled);
        assert_eq!(y.visibility, "public", "empty falls back to [upload]");

        // Enabled without a refresh token stays disabled.
        let c = Config::from_toml_str("[youtube]\nenabled = true\n").expect("parses");
        assert!(!c.youtube().enabled, "no token means not enabled");

        let c = Config::from_toml_str(
            "[upload]\nvisibility = \"unlisted\"\n\
             [youtube]\nenabled = true\nrefresh_token = \" rt_x \"\nclient_id = \" my.id \"\n",
        )
        .expect("parses");
        let y = c.youtube();
        assert!(y.enabled);
        assert_eq!(y.refresh_token, "rt_x", "token is trimmed");
        assert_eq!(y.client_id, "my.id", "client id is trimmed");
        assert_eq!(y.visibility, "unlisted", "falls back to the [upload] value");

        let own = Config::from_toml_str(
            "[upload]\nvisibility = \"unlisted\"\n[youtube]\nvisibility = \"public\"\n",
        )
        .expect("parses");
        assert_eq!(own.youtube().visibility, "public", "own value wins");
    }

    #[test]
    fn youtube_setters_round_trip() {
        let mut c = Config::default();
        c.set_youtube_enabled(true);
        c.set_youtube_client_id("id.apps.googleusercontent.com".to_owned());
        c.set_youtube_client_secret("cs".to_owned());
        c.set_youtube_refresh_token("rt".to_owned());
        c.set_youtube_visibility("unlisted".to_owned());
        let back = Config::from_toml_str(&c.to_toml_string().expect("serialize")).expect("reparse");
        assert_eq!(back, c);
        assert!(back.youtube_enabled());
        assert_eq!(back.youtube_client_id(), "id.apps.googleusercontent.com");
        assert_eq!(back.youtube_client_secret(), "cs");
        assert_eq!(back.youtube_refresh_token(), "rt");
        assert_eq!(back.youtube_visibility(), "unlisted");
    }

    #[test]
    fn youtube_secrets_are_redacted_in_debug() {
        let mut c = Config::default();
        c.set_youtube_client_secret("hushhush".to_owned());
        c.set_youtube_refresh_token("rt_hidden".to_owned());
        let dump = format!("{c:?} {:?}", c.youtube());
        assert!(
            !dump.contains("hushhush") && !dump.contains("rt_hidden"),
            "{dump}"
        );
    }

    #[test]
    fn upload_setters_round_trip() {
        let mut c = Config::default();
        c.set_upload_enabled(true);
        c.set_upload_api_key("gtv_abc".to_owned());
        c.set_upload_api_url("http://localhost:5050".to_owned());
        c.set_upload_share_url("http://localhost:5173".to_owned());
        c.set_upload_visibility("unlisted".to_owned());
        let back = Config::from_toml_str(&c.to_toml_string().expect("serialize")).expect("reparse");
        assert_eq!(back, c);
        assert!(back.upload_enabled());
        assert_eq!(back.upload_api_key(), "gtv_abc");
        assert_eq!(back.upload_api_url(), "http://localhost:5050");
        assert_eq!(back.upload_share_url(), "http://localhost:5173");
        assert_eq!(back.upload_visibility(), "unlisted");
    }

    #[cfg(unix)]
    #[test]
    fn save_to_tightens_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut c = Config::default();
        c.set_upload_api_key("gtv_secret".to_owned());

        // Fresh file: created owner-only.
        let path = dir.path().join("fresh.toml");
        c.save_to(&path).expect("save");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "config holding a key is owner-only");

        // The first-run case: a world-readable template already exists. The atomic rename must
        // replace it with an owner-only file, never exposing the key at the old mode.
        let loose = dir.path().join("loose.toml");
        std::fs::write(&loose, "# template").expect("seed");
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        c.save_to(&loose).expect("save over loose file");
        let mode = std::fs::metadata(&loose)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "pre-existing 0644 file is replaced by 0600"
        );
        let back = Config::from_toml_str(&std::fs::read_to_string(&loose).expect("read"))
            .expect("reparse");
        assert_eq!(back, c, "content survives the atomic replace");
    }

    #[test]
    fn zero_buffer_seconds_clamps_to_one() {
        let c = Config::from_toml_str("[buffer]\nseconds = 0\n").expect("parses");
        assert_eq!(c.buffer_window(), Duration::from_secs(1));
    }

    #[test]
    fn hotkey_trigger_is_trimmed_with_default_fallback() {
        let c = Config::from_toml_str("[hotkey]\ntrigger = \"\"\n").expect("parses");
        assert_eq!(c.hotkey_trigger(), "CTRL+ALT+R");
        let ws = Config::from_toml_str("[hotkey]\ntrigger = \"   \"\n").expect("parses");
        assert_eq!(
            ws.hotkey_trigger(),
            "CTRL+ALT+R",
            "whitespace-only → default"
        );
        let padded =
            Config::from_toml_str("[hotkey]\ntrigger = \" CTRL+ALT+K \"\n").expect("parses");
        assert_eq!(padded.hotkey_trigger(), "CTRL+ALT+K", "padding is trimmed");
    }

    #[test]
    fn negative_or_nonfinite_gain_falls_back_to_unity() {
        let c =
            Config::from_toml_str("[audio]\nmic_gain = -1.0\nsystem_gain = 3.0\n").expect("parses");
        assert_eq!(c.mic_gain(), 1.0, "negative gain → unity");
        assert_eq!(c.system_gain(), 3.0, "positive gain kept");
        let nan = Config::from_toml_str("[audio]\nmic_gain = nan\n").expect("parses");
        assert_eq!(nan.mic_gain(), 1.0, "NaN gain → unity");
    }

    #[test]
    fn env_overrides_take_precedence_over_file() {
        // Every numeric override lands on its own distinct field (guards against a copy-paste
        // mix-up), with a deliberately distinct value each, plus the non-positive/unparseable
        // fallbacks and the output-dir string override.
        let mut c = Config::from_toml_str("[video]\nwidth = 1280\n[audio]\nbitrate_bps = 64000\n")
            .expect("parses");
        let env = std::collections::HashMap::from([
            ("REWYND_WIDTH", "3840"),
            ("REWYND_HEIGHT", "2160"),
            ("REWYND_FPS", "120"),
            ("REWYND_BITRATE_BPS", "25000000"),
            ("REWYND_IDR_PERIOD", "240"),
            ("REWYND_AUDIO_BITRATE_BPS", "0"), // non-positive → ignored
            ("REWYND_OUTPUT_DIR", "/tmp/over"),
        ]);
        c.apply_env_overrides(|k| env.get(k).map(|s| (*s).to_owned()));
        let v = c.video();
        assert_eq!(v.width, 3840, "WIDTH overrides the file value");
        assert_eq!(v.height, 2160, "HEIGHT overrides the default");
        assert_eq!(v.framerate, 120, "FPS overrides the default");
        assert_eq!(
            v.bitrate_bps, 25_000_000,
            "BITRATE_BPS overrides the default"
        );
        assert_eq!(v.idr_period, 240, "IDR_PERIOD overrides the default");
        assert_eq!(
            c.audio().bitrate_bps,
            64_000,
            "zero AUDIO_BITRATE_BPS ignored → file value"
        );
        assert_eq!(c.output_dir(), Some(PathBuf::from("/tmp/over")));

        // An unparseable numeric override is ignored, leaving the file/default value intact.
        let mut c2 = Config::from_toml_str("[video]\nwidth = 1600\n").expect("parses");
        let bad = std::collections::HashMap::from([("REWYND_WIDTH", "not-a-number")]);
        c2.apply_env_overrides(|k| bad.get(k).map(|s| (*s).to_owned()));
        assert_eq!(
            c2.video().width,
            1600,
            "unparseable override ignored → file value"
        );
    }

    #[test]
    fn buffer_seconds_is_clamped_to_the_ceiling() {
        let c =
            Config::from_toml_str(&format!("[buffer]\nseconds = {}\n", u64::MAX)).expect("parses");
        assert_eq!(
            c.buffer_window(),
            Duration::from_secs(MAX_BUFFER_SECONDS),
            "an absurd window is capped, not left to grow the ring buffer unbounded"
        );
    }

    #[test]
    fn load_from_none_is_defaults_plus_env() {
        let env = std::collections::HashMap::from([("REWYND_WIDTH", "800")]);
        let c = load_from(None, |k| env.get(k).map(|s| (*s).to_owned()));
        assert_eq!(c.video().width, 800);
        assert_eq!(c.video().height, 1080); // default survives
    }

    #[test]
    fn load_from_reads_valid_file_then_env_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[video]\nwidth = 1280\nheight = 720\n").expect("write");
        let env = std::collections::HashMap::from([("REWYND_WIDTH", "2560")]);
        let c = load_from(Some(&path), |k| env.get(k).map(|s| (*s).to_owned()));
        assert_eq!(c.video().width, 2560, "env beats the file");
        assert_eq!(c.video().height, 720, "file beats the default");
    }

    #[test]
    fn load_from_malformed_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this = is = not valid toml").expect("write");
        let c = load_from(Some(&path), |_| None);
        assert_eq!(c, Config::default());
    }

    #[test]
    fn load_from_missing_file_is_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("never-created.toml");
        let c = load_from(Some(&path), |_| None);
        assert_eq!(c, Config::default());
    }

    #[test]
    fn load_from_salvages_valid_sections_around_a_bad_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[video]\nwidth = 1280\n[audio]\nmic_gain = \"loud\"\n[buffer]\nseconds = 45\n",
        )
        .expect("write");
        let c = load_from(Some(&path), |_| None);
        assert_eq!(c.video().width, 1280, "section before the bad one survives");
        assert_eq!(c.buffer_seconds(), 45, "section after the bad one survives");
        assert_eq!(c.mic_gain(), 1.0, "the bad section falls back to defaults");
    }

    #[test]
    fn load_from_keeps_known_sections_despite_an_unknown_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[video]\nwidth = 1280\n[nonsense]\nx = 1\n").expect("write");
        let c = load_from(Some(&path), |_| None);
        assert_eq!(c.video().width, 1280, "known section survives the stranger");
        assert_eq!(c.audio(), Config::default().audio());
    }

    #[test]
    fn write_default_file_at_writes_parseable_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rewynd").join("config.toml");
        write_default_file_at(&path).expect("write default");
        let text = std::fs::read_to_string(&path).expect("read back");
        let c = Config::from_toml_str(&text).expect("written template parses");
        assert_eq!(c, Config::default());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600, "template may later hold the API key");
            assert_eq!(
                mode(path.parent().expect("parent")),
                0o700,
                "dir is private"
            );
        }
    }

    #[test]
    fn setters_round_trip_through_toml() {
        let mut c = Config::default();
        c.set_mic_gain(2.5);
        c.set_system_gain(0.75);
        c.set_buffer_seconds(120);
        c.set_output_directory(Some("/tmp/clips".to_owned()));
        c.set_hotkey_trigger("CTRL+ALT+K".to_owned());
        c.set_always_prompt(true);
        c.set_video(VideoSettings {
            width: 2560,
            height: 1440,
            framerate: 144,
            bitrate_bps: 25_000_000,
            idr_period: 144,
        });

        let toml = c.to_toml_string().expect("serialize");
        let back = Config::from_toml_str(&toml).expect("reparse");
        assert_eq!(back, c, "config survives a TOML round-trip");
        // Spot-check via the public view.
        assert_eq!(back.video().width, 2560);
        assert_eq!(back.video().framerate, 144);
        assert_eq!(back.mic_gain(), 2.5);
        assert_eq!(back.buffer_seconds(), 120);
        assert_eq!(back.output_directory(), Some("/tmp/clips"));
        assert!(back.always_prompt());
    }

    #[test]
    fn set_output_directory_empty_clears_it() {
        let mut c = Config::default();
        c.set_output_directory(Some(String::new()));
        assert_eq!(c.output_directory(), None, "empty string clears the dir");
        assert!(
            !c.to_toml_string().expect("serialize").contains("directory"),
            "an unset directory is omitted from the file (TOML has no null)"
        );
    }

    #[test]
    fn save_to_writes_a_loadable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut c = Config::default();
        c.set_mic_gain(3.0);
        c.save_to(&path).expect("save");
        let back =
            Config::from_toml_str(&std::fs::read_to_string(&path).expect("read")).expect("reparse");
        assert_eq!(back, c);
    }
}
