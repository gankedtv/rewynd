//! The automatic video bitrate: 12 Mbps at 1080p60 (YouTube's H.264 figure), scaled with the
//! pixel count and, sub-linearly, the frame rate.

const REFERENCE_BPS: f64 = 12_000_000.0;
const REFERENCE_PIXELS: f64 = 1920.0 * 1080.0;
const REFERENCE_FPS: f64 = 60.0;

/// Consecutive frames differ less at higher rates. 0.6 puts 1080p30 near YouTube's 8 Mbps.
const FPS_EXPONENT: f64 = 0.6;

const MIN_BPS: f64 = 1_000_000.0;
/// The config's own cap on any bitrate.
const MAX_BPS: f64 = 100_000_000.0;

const STEP_BPS: f64 = 100_000.0;

/// The bitrate for a `width`×`height` stream at `framerate`.
#[must_use]
pub fn auto_bitrate_bps(width: u32, height: u32, framerate: u32) -> u32 {
    let pixels = f64::from(width) * f64::from(height);
    let fps = f64::from(framerate.max(1));
    let bps =
        REFERENCE_BPS * (pixels / REFERENCE_PIXELS) * (fps / REFERENCE_FPS).powf(FPS_EXPONENT);
    let stepped = (bps.clamp(MIN_BPS, MAX_BPS) / STEP_BPS).round() * STEP_BPS;
    stepped as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_frame_gets_the_reference_bitrate() {
        assert_eq!(auto_bitrate_bps(1920, 1080, 60), 12_000_000);
    }

    #[test]
    fn bitrate_scales_with_the_pixel_count() {
        assert_eq!(auto_bitrate_bps(2560, 1440, 60), 21_300_000);
        assert_eq!(auto_bitrate_bps(3440, 1440, 60), 28_700_000);
        assert_eq!(auto_bitrate_bps(3840, 2160, 60), 48_000_000);
    }

    #[test]
    fn a_lower_frame_rate_needs_less_but_not_proportionally_less() {
        assert_eq!(auto_bitrate_bps(1920, 1080, 30), 7_900_000);
        let at_120 = auto_bitrate_bps(1920, 1080, 120);
        assert!(at_120 > 12_000_000 && at_120 < 24_000_000, "{at_120}");
    }

    #[test]
    fn the_figure_stays_inside_its_bounds() {
        assert_eq!(auto_bitrate_bps(320, 240, 30), 1_000_000);
        assert_eq!(auto_bitrate_bps(7680, 4320, 240), 100_000_000);
        assert_eq!(
            auto_bitrate_bps(1920, 1080, 0),
            auto_bitrate_bps(1920, 1080, 1)
        );
    }
}
