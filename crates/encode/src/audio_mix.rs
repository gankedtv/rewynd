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
//! Sources are summed in whatever channel layout they arrive in. A microphone often arrives on
//! a single channel of a multi-channel device (the right silent), which would play from one
//! speaker; [`center_mono_into`] collapses such a source to centered mono before it's added,
//! so the mic sits in front while system audio keeps its true stereo image.
//!
//! The mixer is pure CPU logic (no hardware), so it's fully unit-tested.

use std::collections::VecDeque;
use std::time::Duration;

/// Guards against a source reporting a wildly-future PTS (a clock glitch): never buffer more
/// than this many seconds of mixed audio ahead of the drained position.
const MAX_BUFFERED: Duration = Duration::from_secs(5);

/// Collapse an interleaved multi-channel buffer to *centered mono*, writing the result (same
/// frame count and channel width) into `out` (cleared first).
///
/// Each frame's channels are **summed** into one value placed on every output channel, so a
/// microphone plays from the centre instead of one side. The two ways a mic reaches a stereo
/// capture both end up centred:
/// - **One channel carries signal, the other is silent** — e.g. an XLR mic on input 1 of a
///   2-in interface. Summing recovers the signal at its original level (`L + 0 = L`).
/// - **The signal is duplicated on every channel** — what PipeWire's upmix does to a true
///   mono device. Summing doubles it (`L + L = 2L`, +6 dB); [`AudioMixer`] clamps on drain so
///   it can't overflow, and per-source gain (a future refinement) is where to trim it.
///
/// Summing is chosen over averaging because it keeps the silent-other-channel case at unity
/// instead of halving it. Whole frames only: a trailing partial frame is dropped, matching
/// [`AudioMixer::add`]. A `channels <= 1` buffer is copied verbatim.
pub fn center_mono_into(pcm: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    let channels = channels.max(1);
    if channels == 1 {
        out.extend_from_slice(pcm);
        return;
    }
    out.reserve(pcm.len());
    for frame in pcm.chunks_exact(channels) {
        let sum: f32 = frame.iter().sum();
        out.extend(std::iter::repeat_n(sum, channels));
    }
}

/// Scale every sample of an interleaved buffer in place by a linear `gain` (per-source mix
/// level). A `gain` of 1.0 (or a non-finite value) is a no-op; [`AudioMixer`] clamps the
/// summed result on drain, so a gain above unity can't overflow the output.
pub fn apply_gain(pcm: &mut [f32], gain: f32) {
    if !gain.is_finite() || (gain - 1.0).abs() < f32::EPSILON {
        return;
    }
    for s in pcm {
        *s *= gain;
    }
}

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
    fn far_future_pts_is_dropped_by_the_buffer_cap() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        m.add(&vec![0.2_f32; 480 * 2], Duration::ZERO);
        // Stamped 6 s ahead of the drained base — past the 5 s cap, so it's dropped
        // instead of ballooning the ring.
        m.add(&vec![0.9_f32; 480 * 2], ms(6_000));
        let (pts, out) = m.drain_all().expect("anchored audio");
        assert_eq!(pts, Duration::ZERO);
        assert_eq!(out.len(), 480 * 2, "only the anchored 10 ms is buffered");
        assert!(out.iter().all(|&s| (s - 0.2).abs() < 1e-6));
        assert!(m.drain_all().is_none(), "the future buffer never lands");
    }

    #[test]
    fn straddling_buffer_sums_only_trailing_frames_keeping_interleaving() {
        let mut m = AudioMixer::new(SR, CH, ms(100));
        m.add(&vec![0.0_f32; 480 * 2], Duration::ZERO);
        let _ = m.drain_all(); // base advances to frame 480 (10 ms)
        // 480 frames starting at 5 ms straddle the drained base: the first 240 frames
        // are already drained and dropped; the trailing 240 land at the base with their
        // L/R interleaving intact.
        let pcm: Vec<f32> = (0..480).flat_map(|_| [0.5, -0.5]).collect();
        m.add(&pcm, ms(5));
        let (pts, out) = m.drain_all().expect("trailing half");
        assert_eq!(pts, ms(10));
        assert_eq!(out.len(), 240 * 2);
        assert!(
            out.chunks_exact(2)
                .all(|f| (f[0] - 0.5).abs() < 1e-6 && (f[1] + 0.5).abs() < 1e-6),
            "left stays left, right stays right"
        );
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

    #[test]
    fn center_mono_spreads_left_only_signal_to_both_channels() {
        // A mic on the left channel only (right silent) — the common XLR-on-input-1 case.
        let mut out = vec![999.0]; // pre-existing content must be cleared
        center_mono_into(&[0.4, 0.0, 0.7, 0.0], 2, &mut out);
        // Each frame's sum (0.4, 0.7) lands on both channels at its original level.
        assert_eq!(out, [0.4, 0.4, 0.7, 0.7]);
    }

    #[test]
    fn center_mono_spreads_right_only_signal_to_both_channels() {
        let mut out = Vec::new();
        center_mono_into(&[0.0, 0.5, 0.0, -0.25], 2, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn center_mono_sums_both_channels() {
        let mut out = Vec::new();
        center_mono_into(&[0.3, 0.2], 2, &mut out);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn center_mono_duplicated_signal_doubles_then_mixer_clamps() {
        // A true-mono mic the OS duplicates onto both channels sums to 2x — the deliberate
        // tradeoff (unity for a single-sided mic costs +6 dB here). The drain clamp keeps it
        // from overflowing, and per-source gain is the tuning path.
        let mut out = Vec::new();
        center_mono_into(&[0.6, 0.6], 2, &mut out);
        assert!((out[0] - 1.2).abs() < 1e-6);
        assert!((out[1] - 1.2).abs() < 1e-6);
        let mut m = AudioMixer::new(SR, CH, ms(100));
        m.add(&out, Duration::ZERO);
        let (_, drained) = m.drain_all().expect("tail");
        assert!(
            drained.iter().all(|&s| (s - 1.0).abs() < 1e-6),
            "1.2 clamps to 1.0 on drain"
        );
    }

    #[test]
    fn center_mono_mono_input_is_copied_verbatim() {
        let mut out = Vec::new();
        center_mono_into(&[0.1, 0.2, 0.3], 1, &mut out);
        assert_eq!(out, [0.1, 0.2, 0.3]);
        // channels == 0 is treated as mono (max(1)), not a divide-by-zero.
        let mut out0 = Vec::new();
        center_mono_into(&[0.4, 0.5], 0, &mut out0);
        assert_eq!(out0, [0.4, 0.5]);
    }

    #[test]
    fn center_mono_drops_partial_trailing_frame() {
        let mut out = Vec::new();
        // 1.5 stereo frames — the trailing half-frame is dropped (matches add()).
        center_mono_into(&[0.2, 0.2, 0.9], 2, &mut out);
        assert_eq!(out, [0.4, 0.4]);
    }

    #[test]
    fn center_mono_empty_clears_out() {
        let mut out = vec![1.0, 2.0, 3.0];
        center_mono_into(&[], 2, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn apply_gain_scales_and_no_ops_at_unity() {
        let mut buf = [0.1, -0.2, 0.3, -0.4];
        apply_gain(&mut buf, 2.0);
        assert!((buf[0] - 0.2).abs() < 1e-6);
        assert!((buf[1] + 0.4).abs() < 1e-6);

        let mut unchanged = [0.5, -0.5];
        apply_gain(&mut unchanged, 1.0);
        assert_eq!(unchanged, [0.5, -0.5], "unity is a no-op");

        let mut nonfinite = [0.5, -0.5];
        apply_gain(&mut nonfinite, f32::NAN);
        assert_eq!(nonfinite, [0.5, -0.5], "non-finite gain is a no-op");

        let mut silence = [0.5, -0.5];
        apply_gain(&mut silence, 0.0);
        assert_eq!(silence, [0.0, 0.0], "zero gain mutes");
    }

    #[test]
    fn center_mono_then_mixed_lands_on_both_channels() {
        // End-to-end: a left-only mic, centered, sums into both channels of the timeline.
        let mut m = AudioMixer::new(SR, CH, ms(100));
        let mic: Vec<f32> = (0..480).flat_map(|_| [0.5, 0.0]).collect(); // left only
        let mut centered = Vec::new();
        center_mono_into(&mic, 2, &mut centered);
        m.add(&centered, Duration::ZERO);
        let (_, out) = m.drain_settled(ms(200)).expect("settled");
        assert_eq!(out.len(), 480 * 2);
        assert!(
            out.iter().all(|&s| (s - 0.5).abs() < 1e-6),
            "both channels carry the mic"
        );
    }
}
