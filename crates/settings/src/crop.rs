//! Pure crop-rectangle math for the export crop editor: ratio presets, clamping, handle
//! dragging and the pixel rect ffmpeg gets. No widget state, so every regression-prone part
//! is unit-testable without a window.
//!
//! A rect lives in NORMALIZED 0..1 fractions of the SOURCE frame, never pixels: the same rect
//! survives a rescale of the master.
//!
//! The load-bearing idea: because the rect is normalized to the frame, a "16:9 output" on a
//! non-16:9 frame is NOT width/height = 16/9 — it is
//!
//! ```text
//! rect ratio = output ratio / frame ratio
//! ```
//!
//! Worked example: a 3440x1440 frame (frame ratio 2.389) cropped to 16:9 (1.778) needs
//! rect width/height = 1.778 / 2.389 = 0.744 — 2560 of 3440 px, centred at x = 440 px. Using
//! 1.778 directly would demand a 6115px-wide rect on a 3440px frame. Every ratio helper here
//! therefore takes the frame ratio; none of them can be written without it.

use iced::{ContentFit, Point, Rectangle, Size};

/// Smallest crop extent, as a fraction of the frame.
pub const MIN_CROP_EXTENT: f32 = 0.05;
/// "Did the user actually crop something" threshold: a drag of less than half a percent on
/// every edge is a mis-click, and re-encoding it would cost time for no visible change.
pub const CROP_CHANGED_EPS: f32 = 0.005;
/// One keyboard nudge, as a fraction of the frame.
pub const STEP: f32 = 0.01;

/// A crop rect in 0..1 fractions of the source frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The uncropped frame.
pub const FULL_FRAME: CropRect = CropRect {
    x: 0.0,
    y: 0.0,
    width: 1.0,
    height: 1.0,
};

/// An aspect-ratio preset for the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ratio {
    Original,
    Wide16x9,
    Ultra21x9,
    Standard4x3,
    Portrait9x16,
    Free,
}

impl Ratio {
    /// In pill display order.
    pub const ALL: [Ratio; 6] = [
        Ratio::Original,
        Ratio::Wide16x9,
        Ratio::Ultra21x9,
        Ratio::Standard4x3,
        Ratio::Portrait9x16,
        Ratio::Free,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Ratio::Original => "Original",
            Ratio::Wide16x9 => "16:9",
            Ratio::Ultra21x9 => "21:9",
            Ratio::Standard4x3 => "4:3",
            Ratio::Portrait9x16 => "9:16",
            Ratio::Free => "Free",
        }
    }

    /// Target output aspect ratio (width/height), or `None` when the rect is unconstrained.
    pub fn output_ratio(self, frame_ratio: f32) -> Option<f32> {
        match self {
            Ratio::Original => Some(frame_ratio),
            Ratio::Wide16x9 => Some(16.0 / 9.0),
            Ratio::Ultra21x9 => Some(21.0 / 9.0),
            Ratio::Standard4x3 => Some(4.0 / 3.0),
            Ratio::Portrait9x16 => Some(9.0 / 16.0),
            Ratio::Free => None,
        }
    }

    /// The preset's width/height in normalized rect space: output ratio / frame ratio.
    pub fn rect_ratio(self, frame_ratio: f32) -> Option<f32> {
        let output = self.output_ratio(frame_ratio)?;
        if !frame_ratio.is_finite() || frame_ratio <= 0.0 {
            return None;
        }
        Some(output / frame_ratio)
    }
}

/// The 8 resize handles, named by the edge(s) they move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
}

impl Handle {
    pub const ALL: [Handle; 8] = [
        Handle::Nw,
        Handle::N,
        Handle::Ne,
        Handle::E,
        Handle::Se,
        Handle::S,
        Handle::Sw,
        Handle::W,
    ];

    /// Anchor point in normalized rect space, so hit-testing and rendering agree.
    pub fn position(self, rect: CropRect) -> (f32, f32) {
        let left = rect.x;
        let right = rect.x + rect.width;
        let top = rect.y;
        let bottom = rect.y + rect.height;
        let mid_x = rect.x + rect.width / 2.0;
        let mid_y = rect.y + rect.height / 2.0;
        match self {
            Handle::Nw => (left, top),
            Handle::N => (mid_x, top),
            Handle::Ne => (right, top),
            Handle::E => (right, mid_y),
            Handle::Se => (right, bottom),
            Handle::S => (mid_x, bottom),
            Handle::Sw => (left, bottom),
            Handle::W => (left, mid_y),
        }
    }

    fn moves_west(self) -> bool {
        matches!(self, Handle::Nw | Handle::W | Handle::Sw)
    }

    fn moves_east(self) -> bool {
        matches!(self, Handle::Ne | Handle::E | Handle::Se)
    }

    fn moves_north(self) -> bool {
        matches!(self, Handle::Nw | Handle::N | Handle::Ne)
    }

    fn moves_south(self) -> bool {
        matches!(self, Handle::Sw | Handle::S | Handle::Se)
    }
}

fn usable_ratio(rect_ratio: Option<f32>) -> Option<f32> {
    rect_ratio.filter(|r| r.is_finite() && *r > 0.0)
}

/// Forces a rect to `rect_ratio` (width/height in normalized frame space), keeping its centre
/// and staying inside the frame. Shrinks rather than grows, so a locked rect can never overhang.
pub fn apply_rect_ratio(rect: CropRect, rect_ratio: Option<f32>) -> CropRect {
    let Some(ratio) = usable_ratio(rect_ratio) else {
        return rect;
    };

    let mut width = rect.width;
    let mut height = width / ratio;
    if height > 1.0 {
        height = 1.0;
        width = height * ratio;
    }
    if width > 1.0 {
        width = 1.0;
        height = width / ratio;
    }

    let cx = rect.x + rect.width / 2.0;
    let cy = rect.y + rect.height / 2.0;
    CropRect {
        x: (cx - width / 2.0).max(0.0).min(1.0 - width),
        y: (cy - height / 2.0).max(0.0).min(1.0 - height),
        width,
        height,
    }
}

/// The largest rect of the given ratio, centred in the frame.
pub fn max_rect_for_ratio(rect_ratio: Option<f32>) -> CropRect {
    match usable_ratio(rect_ratio) {
        Some(ratio) => apply_rect_ratio(FULL_FRAME, Some(ratio)),
        None => FULL_FRAME,
    }
}

/// Clamps a rect into the frame and enforces the minimum extent.
pub fn clamp_rect(rect: CropRect) -> CropRect {
    let width = rect.width.clamp(MIN_CROP_EXTENT, 1.0);
    let height = rect.height.clamp(MIN_CROP_EXTENT, 1.0);
    CropRect {
        x: rect.x.min(1.0 - width).clamp(0.0, 1.0),
        y: rect.y.min(1.0 - height).clamp(0.0, 1.0),
        width,
        height,
    }
}

/// Whether the rect crops anything worth re-encoding for.
pub fn is_changed(rect: CropRect) -> bool {
    rect.x > CROP_CHANGED_EPS
        || rect.y > CROP_CHANGED_EPS
        || rect.width < 1.0 - CROP_CHANGED_EPS
        || rect.height < 1.0 - CROP_CHANGED_EPS
}

/// The export's crop value: a rect only when the user actually cropped, `None` for the full
/// frame so no no-op crop is ever sent down the encode path.
pub fn to_model(rect: CropRect) -> Option<CropRect> {
    is_changed(rect).then_some(rect)
}

/// Absolute resize: the dragged edge follows the pointer and the opposite edge anchors.
pub fn resize_rect(
    rect: CropRect,
    handle: Handle,
    pointer: (f32, f32),
    rect_ratio: Option<f32>,
) -> CropRect {
    let mut left = rect.x;
    let mut right = rect.x + rect.width;
    let mut top = rect.y;
    let mut bottom = rect.y + rect.height;

    let px = pointer.0.clamp(0.0, 1.0);
    let py = pointer.1.clamp(0.0, 1.0);

    if handle.moves_west() {
        left = px.min(right - MIN_CROP_EXTENT);
    }
    if handle.moves_east() {
        right = px.max(left + MIN_CROP_EXTENT);
    }
    if handle.moves_north() {
        top = py.min(bottom - MIN_CROP_EXTENT);
    }
    if handle.moves_south() {
        bottom = py.max(top + MIN_CROP_EXTENT);
    }

    let next = clamp_rect(CropRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    });
    match rect_ratio {
        None => next,
        Some(_) => clamp_rect(apply_rect_ratio(next, rect_ratio)),
    }
}

/// Delta-based move, deliberately: an absolute move would teleport the rect's centre to the
/// pointer the instant someone presses on the picture.
pub fn move_rect(rect: CropRect, dx: f32, dy: f32) -> CropRect {
    CropRect {
        x: (rect.x + dx).max(0.0).min(1.0 - rect.width),
        y: (rect.y + dy).max(0.0).min(1.0 - rect.height),
        ..rect
    }
}

/// Nearest-hit-wins over the 8 handles, measured in PIXELS so the grab radius feels the same
/// whatever the preview box's aspect ratio. `None` means the press was a body drag.
pub fn hit_test_handle(
    rect: CropRect,
    pointer: (f32, f32),
    box_px: (f32, f32),
    grab_px: f32,
) -> Option<Handle> {
    let mut best = None;
    let mut best_dist = f32::INFINITY;
    for handle in Handle::ALL {
        let (hx, hy) = handle.position(rect);
        let dist = ((hx - pointer.0) * box_px.0).hypot((hy - pointer.1) * box_px.1);
        if dist <= grab_px && dist < best_dist {
            best = Some(handle);
            best_dist = dist;
        }
    }
    best
}

/// Rounds to the nearest pixel before snapping down to even: the rect comes out of f32 ratio
/// math, where an exact 2560 can land at 2559.9998 and a bare floor would drop two pixels.
fn even_px(v: f32) -> u32 {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    (v.round() as u32) / 2 * 2
}

/// Output pixel dimensions for a rect against a known source frame, even (yuv420p chroma
/// alignment) and never zero.
pub fn output_size(rect: CropRect, frame_w: u32, frame_h: u32) -> (u32, u32) {
    (
        even_px(rect.width * frame_w as f32).max(2),
        even_px(rect.height * frame_h as f32).max(2),
    )
}

/// An integer crop rect in source pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The integer crop for ffmpeg's `crop=W:H:X:Y`. Size *and* origin snap to even (yuv420p needs
/// both), the size keeps a 2px floor, and the rect then shrinks — never slides — to fit inside
/// the frame.
pub fn pixel_rect(rect: CropRect, frame_w: u32, frame_h: u32) -> PixelRect {
    let axis = |offset: f32, extent: f32, frame: u32| {
        // Leave room for the 2px minimum, so an origin pinned at the far edge still crops.
        let max_origin = even_px(frame.saturating_sub(2) as f32);
        let origin = even_px(offset).min(max_origin);
        let room = even_px((frame - origin) as f32);
        (origin, even_px(extent).max(2).min(room))
    };
    let (x, width) = axis(
        rect.x * frame_w as f32,
        rect.width * frame_w as f32,
        frame_w,
    );
    let (y, height) = axis(
        rect.y * frame_h as f32,
        rect.height * frame_h as f32,
        frame_h,
    );
    PixelRect {
        x,
        y,
        width,
        height,
    }
}

/// The picture rectangle of a `frame` image fitted into `bounds` with [`ContentFit::Contain`]
/// and centred — exactly what `image(..).content_fit(Contain)` draws, so the crop overlay can
/// sit on top of it.
pub fn contain_rect(frame: (u32, u32), bounds: Rectangle) -> Rectangle {
    let content = Size::new(frame.0 as f32, frame.1 as f32);
    if content.width <= 0.0 || content.height <= 0.0 || bounds.width <= 0.0 || bounds.height <= 0.0
    {
        return Rectangle::new(bounds.position(), Size::ZERO);
    }
    let fitted = ContentFit::Contain.fit(content, bounds.size());
    Rectangle::new(
        Point::new(
            bounds.center_x() - fitted.width / 2.0,
            bounds.center_y() - fitted.height / 2.0,
        ),
        fitted,
    )
}

/// A point's position as fractions of `area`. Unclamped: the caller decides what a press
/// outside the picture means.
pub fn point_fraction(area: Rectangle, p: Point) -> (f32, f32) {
    if area.width <= 0.0 || area.height <= 0.0 {
        return (0.0, 0.0);
    }
    ((p.x - area.x) / area.width, (p.y - area.y) / area.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3440x1440 ultrawide — the shape the whole feature exists for.
    const ULTRAWIDE: f32 = 3440.0 / 1440.0;
    const WIDESCREEN: f32 = 16.0 / 9.0;
    const EPS: f32 = 1e-5;

    #[track_caller]
    fn assert_near(actual: f32, expected: f32, tol: f32) {
        assert!(
            (actual - expected).abs() <= tol,
            "expected {expected} ± {tol}, got {actual}"
        );
    }

    #[test]
    fn rect_ratio_divides_by_the_frame_ratio_not_the_target_alone() {
        // A "16:9 output" on a 21:9 frame is width/height = 1.778/2.389 = 0.744 in normalized
        // rect space — NOT 1.778, which would demand a 6115px-wide rect on a 3440px frame.
        let rect_ratio = Ratio::Wide16x9.rect_ratio(ULTRAWIDE).unwrap();

        assert_near(rect_ratio, WIDESCREEN / ULTRAWIDE, EPS);
        assert_near(rect_ratio, 0.744, 1e-3);
        assert!((rect_ratio - WIDESCREEN).abs() > 0.01);
    }

    #[test]
    fn rect_ratio_is_one_when_the_target_matches_the_frame() {
        assert_near(Ratio::Wide16x9.rect_ratio(WIDESCREEN).unwrap(), 1.0, EPS);
    }

    #[test]
    fn original_always_means_the_whole_frame() {
        assert_near(Ratio::Original.rect_ratio(ULTRAWIDE).unwrap(), 1.0, EPS);
        assert_near(Ratio::Original.rect_ratio(9.0 / 16.0).unwrap(), 1.0, EPS);
    }

    #[test]
    fn free_is_unconstrained() {
        assert_eq!(Ratio::Free.rect_ratio(ULTRAWIDE), None);
        assert_eq!(Ratio::Free.output_ratio(ULTRAWIDE), None);
    }

    #[test]
    fn a_degenerate_frame_ratio_yields_none_rather_than_dividing_by_it() {
        assert_eq!(Ratio::Wide16x9.rect_ratio(0.0), None);
        assert_eq!(Ratio::Wide16x9.rect_ratio(f32::NAN), None);
        assert_eq!(Ratio::Wide16x9.rect_ratio(f32::INFINITY), None);
        assert_eq!(Ratio::Original.rect_ratio(f32::NAN), None);
    }

    #[test]
    fn a_portrait_target_on_a_landscape_frame_stays_inside() {
        let rect_ratio = Ratio::Portrait9x16.rect_ratio(ULTRAWIDE).unwrap();
        assert!(rect_ratio < 0.3, "{rect_ratio}");

        let rect = max_rect_for_ratio(Some(rect_ratio));
        assert!(rect.width <= 1.0);
        assert_near(rect.height, 1.0, EPS);
    }

    #[test]
    fn ratio_labels_read_as_the_pills() {
        let labels: Vec<&str> = Ratio::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(labels, ["Original", "16:9", "21:9", "4:3", "9:16", "Free"]);
    }

    #[test]
    fn max_rect_produces_the_exact_ultrawide_pillarbox() {
        let rect = max_rect_for_ratio(Ratio::Wide16x9.rect_ratio(ULTRAWIDE));

        assert_near(rect.height, 1.0, EPS);
        // 2560 of 3440 px wide, centred → 440px of bar on each side.
        assert_near(rect.width * 3440.0, 2560.0, 1.0);
        assert_near(rect.x * 3440.0, 440.0, 1.0);
        assert_near(rect.y, 0.0, EPS);
    }

    #[test]
    fn max_rect_is_the_full_frame_when_unconstrained() {
        assert_eq!(max_rect_for_ratio(None), FULL_FRAME);
        assert_eq!(max_rect_for_ratio(Some(0.0)), FULL_FRAME);
        assert_eq!(max_rect_for_ratio(Some(f32::NAN)), FULL_FRAME);
    }

    #[test]
    fn max_rect_never_overhangs_the_frame() {
        for ratio in Ratio::ALL {
            for frame_ratio in [ULTRAWIDE, WIDESCREEN, 4.0 / 3.0, 9.0 / 16.0] {
                let rect = max_rect_for_ratio(ratio.rect_ratio(frame_ratio));
                assert!(rect.x + rect.width <= 1.000001, "{rect:?}");
                assert!(rect.y + rect.height <= 1.000001, "{rect:?}");
            }
        }
    }

    #[test]
    fn apply_rect_ratio_shrinks_rather_than_grows() {
        // Growing to satisfy the ratio would push the edge the user just dragged past the frame.
        let rect = apply_rect_ratio(
            CropRect {
                x: 0.9,
                y: 0.9,
                width: 0.1,
                height: 0.1,
            },
            Some(4.0),
        );

        assert!(rect.x + rect.width <= 1.000001);
        assert!(rect.y + rect.height <= 1.000001);
        assert_near(rect.width / rect.height, 4.0, 1e-4);
    }

    #[test]
    fn apply_rect_ratio_keeps_the_rect_centred() {
        let rect = apply_rect_ratio(
            CropRect {
                x: 0.3,
                y: 0.3,
                width: 0.4,
                height: 0.4,
            },
            Some(1.0),
        );

        assert_near(rect.x + rect.width / 2.0, 0.5, 1e-4);
        assert_near(rect.y + rect.height / 2.0, 0.5, 1e-4);
    }

    #[test]
    fn apply_rect_ratio_passes_through_when_unconstrained() {
        let rect = CropRect {
            x: 0.1,
            y: 0.2,
            width: 0.3,
            height: 0.4,
        };
        assert_eq!(apply_rect_ratio(rect, None), rect);
        assert_eq!(apply_rect_ratio(rect, Some(-1.0)), rect);
    }

    #[test]
    fn clamp_rect_pulls_an_out_of_bounds_rect_back_inside() {
        let rect = clamp_rect(CropRect {
            x: -0.5,
            y: 1.5,
            width: 0.5,
            height: 0.5,
        });

        assert!(rect.x >= 0.0);
        assert!(rect.y + rect.height <= 1.000001);
    }

    #[test]
    fn clamp_rect_enforces_the_minimum_extent() {
        let rect = clamp_rect(CropRect {
            x: 0.0,
            y: 0.0,
            width: 0.001,
            height: 0.001,
        });

        assert_eq!(rect.width, MIN_CROP_EXTENT);
        assert_eq!(rect.height, MIN_CROP_EXTENT);
    }

    #[test]
    fn clamp_rect_is_idempotent() {
        let once = clamp_rect(CropRect {
            x: 0.7,
            y: 0.7,
            width: 0.6,
            height: 0.6,
        });
        assert_eq!(clamp_rect(once), once);
    }

    #[test]
    fn the_full_frame_is_not_a_crop() {
        // A no-op crop would cost the user a full re-encode for zero visible change.
        assert_eq!(to_model(FULL_FRAME), None);
        assert!(!is_changed(FULL_FRAME));
    }

    #[test]
    fn a_sub_threshold_nudge_is_not_a_crop() {
        assert_eq!(
            to_model(CropRect {
                x: 0.001,
                y: 0.001,
                width: 0.998,
                height: 0.998,
            }),
            None
        );
    }

    #[test]
    fn a_real_crop_survives_to_the_model() {
        let rect = CropRect {
            x: 0.1279,
            y: 0.0,
            width: 0.7442,
            height: 1.0,
        };
        assert_eq!(to_model(rect), Some(rect));
    }

    #[test]
    fn a_crop_on_any_single_edge_counts() {
        let edges = [
            CropRect {
                x: 0.2,
                y: 0.0,
                width: 0.8,
                height: 1.0,
            },
            CropRect {
                x: 0.0,
                y: 0.2,
                width: 1.0,
                height: 0.8,
            },
            CropRect {
                x: 0.0,
                y: 0.0,
                width: 0.8,
                height: 1.0,
            },
            CropRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 0.8,
            },
        ];
        for rect in edges {
            assert!(is_changed(rect), "{rect:?}");
        }
    }

    #[test]
    fn resize_anchors_the_opposite_edge() {
        let rect = resize_rect(FULL_FRAME, Handle::E, (0.6, 0.5), None);

        assert_near(rect.x, 0.0, EPS);
        assert_near(rect.width, 0.6, EPS);
    }

    #[test]
    fn resize_moves_both_axes_from_a_corner() {
        let rect = resize_rect(FULL_FRAME, Handle::Se, (0.6, 0.7), None);

        assert_near(rect.width, 0.6, EPS);
        assert_near(rect.height, 0.7, EPS);
    }

    #[test]
    fn resize_drags_the_west_edge_without_moving_the_east_one() {
        let rect = resize_rect(FULL_FRAME, Handle::W, (0.25, 0.5), None);

        assert_near(rect.x, 0.25, EPS);
        assert_near(rect.x + rect.width, 1.0, EPS);
    }

    #[test]
    fn resize_refuses_to_invert_past_the_minimum_extent() {
        // Dragging the east edge left past the west one must not produce a negative width.
        let rect = resize_rect(FULL_FRAME, Handle::E, (-1.0, 0.5), None);

        assert!(rect.width >= MIN_CROP_EXTENT);
        assert!(rect.x >= 0.0);
    }

    #[test]
    fn resize_re_locks_to_the_ratio() {
        let rect_ratio = Ratio::Wide16x9.rect_ratio(ULTRAWIDE);
        let rect = resize_rect(FULL_FRAME, Handle::Se, (0.5, 0.9), rect_ratio);

        assert_near(rect.width / rect.height, rect_ratio.unwrap(), 1e-4);
    }

    #[test]
    fn resize_clamps_a_pointer_dragged_outside_the_frame() {
        let rect = resize_rect(FULL_FRAME, Handle::Se, (5.0, 5.0), None);

        assert!(rect.x + rect.width <= 1.000001);
        assert!(rect.y + rect.height <= 1.000001);
    }

    #[test]
    fn move_translates_by_the_delta_and_keeps_the_size() {
        let rect = move_rect(
            CropRect {
                x: 0.2,
                y: 0.2,
                width: 0.5,
                height: 0.5,
            },
            0.1,
            -0.1,
        );

        assert_near(rect.x, 0.3, EPS);
        assert_near(rect.y, 0.1, EPS);
        assert_near(rect.width, 0.5, EPS);
    }

    #[test]
    fn move_stops_at_the_frame_edge() {
        let rect = move_rect(
            CropRect {
                x: 0.2,
                y: 0.2,
                width: 0.5,
                height: 0.5,
            },
            5.0,
            5.0,
        );

        assert_near(rect.x, 0.5, EPS);
        assert_near(rect.y, 0.5, EPS);
    }

    #[test]
    fn move_is_delta_based_so_a_zero_delta_is_a_no_op() {
        // An absolute move would snap the rect's centre to the pointer on the first press,
        // which reads as the editor throwing away the user's framing.
        let rect = CropRect {
            x: 0.1,
            y: 0.1,
            width: 0.3,
            height: 0.3,
        };
        assert_eq!(move_rect(rect, 0.0, 0.0), rect);

        // A keyboard nudge is one STEP of the same move.
        assert_near(move_rect(rect, STEP, 0.0).x, 0.1 + STEP, EPS);
    }

    #[test]
    fn hit_test_picks_the_handle_under_the_pointer() {
        assert_eq!(
            hit_test_handle(FULL_FRAME, (0.0, 0.0), (640.0, 360.0), 12.0),
            Some(Handle::Nw)
        );
    }

    #[test]
    fn hit_test_is_none_when_the_press_is_far_from_every_handle() {
        // The caller then treats it as a body drag, which is what makes "press anywhere and
        // pan" work.
        assert_eq!(
            hit_test_handle(FULL_FRAME, (0.5, 0.5), (640.0, 360.0), 12.0),
            None
        );
    }

    #[test]
    fn hit_test_is_nearest_hit_wins() {
        // A tiny rect crowds its handles together; picking the far one would feel broken.
        let tiny = CropRect {
            x: 0.5,
            y: 0.5,
            width: MIN_CROP_EXTENT,
            height: MIN_CROP_EXTENT,
        };
        assert_eq!(
            hit_test_handle(tiny, Handle::Se.position(tiny), (640.0, 360.0), 40.0),
            Some(Handle::Se)
        );
    }

    #[test]
    fn hit_test_measures_the_grab_radius_in_pixels() {
        // The same fractional offset is a different pixel distance on a wide box vs a tall one;
        // a fraction-space radius would make handles harder to grab on ultrawide sources.
        let near = (0.02, 0.0);
        assert_eq!(
            hit_test_handle(FULL_FRAME, near, (100.0, 360.0), 12.0),
            Some(Handle::Nw)
        );
        assert_eq!(
            hit_test_handle(FULL_FRAME, near, (2000.0, 360.0), 12.0),
            None
        );
    }

    #[test]
    fn all_eight_handles_are_exposed() {
        assert_eq!(Handle::ALL.len(), 8);
        for (i, a) in Handle::ALL.iter().enumerate() {
            for b in &Handle::ALL[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn handle_position_places_each_handle_on_the_rect_it_describes() {
        let rect = CropRect {
            x: 0.2,
            y: 0.3,
            width: 0.4,
            height: 0.5,
        };

        assert_eq!(Handle::Nw.position(rect), (0.2, 0.3));
        let (sx, sy) = Handle::Se.position(rect);
        assert_near(sx, 0.6, EPS);
        assert_near(sy, 0.8, EPS);
        assert_near(Handle::N.position(rect).0, 0.4, EPS);
        assert_near(Handle::W.position(rect).1, 0.55, EPS);
        assert_near(Handle::Ne.position(rect).0, 0.6, EPS);
        assert_near(Handle::S.position(rect).1, 0.8, EPS);
        assert_near(Handle::Sw.position(rect).0, 0.2, EPS);
        assert_near(Handle::E.position(rect).1, 0.55, EPS);
    }

    #[test]
    fn output_size_reports_the_ultrawide_worked_example() {
        let size = output_size(
            CropRect {
                x: 0.1279,
                y: 0.0,
                width: 0.7442,
                height: 1.0,
            },
            3440,
            1440,
        );

        assert_eq!(size, (2560, 1440));
    }

    #[test]
    fn output_size_rounds_to_even_matching_the_yuv420p_snap() {
        let (w, h) = output_size(
            CropRect {
                x: 0.0,
                y: 0.0,
                width: 0.5,
                height: 0.5,
            },
            1921,
            1081,
        );

        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    #[test]
    fn output_size_never_reports_a_zero_dimension() {
        let (w, h) = output_size(
            CropRect {
                x: 0.0,
                y: 0.0,
                width: 0.0001,
                height: 0.0001,
            },
            100,
            100,
        );

        assert!(w >= 2);
        assert!(h >= 2);
    }

    #[test]
    fn pixel_rect_keeps_the_whole_frame_for_an_uncropped_rect() {
        assert_eq!(
            pixel_rect(FULL_FRAME, 3440, 1440),
            PixelRect {
                x: 0,
                y: 0,
                width: 3440,
                height: 1440,
            }
        );
    }

    #[test]
    fn pixel_rect_snaps_an_odd_frame_down_to_even() {
        assert_eq!(
            pixel_rect(FULL_FRAME, 1921, 1081),
            PixelRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }
        );
    }

    #[test]
    fn pixel_rect_matches_the_ultrawide_16_9_preset() {
        let rect = max_rect_for_ratio(Ratio::Wide16x9.rect_ratio(ULTRAWIDE));

        assert_eq!(
            pixel_rect(rect, 3440, 1440),
            PixelRect {
                x: 440,
                y: 0,
                width: 2560,
                height: 1440,
            }
        );
    }

    #[test]
    fn pixel_rect_stays_inside_an_odd_frame_at_the_right_edge() {
        let edge = pixel_rect(
            CropRect {
                x: 0.9,
                y: 0.9,
                width: 0.1,
                height: 0.1,
            },
            1921,
            1081,
        );

        assert_eq!(edge.x % 2, 0);
        assert_eq!(edge.width % 2, 0);
        assert!(edge.x + edge.width <= 1921, "{edge:?}");
        assert!(edge.y + edge.height <= 1081, "{edge:?}");
    }

    #[test]
    fn pixel_rect_shrinks_rather_than_slides_a_rect_off_the_edge() {
        // A rect pinned past the right/bottom edge still has to be a crop ffmpeg accepts.
        let over = pixel_rect(
            CropRect {
                x: 1.0,
                y: 1.0,
                width: 0.05,
                height: 0.05,
            },
            1921,
            1081,
        );

        assert!(over.x + over.width <= 1921, "{over:?}");
        assert!(over.y + over.height <= 1081, "{over:?}");
        assert!(over.width >= 2 && over.height >= 2, "{over:?}");
        assert_eq!((over.x % 2, over.y % 2), (0, 0));
    }

    #[test]
    fn contain_rect_pillarboxes_a_wide_frame_in_a_wider_box() {
        let bounds = iced::Rectangle {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 300.0,
        };
        let picture = contain_rect((1920, 1080), bounds);

        assert_near(picture.height, 300.0, EPS);
        assert_near(picture.width, 1920.0 * 300.0 / 1080.0, 1e-3);
        assert_near(picture.y, 20.0, EPS);
        // Centred: equal bars left and right.
        assert_near(
            picture.x - bounds.x,
            bounds.x + bounds.width - (picture.x + picture.width),
            1e-3,
        );
    }

    #[test]
    fn contain_rect_letterboxes_a_wide_frame_in_a_taller_box() {
        let bounds = iced::Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let picture = contain_rect((1920, 1080), bounds);

        assert_near(picture.width, 800.0, EPS);
        assert_near(picture.height, 450.0, 1e-3);
        assert_near(picture.x, 0.0, EPS);
        assert_near(picture.y, 75.0, 1e-3);
    }

    #[test]
    fn contain_rect_fills_a_box_of_the_same_aspect() {
        let bounds = iced::Rectangle {
            x: 5.0,
            y: 7.0,
            width: 640.0,
            height: 360.0,
        };
        let picture = contain_rect((1920, 1080), bounds);

        assert_near(picture.x, 5.0, 1e-3);
        assert_near(picture.y, 7.0, 1e-3);
        assert_near(picture.width, 640.0, 1e-3);
        assert_near(picture.height, 360.0, 1e-3);
    }

    #[test]
    fn contain_rect_is_empty_for_a_zero_size_frame_or_box() {
        let bounds = iced::Rectangle {
            x: 12.0,
            y: 34.0,
            width: 0.0,
            height: 300.0,
        };
        let empty = contain_rect((1920, 1080), bounds);
        assert_eq!(
            (empty.x, empty.y, empty.width, empty.height),
            (12.0, 34.0, 0.0, 0.0)
        );

        let no_frame = contain_rect(
            (0, 0),
            iced::Rectangle {
                x: 1.0,
                y: 2.0,
                width: 640.0,
                height: 360.0,
            },
        );
        assert_eq!(
            (no_frame.x, no_frame.y, no_frame.width, no_frame.height),
            (1.0, 2.0, 0.0, 0.0)
        );
    }

    #[test]
    fn contain_rect_agrees_with_iced_content_fit() {
        // The overlay has to sit exactly on what `image(..).content_fit(Contain)` draws.
        for bounds in [
            iced::Rectangle {
                x: 10.0,
                y: 20.0,
                width: 800.0,
                height: 300.0,
            },
            iced::Rectangle {
                x: -5.0,
                y: 0.0,
                width: 320.0,
                height: 900.0,
            },
        ] {
            for frame in [(1920u32, 1080u32), (3440, 1440), (1080, 1920)] {
                let fitted = iced::ContentFit::Contain.fit(
                    iced::Size::new(frame.0 as f32, frame.1 as f32),
                    bounds.size(),
                );
                let picture = contain_rect(frame, bounds);

                assert_near(picture.width, fitted.width, 1e-3);
                assert_near(picture.height, fitted.height, 1e-3);
                assert_near(picture.x, bounds.center_x() - fitted.width / 2.0, 1e-3);
                assert_near(picture.y, bounds.center_y() - fitted.height / 2.0, 1e-3);
            }
        }
    }

    #[test]
    fn point_fraction_round_trips_a_handle_position() {
        let area = iced::Rectangle {
            x: 10.0,
            y: 20.0,
            width: 640.0,
            height: 360.0,
        };
        let rect = CropRect {
            x: 0.2,
            y: 0.3,
            width: 0.4,
            height: 0.5,
        };
        let (hx, hy) = Handle::Se.position(rect);
        let point = iced::Point::new(area.x + hx * area.width, area.y + hy * area.height);

        let (fx, fy) = point_fraction(area, point);
        assert_near(fx, hx, EPS);
        assert_near(fy, hy, EPS);
    }

    #[test]
    fn point_fraction_is_unclamped_and_survives_a_zero_size_area() {
        let area = iced::Rectangle {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 50.0,
        };
        let (fx, fy) = point_fraction(area, iced::Point::new(-50.0, 100.0));
        assert_near(fx, -0.5, EPS);
        assert_near(fy, 2.0, EPS);

        let degenerate = iced::Rectangle {
            x: 3.0,
            y: 4.0,
            width: 0.0,
            height: 0.0,
        };
        assert_eq!(
            point_fraction(degenerate, iced::Point::new(9.0, 9.0)),
            (0.0, 0.0)
        );
    }
}
