//! Background auto-updates (Velopack installs only; without a receipt every entry point
//! is inert). A downloaded update is applied only at a recorder start — either one waiting
//! from an earlier session or one fetched by a short bounded check at boot; never mid-session.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use rewynd_config::Config;

/// The update feed; the settings app's manual check reads the same repo.
const UPDATE_REPO: &str = "https://github.com/gankedtv/rewynd";

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(2 * 60);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a boot waits for the feed alone; a blackholed request must not cost the full budget.
const BOOT_CHECK_WAIT: Duration = Duration::from_secs(10);
/// The whole boot budget, only ever reached while a download is actually running.
const BOOT_UPDATE_WAIT: Duration = Duration::from_secs(90);

/// How far the boot check has got, reported to the waiting main thread.
enum BootPhase {
    NoUpdate,
    Available,
    Downloaded(bool),
}

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

/// Bring the recorder onto the newest release before it starts capturing: apply a download left
/// by an earlier session, or fetch one — [`BOOT_CHECK_WAIT`] for the feed to answer, at most
/// [`BOOT_UPDATE_WAIT`] in total once a download is running. Call after the single-instance lock,
/// before the capture pipeline exists.
pub(crate) fn update_at_boot(config: &Config) {
    if !config.auto_install_updates() {
        return;
    }
    let Some(manager) = update_manager() else {
        return;
    };
    if apply_pending(&manager) {
        return;
    }
    // With a window open the apply would be deferred anyway, so don't spend boot on a check.
    if rewynd_config::settings_running() {
        return;
    }
    // A mid-session relaunch must not wait on the network; the daily check covers it.
    if std::env::args().any(|a| a == "--restart") {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let checker = manager.clone();
    let spawned = std::thread::Builder::new()
        .name("boot-update".into())
        .spawn(move || {
            check_and_download(&checker, |phase| {
                let _ = tx.send(phase);
            });
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the boot update check");
        return;
    }
    let start = Instant::now();
    let mut deadline = start + BOOT_CHECK_WAIT;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(1)) {
            // The feed answered; what is left of the budget is the download's.
            Ok(BootPhase::Available) => deadline = start + BOOT_UPDATE_WAIT,
            Ok(BootPhase::Downloaded(true)) => {
                apply_pending(&manager);
                return;
            }
            Ok(BootPhase::NoUpdate | BootPhase::Downloaded(false))
            | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // A window opened meanwhile: the apply would be deferred anyway.
        if rewynd_config::settings_running() {
            break;
        }
    }
    // The thread runs on: a download that lands late still installs at the next start.
    tracing::debug!("no update installed at boot");
}

/// Apply a downloaded update and restart the recorder on the new version. `true` when a
/// download was waiting — applied, deliberately deferred, or failed — so the caller knows
/// there is nothing fresh left to fetch.
fn apply_pending(manager: &velopack::UpdateManager) -> bool {
    let Some(pending) = manager.get_update_pending_restart() else {
        return false;
    };
    // Velopack's apply force-kills processes in the install dir; spare an open window.
    if rewynd_config::settings_running() {
        tracing::info!(version = %pending.Version, "update ready; deferred while a settings window is open");
        return true;
    }
    tracing::info!(version = %pending.Version, "installing the downloaded update");
    // The updater restarts the package's main exe (the GUI); --recorder hands off windowless.
    if let Err(e) = manager.apply_updates_and_restart_with_args(&pending, ["--recorder"]) {
        tracing::warn!(error = %e, "could not install the downloaded update");
    }
    true
}

/// Check and download on a detached daily timer; applying waits for the next start.
pub(crate) fn spawn_background_check(config: &Config) {
    if !config.auto_install_updates() {
        return;
    }
    let Some(manager) = update_manager() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            std::thread::sleep(FIRST_CHECK_DELAY);
            loop {
                check_and_download(&manager, |_| {});
                std::thread::sleep(CHECK_INTERVAL);
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the background update check");
    }
}

/// Fetch the feed and download whatever it offers, reporting each step to `notify` (the daily
/// timer has nobody listening). Velopack takes an exclusive lock and skips an already-downloaded
/// package, so overlapping calls are safe.
fn check_and_download(manager: &velopack::UpdateManager, notify: impl Fn(BootPhase)) {
    match manager.check_for_updates() {
        Ok(velopack::UpdateCheck::UpdateAvailable(info)) => {
            notify(BootPhase::Available);
            let version = info.TargetFullRelease.Version.clone();
            tracing::info!(%version, "downloading an update in the background");
            let downloaded = match manager.download_updates(&info, None) {
                Ok(()) => {
                    tracing::info!(%version, "update downloaded");
                    true
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not download the update");
                    false
                }
            };
            notify(BootPhase::Downloaded(downloaded));
        }
        Ok(_) => {
            tracing::debug!("no update available");
            notify(BootPhase::NoUpdate);
        }
        // Offline boots are routine; the next interval retries.
        Err(e) => {
            tracing::debug!(error = %e, "update check failed");
            notify(BootPhase::NoUpdate);
        }
    }
}
