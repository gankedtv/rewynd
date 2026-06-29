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
    /// The portal session handle. Kept alive (not otherwise read) so the
    /// negotiated PipeWire node remains valid for the capture's lifetime.
    _session: Session<Screencast>,
    /// The PipeWire global node id of the selected stream.
    pub node_id: u32,
    /// An open fd to the PipeWire remote (pass to `Context::connect_fd`). Taken
    /// out via [`PortalSession::take_fd`].
    fd: Option<OwnedFd>,
    /// The stream size `(width, height)` in the compositor coordinate space, if
    /// the portal reported it (monitor streams only).
    pub size: Option<(i32, i32)>,
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
    let path = token_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %e, path = %parent.display(), "could not create token dir");
            return;
        }
    }
    if let Err(e) = std::fs::write(&path, token) {
        tracing::warn!(error = %e, path = %path.display(), "could not persist restore token");
    } else {
        tracing::info!(path = %path.display(), "persisted screencast restore token");
    }
}

/// Run the full portal handshake and return a live [`PortalSession`].
///
/// This awaits an interactive share-picker dialog on the first run (and whenever
/// the saved restore token is rejected). Must be called inside a tokio runtime
/// (ashpd uses the tokio reactor by default).
pub async fn open_portal() -> Result<PortalSession, CaptureError> {
    let proxy = Screencast::new()
        .await
        .map_err(|e| CaptureError::Portal(format!("connect to ScreenCast portal: {e}")))?;

    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| CaptureError::Portal(format!("create_session: {e}")))?;

    let saved_token = load_token();
    if saved_token.is_some() {
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

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| CaptureError::Portal(format!("open_pipe_wire_remote: {e}")))?;

    Ok(PortalSession {
        _session: session,
        node_id,
        fd: Some(fd),
        size,
    })
}
