//! H.264 Annex-B → MP4 muxing with real PTS from capture timestamps (PLAN §4.3, §6.4).
//!
//! We stamp PTS from capture timestamps and write them into the container so players
//! don't guess the framerate. The muxer-crate-vs-`ffmpeg`-binary choice (ADR-worthy)
//! and the implementation land in #12.

use std::path::Path;

use rewynd_buffer::EncodedChunk;
use thiserror::Error;

/// Errors from muxing.
#[derive(Debug, Error)]
pub enum MuxError {
    /// The chunk sequence did not start on a keyframe, so the file would not be playable.
    #[error("clip does not start on a keyframe")]
    NotKeyframeStart,
    /// Underlying I/O error while writing the container.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Writes encoded chunks into a container file with correct timestamps.
pub trait Muxer {
    /// Mux `chunks` — which must begin on an IDR — into an MP4 at `path`, stamping PTS
    /// from each chunk's timestamp.
    fn write_mp4(&mut self, chunks: &[EncodedChunk], path: &Path) -> Result<(), MuxError>;
}

/// MP4 muxer (Annex-B → AVCC). Implemented in #12.
#[derive(Debug, Default)]
pub struct Mp4Muxer;

impl Muxer for Mp4Muxer {
    fn write_mp4(&mut self, chunks: &[EncodedChunk], path: &Path) -> Result<(), MuxError> {
        let _ = (chunks, path);
        todo!("Annex-B → AVCC MP4 muxing with capture PTS — issue #12")
    }
}
