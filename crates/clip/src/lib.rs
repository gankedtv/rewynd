//! The clip save path.
//!
//! [`ClipSaver`] cuts the video + audio rings, picks an output path, and muxes to MP4 —
//! extracted from the recorder binary so the logic every trigger shares (hotkey, tray, dev
//! flush hook) exists once, holds its own state, and is CI-testable. All its methods are
//! blocking; async callers run [`ClipSaver::save`] on a blocking thread.
//!
//! Where clips live and what they are called (output-dir resolution, naming, listing) is
//! `rewynd-config`'s clip store, which this crate saves through — the settings window uses
//! the store directly, without this crate's ring-buffer/encoder (GPU-adjacent) dependencies.

use std::sync::{Mutex, MutexGuard};

mod saver;
pub use saver::{ClipSaver, SaveError, SharedAudioBuffer, SharedBuffer};

/// Lock a mutex, recovering a poisoned one: the rings must stay usable even if some holder
/// panicked, and a panic across the PipeWire callback boundary would be undefined behaviour.
pub fn lock_unpoisoned<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
