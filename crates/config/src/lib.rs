//! Runtime configuration (see docs/adr/0005): a TOML file layered under the built-in defaults
//! and overridden by `REWYND_*` environment variables.
//!
//! Precedence, low → high: **built-in defaults < `config.toml` < environment overrides**.
//! Env overrides exist for the video/audio/output settings; the other sections are file-only.
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
//!
//! Beyond the config file itself, the crate hosts the other small, GPU-free pieces the recorder
//! and the settings app share: well-known paths, the single-instance locks, desktop-entry
//! installation, and recorder process control.

mod desktop;
mod lock;
mod paths;
mod process;
mod schema;

pub use desktop::{
    BRAND_ICONS, autostart_path, desktop_entry, desktop_exec_value, install_autostart,
    install_icons, install_launcher_entry, refresh_autostart, remove_autostart,
};
pub use lock::{InstanceLock, acquire_recorder_lock, acquire_settings_lock};
pub use paths::{
    APP_ID, config_path, default_output_dir, recorder_pid_path, settings_lock_path, sibling_binary,
};
pub use process::{read_recorder_pid, stop_recorder};
pub use schema::{
    AudioSettings, Config, DEFAULT_HOTKEY_TRIGGER, DEFAULT_TEMPLATE, DEFAULT_UPLOAD_API_URL,
    DEFAULT_UPLOAD_SHARE_URL, MAX_BUFFER_SECONDS, UploadSettings, VideoSettings,
    ensure_default_file, load, load_file, non_empty_or,
};
