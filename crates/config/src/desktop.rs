//! Desktop integration: `.desktop` entries for the launcher (app-id registration) and the
//! login autostart.

use std::path::{Path, PathBuf};

use crate::paths::{APP_ID, config_home_from, data_home_from};

/// Render a path as a single quoted desktop-entry `Exec` value, applying the unescaping layers
/// the Desktop Entry spec runs on read: wrap in double quotes and backslash-escape the reserved
/// characters (`"` `` ` `` `$` `\`), escape every backslash again for the string-value layer,
/// and double `%` so it can't read as a field code. So a literal `\` ends up as four
/// backslashes, and a path with spaces is simply quoted. ASCII control characters cannot be
/// represented (a newline would smuggle extra entry lines) and never occur in a legitimate
/// binary path, so they are stripped with a warning.
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
/// core of the launcher entry (app id registration) and the login autostart entry.
#[must_use]
pub fn desktop_entry(exec: &Path, extra: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=rewynd\n\
         Comment=Instant-replay clip recorder\n\
         Exec={}\n\
         Terminal=false\n\
         {extra}",
        desktop_exec_value(&exec.to_string_lossy()),
    )
}

/// Path of rewynd's XDG autostart entry (`<config-home>/autostart/<APP_ID>.desktop`), or `None`
/// if the environment can't resolve one.
#[must_use]
pub fn autostart_path() -> Option<PathBuf> {
    config_home_from(|k| std::env::var_os(k))
        .map(|home| home.join("autostart").join(format!("{APP_ID}.desktop")))
}

/// Write `entry` to `path` atomically (temp + rename), creating parent directories: a crash
/// can't leave a truncated entry that would silently break the launcher or autostart.
fn write_entry_atomic(path: &Path, entry: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("desktop.tmp");
    let result = std::fs::write(&tmp, entry).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Install (or refresh) the autostart entry at `path`, launching `exec` at login. The testable
/// core of [`install_autostart`].
fn install_autostart_at(path: &Path, exec: &Path) -> std::io::Result<()> {
    write_entry_atomic(path, &desktop_entry(exec, "StartupNotify=false\n"))
}

/// Remove the autostart entry at `path`; an already-absent entry is fine. The testable core of
/// [`remove_autostart`].
fn remove_autostart_at(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn autostart_path_or_err() -> std::io::Result<PathBuf> {
    autostart_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "neither XDG_CONFIG_HOME nor HOME is set",
        )
    })
}

/// Install (or refresh) the login autostart entry, launching `exec` at login.
pub fn install_autostart(exec: &Path) -> std::io::Result<()> {
    install_autostart_at(&autostart_path_or_err()?, exec)
}

/// Remove the login autostart entry (absent is fine).
pub fn remove_autostart() -> std::io::Result<()> {
    remove_autostart_at(&autostart_path_or_err()?)
}

/// Install the launcher entry (`<data-home>/applications/<APP_ID>.desktop`) registering the app
/// id, so trays/notifications resolve rewynd's name and icon — unless one already exists:
/// packaged installs ship the entry, and a package-managed file must stay untouched. Returns the
/// entry path either way.
pub fn install_launcher_entry(exec: &Path) -> std::io::Result<PathBuf> {
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

/// The testable core of [`install_launcher_entry`].
fn install_launcher_entry_at(path: &Path, exec: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    write_entry_atomic(
        path,
        &desktop_entry(exec, "Categories=AudioVideo;Recorder;\n"),
    )
}

#[cfg(test)]
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
        assert!(entry.contains("Categories=AudioVideo;Recorder;\n"));

        // A pre-existing entry (e.g. shipped by a package) stays untouched.
        std::fs::write(&path, "# packaged").expect("seed");
        install_launcher_entry_at(&path, Path::new("/elsewhere/rewynd")).expect("no-op");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "# packaged");
    }
}
