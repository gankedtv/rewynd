//! Keyframe-aware ring buffer — the pure-Rust core of rewynd (PLAN §4.3, §6.3).
//!
//! Holds roughly `window` of encoded H.264 chunks, drops the oldest as new ones
//! arrive, and cuts a clip starting at the most recent IDR boundary so the
//! result is self-decodable. This crate has **no GPU or driver dependency**, so the
//! interesting logic is fully unit-testable.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

/// A single encoded H.264 access unit: the encoder's output and the buffer's stored unit.
#[derive(Debug, Clone)]
pub struct EncodedChunk {
    /// Annex-B encoded bytes for one frame (with inline SPS/PPS before IDRs). Shared so
    /// cutting a clip clones ref-counts, not payloads — the cut happens under the ring
    /// mutex on the capture thread's lock, so it must stay O(chunks).
    pub bytes: Arc<[u8]>,
    /// Whether this chunk begins with an IDR (keyframe) — a valid clip start.
    pub is_keyframe: bool,
    /// Presentation timestamp relative to the start of capture.
    pub pts: Duration,
}

/// A single encoded Opus packet: the audio encoder's output and the audio ring's unit.
#[derive(Debug, Clone)]
pub struct EncodedAudioChunk {
    /// One bare Opus packet (no framing/length prefix). Shared for the same
    /// O(chunks)-cut reason as [`EncodedChunk::bytes`].
    pub bytes: Arc<[u8]>,
    /// Samples per channel the packet decodes to — its duration in 48 kHz audio samples.
    pub frames: u32,
    /// Capture-relative PTS of the packet's first sample, on the *same* monotonic clock as
    /// [`EncodedChunk::pts`], so the audio and video tracks align.
    pub pts: Duration,
}

/// Errors returned by [`RingBuffer`].
#[derive(Debug, Error)]
pub enum BufferError {
    /// The buffer holds no keyframe, so no self-decodable clip can be cut.
    #[error("no keyframe within the requested {0:?} window")]
    NoKeyframe(Duration),
}

/// PTS accessor shared by the ring's element types.
trait HasPts {
    fn pts(&self) -> Duration;
}

impl HasPts for EncodedChunk {
    fn pts(&self) -> Duration {
        self.pts
    }
}

impl HasPts for EncodedAudioChunk {
    fn pts(&self) -> Duration {
        self.pts
    }
}

/// The shared time-bounded ring: retains roughly `window` of items, evicting the oldest
/// relative to the newest item's PTS. The public wrappers add their cut semantics on top.
#[derive(Debug)]
struct TimeRing<T> {
    window: Duration,
    chunks: VecDeque<T>,
}

impl<T: HasPts> TimeRing<T> {
    fn new(window: Duration) -> Self {
        Self {
            window,
            chunks: VecDeque::new(),
        }
    }

    fn push(&mut self, chunk: T) {
        let newest = chunk.pts();
        self.chunks.push_back(chunk);
        while let Some(front) = self.chunks.front() {
            if newest.saturating_sub(front.pts()) > self.window {
                self.chunks.pop_front();
            } else {
                break;
            }
        }
    }

    fn len(&self) -> usize {
        self.chunks.len()
    }

    fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    fn window(&self) -> Duration {
        self.window
    }
}

/// A time-bounded ring of encoded chunks with a keyframe-aware cut
/// ([`flush_last`](RingBuffer::flush_last)).
#[derive(Debug)]
pub struct RingBuffer {
    ring: TimeRing<EncodedChunk>,
}

impl RingBuffer {
    /// Create a buffer that retains roughly `window` of footage.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            ring: TimeRing::new(window),
        }
    }

    /// Append a freshly encoded chunk, evicting any chunk older than `window`
    /// relative to the newest chunk.
    pub fn push(&mut self, chunk: EncodedChunk) {
        self.ring.push(chunk);
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
        let chunks = &self.ring.chunks;
        let newest = chunks.back().ok_or(BufferError::NoKeyframe(duration))?.pts;
        let cutoff = newest.saturating_sub(duration);

        // Prefer the most recent IDR at or before the cutoff (covers >= duration); fall
        // back to the earliest retained IDR when none is that old.
        let start = chunks
            .iter()
            .enumerate()
            .rfind(|(_, c)| c.is_keyframe && c.pts <= cutoff)
            .map(|(i, _)| i)
            .or_else(|| chunks.iter().position(|c| c.is_keyframe))
            .ok_or(BufferError::NoKeyframe(duration))?;

        Ok(chunks.iter().skip(start).cloned().collect())
    }

    /// Number of buffered chunks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Whether the buffer holds no chunks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// The retention window this buffer was created with.
    #[must_use]
    pub fn window(&self) -> Duration {
        self.ring.window()
    }
}

/// A time-bounded ring of encoded audio packets, parallel to [`RingBuffer`].
///
/// Opus packets have no keyframe concept — any packet is a valid clip start — so the cut is
/// a plain time window ([`flush_from`](AudioRingBuffer::flush_from)) rather than the
/// keyframe-aware cut the video ring needs.
#[derive(Debug)]
pub struct AudioRingBuffer {
    ring: TimeRing<EncodedAudioChunk>,
}

impl AudioRingBuffer {
    /// Create a buffer that retains roughly `window` of audio.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            ring: TimeRing::new(window),
        }
    }

    /// Append a freshly encoded packet, evicting any packet older than `window` relative to
    /// the newest one (same eviction policy as [`RingBuffer::push`]).
    pub fn push(&mut self, chunk: EncodedAudioChunk) {
        self.ring.push(chunk);
    }

    /// Return every retained packet whose PTS is at or after `start`, oldest-first — the
    /// audio covering a clip whose video begins at `start`. Filtering on `pts >= start`
    /// keeps the audio track's rebased start non-negative and preserves the small real
    /// offset between the clip start and the first audio packet (so lip-sync is kept).
    #[must_use]
    pub fn flush_from(&self, start: Duration) -> Vec<EncodedAudioChunk> {
        self.ring
            .chunks
            .iter()
            .filter(|c| c.pts >= start)
            .cloned()
            .collect()
    }

    /// Number of buffered packets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Whether the buffer holds no packets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// The retention window this buffer was created with.
    #[must_use]
    pub fn window(&self) -> Duration {
        self.ring.window()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(secs: u64, keyframe: bool) -> EncodedChunk {
        EncodedChunk {
            bytes: vec![0; 4].into(),
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
    fn flushed_chunks_share_payloads_with_the_ring() {
        let mut buf = RingBuffer::new(Duration::from_secs(60));
        buf.push(chunk(0, true));
        let clip = buf.flush_last(Duration::from_secs(10)).unwrap();
        let again = buf.flush_last(Duration::from_secs(10)).unwrap();
        assert!(
            Arc::ptr_eq(&clip[0].bytes, &again[0].bytes),
            "a cut clones the Arc, not the payload"
        );
    }

    #[test]
    fn error_variant_displays() {
        assert_eq!(
            BufferError::NoKeyframe(Duration::from_secs(5)).to_string(),
            "no keyframe within the requested 5s window"
        );
    }

    fn audio(secs: u64) -> EncodedAudioChunk {
        EncodedAudioChunk {
            bytes: vec![0; 8].into(),
            frames: 960,
            pts: Duration::from_secs(secs),
        }
    }

    #[test]
    fn audio_ring_evicts_packets_older_than_window() {
        let mut buf = AudioRingBuffer::new(Duration::from_secs(60));
        assert!(buf.is_empty());
        assert_eq!(buf.window(), Duration::from_secs(60));
        for s in 0..130 {
            buf.push(audio(s));
        }
        // Newest pts is 129s; packets more than 60s older (pts < 69s) are evicted,
        // leaving pts 69..=129 inclusive.
        assert_eq!(buf.len(), 61);
    }

    #[test]
    fn audio_flush_from_returns_packets_at_or_after_start() {
        let mut buf = AudioRingBuffer::new(Duration::from_secs(60));
        for s in 0..=10 {
            buf.push(audio(s));
        }
        let clip = buf.flush_from(Duration::from_secs(4));
        let secs: Vec<u64> = clip.iter().map(|c| c.pts.as_secs()).collect();
        assert_eq!(secs, vec![4, 5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn audio_flush_from_future_start_is_empty() {
        let mut buf = AudioRingBuffer::new(Duration::from_secs(60));
        for s in 0..=5 {
            buf.push(audio(s));
        }
        assert!(buf.flush_from(Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn video_and_audio_rings_evict_identically() {
        // Both wrappers share TimeRing; the same irregular pts sequence must leave the
        // same retained set in each.
        let window = Duration::from_secs(15);
        let mut video = RingBuffer::new(window);
        let mut audio_ring = AudioRingBuffer::new(window);
        for &s in &[0u64, 3, 4, 9, 15, 16, 30] {
            video.push(chunk(s, true));
            audio_ring.push(audio(s));
        }
        let video_pts = pts_secs(&video.flush_last(Duration::from_secs(600)).unwrap());
        let audio_pts: Vec<u64> = audio_ring
            .flush_from(Duration::ZERO)
            .iter()
            .map(|c| c.pts.as_secs())
            .collect();
        assert_eq!(video_pts, vec![15, 16, 30]);
        assert_eq!(video_pts, audio_pts);
        assert_eq!(video.len(), audio_ring.len());
    }
}
