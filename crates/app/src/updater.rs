//! Background auto-updates (Velopack installs only; dev runs and package-manager
//! installs have no receipt, so every entry point here is inert for them).
//!
//! The recorder never interrupts a session to update: a new release is only
//! downloaded in the background, and a previously downloaded one is applied at the
//! next recorder start, before any capture begins. Applying restarts through the
//! GUI binary's `--recorder` hand-off (the package's main exe is the GUI), so no
//! settings window appears at boot. While a settings window is open the apply is
//! skipped entirely: Velopack's updater force-kills every process left in the
//! install dir, and an open window must not die that way.

use std::time::Duration;

use rewynd_config::Config;

/// The GitHub repo whose Releases are the update feed (the settings app's manual
/// "Check for updates" reads the same feed).
const UPDATE_REPO: &str = "https://github.com/gankedtv/rewynd";

/// Delay before the first background check, leaving startup I/O and the first
/// capture frames undisturbed.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(2 * 60);

/// Interval between background checks while the recorder stays up. Anonymous
/// GitHub API calls are limited to 60/h per IP; one a day is nothing.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The update manager for this install, or `None` outside a real Velopack install
/// (no receipt): dev runs and package-manager installs land there.
fn update_manager() -> Option<velopack::UpdateManager> {
    // Prereleases are offered only to a prerelease build, matching the settings app.
    let source = velopack::sources::GithubSource::new(
        UPDATE_REPO,
        None,
        env!("CARGO_PKG_VERSION").contains('-'),
    );
    velopack::UpdateManager::new(source, None, None).ok()
}

/// Install an update a previous run downloaded, restarting the recorder on the new
/// version. Called after the single-instance lock and before the capture pipeline
/// exists, so nothing is mid-write when the process is replaced. A no-op unless the
/// config allows it, an update is actually pending, and no settings window is open.
pub(crate) fn apply_pending_update(config: &Config) {
    if !config.auto_install_updates() {
        return;
    }
    let Some(manager) = update_manager() else {
        return;
    };
    let Some(pending) = manager.get_update_pending_restart() else {
        return;
    };
    if rewynd_config::settings_running() {
        tracing::info!(
            version = %pending.Version,
            "an update is ready, but a settings window is open; it installs at the next start"
        );
        return;
    }
    tracing::info!(version = %pending.Version, "installing the downloaded update");
    // On success this exits the process; the updater relaunches the GUI with
    // `--recorder`, which hands straight off to the new recorder without a window.
    if let Err(e) = manager.apply_updates_and_restart_with_args(&pending, ["--recorder"]) {
        tracing::warn!(error = %e, "could not install the downloaded update; continuing on this version");
    }
}

/// Check for and download updates in the background, never applying them: a
/// download installs at the next recorder start (or right away via the settings
/// window). The thread is detached and dies with the process. A no-op unless the
/// config allows it and this is a Velopack install.
pub(crate) fn spawn_background_check(config: &Config) {
    if !config.auto_install_updates() || update_manager().is_none() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(|| {
            std::thread::sleep(FIRST_CHECK_DELAY);
            loop {
                check_and_download();
                std::thread::sleep(CHECK_INTERVAL);
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the background update check");
    }
}

/// One check-and-download pass. Network failures are routine (an offline boot), so
/// they log at debug and the next interval retries.
fn check_and_download() {
    let Some(manager) = update_manager() else {
        return;
    };
    match manager.check_for_updates() {
        Ok(velopack::UpdateCheck::UpdateAvailable(info)) => {
            let version = info.TargetFullRelease.Version.clone();
            tracing::info!(%version, "downloading an update in the background");
            match manager.download_updates(&info, None) {
                Ok(()) => tracing::info!(
                    %version,
                    "update downloaded; it installs at the next recorder start"
                ),
                Err(e) => tracing::warn!(error = %e, "could not download the update"),
            }
        }
        Ok(_) => tracing::debug!("no update available"),
        Err(e) => tracing::debug!(error = %e, "update check failed"),
    }
}
