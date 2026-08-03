//! Aspect-preserving fit maths for the capture-size → encode-size step.
//!
//! The RGBA→NV12 pass samples the whole source across the whole output, so a source and an
//! output with different aspect ratios come out stretched. The recorder normally derives the
//! encode size from the display so the two agree, but a pinned size (or a mid-session display-mode
//! change) can still disagree — and a squashed clip is worse than a bordered one. These helpers
//! decide when that happens and by how much the picture has to shrink to fit inside the frame.
//!
//! Pure arithmetic, deliberately separate from the wgpu glue in `gpu_video_backend`, so the rule
//! is unit-testable without a GPU.

/// How far two aspect ratios may differ before the picture visibly distorts.
///
/// Both dimensions are rounded to even for 4:2:0, which can move a 4K-wide frame's aspect by
/// ~0.03 %. 0.5 % leaves room for that (and for a 1366×768 panel's odd 16:9 approximation)
/// while still catching a 16:10 source in a 16:9 frame, which is 11 % out.
const ASPECT_EPSILON: f32 = 0.005;

/// Whether `src` and `dst` are close enough in shape that scaling one onto the other is
/// indistinguishable from a proper fit — in which case the letterbox pass can be skipped
/// entirely and the frame goes down the plain single-pass path.
///
/// A degenerate dimension reports `true`: there is nothing sensible to letterbox, and the caller's
/// plain path already handles it.
#[must_use]
pub fn aspect_matches(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> bool {
    let (Some(src), Some(dst)) = (aspect(src_w, src_h), aspect(dst_w, dst_h)) else {
        return true;
    };
    (src - dst).abs() <= ASPECT_EPSILON * dst
}

/// The `(x, y)` factors that shrink a fullscreen quad so `src` fits *inside* `dst` with its
/// aspect ratio intact — the "contain" fit, never a crop. Exactly one factor is 1.0; the other
/// is the fraction of the frame the picture occupies, leaving equal bars on the short axis.
///
/// Mirrors the letterbox the clip player already does for playback
/// (`crates/settings/src/video.rs`), so recording and playback agree on what "fit" means.
#[must_use]
pub fn contain_scale(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> [f32; 2] {
    let (Some(src), Some(dst)) = (aspect(src_w, src_h), aspect(dst_w, dst_h)) else {
        return [1.0, 1.0];
    };
    if src > dst {
        // Source is wider: it spans the full width, bars go top and bottom.
        [1.0, dst / src]
    } else {
        // Source is taller (or equal): it spans the full height, bars go left and right.
        [src / dst, 1.0]
    }
}

/// Width ÷ height as a positive ratio, or `None` for a degenerate size.
fn aspect(w: u32, h: u32) -> Option<f32> {
    (w > 0 && h > 0).then(|| w as f32 / h as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scale factors are compared as pixels-of-picture, which is what actually matters.
    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn identical_shapes_need_no_letterbox() {
        assert!(aspect_matches(1920, 1080, 1920, 1080));
        assert!(aspect_matches(3840, 2160, 1920, 1080), "same 16:9, scaled");
        assert!(aspect_matches(3440, 1440, 2580, 1080), "same 21:9, scaled");
    }

    #[test]
    fn the_even_rounding_wobble_stays_within_tolerance() {
        // 2580×1080 is 3440×1440 after the even-rounding; the tiny aspect drift must not
        // trigger a pointless extra GPU pass on every frame.
        assert!(aspect_matches(3440, 1440, 2580, 1080));
        assert!(aspect_matches(1367, 769, 1366, 768));
    }

    #[test]
    fn different_shapes_need_a_letterbox() {
        assert!(!aspect_matches(3440, 1440, 1920, 1080), "21:9 into 16:9");
        assert!(!aspect_matches(1920, 1200, 1920, 1080), "16:10 into 16:9");
        assert!(!aspect_matches(1600, 1200, 1920, 1080), "4:3 into 16:9");
        assert!(
            !aspect_matches(1080, 1920, 1920, 1080),
            "portrait into 16:9"
        );
    }

    #[test]
    fn a_degenerate_size_falls_through_to_the_plain_path() {
        assert!(aspect_matches(0, 1080, 1920, 1080));
        assert!(aspect_matches(1920, 1080, 1920, 0));
        assert_eq!(contain_scale(0, 0, 1920, 1080), [1.0, 1.0]);
    }

    #[test]
    fn a_wider_source_gets_bars_top_and_bottom() {
        let [x, y] = contain_scale(3440, 1440, 1920, 1080);
        assert!(close(x, 1.0), "spans the full width");
        // 1920 px wide at 21:9 is 804 lines of picture inside a 1080-line frame.
        assert!(close(y, (1920.0 / (3440.0 / 1440.0)) / 1080.0), "y = {y}");
        assert!(y < 1.0);
    }

    #[test]
    fn a_taller_source_gets_bars_left_and_right() {
        let [x, y] = contain_scale(1600, 1200, 1920, 1080);
        assert!(close(y, 1.0), "spans the full height");
        // 1080 lines at 4:3 is 1440 px of picture inside a 1920 px frame.
        assert!(close(x, 1440.0 / 1920.0), "x = {x}");
    }

    #[test]
    fn a_matching_source_fills_the_frame() {
        assert_eq!(contain_scale(1920, 1080, 3840, 2160), [1.0, 1.0]);
    }

    #[test]
    fn the_fit_never_crops() {
        for (sw, sh) in [(3440, 1440), (1920, 1200), (1600, 1200), (1080, 1920)] {
            let [x, y] = contain_scale(sw, sh, 1920, 1080);
            assert!(x <= 1.0 && y <= 1.0, "{sw}x{sh} scaled past the frame");
            assert!(x > 0.0 && y > 0.0, "{sw}x{sh} collapsed");
        }
    }
}
