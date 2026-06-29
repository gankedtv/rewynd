//! Keyframe-aware ring buffer — the pure-Rust core of rewynd (PLAN §4.3, §6.3).
//!
//! Holds roughly `window` of encoded H.264 chunks, drops the oldest as new ones
//! arrive, and cuts a clip starting at the most recent IDR boundary so the
//! result is self-decodable. This crate has **no GPU or driver dependency**, so the
//! interesting logic is fully unit-testable.

use std::collections::VecDeque;
use std::time::Duration;

use thiserror::Error;

/// A single encoded H.264 access unit: the encoder's output and the buffer's stored unit.
#[derive(Debug, Clone)]
pub struct EncodedChunk {
    /// Annex-B encoded bytes for one frame (with inline SPS/PPS before IDRs).
    pub bytes: Vec<u8>,
    /// Whether this chunk begins with an IDR (keyframe) — a valid clip start.
    pub is_keyframe: bool,
    /// Presentation timestamp relative to the start of capture.
    pub pts: Duration,
}

/// Errors returned by [`RingBuffer`].
#[derive(Debug, Error)]
pub enum BufferError {
    /// No keyframe exists within the requested window, so no self-decodable cut is possible.
    #[error("no keyframe within the requested {0:?} window")]
    NoKeyframe(Duration),
    /// The keyframe-aware cut is not yet implemented.
    #[error("ring-buffer flush is not yet implemented")]
    NotImplemented,
}

/// A time-bounded ring of encoded chunks.
///
/// The keyframe-aware cut ([`flush_last`](RingBuffer::flush_last)) is implemented
/// later; the scaffold provides storage and time-based eviction.
#[derive(Debug)]
pub struct RingBuffer {
    window: Duration,
    chunks: VecDeque<EncodedChunk>,
}

impl RingBuffer {
    /// Create a buffer that retains roughly `window` of footage.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            chunks: VecDeque::new(),
        }
    }

    /// Append a freshly encoded chunk, evicting any chunk older than `window`
    /// relative to the newest chunk.
    pub fn push(&mut self, chunk: EncodedChunk) {
        let newest = chunk.pts;
        self.chunks.push_back(chunk);
        while let Some(front) = self.chunks.front() {
            if newest.saturating_sub(front.pts) > self.window {
                self.chunks.pop_front();
            } else {
                break;
            }
        }
    }

    /// Return the chunks for a clip covering up to `duration`, starting at the most
    /// recent IDR within that window so the clip is self-decodable.
    ///
    /// The real keyframe-aware walk lands later.
    pub fn flush_last(&self, duration: Duration) -> Result<Vec<EncodedChunk>, BufferError> {
        // The keyframe-aware cut from the most recent IDR lands later.
        let _ = duration;
        Err(BufferError::NotImplemented)
    }

    /// Number of buffered chunks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Whether the buffer holds no chunks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// The retention window this buffer was created with.
    #[must_use]
    pub fn window(&self) -> Duration {
        self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(secs: u64, keyframe: bool) -> EncodedChunk {
        EncodedChunk {
            bytes: vec![0; 4],
            is_keyframe: keyframe,
            pts: Duration::from_secs(secs),
        }
    }

    #[test]
    fn evicts_chunks_older_than_window() {
        let mut buf = RingBuffer::new(Duration::from_secs(60));
        assert!(buf.is_empty());
        for s in 0..130 {
            buf.push(chunk(s, s % 60 == 0));
        }
        // Newest pts is 129s; chunks more than 60s older (pts < 69s) are evicted,
        // leaving pts 69..=129 inclusive.
        assert_eq!(buf.len(), 61);
    }

    #[test]
    fn getters_reflect_state() {
        let window = Duration::from_secs(30);
        let mut buf = RingBuffer::new(window);
        assert_eq!(buf.window(), window);
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());

        buf.push(chunk(0, true));
        assert_eq!(buf.len(), 1);
        assert!(!buf.is_empty());
    }

    #[test]
    fn flush_last_is_not_yet_implemented() {
        let buf = RingBuffer::new(Duration::from_secs(60));
        let err = buf.flush_last(Duration::from_secs(10)).unwrap_err();
        assert!(matches!(err, BufferError::NotImplemented));
    }

    #[test]
    fn error_variants_display() {
        assert_eq!(
            BufferError::NotImplemented.to_string(),
            "ring-buffer flush is not yet implemented"
        );
        assert_eq!(
            BufferError::NoKeyframe(Duration::from_secs(5)).to_string(),
            "no keyframe within the requested 5s window"
        );
    }
}
