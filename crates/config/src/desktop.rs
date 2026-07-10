//! Desktop integration: the login autostart (an XDG `.desktop` entry on Linux, a launchd
//! LaunchAgent on macOS, an HKCU Run-key value on Windows), the Linux launcher entry
//! (app-id registration), and the brand icons.

#[cfg(any(unix, windows))]
use std::path::Path;
#[cfg(any(unix, windows))]
use std::path::PathBuf;

use crate::paths::APP_ID;
#[cfg(target_os = "linux")]
use crate::paths::{config_home_from, data_home_from};

/// The brand mark's PNG renders as `(pixel_size, png_bytes)`, smallest first — the one owner
/// for every consumer: the hicolor install below, the recorder's tray pixmaps, and the
/// settings window's icon and header mark. (The master vector is `docs/design/logo.svg`.)
pub const BRAND_ICONS: &[(u32, &[u8])] = &[
    (24, include_bytes!("../assets/brand/logo-24.png")),
    (32, include_bytes!("../assets/brand/logo-32.png")),
    (48, include_bytes!("../assets/brand/logo-48.png")),
    (64, include_bytes!("../assets/brand/logo-64.png")),
    (128, include_bytes!("../assets/brand/logo-128.png")),
];

/// Render a path as a single quoted desktop-entry `Exec` value, applying the unescaping layers
/// the Desktop Entry spec runs on read: wrap in double quotes and backslash-escape the reserved
/// characters (`"` `` ` `` `$` `\`), escape every backslash again for the string-value layer,
/// and double `%` so it can't read as a field code. So a literal `\` ends up as four
/// backslashes, and a path with spaces is simply quoted. ASCII control characters cannot be
/// represented (a newline would smuggle extra entry lines) and never occur in a legitimate
/// binary path, so they are stripped with a warning.
#[cfg(target_os = "linux")]
#[must_use]
pub fn desktop_exec_value(path: &str) -> String {
    if path.chars().any(|c| c.is_ascii_control()) {
        tracing::warn!("stripping control characters from desktop Exec path");
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for ch in path.chars().filter(|c| !c.is_ascii_control()) {
        if matches!(ch, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\").replace('%', "%%")
}

/// A minimal `[Desktop Entry]` body for `exec`, with `extra` key-lines appended — the shared
/// core of the launcher entry (app id registration) and the login autostart entry. `Icon` is
/// the app id, resolved through the icon theme (see [`install_icons`]).
#[cfg(target_os = "linux")]
#[must_use]
pub fn desktop_entry(exec: &Path, extra: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=rewynd\n\
         Comment=Instant-replay clip recorder\n\
         Icon={APP_ID}\n\
         Exec={}\n\
         Terminal=false\n\
         {extra}",
        desktop_exec_value(&exec.to_string_lossy()),
    )
}

/// Path of rewynd's XDG autostart entry (`<config-home>/autostart/<APP_ID>.desktop`), or `None`
/// if the environment can't resolve one.
#[cfg(target_os = "linux")]
#[must_use]
pub fn autostart_path() -> Option<PathBuf> {
    config_home_from(|k| std::env::var_os(k))
        .map(|home| home.join("autostart").join(format!("{APP_ID}.desktop")))
}

/// Write `contents` to `path` atomically (temp + rename), creating parent directories: a crash
/// can't leave a truncated file that would silently break the launcher, autostart, or icon.
#[cfg(unix)]
fn write_file_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let result = std::fs::write(&tmp, contents).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Install (or refresh) the autostart entry at `path`, launching `exec` at login. The testable
/// core of [`install_autostart`].
#[cfg(target_os = "linux")]
fn install_autostart_at(path: &Path, exec: &Path) -> std::io::Result<()> {
    write_file_atomic(
        path,
        desktop_entry(exec, "StartupNotify=false\n").as_bytes(),
    )
}

/// Remove the autostart entry at `path`; an already-absent entry is fine. The testable core of
/// [`remove_autostart`].
#[cfg(unix)]
fn remove_autostart_at(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(target_os = "linux")]
fn autostart_path_or_err() -> std::io::Result<PathBuf> {
    autostart_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "neither XDG_CONFIG_HOME nor HOME is set",
        )
    })
}

/// Install (or refresh) the login autostart entry, launching `exec` at login.
#[cfg(target_os = "linux")]
pub fn install_autostart(exec: &Path) -> std::io::Result<()> {
    install_autostart_at(&autostart_path_or_err()?, exec)
}

/// Remove the login autostart entry (absent is fine).
#[cfg(target_os = "linux")]
pub fn remove_autostart() -> std::io::Result<()> {
    remove_autostart_at(&autostart_path_or_err()?)
}

/// Install the launcher entry (`<data-home>/applications/<APP_ID>.desktop`) registering the app
/// id, so trays/notifications resolve rewynd's name and icon — unless one already exists:
/// packaged installs ship the entry, and a package-managed file must stay untouched. Returns the
/// entry path either way.
#[cfg(target_os = "linux")]
pub fn install_launcher_entry(exec: &Path) -> std::io::Result<PathBuf> {
    // In an AppImage the launcher must point at the image itself ($APPIMAGE), not the ephemeral
    // mount path in `exec` (stale once the image unmounts). The AppImage's mainExe is the GUI,
    // which is exactly what the launcher opens, so this holds for both callers.
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    let exec = appimage.as_deref().unwrap_or(exec);
    let path = data_home_from(|k| std::env::var_os(k))
        .map(|home| home.join("applications").join(format!("{APP_ID}.desktop")))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither XDG_DATA_HOME nor HOME is set",
            )
        })?;
    install_launcher_entry_at(&path, exec)?;
    Ok(path)
}

/// Whether a desktop entry carries an `Icon` key, in any of the spec's spellings: optional
/// space around `=`, optional `[locale]` suffix. Used as the ownership heuristic below — every
/// entry we write (and any real packaged one) has an icon, so a missing key marks one of our
/// own pre-icon self-installs.
#[cfg(target_os = "linux")]
fn has_icon_key(entry: &str) -> bool {
    entry.lines().any(|line| {
        let Some(rest) = line.trim_start().strip_prefix("Icon") else {
            return false;
        };
        let rest = match rest.trim_start().strip_prefix('[') {
            Some(bracketed) => match bracketed.split_once(']') {
                Some((_, after)) => after,
                None => return false,
            },
            None => rest,
        };
        rest.trim_start().starts_with('=')
    })
}

/// Refresh `path` with a fresh entry unless the existing one carries an Icon key (then it is
/// packaged or user-managed and must stay untouched). Unreadable-as-text entries are treated
/// as foreign and also left alone.
#[cfg(target_os = "linux")]
fn refresh_iconless_entry_at(path: &Path, entry: &str) -> std::io::Result<()> {
    match std::fs::read_to_string(path) {
        Ok(existing) if has_icon_key(&existing) => return Ok(()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Ok(()),
        Err(e) => return Err(e),
    }
    write_file_atomic(path, entry.as_bytes())
}

/// The testable core of [`install_launcher_entry`].
#[cfg(target_os = "linux")]
fn install_launcher_entry_at(path: &Path, exec: &Path) -> std::io::Result<()> {
    refresh_iconless_entry_at(
        path,
        &desktop_entry(exec, "Categories=AudioVideo;Recorder;\n"),
    )
}

/// Bring a pre-icon autostart entry up to date so it gains the `Icon=` key. Only an existing,
/// icon-less entry is rewritten: a missing one means start-on-boot is off (this must not turn
/// it on), and one with an icon may be user-managed.
#[cfg(target_os = "linux")]
pub fn refresh_autostart(exec: &Path) -> std::io::Result<()> {
    refresh_autostart_at(&autostart_path_or_err()?, exec)
}

/// Old binary basenames a rename must migrate away from: the recorder used to be `rewynd` (now
/// the GUI's name) and the GUI `rewynd-settings`. An autostart entry still launching one of these
/// is a pre-rename entry of ours and is repointed at the current recorder — otherwise, because the
/// old recorder name is now the GUI, boot would open the library window instead of recording.
#[cfg(target_os = "linux")]
const RENAMED_BINARIES: &[&str] = &["rewynd", "rewynd-settings"];

/// The binary basename in a desktop entry's `Exec` line, best-effort (the Exec is a quoted path;
/// pathological paths with embedded quotes just yield a wrong basename, which is harmless here).
#[cfg(target_os = "linux")]
fn entry_exec_basename(entry: &str) -> Option<String> {
    let value = entry
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("Exec="))?;
    let path = match value.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => value.split_whitespace().next().unwrap_or(value),
    };
    path.rsplit('/').next().map(str::to_owned)
}

/// The testable core of [`refresh_autostart`].
#[cfg(target_os = "linux")]
fn refresh_autostart_at(path: &Path, exec: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let desired = desktop_entry(exec, "StartupNotify=false\n");
    // Rename migration: an autostart entry still launching an old binary name gets repointed at
    // the current recorder even when it already carries an icon. This overrides the icon-ownership
    // guard because it's our own rename, not a packaged file (no packaged autostart exists).
    let target = exec
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if let Ok(existing) = std::fs::read_to_string(path)
        && let Some(basename) = entry_exec_basename(&existing)
        && basename != target
        && RENAMED_BINARIES.contains(&basename.as_str())
    {
        return write_file_atomic(path, desired.as_bytes());
    }
    refresh_iconless_entry_at(path, &desired)
}

/// Install [`BRAND_ICONS`] into the per-user hicolor theme
/// (`<data-home>/icons/hicolor/<S>x<S>/apps/<APP_ID>.png`), so the desktop can resolve the
/// `Icon=` name in our entries, the taskbar icon for our app id, and notification icons.
/// Unlike the launcher entry a stale icon is refreshed: packaged icons live under
/// `/usr/share/icons`, never in the user's data home, so nothing package-managed is at risk.
#[cfg(target_os = "linux")]
pub fn install_icons() -> std::io::Result<()> {
    let hicolor = data_home_from(|k| std::env::var_os(k))
        .map(|home| home.join("icons").join("hicolor"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither XDG_DATA_HOME nor HOME is set",
            )
        })?;
    install_icons_at(&hicolor, BRAND_ICONS)
}

/// The testable core of [`install_icons`]. Writes only what differs — the common case (every
/// start after the first install) touches nothing.
#[cfg(target_os = "linux")]
fn install_icons_at(hicolor: &Path, icons: &[(u32, &[u8])]) -> std::io::Result<()> {
    let mut changed = false;
    for (size, png) in icons {
        let path = hicolor
            .join(format!("{size}x{size}"))
            .join("apps")
            .join(format!("{APP_ID}.png"));
        if std::fs::read(&path).is_ok_and(|current| current == *png) {
            continue;
        }
        write_file_atomic(&path, png)?;
        changed = true;
    }
    if changed {
        // Bump the theme directory's mtime so icon caches re-scan on their next lookup. Best
        // effort — a session that already cached a miss may still need a re-login to see it.
        let _ = std::fs::File::open(hicolor)
            .and_then(|dir| dir.set_modified(std::time::SystemTime::now()));
    }
    Ok(())
}

// macOS autostart: a launchd LaunchAgent plist under ~/Library/LaunchAgents. RunAtLoad
// launches the recorder at login; KeepAlive stays off so quitting from the tray sticks.

/// Path of rewynd's LaunchAgent plist (`~/Library/LaunchAgents/<APP_ID>.plist`), or `None`
/// if `HOME` is unusable.
#[cfg(target_os = "macos")]
#[must_use]
fn launch_agent_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|home| {
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{APP_ID}.plist"))
        })
}

#[cfg(target_os = "macos")]
fn launch_agent_path_or_err() -> std::io::Result<PathBuf> {
    launch_agent_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))
}

/// Escape the XML-reserved characters that can occur in a filesystem path (`&`, `<`, `>`), so
/// the path can't break out of its `<string>` element.
#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// The LaunchAgent plist launching `exec` at login.
#[cfg(target_os = "macos")]
#[must_use]
fn launch_agent_plist(exec: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{APP_ID}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<false/>
</dict>
</plist>
"#,
        xml_escape(&exec.to_string_lossy()),
    )
}

/// Install (or refresh) the LaunchAgent at `path`, launching `exec` at login. The testable
/// core of [`install_autostart`].
#[cfg(target_os = "macos")]
fn install_autostart_at(path: &Path, exec: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    write_file_atomic(path, launch_agent_plist(exec).as_bytes())?;
    // launchd refuses group/world-writable agent plists.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
}

/// The program a LaunchAgent plist launches: the first `<string>` after the
/// `ProgramArguments` key, best-effort (our own plist shape; a hand-built exotic one
/// simply yields `None` and is left alone).
#[cfg(target_os = "macos")]
fn plist_program(plist: &str) -> Option<&str> {
    let (_, tail) = plist.split_once("<key>ProgramArguments</key>")?;
    let (_, tail) = tail.split_once("<string>")?;
    tail.split_once("</string>").map(|(program, _)| program)
}

/// The testable core of [`refresh_autostart`]: repoint an existing agent whose program
/// no longer exists (the install moved with an update). Mirrors the other platforms'
/// ownership caution — a missing agent means start-on-login is off and must stay off,
/// and a plist launching a live binary (ours from another install, or user-customized)
/// is not touched, so a dev build never hijacks a packaged install's autostart. An
/// unreadable one is ours (our label, our filename) and is rewritten.
#[cfg(target_os = "macos")]
fn refresh_autostart_at(path: &Path, exec: &Path) -> std::io::Result<()> {
    match std::fs::read_to_string(path) {
        Ok(existing) if existing == launch_agent_plist(exec) => Ok(()),
        Ok(existing) => match plist_program(&existing) {
            Some(program) if !Path::new(program).exists() => install_autostart_at(path, exec),
            _ => Ok(()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => install_autostart_at(path, exec),
        Err(e) => Err(e),
    }
}

/// Install (or refresh) the login LaunchAgent, launching `exec` at login.
#[cfg(target_os = "macos")]
pub fn install_autostart(exec: &Path) -> std::io::Result<()> {
    install_autostart_at(&launch_agent_path_or_err()?, exec)
}

/// Remove the login LaunchAgent (absent is fine).
#[cfg(target_os = "macos")]
pub fn remove_autostart() -> std::io::Result<()> {
    remove_autostart_at(&launch_agent_path_or_err()?)
}

/// Repoint an existing LaunchAgent at the current binary (e.g. after the install moved).
/// A missing agent means start-on-login is off; this must not turn it on.
#[cfg(target_os = "macos")]
pub fn refresh_autostart(exec: &Path) -> std::io::Result<()> {
    refresh_autostart_at(&launch_agent_path_or_err()?, exec)
}

/// No launcher entry on macOS: the `.app` bundle owns the app's identity.
#[cfg(target_os = "macos")]
pub fn install_launcher_entry(_exec: &Path) -> std::io::Result<()> {
    Ok(())
}

/// No icon install on macOS: the `.app` bundle owns the app's icons.
#[cfg(target_os = "macos")]
pub fn install_icons() -> std::io::Result<()> {
    Ok(())
}

// Windows autostart: a value under the per-user Run key. The kernel object model has no
// packaged-vs-user distinction here, so the ownership heuristic is the value's target:
// only a command pointing at a rewynd binary is ever rewritten.
#[cfg(windows)]
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn registry_io_err(e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(e)
}

/// `ERROR_FILE_NOT_FOUND` as the HRESULT the registry API reports for a missing
/// key or value — the "autostart is off" case, not an error.
#[cfg(windows)]
fn is_not_found(e: &windows::core::Error) -> bool {
    e.code().0 as u32 == 0x8007_0002
}

/// The Run-key command for `exec`: quoted, so paths with spaces survive.
#[cfg(windows)]
fn run_command_value(exec: &Path) -> String {
    format!("\"{}\"", exec.display())
}

/// Whether an existing Run-key command points at a rewynd binary (ours to manage);
/// a user-managed wrapper stays untouched.
#[cfg(windows)]
fn is_rewynd_command(command: &str) -> bool {
    command
        .trim()
        .trim_matches('"')
        .to_ascii_lowercase()
        .ends_with("rewynd.exe")
}

/// Set the autostart value under `key_path`. The testable core of [`install_autostart`].
#[cfg(windows)]
fn install_autostart_at_key(key_path: &str, exec: &Path) -> std::io::Result<()> {
    windows_registry::CURRENT_USER
        .create(key_path)
        .and_then(|key| key.set_string(APP_ID, run_command_value(exec)))
        .map_err(registry_io_err)
}

/// Remove the autostart value under `key_path`; absent is fine. The testable core of
/// [`remove_autostart`].
#[cfg(windows)]
fn remove_autostart_at_key(key_path: &str) -> std::io::Result<()> {
    match windows_registry::CURRENT_USER
        .create(key_path)
        .and_then(|key| key.remove_value(APP_ID))
    {
        Err(e) if is_not_found(&e) => Ok(()),
        other => other.map_err(registry_io_err),
    }
}

/// Refresh an existing autostart value under `key_path` to point at `exec`. Only an
/// existing value is touched (absent = start-on-boot is off; this must not turn it on),
/// and only one pointing at a rewynd binary. The testable core of [`refresh_autostart`].
#[cfg(windows)]
fn refresh_autostart_at_key(key_path: &str, exec: &Path) -> std::io::Result<()> {
    let key = windows_registry::CURRENT_USER
        .create(key_path)
        .map_err(registry_io_err)?;
    let current = match key.get_string(APP_ID) {
        Err(e) if is_not_found(&e) => return Ok(()),
        other => other.map_err(registry_io_err)?,
    };
    if !is_rewynd_command(&current) {
        return Ok(());
    }
    let desired = run_command_value(exec);
    if current == desired {
        return Ok(());
    }
    key.set_string(APP_ID, desired).map_err(registry_io_err)
}

/// Install (or refresh) the login autostart Run-key value, launching `exec` at login.
#[cfg(windows)]
pub fn install_autostart(exec: &Path) -> std::io::Result<()> {
    install_autostart_at_key(RUN_KEY_PATH, exec)
}

/// Remove the login autostart Run-key value (absent is fine).
#[cfg(windows)]
pub fn remove_autostart() -> std::io::Result<()> {
    remove_autostart_at_key(RUN_KEY_PATH)
}

/// Point an existing autostart value at the current binary (e.g. after the install
/// moved). Missing or user-managed values stay untouched.
#[cfg(windows)]
pub fn refresh_autostart(exec: &Path) -> std::io::Result<()> {
    refresh_autostart_at_key(RUN_KEY_PATH, exec)
}

/// Registry parent for AppUserModelID metadata: gives our app id a display name and
/// icon, so toasts carry rewynd's identity instead of the launching host's (an
/// unregistered AUMID shows as e.g. "Windows PowerShell").
#[cfg(windows)]
const AUMID_KEY_PATH: &str = r"Software\Classes\AppUserModelId";

/// Where the toast icon PNG lives (`%LOCALAPPDATA%\rewynd\toast-icon.png`) — the
/// registry's `IconUri` needs a file path, not embedded bytes.
#[cfg(windows)]
fn toast_icon_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("rewynd").join("toast-icon.png"))
}

/// Register the toast identity for [`APP_ID`]: write the brand icon to disk and point
/// the AppUserModelId registry entry's `DisplayName`/`IconUri` at it. Idempotent —
/// call at every startup.
#[cfg(windows)]
pub fn register_toast_identity() -> std::io::Result<()> {
    let icon = toast_icon_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no local data dir"))?;
    // The largest render: Windows scales down, and small sources blur in the header.
    let (_, png) = BRAND_ICONS.last().expect("BRAND_ICONS is non-empty");
    if !std::fs::read(&icon).is_ok_and(|current| current == *png) {
        if let Some(parent) = icon.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&icon, png)?;
    }
    register_toast_identity_at(AUMID_KEY_PATH, &icon)
}

/// The testable core of [`register_toast_identity`].
#[cfg(windows)]
fn register_toast_identity_at(key_path: &str, icon: &Path) -> std::io::Result<()> {
    let key = windows_registry::CURRENT_USER
        .create(format!(r"{key_path}\{APP_ID}"))
        .map_err(registry_io_err)?;
    key.set_string("DisplayName", "rewynd")
        .map_err(registry_io_err)?;
    key.set_string("IconUri", icon.display().to_string())
        .map_err(registry_io_err)
}

/// Register the `rewynd://` URL protocol so a clickable "clip saved" toast can deep-link back
/// into its clip: `HKCU\Software\Classes\rewynd` points its open command at `gui_exe`. Idempotent
/// — call at every startup. The GUI path is passed in because the recorder (which registers this)
/// is a sibling binary, not the launch target itself.
#[cfg(windows)]
pub fn register_clip_protocol(gui_exe: &Path) -> std::io::Result<()> {
    let scheme = format!(r"Software\Classes\{}", crate::CLIP_URL_SCHEME);
    let key = windows_registry::CURRENT_USER
        .create(&scheme)
        .map_err(registry_io_err)?;
    // The default value + the "URL Protocol" marker are what make the shell treat this key as a
    // launchable protocol handler.
    key.set_string("", "URL:rewynd protocol")
        .map_err(registry_io_err)?;
    key.set_string("URL Protocol", "")
        .map_err(registry_io_err)?;
    let command = windows_registry::CURRENT_USER
        .create(format!(r"{scheme}\shell\open\command"))
        .map_err(registry_io_err)?;
    // "%1" is the full rewynd://clip/<name> URL the shell hands us as argv.
    command
        .set_string("", format!("\"{}\" \"%1\"", gui_exe.display()))
        .map_err(registry_io_err)
}

/// Reconnect stdout/stderr to the launching console, if any. A windows-subsystem exe starts
/// with no std handles, so `cargo run` / terminal launches would otherwise show no tracing
/// output or `--version`. Inherited handles (e.g. the settings app's `--probe-encoders` pipes)
/// are left untouched, and an Explorer launch (no parent console) is a no-op.
#[cfg(windows)]
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    // SAFETY: trivially safe FFI; a missing/invalid handle just means "not inherited".
    let inherited = |which| unsafe { GetStdHandle(which) }.is_ok_and(|h| !h.is_invalid());
    if inherited(STD_OUTPUT_HANDLE) || inherited(STD_ERROR_HANDLE) {
        return;
    }
    // SAFETY: trivially safe FFI; failure (no parent console) leaves us detached, as launched.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// No console attachment needed off Windows.
#[cfg(not(windows))]
pub fn attach_parent_console() {}

// No autostart mechanism on other targets; the settings toggle surfaces the error.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn install_autostart(_exec: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "autostart is not supported on this platform",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn remove_autostart() -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn refresh_autostart(_exec: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// A registry key unique to one test, so parallel tests never collide. Removed
    /// (tree and all) by [`TestKey`]'s drop.
    struct TestKey(String);

    impl TestKey {
        fn new(tag: &str) -> Self {
            Self(format!(
                r"Software\rewynd-test-{}-{tag}",
                std::process::id()
            ))
        }
        fn value(&self) -> Option<String> {
            windows_registry::CURRENT_USER
                .create(&self.0)
                .and_then(|key| key.get_string(crate::paths::APP_ID))
                .ok()
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            let _ = windows_registry::CURRENT_USER.remove_tree(&self.0);
        }
    }

    #[test]
    fn autostart_value_installs_refreshes_and_removes() {
        let key = TestKey::new("cycle");
        install_autostart_at_key(&key.0, Path::new(r"C:\apps\re wynd\rewynd.exe"))
            .expect("install");
        assert_eq!(
            key.value().as_deref(),
            Some(r#""C:\apps\re wynd\rewynd.exe""#)
        );

        // A rewynd-owned value follows the binary when it moves; idempotent after that.
        refresh_autostart_at_key(&key.0, Path::new(r"C:\new\rewynd.exe")).expect("refresh");
        assert_eq!(key.value().as_deref(), Some(r#""C:\new\rewynd.exe""#));
        refresh_autostart_at_key(&key.0, Path::new(r"C:\new\rewynd.exe")).expect("idempotent");
        assert_eq!(key.value().as_deref(), Some(r#""C:\new\rewynd.exe""#));

        remove_autostart_at_key(&key.0).expect("remove");
        assert_eq!(key.value(), None, "disabling removes the value");
        remove_autostart_at_key(&key.0).expect("idempotent remove");
    }

    #[test]
    fn toast_identity_registers_name_and_icon() {
        let key = TestKey::new("aumid");
        let icon = Path::new(r"C:\somewhere\toast-icon.png");
        register_toast_identity_at(&key.0, icon).expect("register");
        let entry = windows_registry::CURRENT_USER
            .create(format!(r"{}\{}", key.0, crate::paths::APP_ID))
            .expect("open");
        assert_eq!(entry.get_string("DisplayName").as_deref(), Ok("rewynd"));
        assert_eq!(
            entry.get_string("IconUri").as_deref(),
            Ok(r"C:\somewhere\toast-icon.png")
        );
        // Idempotent re-register.
        register_toast_identity_at(&key.0, icon).expect("re-register");
    }

    #[test]
    fn refresh_leaves_missing_and_user_managed_values_alone() {
        let key = TestKey::new("refresh");
        // Missing value: start-on-boot is off; the refresh must not create one.
        refresh_autostart_at_key(&key.0, Path::new(r"C:\x\rewynd.exe")).expect("absent ok");
        assert_eq!(key.value(), None);

        // A command pointing at something that isn't a rewynd binary is user-managed.
        windows_registry::CURRENT_USER
            .create(&key.0)
            .and_then(|k| k.set_string(crate::paths::APP_ID, r#""C:\wrapper\launcher.exe""#))
            .expect("seed");
        refresh_autostart_at_key(&key.0, Path::new(r"C:\x\rewynd.exe")).expect("no-op");
        assert_eq!(key.value().as_deref(), Some(r#""C:\wrapper\launcher.exe""#));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn plist_carries_label_exec_and_launch_keys() {
        let plist = launch_agent_plist(Path::new("/opt/rewynd/rewynd-recorder"));
        assert!(plist.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
        assert!(plist.contains(&format!("<string>{APP_ID}</string>")));
        assert!(plist.contains("<string>/opt/rewynd/rewynd-recorder</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n\t<false/>"));
    }

    #[test]
    fn plist_escapes_xml_reserved_path_characters() {
        let plist = launch_agent_plist(Path::new("/apps/a&b <c>/rewynd-recorder"));
        assert!(plist.contains("<string>/apps/a&amp;b &lt;c&gt;/rewynd-recorder</string>"));
        assert_eq!(xml_escape("plain/path"), "plain/path");
        assert_eq!(xml_escape("&&"), "&amp;&amp;");
    }

    #[test]
    fn autostart_install_refresh_and_remove() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("LaunchAgents").join("rewynd.plist");

        install_autostart_at(&path, Path::new("/opt/rewynd/rewynd-recorder")).expect("install");
        let agent = std::fs::read_to_string(&path).expect("read agent");
        assert_eq!(
            agent,
            launch_agent_plist(Path::new("/opt/rewynd/rewynd-recorder"))
        );
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o644,
            "launchd wants a non-group-writable plist"
        );

        // A stale exec path (the install moved) is refreshed in place; a current one is not
        // rewritten.
        refresh_autostart_at(&path, Path::new("/new/rewynd-recorder")).expect("refresh");
        let refreshed = std::fs::read_to_string(&path).expect("read refreshed");
        assert!(refreshed.contains("<string>/new/rewynd-recorder</string>"));
        refresh_autostart_at(&path, Path::new("/new/rewynd-recorder")).expect("idempotent");

        remove_autostart_at(&path).expect("remove");
        assert!(!path.exists(), "disabling removes the agent");
        remove_autostart_at(&path).expect("idempotent remove");
    }

    #[test]
    fn autostart_refresh_leaves_a_live_program_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("LaunchAgents").join("rewynd.plist");
        // A plist whose program still exists (another install, or user-managed) must not
        // be hijacked by a differently-located build.
        let live = dir.path().join("rewynd-recorder");
        std::fs::write(&live, b"").expect("live binary");
        install_autostart_at(&path, &live).expect("install");
        let before = std::fs::read_to_string(&path).expect("read");
        refresh_autostart_at(&path, Path::new("/somewhere/else/rewynd-recorder")).expect("refresh");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            before,
            "live-program plist stays untouched"
        );
    }

    #[test]
    fn autostart_refresh_never_creates_a_missing_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("LaunchAgents").join("rewynd.plist");
        refresh_autostart_at(&path, Path::new("/opt/rewynd/rewynd-recorder")).expect("absent ok");
        assert!(!path.exists(), "start-on-login stays off");
    }

    #[test]
    fn launcher_and_icon_installs_are_no_ops() {
        install_launcher_entry(Path::new("/opt/rewynd/rewynd")).expect("no-op");
        install_icons().expect("no-op");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn exec_value_quotes_plain_and_spaced_paths() {
        assert_eq!(
            desktop_exec_value("/usr/bin/rewynd"),
            r#""/usr/bin/rewynd""#
        );
        assert_eq!(
            desktop_exec_value("/home/a b/rewynd"),
            r#""/home/a b/rewynd""#
        );
    }

    #[test]
    fn exec_value_double_escapes_reserved_characters() {
        // `$` -> `\$` (quote layer) -> `\\$` (string layer).
        assert_eq!(desktop_exec_value("/x/$y/rewynd"), r#""/x/\\$y/rewynd""#);
        // A literal backslash becomes four.
        assert_eq!(desktop_exec_value("/x\\y"), r#""/x\\\\y""#);
        // `%` doubles so it can't read as an Exec field code.
        assert_eq!(desktop_exec_value("/x/100%f/y"), r#""/x/100%%f/y""#);
    }

    #[test]
    fn exec_value_strips_control_characters() {
        // A newline could otherwise smuggle extra key-lines into the entry.
        assert_eq!(desktop_exec_value("/x/a\nb/rewynd"), r#""/x/ab/rewynd""#);
        assert_eq!(desktop_exec_value("/x\u{7f}y/re\twynd"), r#""/xy/rewynd""#);
    }

    #[test]
    fn autostart_install_refresh_and_remove() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("autostart").join("rewynd.desktop");

        install_autostart_at(&path, Path::new("/opt/rewynd/rewynd")).expect("install");
        let entry = std::fs::read_to_string(&path).expect("read entry");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=\"/opt/rewynd/rewynd\"\n"));
        assert!(entry.contains("StartupNotify=false\n"));

        // Re-installing refreshes a stale Exec path in place.
        install_autostart_at(&path, Path::new("/new/rewynd")).expect("refresh");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read refreshed")
                .contains("Exec=\"/new/rewynd\"\n")
        );

        remove_autostart_at(&path).expect("remove");
        assert!(!path.exists(), "disabling removes the entry");
        remove_autostart_at(&path).expect("idempotent remove");
    }

    #[test]
    fn launcher_entry_installs_fresh_and_leaves_existing_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("applications")
            .join(format!("{APP_ID}.desktop"));

        install_launcher_entry_at(&path, Path::new("/opt/rewynd/rewynd")).expect("install");
        let entry = std::fs::read_to_string(&path).expect("read entry");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=\"/opt/rewynd/rewynd\"\n"));
        assert!(entry.contains(&format!("Icon={APP_ID}\n")));
        assert!(entry.contains("Categories=AudioVideo;Recorder;\n"));

        // A pre-existing entry with an Icon key (e.g. shipped by a package, or user-managed)
        // stays untouched — in any spec-legal spelling.
        for packaged in [
            "[Desktop Entry]\nIcon=rewynd\n",
            "[Desktop Entry]\nIcon = rewynd\nExec=env FOO=1 /custom/rewynd\n",
            "[Desktop Entry]\nIcon[da]=rewynd\n",
        ] {
            std::fs::write(&path, packaged).expect("seed");
            install_launcher_entry_at(&path, Path::new("/elsewhere/rewynd")).expect("no-op");
            assert_eq!(std::fs::read_to_string(&path).expect("read"), packaged);
        }

        // An icon-less entry is one of our own pre-icon installs: refreshed in place. A key
        // merely starting with "Icon" doesn't count.
        std::fs::write(&path, "[Desktop Entry]\nName=rewynd\nIconTheme=x\n").expect("seed old");
        install_launcher_entry_at(&path, Path::new("/elsewhere/rewynd")).expect("refresh");
        let refreshed = std::fs::read_to_string(&path).expect("read refreshed");
        assert!(refreshed.contains(&format!("Icon={APP_ID}\n")));
        assert!(refreshed.contains("Exec=\"/elsewhere/rewynd\"\n"));

        // A non-UTF-8 file is foreign: left alone rather than clobbered or errored on.
        std::fs::write(&path, [0x80u8, 0xff]).expect("seed binary");
        install_launcher_entry_at(&path, Path::new("/elsewhere/rewynd")).expect("tolerated");
        assert_eq!(std::fs::read(&path).expect("read"), [0x80u8, 0xff]);
    }

    #[test]
    fn autostart_refresh_only_touches_existing_iconless_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("autostart").join("rewynd.desktop");

        // Missing entry: start-on-boot is off; the refresh must not create one.
        refresh_autostart_at(&path, Path::new("/opt/rewynd/rewynd")).expect("absent ok");
        assert!(!path.exists());

        // Pre-icon entry gains the Icon key (and the current exec).
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "[Desktop Entry]\nName=rewynd\n").expect("seed old");
        refresh_autostart_at(&path, Path::new("/opt/rewynd/rewynd")).expect("refresh");
        let refreshed = std::fs::read_to_string(&path).expect("read");
        assert!(refreshed.contains(&format!("Icon={APP_ID}\n")));
        assert!(refreshed.contains("StartupNotify=false\n"));

        // Already-current (or user-managed) entries stay untouched.
        let managed = "[Desktop Entry]\nIcon=custom\nExec=/my/wrapper\n";
        std::fs::write(&path, managed).expect("seed managed");
        refresh_autostart_at(&path, Path::new("/opt/rewynd/rewynd")).expect("no-op");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), managed);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn autostart_refresh_migrates_the_pre_rename_recorder_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("autostart").join("rewynd.desktop");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");

        // A pre-rename entry (icon-bearing) that still launches the old `rewynd` recorder — now
        // the GUI's name — is repointed at the current recorder despite carrying an icon; without
        // this, start-on-boot would open the library window instead of recording.
        let stale = desktop_entry(Path::new("/opt/rewynd/rewynd"), "StartupNotify=false\n");
        std::fs::write(&path, &stale).expect("seed stale");
        refresh_autostart_at(&path, Path::new("/opt/rewynd/rewynd-recorder")).expect("migrate");
        let migrated = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            entry_exec_basename(&migrated).as_deref(),
            Some("rewynd-recorder")
        );

        // Idempotent: the now-current entry is left alone on a second pass.
        refresh_autostart_at(&path, Path::new("/opt/rewynd/rewynd-recorder")).expect("idempotent");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), migrated);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn entry_exec_basename_reads_the_quoted_exec_path() {
        let entry = desktop_entry(Path::new("/opt/rewynd/rewynd-recorder"), "");
        assert_eq!(
            entry_exec_basename(&entry).as_deref(),
            Some("rewynd-recorder")
        );
        assert_eq!(
            entry_exec_basename("[Desktop Entry]\nExec=/usr/bin/rewynd\n").as_deref(),
            Some("rewynd")
        );
        assert_eq!(entry_exec_basename("[Desktop Entry]\nName=x\n"), None);
    }

    // The mtime pinning below opens the theme *directory* as a file, which Windows
    // refuses (and hicolor icon installs are a Linux desktop concern anyway).
    #[cfg(target_os = "linux")]
    #[test]
    fn icons_install_into_hicolor_skip_unchanged_and_refresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hicolor = dir.path().join("icons").join("hicolor");

        install_icons_at(&hicolor, &[(24, b"png-24"), (48, b"png-48")]).expect("install");
        let icon = hicolor
            .join("24x24")
            .join("apps")
            .join(format!("{APP_ID}.png"));
        assert_eq!(std::fs::read(&icon).expect("read 24"), b"png-24");
        assert!(
            hicolor
                .join("48x48")
                .join("apps")
                .join(format!("{APP_ID}.png"))
                .is_file()
        );

        // Identical bytes are skipped entirely: the theme dir's mtime stays put.
        let epoch = std::time::SystemTime::UNIX_EPOCH;
        std::fs::File::open(&hicolor)
            .and_then(|d| d.set_modified(epoch))
            .expect("pin mtime");
        install_icons_at(&hicolor, &[(24, b"png-24"), (48, b"png-48")]).expect("no-op");
        assert_eq!(
            std::fs::metadata(&hicolor)
                .expect("meta")
                .modified()
                .expect("mtime"),
            epoch,
            "unchanged icons must not touch the theme"
        );

        // A stale icon is refreshed in place, and the theme mtime moves so caches re-scan.
        install_icons_at(&hicolor, &[(24, b"png-24-v2")]).expect("refresh");
        assert_eq!(std::fs::read(&icon).expect("read refreshed"), b"png-24-v2");
        assert_ne!(
            std::fs::metadata(&hicolor)
                .expect("meta")
                .modified()
                .expect("mtime"),
            epoch
        );
    }
}
