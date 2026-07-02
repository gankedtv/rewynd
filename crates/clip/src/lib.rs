//! The clip save path and the clip store.
//!
//! [`ClipSaver`] (feature `saver`, default) cuts the video + audio rings, picks an output path,
//! and muxes to MP4 — extracted from the recorder binary so the logic every trigger shares
//! (hotkey, tray, dev flush hook) exists once, holds its own state, and is CI-testable. All its
//! methods are blocking; async callers run [`ClipSaver::save`] on a blocking thread.
//!
//! The [`store`] side (always on) owns where clips live and what they are called: output-dir
//! resolution, the `rewynd-<millis>-<seq>.mp4` naming, and listing saved clips. The settings
//! window depends on the crate with `default-features = false` for just that, keeping the
//! ring-buffer/encoder dependencies (and their GPU tree) out of its build.

use std::sync::{Mutex, MutexGuard};

mod store;
pub use store::{ClipEntry, clips_dir, list_clips};

#[cfg(feature = "saver")]
mod saver;
#[cfg(feature = "saver")]
pub use saver::{ClipSaver, SaveError, SharedAudioBuffer, SharedBuffer};

/// Lock a mutex, recovering a poisoned one: the rings must stay usable even if some holder
/// panicked, and a panic across the PipeWire callback boundary would be undefined behaviour.
pub fn lock_unpoisoned<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
