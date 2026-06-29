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
    /// The buffer holds no keyframe, so no self-decodable clip can be cut.
    #[error("no keyframe within the requested {0:?} window")]
    NoKeyframe(Duration),
}

/// A time-bounded ring of encoded chunks with a keyframe-aware cut
/// ([`flush_last`](RingBuffer::flush_last)).
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

    /// Cut a self-decodable clip of the most recent ~`duration` of footage, returning
    /// its chunks oldest-first.
    ///
    /// The clip starts on an IDR so it decodes standalone (encoded with inline
    /// SPS/PPS). To avoid losing the moment, the start is the most recent IDR at or
    /// before `newest_pts - duration`, so the clip spans *at least* `duration` when
    /// the buffer reaches that far back; otherwise it starts at the earliest retained
    /// IDR (the whole buffer from its first keyframe). Returns [`BufferError::NoKeyframe`]
    /// only when the buffer holds no IDR at all.
    pub fn flush_last(&self, duration: Duration) -> Result<Vec<EncodedChunk>, BufferError> {
        let newest = self
            .chunks
            .back()
            .ok_or(BufferError::NoKeyframe(duration))?
            .pts;
        let cutoff = newest.saturating_sub(duration);

        // Prefer the most recent IDR at or before the cutoff (covers >= duration); fall
        // back to the earliest retained IDR when none is that old.
        let start = self
            .chunks
            .iter()
            .enumerate()
            .rfind(|(_, c)| c.is_keyframe && c.pts <= cutoff)
            .map(|(i, _)| i)
            .or_else(|| self.chunks.iter().position(|c| c.is_keyframe))
            .ok_or(BufferError::NoKeyframe(duration))?;

        Ok(self.chunks.iter().skip(start).cloned().collect())
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

    /// Collect the `pts` (in whole seconds) of each chunk in a flushed clip.
    fn pts_secs(clip: &[EncodedChunk]) -> Vec<u64> {
        clip.iter().map(|c| c.pts.as_secs()).collect()
    }

    #[test]
    fn flush_starts_at_the_idr_before_the_cutoff() {
        // IDR every 2s, P-frames on the odd seconds; newest = 10s.
        let mut buf = RingBuffer::new(Duration::from_secs(60));
        for s in 0..=10 {
            buf.push(chunk(s, s % 2 == 0));
        }
        // duration 5s → cutoff 5s. The most recent IDR at/before 5s is the one at 4s,
        // so the clip is 4..=10 (spans 6s ≥ the requested 5s) and starts on a keyframe.
        let clip = buf.flush_last(Duration::from_secs(5)).unwrap();
        assert!(clip.first().unwrap().is_keyframe);
        assert_eq!(pts_secs(&clip), vec![4, 5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn flush_with_window_larger_than_buffer_returns_from_earliest_idr() {
        // Buffer starts with a couple of orphan P-frames (their IDR was evicted),
        // then an IDR at 2s. A long request can't reach back `duration`, so the clip
        // starts at the earliest retained IDR and excludes the undecodable P-frames.
        let mut buf = RingBuffer::new(Duration::from_secs(60));
        buf.push(chunk(0, false));
        buf.push(chunk(1, false));
        for s in 2..=5 {
            buf.push(chunk(s, s == 2));
        }
        let clip = buf.flush_last(Duration::from_secs(60)).unwrap();
        assert_eq!(pts_secs(&clip), vec![2, 3, 4, 5]);
    }

    #[test]
    fn flush_after_eviction_uses_the_surviving_idr() {
        // Mirrors the eviction test: IDRs at 0/60/120, window 60s. After pushing up to
        // 129s the only surviving IDR is at 120s, so a 60s flush yields 120..=129.
        let mut buf = RingBuffer::new(Duration::from_secs(60));
        for s in 0..130 {
            buf.push(chunk(s, s % 60 == 0));
        }
        let clip = buf.flush_last(Duration::from_secs(60)).unwrap();
        assert_eq!(clip.first().unwrap().pts, Duration::from_secs(120));
        assert_eq!(clip.len(), 10);
    }

    #[test]
    fn flush_without_any_keyframe_errors() {
        let mut buf = RingBuffer::new(Duration::from_secs(60));
        for s in 0..5 {
            buf.push(chunk(s, false));
        }
        let err = buf.flush_last(Duration::from_secs(10)).unwrap_err();
        assert!(matches!(err, BufferError::NoKeyframe(_)));
    }

    #[test]
    fn flush_empty_buffer_errors() {
        let buf = RingBuffer::new(Duration::from_secs(60));
        assert!(matches!(
            buf.flush_last(Duration::from_secs(10)).unwrap_err(),
            BufferError::NoKeyframe(_)
        ));
    }

    #[test]
    fn error_variant_displays() {
        assert_eq!(
            BufferError::NoKeyframe(Duration::from_secs(5)).to_string(),
            "no keyframe within the requested 5s window"
        );
    }
}
