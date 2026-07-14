//! Rate conversion for the SCK audio legs.
//!
//! The system-audio leg honours `SCStreamConfiguration.sampleRate`, but the microphone
//! leg delivers the capture device's native format — 44.1 kHz on many USB mics, 16/24 kHz
//! on Bluetooth headsets — so the pipeline's fixed 48 kHz contract has to be re-established
//! here. Linear interpolation is enough for that: the ratios are gentle, the source is a
//! voice track mixed under game audio, and the alternative (an `AudioConverter` per stream)
//! buys inaudible quality for real complexity.

/// Resamples interleaved f32 frames between two rates, carrying the fractional read
/// position and the previous frame across buffers so consecutive buffers don't click.
pub struct Resampler {
    channels: usize,
    ratio: f64,
    /// Read position within the current buffer, in source frames; fractional part is
    /// carried into the next buffer.
    pos: f64,
    /// The final frame of the previous buffer, interpolated against by the next one.
    last: Vec<f32>,
    started: bool,
}

impl Resampler {
    /// A resampler from `src_rate` to `dst_rate` for `channels` interleaved channels.
    pub fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
        Self {
            channels: channels.max(1),
            ratio: f64::from(src_rate.max(1)) / f64::from(dst_rate.max(1)),
            pos: 0.0,
            last: Vec::new(),
            started: false,
        }
    }

    /// Convert one buffer of interleaved source frames into `out` (cleared first).
    pub fn process(&mut self, src: &[f32], out: &mut Vec<f32>) {
        out.clear();
        let channels = self.channels;
        let frames = src.len() / channels;
        if frames == 0 {
            return;
        }
        if !self.started {
            self.last = src[..channels].to_vec();
            self.started = true;
        }

        // `pos` is relative to the previous buffer's last frame, held at index -1.
        while self.pos < frames as f64 {
            let idx = self.pos.floor();
            let frac = (self.pos - idx) as f32;
            let idx = idx as isize;
            for channel in 0..channels {
                let a = if idx < 0 {
                    self.last[channel]
                } else {
                    src[idx as usize * channels + channel]
                };
                let next = idx + 1;
                let b = if (next as usize) < frames {
                    src[next as usize * channels + channel]
                } else {
                    // The next frame lives in the buffer that hasn't arrived yet; hold
                    // the current sample rather than inventing one.
                    a
                };
                out.push(a + (b - a) * frac);
            }
            self.pos += self.ratio;
        }
        self.last
            .copy_from_slice(&src[(frames - 1) * channels..frames * channels]);
        self.pos -= frames as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_rates_pass_frames_through() {
        let mut r = Resampler::new(48_000, 48_000, 2);
        let mut out = Vec::new();
        r.process(&[0.1, 0.2, 0.3, 0.4], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn upsampling_doubles_the_frame_count() {
        let mut r = Resampler::new(24_000, 48_000, 1);
        let mut out = Vec::new();
        r.process(&[0.0, 1.0, 0.0, 1.0], &mut out);
        // 4 source frames at ratio 0.5 → 8 output frames, interpolated halfway.
        assert_eq!(out.len(), 8);
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6, "midpoint is interpolated");
    }

    #[test]
    fn downsampling_halves_the_frame_count() {
        let mut r = Resampler::new(48_000, 24_000, 1);
        let mut out = Vec::new();
        r.process(&[0.0, 0.1, 0.2, 0.3, 0.4, 0.5], &mut out);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn the_fractional_position_carries_across_buffers() {
        // 44.1 → 48 kHz: no whole-frame alignment, so the position must carry or the
        // output would click at every buffer boundary.
        let mut r = Resampler::new(44_100, 48_000, 1);
        let (mut out, mut total) = (Vec::new(), 0usize);
        for _ in 0..10 {
            r.process(&[0.25; 441], &mut out);
            total += out.len();
            assert!(
                out.iter().all(|&s| (s - 0.25).abs() < 1e-6),
                "constant in, constant out"
            );
        }
        // 10 × 10 ms of 44.1 kHz is 100 ms → ~4800 frames at 48 kHz, ±1 for the phase.
        assert!((4799..=4801).contains(&total), "got {total} frames");
    }

    #[test]
    fn stereo_channels_stay_separate() {
        let mut r = Resampler::new(24_000, 48_000, 2);
        let mut out = Vec::new();
        r.process(&[0.0, 1.0, 0.0, 1.0], &mut out);
        // Left stays 0, right stays 1 through the interpolation.
        for frame in out.chunks_exact(2) {
            assert!((frame[0] - 0.0).abs() < 1e-6);
            assert!((frame[1] - 1.0).abs() < 1e-6);
        }
    }
}
