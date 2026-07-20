//! Background auto-updates (Velopack installs only; without a receipt every entry point
//! is inert). Downloads happen in the background; a downloaded update is applied only at
//! the next recorder start, never mid-session.

use std::time::Duration;

use rewynd_config::Config;

/// The update feed; the settings app's manual check reads the same repo.
const UPDATE_REPO: &str = "https://github.com/gankedtv/rewynd";

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(2 * 60);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// `None` outside a real Velopack install (dev runs, package managers).
fn update_manager() -> Option<velopack::UpdateManager> {
    // Prerelease builds track the prerelease channel, matching the settings app.
    let source = velopack::sources::GithubSource::new(
        UPDATE_REPO,
        None,
        env!("CARGO_PKG_VERSION").contains('-'),
    );
    velopack::UpdateManager::new(source, None, None).ok()
}

/// Apply a previously downloaded update and restart the recorder on the new version.
/// Call after the single-instance lock, before the capture pipeline exists.
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
    // Velopack's apply force-kills processes in the install dir; spare an open window.
    if rewynd_config::settings_running() {
        tracing::info!(version = %pending.Version, "update ready; deferred while a settings window is open");
        return;
    }
    tracing::info!(version = %pending.Version, "installing the downloaded update");
    // The updater restarts the package's main exe (the GUI); --recorder hands off windowless.
    if let Err(e) = manager.apply_updates_and_restart_with_args(&pending, ["--recorder"]) {
        tracing::warn!(error = %e, "could not install the downloaded update");
    }
}

/// Check and download on a detached daily timer; applying waits for the next start.
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

fn check_and_download() {
    let Some(manager) = update_manager() else {
        return;
    };
    match manager.check_for_updates() {
        Ok(velopack::UpdateCheck::UpdateAvailable(info)) => {
            let version = info.TargetFullRelease.Version.clone();
            tracing::info!(%version, "downloading an update in the background");
            match manager.download_updates(&info, None) {
                Ok(()) => {
                    tracing::info!(%version, "update downloaded; installs at the next recorder start")
                }
                Err(e) => tracing::warn!(error = %e, "could not download the update"),
            }
        }
        Ok(_) => tracing::debug!("no update available"),
        // Offline boots are routine; the next interval retries.
        Err(e) => tracing::debug!(error = %e, "update check failed"),
    }
}
