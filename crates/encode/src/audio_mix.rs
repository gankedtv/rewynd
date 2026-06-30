//! Mixing two capture sources (system output + microphone) into one PCM stream.
//!
//! The system-monitor and mic streams arrive on separate threads with independent buffering,
//! but both stamp each buffer with a PTS on the *same* capture epoch. [`AudioMixer`] places
//! each buffer onto a shared sample timeline at `frame = pts × sample_rate` and **sums**
//! overlapping samples, so the two sources line up by arrival time. A drain releases only
//! samples old enough that both sources have had time to contribute (a *settle* delay) — for
//! a replay buffer the added latency is irrelevant, and it absorbs the streams' jitter.
//!
//! Two notes on alignment: the PTS is each buffer's dequeue time, not a hardware capture
//! timestamp, so the two sources align to within their pipeline-latency delta (tens of ms —
//! within lip-sync tolerance; true hardware-timestamp alignment is a future refinement). And
//! the drain output **must stay PTS-contiguous** (gaps are zero-filled, never skipped):
//! [`OpusAudioEncoder`](crate::OpusAudioEncoder) re-anchors per push, so a non-contiguous
//! drain would inject a gap the muxer can't reconstruct.
//!
//! The mixer is pure CPU logic (no hardware), so it's fully unit-tested.

use std::collections::VecDeque;
use std::time::Duration;

/// Guards against a source reporting a wildly-future PTS (a clock glitch): never buffer more
/// than this many seconds of mixed audio ahead of the drained position.
const MAX_BUFFERED: Duration = Duration::from_secs(5);

/// Sums multiple capture sources onto one PTS-aligned interleaved-`f32` timeline.
#[derive(Debug)]
pub struct AudioMixer {
    channels: usize,
    sample_rate: u32,
    /// How far behind "now" a frame must be before it's drained, so both sources have
    /// contributed. Larger = more jitter tolerance, more latency (irrelevant for a buffer).
    settle: Duration,
    /// Absolute (epoch-relative) frame index of `buf`'s front sample.
    base_frame: u64,
    /// Mixed interleaved samples from `base_frame` onward (summed across sources).
    buf: VecDeque<f32>,
    /// Whether `base_frame` has been anchored to the first sample seen.
    started: bool,
}

impl AudioMixer {
    /// Create a mixer for interleaved audio of `channels` at `sample_rate`, releasing frames
    /// `settle` behind the drain clock.
    #[must_use]
    pub fn new(sample_rate: u32, channels: u32, settle: Duration) -> Self {
        Self {
            channels: channels.max(1) as usize,
            sample_rate: sample_rate.max(1),
            settle,
            base_frame: 0,
            buf: VecDeque::new(),
            started: false,
        }
    }

    /// The absolute (epoch-relative) per-channel frame index a PTS maps to.
    fn frame_at(&self, pts: Duration) -> u64 {
        (pts.as_nanos() * u128::from(self.sample_rate) / 1_000_000_000) as u64
    }

    /// The PTS of an absolute frame index (inverse of [`frame_at`](Self::frame_at)). Computed
    /// in u128 so a multi-day session (frame index past ~1.8e10) can't overflow the multiply.
    fn pts_of(&self, frame: u64) -> Duration {
        let nanos = u128::from(frame) * 1_000_000_000 / u128::from(self.sample_rate);
        Duration::from_nanos(nanos as u64)
    }

    /// Sum one source's interleaved buffer onto the timeline at its capture `pts`. Samples
    /// older than the already-drained position are dropped; the rest extend/overlap `buf`.
    pub fn add(&mut self, pcm: &[f32], pts: Duration) {
        if pcm.is_empty() {
            return;
        }
        let start_frame = self.frame_at(pts);
        if !self.started {
            self.base_frame = start_frame;
            self.started = true;
        }

        // Where this buffer lands relative to buf's front, in frames (may be negative if it
        // starts before the front — those leading frames are already drained, so skip them).
        let rel = start_frame as i64 - self.base_frame as i64;
        let (skip_frames, dst_frame) = if rel < 0 {
            ((-rel) as usize, 0usize)
        } else {
            (0usize, rel as usize)
        };
        let skip = skip_frames * self.channels;
        if skip >= pcm.len() {
            return; // entirely before the drained window
        }
        // Only sum whole frames: a torn buffer with a partial trailing frame would otherwise
        // leave an orphan sample that permanently shifts the L/R interleaving.
        let src = &pcm[skip..];
        let src = &src[..src.len() - src.len() % self.channels];
        if src.is_empty() {
            return;
        }

        // Clamp how far ahead we'll buffer (a glitchy future PTS shouldn't OOM us).
        let max_frames = (MAX_BUFFERED.as_nanos() * u128::from(self.sample_rate) / 1_000_000_000)
            as usize
            * self.channels;
        let dst = dst_frame * self.channels;
        if dst >= max_frames {
            return;
        }

        let end = dst + src.len();
        if end > self.buf.len() {
            self.buf.resize(end, 0.0);
        }
        // Sum into place (a second source overlapping the same frames adds to the first).
        for (i, &s) in src.iter().enumerate() {
            self.buf[dst + i] += s;
        }
    }

    /// Drain `n_frames` from the front, returning the PTS of the first and the interleaved
    /// samples. Each sample is sanitized to a finite value in [-1, 1] (summed sources can
    /// exceed full scale, and a glitchy source could deliver a non-finite sample that must
    /// not reach the Opus encoder). Advances `base_frame` so drains stay PTS-contiguous.
    fn take_frames(&mut self, n_frames: usize) -> Option<(Duration, Vec<f32>)> {
        if n_frames == 0 {
            return None;
        }
        let n = n_frames * self.channels;
        let start_pts = self.pts_of(self.base_frame);
        let out: Vec<f32> = self
            .buf
            .drain(..n)
            .map(|s| {
                if s.is_finite() {
                    s.clamp(-1.0, 1.0)
                } else {
                    0.0
                }
            })
            .collect();
        self.base_frame += n_frames as u64;
        Some((start_pts, out))
    }

    /// Drain every frame settled before `now` (i.e. older than `now - settle`). Returns the
    /// PTS of the first drained frame and the mixed interleaved samples, or `None` when
    /// nothing is ready yet.
    pub fn drain_settled(&mut self, now: Duration) -> Option<(Duration, Vec<f32>)> {
        if !self.started {
            return None;
        }
        let settle_frame = self.frame_at(now.saturating_sub(self.settle));
        let ready_frames = settle_frame.saturating_sub(self.base_frame) as usize;
        let available_frames = self.buf.len() / self.channels;
        self.take_frames(ready_frames.min(available_frames))
    }

    /// Drain everything buffered regardless of the settle delay — used at shutdown to flush
    /// the tail.
    pub fn drain_all(&mut self) -> Option<(Duration, Vec<f32>)> {
        if !self.started {
            return None;
        }
        self.take_frames(self.buf.len() / self.channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;
    const CH: u32 = 2;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn frame_pts_round_trip() {
        let m = AudioMixer::new(SR, CH, ms(100));
        assert_eq!(m.frame_at(Duration::ZERO), 0);
        assert_eq!(m.frame_at(ms(1000)), 48_000);
        assert_eq!(m.frame_at(ms(10)), 480);
        assert_eq!(m.pts_of(48_000), ms(1000));
    }

    #[test]
    fn single_source_drains_after_settle() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        // 480 frames (10 ms) of stereo starting at t=0.
        let pcm: Vec<f32> = (0..480 * 2).map(|_| 0.25).collect();
        m.add(&pcm, Duration::ZERO);
        // Not yet settled (now=50ms, settle=100ms → settle_frame for -50ms = 0).
        assert!(m.drain_settled(ms(50)).is_none());
        // now=200ms → settle point 100ms → all 10ms of audio is ready.
        let (pts, out) = m.drain_settled(ms(200)).expect("settled");
        assert_eq!(pts, Duration::ZERO);
        assert_eq!(out.len(), 480 * 2);
        assert!(out.iter().all(|&s| (s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn two_sources_at_same_pts_sum_and_clamp() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        let a: Vec<f32> = vec![0.6; 480 * 2];
        let b: Vec<f32> = vec![0.6; 480 * 2];
        m.add(&a, Duration::ZERO);
        m.add(&b, Duration::ZERO); // overlapping → sums to 1.2, clamps to 1.0
        let (_, out) = m.drain_settled(ms(200)).expect("settled");
        assert_eq!(out.len(), 480 * 2);
        assert!(
            out.iter().all(|&s| (s - 1.0).abs() < 1e-6),
            "1.2 clamps to 1.0"
        );
    }

    #[test]
    fn two_sources_offset_align_by_pts() {
        let mut m = AudioMixer::new(SR, CH, ms(1000));
        // Source A: frames [0,480) at 0.5. Source B: frames [240,720) at 0.5, starting at 5ms.
        let a = vec![0.5_f32; 480 * 2];
        let b = vec![0.5_f32; 480 * 2];
        m.add(&a, Duration::ZERO);
        m.add(&b, ms(5)); // 5ms = 240 frames
        let (pts, out) = m.drain_settled(ms(2000)).expect("settled");
        assert_eq!(pts, Duration::ZERO);
        // Timeline spans frames [0,720) = 720*2 samples.
        assert_eq!(out.len(), 720 * 2);
        // [0,240): only A = 0.5. [240,480): A+B = 1.0. [480,720): only B = 0.5.
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[240 * 2] - 1.0).abs() < 1e-6);
        assert!((out[479 * 2] - 1.0).abs() < 1e-6);
        assert!((out[480 * 2] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn settle_releases_incrementally() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        // 100ms of audio at t=0..100ms.
        let pcm = vec![0.1_f32; 4800 * 2];
        m.add(&pcm, Duration::ZERO);
        // now=150ms → settle point 50ms → first 50ms (2400 frames) ready.
        let (pts0, first) = m.drain_settled(ms(150)).expect("first half");
        assert_eq!(pts0, Duration::ZERO);
        assert_eq!(first.len(), 2400 * 2);
        // now=250ms → settle point 150ms → the remaining 50ms ready, PTS continues at 50ms.
        let (pts1, second) = m.drain_settled(ms(250)).expect("second half");
        assert_eq!(pts1, ms(50));
        assert_eq!(second.len(), 2400 * 2);
    }

    #[test]
    fn late_samples_before_base_are_dropped() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        // First buffer anchors base at 100ms.
        m.add(&vec![0.5_f32; 480 * 2], ms(100));
        // Drain it so base advances.
        let _ = m.drain_settled(ms(1000));
        // A late buffer for t=0 (well before the drained base) is dropped, not panicking.
        m.add(&vec![0.9_f32; 480 * 2], Duration::ZERO);
        assert!(m.drain_settled(ms(2000)).is_none());
    }

    #[test]
    fn drain_all_flushes_tail_ignoring_settle() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        m.add(&vec![0.2_f32; 480 * 2], Duration::ZERO);
        // Nothing settled yet at now=10ms, but drain_all flushes it.
        assert!(m.drain_settled(ms(10)).is_none());
        let (pts, out) = m.drain_all().expect("tail");
        assert_eq!(pts, Duration::ZERO);
        assert_eq!(out.len(), 480 * 2);
        assert!(m.drain_all().is_none());
    }

    #[test]
    fn empty_add_is_noop() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        m.add(&[], ms(10));
        assert!(m.drain_settled(ms(1000)).is_none());
    }

    #[test]
    fn non_finite_samples_are_sanitized_to_zero() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        let mut pcm = vec![0.3_f32; 480 * 2];
        pcm[0] = f32::NAN;
        pcm[1] = f32::INFINITY;
        pcm[2] = f32::NEG_INFINITY;
        m.add(&pcm, Duration::ZERO);
        let (_, out) = m.drain_all().expect("tail");
        assert_eq!(out[0], 0.0, "NaN → 0");
        assert_eq!(out[1], 0.0, "inf → 0");
        assert_eq!(out[2], 0.0, "-inf → 0");
        assert!((out[3] - 0.3).abs() < 1e-6);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn partial_trailing_frame_is_not_summed() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        // 2.5 stereo frames (5 samples) — the trailing half-frame must be dropped so it
        // can't shift the L/R interleaving of later buffers.
        m.add(&[0.5, 0.5, 0.5, 0.5, 0.9], Duration::ZERO);
        let (_, out) = m.drain_all().expect("tail");
        assert_eq!(out.len(), 4, "only the 2 whole frames are kept");
    }
}
