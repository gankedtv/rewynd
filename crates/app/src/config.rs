//! Runtime configuration (issue #16, docs/adr/0005): a TOML file layered under the built-in
//! defaults and overridden by `REWYND_*` environment variables.
//!
//! Precedence, low → high: **built-in defaults < `config.toml` < environment overrides**.
//! Resolution / framerate / bitrate (and the audio rate / channels / bitrate) stay parameters
//! sourced here, never hard-coded (PLAN §9).
//!
//! The file lives at `$XDG_CONFIG_HOME/rewynd/config.toml` (falling back to
//! `$HOME/.config/rewynd/config.toml`); [`ensure_default_file`] writes a commented
//! [`DEFAULT_TEMPLATE`] there on first run so the settings are discoverable.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rewynd_encode::{AudioEncodeParams, EncodeParams};
use serde::Deserialize;

/// Default retention window in seconds (PLAN §2's 60 s, now configurable).
const DEFAULT_BUFFER_SECONDS: u64 = 60;
/// Default preferred global-shortcut trigger; the compositor may rebind it.
const DEFAULT_HOTKEY_TRIGGER: &str = "CTRL+ALT+R";

/// Video encode settings; mirrors [`EncodeParams`] for TOML, defaulting to its built-ins.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
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
        let d = EncodeParams::default();
        Self {
            width: d.width,
            height: d.height,
            framerate: d.framerate,
            bitrate_bps: d.bitrate_bps,
            idr_period: d.idr_period,
        }
    }
}

/// Audio settings: the Opus encode params plus per-source linear mix gains.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
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
        let d = AudioEncodeParams::default();
        Self {
            sample_rate: d.sample_rate,
            channels: d.channels,
            bitrate_bps: d.bitrate_bps,
            mic_gain: 1.0,
            system_gain: 1.0,
        }
    }
}

/// Ring-buffer retention.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OutputConfig {
    directory: Option<String>,
}

/// Global-shortcut preference.
#[derive(Debug, Clone, PartialEq, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CaptureConfig {
    /// Re-show the ScreenCast monitor picker each launch (ignore the saved restore token),
    /// so a different monitor can be chosen; `false` reuses the saved selection.
    always_prompt: bool,
}

/// The parsed, layered configuration. Build it with [`load`]; read it through the accessors.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
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

    /// The H.264 encode parameters.
    #[must_use]
    pub fn encode_params(&self) -> EncodeParams {
        EncodeParams {
            width: self.video.width,
            height: self.video.height,
            framerate: self.video.framerate,
            bitrate_bps: self.video.bitrate_bps,
            idr_period: self.video.idr_period,
        }
    }

    /// The Opus encode parameters (frame size stays at the encoder's default).
    #[must_use]
    pub fn audio_params(&self) -> AudioEncodeParams {
        AudioEncodeParams {
            sample_rate: self.audio.sample_rate,
            channels: self.audio.channels,
            bitrate_bps: self.audio.bitrate_bps,
            ..Default::default()
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

    /// Retention window; at least one second (a zero window would keep nothing).
    #[must_use]
    pub fn buffer_window(&self) -> Duration {
        Duration::from_secs(self.buffer.seconds.max(1))
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
        let e = c.encode_params();
        assert_eq!((e.width, e.height, e.framerate), (1920, 1080, 60));
        let a = c.audio_params();
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
        let e = c.encode_params();
        assert_eq!(e.width, 1280); // set
        assert_eq!(e.framerate, 30); // set
        assert_eq!(e.height, 1080); // default
        assert_eq!(e.bitrate_bps, 12_000_000); // default
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
        let mut c = Config::from_toml_str("[video]\nwidth = 1280\n[audio]\nbitrate_bps = 64000\n")
            .expect("parses");
        let env = std::collections::HashMap::from([
            ("REWYND_WIDTH", "3840"),
            ("REWYND_FPS", "120"),
            ("REWYND_AUDIO_BITRATE_BPS", "0"), // non-positive → ignored
            ("REWYND_OUTPUT_DIR", "/tmp/over"),
        ]);
        c.apply_env_overrides(|k| env.get(k).map(|s| (*s).to_owned()));
        let e = c.encode_params();
        assert_eq!(e.width, 3840, "env overrides the file value");
        assert_eq!(e.framerate, 120, "env overrides the default");
        assert_eq!(
            c.audio_params().bitrate_bps,
            64_000,
            "zero env override ignored → file value"
        );
        assert_eq!(c.output_dir(), Some(PathBuf::from("/tmp/over")));
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
        assert_eq!(c.encode_params().width, 800);
        assert_eq!(c.encode_params().height, 1080); // default survives
    }

    #[test]
    fn load_from_reads_valid_file_then_env_overrides() {
        let path = unique_tmp_path();
        std::fs::write(&path, "[video]\nwidth = 1280\nheight = 720\n").expect("write");
        let env = std::collections::HashMap::from([("REWYND_WIDTH", "2560")]);
        let c = load_from(Some(&path), |k| env.get(k).map(|s| (*s).to_owned()));
        assert_eq!(c.encode_params().width, 2560, "env beats the file");
        assert_eq!(c.encode_params().height, 720, "file beats the default");
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
}
