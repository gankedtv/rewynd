//! XDG ScreenCast portal handshake via `ashpd`.
//!
//! Drives the portal flow (PLAN §3.5):
//! `create_session` → `select_sources` → `start` (interactive share dialog) →
//! `open_pipe_wire_remote`. The resulting PipeWire node id + remote fd are handed
//! to [`super::pipewire_capture`] for stream negotiation.
//!
//! The `start` step pops an interactive share-picker (KDE/GNOME) the first time.
//! We persist the returned restore token to `$XDG_STATE_HOME/rewynd/screencast.token`
//! (falling back to `~/.local/state/...`) so subsequent runs skip the dialog.

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use ashpd::desktop::{
    PersistMode, Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use ashpd::enumflags2::BitFlags;

use crate::CaptureError;

/// A live screencast session.
///
/// The portal [`Session`] MUST be kept alive for the whole capture: dropping it
/// tears down the portal session and the PipeWire node disappears. Hold on to
/// this `PortalSession` value until capture is finished.
pub struct PortalSession {
    /// The portal session handle. Kept alive so the negotiated PipeWire node remains
    /// valid for the capture's lifetime; [`close`](PortalSession::close) ends it.
    session: Session<Screencast>,
    /// The PipeWire global node id of the selected stream.
    pub node_id: u32,
    /// An open fd to the PipeWire remote (pass to `Context::connect_fd`). Taken
    /// out via [`PortalSession::take_fd`].
    fd: Option<OwnedFd>,
    /// The stream size `(width, height)` in the compositor coordinate space, if
    /// the portal reported it (monitor streams only).
    pub size: Option<(i32, i32)>,
    /// The stream's top-left `(x, y)` in the compositor coordinate space, if the portal reported it
    /// (monitor streams only). Identifies which monitor is captured, so overlays can target it.
    pub position: Option<(i32, i32)>,
}

impl PortalSession {
    /// The raw fd of the PipeWire remote, for logging. Returns `-1` if the fd has
    /// already been taken.
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_ref().map_or(-1, |fd| fd.as_raw_fd())
    }

    /// Take ownership of the PipeWire remote fd (to hand to `connect_fd`). The
    /// [`PortalSession`] keeps the portal `Session` alive after this; do not drop
    /// the `PortalSession` until capture is complete.
    ///
    /// # Panics
    /// Panics if called twice.
    pub fn take_fd(&mut self) -> OwnedFd {
        self.fd.take().expect("PipeWire fd already taken")
    }

    /// Close the portal session, tearing down the PipeWire node so any consumer
    /// stream errors out and stops. Call this at shutdown to end the capture.
    ///
    /// The session has no `Drop`-based teardown (the portal's `Close` is an explicit
    /// call), so dropping a [`PortalSession`] leaves the screencast running until the
    /// shared D-Bus connection itself goes away.
    pub async fn close(self) -> Result<(), CaptureError> {
        self.session
            .close()
            .await
            .map_err(|e| CaptureError::Portal(format!("close screencast session: {e}")))
    }
}

/// Location of the persisted restore token.
fn token_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".local/state")
        });
    base.join("rewynd").join("screencast.token")
}

/// Read the persisted restore token, if any.
fn load_token() -> Option<String> {
    let path = token_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let s = s.trim().to_owned();
            if s.is_empty() { None } else { Some(s) }
        }
        Err(_) => None,
    }
}

/// Persist a restore token for the next run (best-effort; logs on failure).
fn save_token(token: &str) {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let path = token_path();
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, path = %parent.display(), "could not create token dir");
        return;
    }
    // The restore token enables prompt-less screen capture, so treat it as a secret:
    // create it owner-only (0600).
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(token.as_bytes()));
    match result {
        Ok(()) => {
            // Tighten perms even if the file pre-existed with a looser mode.
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            tracing::info!(path = %path.display(), "persisted screencast restore token");
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "could not persist restore token");
        }
    }
}

/// Run the full portal handshake and return a live [`PortalSession`], reusing the saved
/// restore token so the share dialog is skipped after the first run. See
/// [`open_portal_with`] to force the picker.
pub async fn open_portal() -> Result<PortalSession, CaptureError> {
    open_portal_with(false).await
}

/// Run the full portal handshake and return a live [`PortalSession`].
///
/// This awaits an interactive share-picker dialog on the first run (and whenever
/// the saved restore token is rejected). Must be called inside a tokio runtime
/// (ashpd uses the tokio reactor by default).
///
/// `force_picker` ignores any saved restore token so the monitor picker is shown again,
/// letting the user select a different monitor; the new selection is persisted for next time.
pub async fn open_portal_with(force_picker: bool) -> Result<PortalSession, CaptureError> {
    let proxy = Screencast::new()
        .await
        .map_err(|e| CaptureError::Portal(format!("connect to ScreenCast portal: {e}")))?;

    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| CaptureError::Portal(format!("create_session: {e}")))?;

    let saved_token = if force_picker { None } else { load_token() };
    if force_picker {
        tracing::info!("forcing the share dialog (monitor re-selection requested)");
    } else if saved_token.is_some() {
        tracing::info!("found saved restore token; the share dialog may be skipped");
    }

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_sources(BitFlags::from(SourceType::Monitor))
                .set_cursor_mode(CursorMode::Embedded)
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(saved_token.as_deref()),
        )
        .await
        .map_err(|e| CaptureError::Portal(format!("select_sources: {e}")))?;

    let streams = proxy
        .start(&session, None, Default::default())
        .await
        .map_err(|e| CaptureError::Portal(format!("start request: {e}")))?
        .response()
        .map_err(|e| match e {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
                CaptureError::Cancelled
            }
            other => CaptureError::Portal(format!("start response: {other}")),
        })?;

    // Persist the restore token so the next run can skip the dialog.
    if let Some(token) = streams.restore_token() {
        save_token(token);
    }

    let stream = streams
        .streams()
        .first()
        .ok_or_else(|| CaptureError::Portal("portal returned no streams".to_owned()))?;

    let node_id = stream.pipe_wire_node_id();
    let size = stream.size();
    let position = stream.position();

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| CaptureError::Portal(format!("open_pipe_wire_remote: {e}")))?;

    Ok(PortalSession {
        session,
        node_id,
        fd: Some(fd),
        size,
        position,
    })
}
