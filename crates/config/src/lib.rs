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
//! installation, recorder process control, and the clip store (where clips live and what they
//! are called).

mod activation;
mod clips;
mod desktop;
mod devices;
mod encoders;
mod lock;
mod paths;
mod process;
mod schema;
mod status;
pub mod upload_history;

pub use activation::{send_settings_activation, take_settings_activation};
pub use clips::{
    CLIP_URL_PREFIX, CLIP_URL_SCHEME, ClipEntry, clip_deeplink, clip_from_deeplink,
    clip_output_path, clips_dir, ensure_private_dir, folder_name, list_clips, newest_clip_in,
};
pub use desktop::{
    BRAND_ICONS, attach_parent_console, install_autostart, refresh_autostart, remove_autostart,
};
#[cfg(windows)]
pub use desktop::{register_clip_protocol, register_toast_identity};
pub use devices::{AudioInput, list_audio_inputs};
pub use encoders::{
    ENCODER_PROBE_VERSION, EncoderChoice, EncoderProbe, ProbeAdapter, choose_encoder,
};
// XDG desktop entries exist only on Linux desktops; Windows autostart is a Run-key value and
// macOS autostart a LaunchAgent plist, all behind the same install/remove/refresh surface above.
#[cfg(target_os = "linux")]
pub use desktop::{autostart_path, desktop_entry, desktop_exec_value};
// The launcher/icon installers run at both recorder and GUI startup; on macOS they are no-ops
// (the .app bundle owns the app's identity and icons).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use desktop::{install_icons, install_launcher_entry};
pub use lock::{InstanceLock, acquire_recorder_lock, acquire_settings_lock};
pub use paths::{
    APP_ID, config_path, default_output_dir, recorder_pid_path, settings_activation_path,
    settings_lock_path, sibling_binary,
};
#[cfg(windows)]
pub use process::{RecorderSaveEvent, RecorderStopEvent};
pub use process::{read_recorder_pid, request_recorder_save, stop_recorder};
pub use schema::{
    AudioSettings, Config, DEFAULT_HOTKEY_TRIGGER, DEFAULT_TEMPLATE, DEFAULT_UPLOAD_API_URL,
    DEFAULT_UPLOAD_SHARE_URL, EncoderPreference, MAX_BUFFER_SECONDS, UploadSettings, VideoSettings,
    YouTubeSettings, ensure_default_file, load, load_file, non_empty_or,
};
pub use status::{
    RECORDER_STATUS_VERSION, RecorderState, RecorderStatus, clear_recorder_status,
    read_recorder_status, recorder_status_path, write_recorder_status,
};
