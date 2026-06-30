//! Runtime configuration (see docs/adr/0005): a TOML file layered under the built-in defaults
//! and overridden by `REWYND_*` environment variables.
//!
//! Precedence, low → high: **built-in defaults < `config.toml` < environment overrides**.
//! Resolution / framerate / bitrate (and the audio rate / channels / bitrate) stay parameters
//! sourced here, never hard-coded (PLAN §9).
//!
//! The file lives at `$XDG_CONFIG_HOME/rewynd/config.toml` (falling back to
//! `$HOME/.config/rewynd/config.toml`); [`ensure_default_file`] writes a commented
//! [`DEFAULT_TEMPLATE`] there on first run so the settings are discoverable.
//!
//! This crate is intentionally GPU-free: it exposes plain [`VideoSettings`] / [`AudioSettings`]
//! rather than `rewynd-encode`'s param types, so the settings app can depend on it without
//! pulling the wgpu/gpu-video stack. The consumer maps these onto the encoder's params (and a
//! test there guards that the defaults stay in lockstep).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

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
/// Default retention window in seconds (PLAN §2's 60 s, now configurable).
const DEFAULT_BUFFER_SECONDS: u64 = 60;
/// Upper bound on the retention window. The ring buffer holds encoded frames in memory, so an
/// absurd value (a fat-fingered `seconds`) would grow it without limit; cap it to a generous
/// hour rather than let a typo OOM the machine.
const MAX_BUFFER_SECONDS: u64 = 3600;
/// Default preferred global-shortcut trigger; the compositor may rebind it.
pub const DEFAULT_HOTKEY_TRIGGER: &str = "CTRL+ALT+R";

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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AudioConfig {
    sample_rate: u32,
    channels: u32,
    bitrate_bps: u32,
    /// Linear gain applied to the microphone before mixing (raise for a quiet mic).
    mic_gain: f32,
    /// Linear gain applied to system audio before mixing.
    system_gain: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
            bitrate_bps: DEFAULT_AUDIO_BITRATE_BPS,
            mic_gain: 1.0,
            system_gain: 1.0,
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
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OutputConfig {
    // Omitted from the serialized file when unset (TOML has no null).
    #[serde(skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
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
        if let Some(dir) = get("REWYND_OUTPUT_DIR").filter(|s| !s.is_empty()) {
            self.output.directory = Some(dir);
        }
    }

    /// The video encode settings.
    #[must_use]
    pub fn video(&self) -> VideoSettings {
        VideoSettings {
            width: self.video.width,
            height: self.video.height,
            framerate: self.video.framerate,
            bitrate_bps: self.video.bitrate_bps,
            idr_period: self.video.idr_period,
        }
    }

    /// The audio encode settings (sample rate / channels / bitrate).
    #[must_use]
    pub fn audio(&self) -> AudioSettings {
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

    /// The preferred global-shortcut trigger hint (never empty).
    #[must_use]
    pub fn hotkey_trigger(&self) -> &str {
        if self.hotkey.trigger.is_empty() {
            DEFAULT_HOTKEY_TRIGGER
        } else {
            &self.hotkey.trigger
        }
    }

    /// Whether to re-show the ScreenCast monitor picker each launch.
    #[must_use]
    pub fn always_prompt(&self) -> bool {
        self.capture.always_prompt
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

    /// Set the retention window in seconds (stored as-is; clamped on read by [`buffer_window`]).
    pub fn set_buffer_seconds(&mut self, seconds: u64) {
        self.buffer.seconds = seconds;
    }

    /// Set the output directory; an empty string clears it (back to the caller's default).
    pub fn set_output_directory(&mut self, dir: Option<String>) {
        self.output.directory = dir.filter(|s| !s.is_empty());
    }

    /// Set the preferred global-shortcut trigger.
    pub fn set_hotkey_trigger(&mut self, trigger: String) {
        self.hotkey.trigger = trigger;
    }

    /// Set whether to re-show the monitor picker each launch.
    pub fn set_always_prompt(&mut self, always_prompt: bool) {
        self.capture.always_prompt = always_prompt;
    }

    /// Serialize to a TOML string (the editor writes this back to the config file).
    pub fn to_toml_string(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Serialize and write the config to `path`, creating parent directories.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let toml = self
            .to_toml_string()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml)
    }
}

/// Clamp a gain to a usable linear multiplier: non-finite or negative values fall back to unity.
fn sanitize_gain(g: f32) -> f32 {
    if g.is_finite() && g >= 0.0 { g } else { 1.0 }
}

/// Resolve the config file path from an environment lookup: `$XDG_CONFIG_HOME/rewynd/config.toml`,
/// falling back to `$HOME/.config/rewynd/config.toml`. `None` if neither var is usable.
fn config_path_from(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let base = get("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| get("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("rewynd").join("config.toml"))
}

/// The config file path using the process environment.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_path_from(|k| std::env::var_os(k))
}

/// Read the config file at `path` (if any) and layer `REWYND_*` overrides via `get_env`. A
/// missing or malformed file falls back to the built-in defaults (logging why). The testable
/// core of [`load`].
fn load_from(path: Option<&Path>, get_env: impl Fn(&str) -> Option<String>) -> Config {
    let mut config = match path {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => match Config::from_toml_str(&text) {
                Ok(c) => {
                    tracing::info!(path = %path.display(), "loaded config");
                    c
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "invalid config; using defaults");
                    Config::default()
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
# variables override these.

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

[buffer]
# How many seconds of footage to keep for a clip.
seconds = 60

[output]
# Directory for saved clips. Unset = the system temp dir.
# directory = \"/home/you/Videos/rewynd\"

[hotkey]
# Preferred trigger; the desktop may let you rebind it in its shortcut settings.
trigger = \"CTRL+ALT+R\"

[capture]
# Re-show the monitor picker each launch (so you can pick a different screen).
always_prompt = false
";

/// Write [`DEFAULT_TEMPLATE`] to `path`, creating parent directories. The testable core of
/// [`ensure_default_file`].
fn write_default_file_at(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DEFAULT_TEMPLATE)
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
        assert_eq!(c.buffer_window(), Duration::from_secs(60));
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
    fn zero_buffer_seconds_clamps_to_one() {
        let c = Config::from_toml_str("[buffer]\nseconds = 0\n").expect("parses");
        assert_eq!(c.buffer_window(), Duration::from_secs(1));
    }

    #[test]
    fn empty_hotkey_falls_back_to_default() {
        let c = Config::from_toml_str("[hotkey]\ntrigger = \"\"\n").expect("parses");
        assert_eq!(c.hotkey_trigger(), "CTRL+ALT+R");
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

    /// A unique temp path per call, so parallel IO tests don't collide. Removed by the caller.
    fn unique_tmp_path() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rewynd-cfg-{}-{n}.toml", std::process::id()))
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
        let path = unique_tmp_path();
        std::fs::write(&path, "[video]\nwidth = 1280\nheight = 720\n").expect("write");
        let env = std::collections::HashMap::from([("REWYND_WIDTH", "2560")]);
        let c = load_from(Some(&path), |k| env.get(k).map(|s| (*s).to_owned()));
        assert_eq!(c.video().width, 2560, "env beats the file");
        assert_eq!(c.video().height, 720, "file beats the default");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_malformed_file_falls_back_to_defaults() {
        let path = unique_tmp_path();
        std::fs::write(&path, "this = is = not valid toml").expect("write");
        let c = load_from(Some(&path), |_| None);
        assert_eq!(c, Config::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_missing_file_is_defaults() {
        let path = unique_tmp_path(); // never created
        let c = load_from(Some(&path), |_| None);
        assert_eq!(c, Config::default());
    }

    #[test]
    fn write_default_file_at_writes_parseable_defaults() {
        let path = unique_tmp_path();
        write_default_file_at(&path).expect("write default");
        let text = std::fs::read_to_string(&path).expect("read back");
        let c = Config::from_toml_str(&text).expect("written template parses");
        assert_eq!(c, Config::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn config_path_prefers_xdg_then_home() {
        let xdg = config_path_from(|k| match k {
            "XDG_CONFIG_HOME" => Some(OsString::from("/xdg")),
            "HOME" => Some(OsString::from("/home/u")),
            _ => None,
        });
        assert_eq!(xdg, Some(PathBuf::from("/xdg/rewynd/config.toml")));

        let home = config_path_from(|k| (k == "HOME").then(|| OsString::from("/home/u")));
        assert_eq!(
            home,
            Some(PathBuf::from("/home/u/.config/rewynd/config.toml"))
        );

        // A relative XDG_CONFIG_HOME is rejected, falling back to HOME.
        let rel = config_path_from(|k| match k {
            "XDG_CONFIG_HOME" => Some(OsString::from("relative/path")),
            "HOME" => Some(OsString::from("/home/u")),
            _ => None,
        });
        assert_eq!(
            rel,
            Some(PathBuf::from("/home/u/.config/rewynd/config.toml"))
        );

        assert!(config_path_from(|_| None).is_none());
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
        let mut c = Config::default();
        c.set_mic_gain(3.0);
        let path = unique_tmp_path();
        c.save_to(&path).expect("save");
        let back =
            Config::from_toml_str(&std::fs::read_to_string(&path).expect("read")).expect("reparse");
        assert_eq!(back, c);
        let _ = std::fs::remove_file(&path);
    }
}
