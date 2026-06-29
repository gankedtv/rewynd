//! ganked.tv upload client (PLAN §8, Phase 8) — a **later** feature, stubbed for now.
//!
//! Sequencing is deliberate: ship the general-purpose recorder first, then add
//! hotkey → clip → auto-upload as a UI-triggered feature. Encode H.264
//! (browser-compatible) for this path.

use std::path::Path;

use thiserror::Error;

/// Errors from uploading a clip.
#[derive(Debug, Error)]
pub enum UploadError {
    /// The upload feature is not yet implemented (Phase 8).
    #[error("ganked.tv upload not yet implemented")]
    NotImplemented,
}

/// Client for uploading finished clips to ganked.tv.
#[derive(Debug, Default)]
pub struct GankedClient;

impl GankedClient {
    /// Upload a finished clip file to ganked.tv.
    pub async fn upload(&self, path: &Path) -> Result<(), UploadError> {
        let _ = path;
        Err(UploadError::NotImplemented)
    }
}
