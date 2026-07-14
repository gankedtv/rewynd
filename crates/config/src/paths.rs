//! Filesystem locations: config/data homes, the per-user instance dir for pid/lock files,
//! and sibling-binary resolution.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The application id: portal registration, desktop-entry filenames, tray/notification icon.
pub const APP_ID: &str = "tv.ganked.rewynd";

/// The user's config home from an environment lookup: `$XDG_CONFIG_HOME`, falling back to
/// `$HOME/.config`. Relative values are rejected (a relative path would silently resolve
/// against the process cwd). `None` if neither var is usable.
pub(crate) fn config_home_from(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    get("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| get("HOME").map(|h| Path::new(&h).join(".config")))
        .filter(|p| p.is_absolute())
}

/// The user's data home from an environment lookup: `$XDG_DATA_HOME`, falling back to
/// `$HOME/.local/share`. Absolute-only, like [`config_home_from`]. Only Linux desktops
/// consume it (launcher entries, hicolor icons).
#[cfg(target_os = "linux")]
pub(crate) fn data_home_from(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    get("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| get("HOME").map(|h| Path::new(&h).join(".local").join("share")))
        .filter(|p| p.is_absolute())
}

/// Resolve the config file path from an environment lookup: `$XDG_CONFIG_HOME/rewynd/config.toml`,
/// falling back to `$HOME/.config/rewynd/config.toml`. `None` if neither var is usable.
pub(crate) fn config_path_from(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(config_home_from(get)?.join("rewynd").join("config.toml"))
}

/// The config file path using the process environment: the XDG resolution first (so
/// `$XDG_CONFIG_HOME` stays an override everywhere), then the platform's config dir
/// (`%APPDATA%` on Windows, where the XDG vars and `HOME` don't exist).
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_path_from(|k| std::env::var_os(k)).or_else(|| {
        dirs::config_dir()
            // Same absolute-only rule as the env route: a relative dir would silently
            // resolve against the process cwd.
            .filter(|p| p.is_absolute())
            .map(|home| home.join("rewynd").join("config.toml"))
    })
}

/// The default directory for saved clips when none is configured: a `rewynd` subfolder of the
/// user's **Videos** folder (XDG user-dirs on Linux, the Known Folder on Windows). The subfolder
/// keeps clips together instead of loose among the user's existing videos. `None` if Videos can't
/// be resolved, in which case the caller falls back (e.g. the temp dir).
#[must_use]
pub fn default_output_dir() -> Option<PathBuf> {
    dirs::video_dir().map(|v| v.join("rewynd"))
}

/// A resolved per-user instance dir, plus whether it fell back under the shared temp dir.
/// `$XDG_RUNTIME_DIR` is private (0700) by contract; the temp fallback is world-writable, so
/// [`ensure_instance_dir`] must verify ownership there before pid/lock files are trusted.
pub(crate) struct InstanceDir {
    pub(crate) path: PathBuf,
    pub(crate) in_shared_temp: bool,
}

/// Per-user runtime directory for rewynd's pid/lock files: `$XDG_RUNTIME_DIR/rewynd`, falling
/// back to a uid-scoped dir under the temp dir when the runtime dir is unset or relative (so the
/// guard stays per-user rather than machine-wide on a shared, world-writable `/tmp`).
fn instance_dir_from(runtime_dir: Option<OsString>, temp: PathBuf) -> InstanceDir {
    if let Some(base) = runtime_dir.map(PathBuf::from).filter(|p| p.is_absolute()) {
        return InstanceDir {
            path: base.join("rewynd"),
            in_shared_temp: false,
        };
    }
    #[cfg(unix)]
    let name = {
        // SAFETY: `geteuid` is infallible and takes no arguments.
        let uid = unsafe { libc::geteuid() };
        format!("rewynd-{uid}")
    };
    #[cfg(not(unix))]
    let name = "rewynd".to_owned();
    InstanceDir {
        path: temp.join(name),
        in_shared_temp: true,
    }
}

/// [`instance_dir_from`] resolved against the process environment.
pub(crate) fn instance_dir() -> InstanceDir {
    instance_dir_from(std::env::var_os("XDG_RUNTIME_DIR"), std::env::temp_dir())
}

/// Create the instance dir (0700) and, when it lives under the shared temp dir, verify it is a
/// real directory (no symlink — lstat), owned by us, mode 0700. Anything else fails closed:
/// another user could otherwise pre-plant the path and redirect or read the pid/lock files.
#[cfg(unix)]
pub(crate) fn ensure_instance_dir(dir: &InstanceDir) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(&dir.path)?;
    if !dir.in_shared_temp {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(&dir.path)?;
    // SAFETY: `geteuid` is infallible and takes no arguments.
    let euid = unsafe { libc::geteuid() };
    if !meta.file_type().is_dir() || meta.uid() != euid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing unsafe instance dir {}", dir.path.display()),
        ));
    }
    // A dir we own but with a loose mode (an older release created it 0755) is tightened, not
    // refused: failing closed here would silently disable the single-instance guard forever.
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir.path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn ensure_instance_dir(dir: &InstanceDir) -> std::io::Result<()> {
    if dir.in_shared_temp {
        // Windows' temp dir is already per-user, so no ownership dance is needed there.
        tracing::debug!(path = %dir.path.display(), "instance dir falls back under the temp dir");
    }
    std::fs::create_dir_all(&dir.path)
}

/// Path to the recorder's pid file. The recorder locks it (single-instance guard) and writes its
/// pid here on start; the settings app reads it to stop the running recorder before relaunching.
#[must_use]
pub fn recorder_pid_path() -> PathBuf {
    instance_dir().path.join("recorder.pid")
}

/// Path to the settings app's single-instance lock file.
#[must_use]
pub fn settings_lock_path() -> PathBuf {
    instance_dir().path.join("settings.lock")
}

/// File name of the settings activation hand-off inside the instance dir (docs/adr/0016).
pub(crate) const SETTINGS_ACTIVATION_FILE: &str = "settings.activate";

/// Path of the settings activation file: a pending `rewynd://clip/<name>` link dropped by a
/// refused second settings instance for the running window to pick up (docs/adr/0016).
#[must_use]
pub fn settings_activation_path() -> PathBuf {
    instance_dir().path.join(SETTINGS_ACTIVATION_FILE)
}

/// The staging sibling for an atomic write of `path`: the full file name plus a pid-unique
/// suffix. Appended, not `with_extension` — replacing the extension would collide two files
/// sharing a stem (`settings.lock` / `settings.activate`) on the same staging name.
fn staged_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(format!(".{}.part", std::process::id()));
    path.with_file_name(name)
}

/// Write `contents` to `path` atomically (write a staged sibling, then rename over), creating
/// parent directories. A crash can't leave a truncated file, and a reader never sees a partial
/// write. The staged name carries our pid so concurrent writers of the same path can't rename
/// each other's half-written staging file into place.
pub(crate) fn write_file_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged = staged_path(path);
    let result = std::fs::write(&staged, contents).and_then(|()| std::fs::rename(&staged, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

/// `name` beside `exe`, with the platform's executable suffix. The testable core of
/// [`sibling_binary`].
fn sibling_of(exe: &Path, name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    Some(exe.parent()?.join(file))
}

/// Path of a binary expected beside the current executable (e.g. the recorder next to the
/// settings app). `None` if the executable path can't be resolved.
#[must_use]
pub fn sibling_binary(name: &str) -> Option<PathBuf> {
    sibling_of(&std::env::current_exe().ok()?, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_path_appends_to_the_full_file_name() {
        let staged = staged_path(Path::new("run").join("settings.activate").as_path());
        assert_eq!(
            staged.file_name().and_then(|n| n.to_str()),
            Some(format!("settings.activate.{}.part", std::process::id()).as_str()),
            "the suffix is appended, not swapped for the extension"
        );
    }

    #[test]
    fn write_file_atomic_creates_parents_and_leaves_no_staging_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("nested").join("status.json");
        write_file_atomic(&target, b"{}").expect("write");
        assert_eq!(std::fs::read(&target).expect("read"), b"{}");
        let entries = std::fs::read_dir(target.parent().expect("parent"))
            .expect("read_dir")
            .count();
        assert_eq!(entries, 1, "only the target remains, no staging leftovers");
    }

    // The XDG-semantics tests assert against unix path literals (`/home/u` is not an
    // absolute path on Windows), so they only run there — matching the environments
    // where the XDG lookup is the real code path.
    #[cfg(unix)]
    #[test]
    fn config_home_rejects_relative_values() {
        // Relative XDG_CONFIG_HOME falls back to HOME; a relative HOME resolves to nothing.
        let rel_home = config_home_from(|k| (k == "HOME").then(|| OsString::from("relative")));
        assert_eq!(rel_home, None);
        let both = config_home_from(|k| match k {
            "XDG_CONFIG_HOME" => Some(OsString::from("rel")),
            "HOME" => Some(OsString::from("/home/u")),
            _ => None,
        });
        assert_eq!(both, Some(PathBuf::from("/home/u/.config")));
    }

    #[cfg(unix)]
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

    #[cfg(windows)]
    #[test]
    fn config_path_falls_back_to_the_platform_config_dir() {
        // A stock Windows session has neither XDG_CONFIG_HOME nor HOME, so the platform
        // fallback (%APPDATA%) must resolve; either route ends at the same file name.
        let path = config_path().expect("resolves on Windows");
        assert!(path.ends_with(r"rewynd\config.toml"), "{}", path.display());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn data_home_prefers_xdg_then_home() {
        let xdg = data_home_from(|k| match k {
            "XDG_DATA_HOME" => Some(OsString::from("/xdg-data")),
            "HOME" => Some(OsString::from("/home/u")),
            _ => None,
        });
        assert_eq!(xdg, Some(PathBuf::from("/xdg-data")));

        let home = data_home_from(|k| (k == "HOME").then(|| OsString::from("/home/u")));
        assert_eq!(home, Some(PathBuf::from("/home/u/.local/share")));

        let rel = data_home_from(|k| (k == "XDG_DATA_HOME").then(|| OsString::from("rel")));
        assert_eq!(rel, None, "relative XDG_DATA_HOME without HOME is unusable");
        assert!(data_home_from(|_| None).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn instance_dir_prefers_runtime_dir() {
        let rt = instance_dir_from(Some(OsString::from("/run/u")), PathBuf::from("/tmp"));
        assert_eq!(rt.path, PathBuf::from("/run/u/rewynd"));
        assert!(!rt.in_shared_temp);
        // Unset or relative runtime dir falls back under the temp dir, scoped per user on unix.
        #[cfg(unix)]
        let expected = {
            // SAFETY: `geteuid` is infallible and takes no arguments.
            let uid = unsafe { libc::geteuid() };
            PathBuf::from(format!("/tmp/rewynd-{uid}"))
        };
        #[cfg(not(unix))]
        let expected = PathBuf::from("/tmp/rewynd");
        let unset = instance_dir_from(None, PathBuf::from("/tmp"));
        assert_eq!(unset.path, expected);
        assert!(unset.in_shared_temp);
        let rel = instance_dir_from(Some(OsString::from("rel")), PathBuf::from("/tmp"));
        assert_eq!(rel.path, expected);
        assert!(rel.in_shared_temp);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_instance_dir_creates_a_private_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = InstanceDir {
            path: tmp.path().join("inst"),
            in_shared_temp: true,
        };
        ensure_instance_dir(&dir).expect("create");
        let mode = std::fs::metadata(&dir.path)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "instance dir is owner-only");
        ensure_instance_dir(&dir).expect("idempotent");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_instance_dir_rejects_symlinks_and_tightens_loose_modes() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");

        let real = tmp.path().join("real");
        std::fs::create_dir(&real).expect("real dir");
        let link = InstanceDir {
            path: tmp.path().join("link"),
            in_shared_temp: true,
        };
        std::os::unix::fs::symlink(&real, &link.path).expect("symlink");
        assert!(ensure_instance_dir(&link).is_err(), "symlink is rejected");

        // A loose dir WE own (an older release created it 0755) is tightened, not refused —
        // refusing would permanently disable the single-instance guard on upgrade.
        let loose = InstanceDir {
            path: tmp.path().join("loose"),
            in_shared_temp: true,
        };
        std::fs::create_dir(&loose.path).expect("dir");
        std::fs::set_permissions(&loose.path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        ensure_instance_dir(&loose).expect("owned loose dir is accepted");
        let mode = std::fs::metadata(&loose.path)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "loose mode is tightened to 0700");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_instance_dir_trusts_the_runtime_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = InstanceDir {
            path: tmp.path().join("rt").join("rewynd"),
            in_shared_temp: false,
        };
        ensure_instance_dir(&dir).expect("create without the ownership dance");
        assert!(dir.path.is_dir());
    }

    #[test]
    fn default_output_dir_does_not_panic() {
        // Thin wrapper over a platform call; just exercise it (the result is environment-specific
        // and may be None on a headless box, which is a valid outcome).
        let _ = default_output_dir();
    }

    #[test]
    fn sibling_of_joins_the_parent_dir() {
        let sib = sibling_of(Path::new("/opt/rewynd/rewynd-settings"), "rewynd")
            .expect("exe has a parent");
        #[cfg(windows)]
        assert_eq!(sib, PathBuf::from("/opt/rewynd/rewynd.exe"));
        #[cfg(not(windows))]
        assert_eq!(sib, PathBuf::from("/opt/rewynd/rewynd"));
        assert_eq!(sibling_of(Path::new("/"), "rewynd"), None, "no parent");
    }
}
