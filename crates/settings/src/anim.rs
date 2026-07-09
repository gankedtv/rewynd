//! Tiny shared animation primitives for the GUI's few, deliberate movements (Arena keeps
//! motion decorative: short fades, one pulse, nothing structural). All types anchor their
//! clock on the first `advance` tick so time comes from the frame subscription, and the
//! caller drops them (or stops subscribing) when they finish, returning the app to zero
//! idle redraws.

use std::time::{Duration, Instant};

/// Smoothstep easing, clamped to `0.0..=1.0`.
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Per-channel linear interpolation between two colours.
pub fn lerp_color(a: iced::Color, b: iced::Color, t: f32) -> iced::Color {
    iced::Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// A one-shot eased 0-to-1 ramp over a fixed duration.
#[derive(Debug)]
pub struct Fade {
    dur: Duration,
    start: Option<Instant>,
    progress: f32,
}

impl Fade {
    pub fn new(dur: Duration) -> Self {
        Self {
            dur,
            start: None,
            progress: 0.0,
        }
    }

    /// Advance to frame time `now`. Returns `true` once the ramp is complete; the caller
    /// then drops the fade so the frame subscription can wind down.
    pub fn advance(&mut self, now: Instant) -> bool {
        let start = *self.start.get_or_insert(now);
        let linear = now.duration_since(start).as_secs_f32() / self.dur.as_secs_f32();
        self.progress = ease(linear);
        linear >= 1.0
    }

    /// Eased progress in `0.0..=1.0`.
    pub fn progress(&self) -> f32 {
        self.progress
    }
}

/// An unbounded repeating cycle exposing a linear phase in `0.0..1.0`. It never reports
/// completion; the caller gates the frame subscription on the state that wants it (a spinner
/// that stops when the work does).
#[derive(Debug)]
pub struct Cycle {
    period: Duration,
    start: Option<Instant>,
    phase: f32,
}

impl Cycle {
    pub fn new(period: Duration) -> Self {
        Self {
            period,
            start: None,
            phase: 0.0,
        }
    }

    pub fn advance(&mut self, now: Instant) {
        let start = *self.start.get_or_insert(now);
        let t = now.duration_since(start).as_secs_f32() / self.period.as_secs_f32();
        self.phase = t.fract();
    }

    /// Linear phase in `0.0..1.0`.
    pub fn phase(&self) -> f32 {
        self.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_clamps_and_ends() {
        assert_eq!(ease(-1.0), 0.0);
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert_eq!(ease(2.0), 1.0);
        assert!(ease(0.5) > 0.49 && ease(0.5) < 0.51);
    }

    #[test]
    fn fade_anchors_on_first_tick_and_completes() {
        let mut fade = Fade::new(Duration::from_millis(200));
        let t0 = Instant::now();
        assert!(!fade.advance(t0));
        assert_eq!(fade.progress(), 0.0);
        assert!(!fade.advance(t0 + Duration::from_millis(100)));
        assert!(fade.progress() > 0.0 && fade.progress() < 1.0);
        assert!(fade.advance(t0 + Duration::from_millis(200)));
        assert_eq!(fade.progress(), 1.0);
    }

    #[test]
    fn cycle_wraps_phase() {
        let mut cycle = Cycle::new(Duration::from_millis(100));
        let t0 = Instant::now();
        cycle.advance(t0);
        assert_eq!(cycle.phase(), 0.0);
        cycle.advance(t0 + Duration::from_millis(150));
        assert!((cycle.phase() - 0.5).abs() < 1e-3);
    }

    #[test]
    fn lerp_color_endpoints() {
        let a = iced::Color::from_rgb(0.0, 0.2, 0.4);
        let b = iced::Color::from_rgb(1.0, 0.8, 0.6);
        assert_eq!(lerp_color(a, b, 0.0), a);
        assert_eq!(lerp_color(a, b, 1.0), b);
    }
}
