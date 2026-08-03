//! How the stored `[video] width`/`height`/`match_display` turn into the encoder's dimensions.
//!
//! The recorder cannot know the output size from the config alone: the whole point of
//! "match display" is that the answer depends on the monitor the user actually shares. So the
//! config stores an *intent* — [`ResolutionMode`] — and the recorder resolves it against the
//! captured source once it knows the display geometry. Everything here is pure arithmetic; the
//! per-platform detection lives in `rewynd-capture` / the recorder.

/// Ceiling on an auto-derived frame, as a pixel count: 3840×2160. Matching an 8K panel 1:1 would
/// blow past every consumer encoder's limits (and any sane bitrate), so "match display" tops out
/// around 4K.
///
/// A budget rather than a line count, because displays are not all 16:9. A 3440×2236 laptop panel
/// is 7.7 MP — smaller than 4K — and capping its *height* at 2160 would scale it down 3 % for no
/// gain, while a 5120×1440 ultrawide would sail past a height cap at 7.4 MP. Area is what actually
/// costs encoder time and bitrate.
pub const MAX_AUTO_PIXELS: u64 = 3840 * 2160;

/// Smallest encodable dimension: one 16x16 H.264 macroblock.
pub const MIN_DIM: u32 = 16;

/// Largest dimension we will ask an encoder for (8K wide).
pub const MAX_DIM: u32 = 7680;

/// Used when no display could be detected — also `rewynd_encode::EncodeParams::default()`, so a
/// headless/undetectable run still targets the project's 1080p reference (PLAN §2).
pub const FALLBACK_DIMS: (u32, u32) = (1920, 1080);

/// What the stored settings mean, resolved against the captured display by [`Self::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionMode {
    /// Record the display at its native size, scaled down only if it exceeds [`MAX_AUTO_PIXELS`].
    MatchDisplay,
    /// Record at this many lines, taking the width from the display's aspect ratio. Never
    /// upscales: a 1080-line display asked for 1440 stays at 1080.
    Height(u32),
    /// Record at exactly these dimensions. The record path letterboxes when the source aspect
    /// differs, so a pinned size is honoured without distorting the picture.
    Fixed { width: u32, height: u32 },
}

impl ResolutionMode {
    /// Read the mode out of the stored `[video]` values.
    ///
    /// `width`/`height` of `0` mean "auto" — a fresh config ships `0`/`0`, which is
    /// [`MatchDisplay`](Self::MatchDisplay). A pin needs both dimensions, so `match_display =
    /// false` with a zero in either falls back to the auto reading rather than inventing one.
    #[must_use]
    pub fn from_stored(match_display: bool, width: u32, height: u32) -> Self {
        if !match_display && width > 0 && height > 0 {
            return Self::Fixed { width, height };
        }
        if height == 0 {
            Self::MatchDisplay
        } else {
            Self::Height(height)
        }
    }

    /// The dimensions to encode at, given the captured source's size (`None` when the display
    /// could not be detected).
    #[must_use]
    pub fn resolve(self, source: Option<(u32, u32)>) -> (u32, u32) {
        let (src_w, src_h) = source.unwrap_or(FALLBACK_DIMS);
        match self {
            Self::Fixed { width, height } => sanitize_dims(width, height),
            // Both auto modes are "the display's aspect at N lines"; they differ only in where N
            // comes from. Capping at the source height keeps us from upscaling — extra lines cost
            // bitrate and encoder time without adding a single pixel of detail.
            Self::MatchDisplay => scale_to_height(src_w, src_h, budgeted_height(src_w, src_h)),
            Self::Height(h) => scale_to_height(src_w, src_h, h.min(src_h)),
        }
    }

    /// The height this mode targets, for a UI that wants to preselect a quality preset.
    /// `None` for [`MatchDisplay`](Self::MatchDisplay), which has no fixed height.
    #[must_use]
    pub fn height(self) -> Option<u32> {
        match self {
            Self::MatchDisplay => None,
            Self::Height(h) | Self::Fixed { height: h, .. } => Some(h),
        }
    }
}

/// The tallest the source may be recorded at without exceeding [`MAX_AUTO_PIXELS`] — its own
/// height when it already fits, which is the common case.
fn budgeted_height(src_w: u32, src_h: u32) -> u32 {
    let (w, h) = (u64::from(src_w), u64::from(src_h));
    if w == 0 || w * h <= MAX_AUTO_PIXELS {
        return src_h;
    }
    // Holding the aspect ratio, area = h² · (w/h), so the tallest frame within the budget is
    // sqrt(budget · h / w). Integer sqrt floors, keeping the result inside the budget.
    u32::try_from((MAX_AUTO_PIXELS * h / w).isqrt()).unwrap_or(src_h)
}

/// `src_w`×`src_h` scaled to `height` lines, both dimensions rounded to encodable values.
#[must_use]
pub fn scale_to_height(src_w: u32, src_h: u32, height: u32) -> (u32, u32) {
    (fit_width(src_w, src_h, height), clamp_even(height))
}

/// The width that preserves `src_w`:`src_h` at `height`, rounded to an encodable value.
///
/// A degenerate source (either dimension zero) falls back to 16:9, so a failed detection still
/// produces a sane frame instead of a 16-pixel sliver.
#[must_use]
pub fn fit_width(src_w: u32, src_h: u32, height: u32) -> u32 {
    let (src_w, src_h) = if src_w == 0 || src_h == 0 {
        FALLBACK_DIMS
    } else {
        (src_w, src_h)
    };
    // u64 throughout: 7680 * 7680 overflows nothing here, but a u32 product of two large
    // dimensions would be uncomfortably close. Round to nearest before the even mask so the
    // aspect error stays under half a pixel.
    let scaled = (u64::from(src_w) * u64::from(height) + u64::from(src_h) / 2) / u64::from(src_h);
    clamp_even(u32::try_from(scaled).unwrap_or(MAX_DIM))
}

/// Clamp both dimensions to what the encoders accept: `[MIN_DIM, MAX_DIM]`, rounded down to even
/// for 4:2:0 chroma subsampling. The single place that rule lives.
#[must_use]
pub fn sanitize_dims(width: u32, height: u32) -> (u32, u32) {
    (clamp_even(width), clamp_even(height))
}

fn clamp_even(v: u32) -> u32 {
    v.clamp(MIN_DIM, MAX_DIM) & !1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Common panel shapes, so a regression on any of them is named rather than numeric.
    const ULTRAWIDE: (u32, u32) = (3440, 1440);
    const SUPER_ULTRAWIDE: (u32, u32) = (5120, 1440);
    const SIXTEEN_TEN: (u32, u32) = (1920, 1200);
    const FOUR_THREE: (u32, u32) = (1600, 1200);
    const UHD: (u32, u32) = (3840, 2160);
    const EIGHT_K: (u32, u32) = (7680, 4320);
    /// A 3:2 laptop panel: taller than 4K but well under its pixel count.
    const RETINA_3_2: (u32, u32) = (3440, 2236);

    #[test]
    fn stored_zeroes_mean_match_display() {
        assert_eq!(
            ResolutionMode::from_stored(true, 0, 0),
            ResolutionMode::MatchDisplay
        );
        assert_eq!(
            ResolutionMode::from_stored(true, 1920, 0),
            ResolutionMode::MatchDisplay,
            "a stored width without a height is still auto"
        );
    }

    #[test]
    fn stored_height_with_match_display_is_a_quality_preset() {
        assert_eq!(
            ResolutionMode::from_stored(true, 1920, 1080),
            ResolutionMode::Height(1080),
            "the stored width is advisory while match_display is on"
        );
    }

    #[test]
    fn clearing_match_display_pins_the_stored_pair() {
        assert_eq!(
            ResolutionMode::from_stored(false, 2560, 1080),
            ResolutionMode::Fixed {
                width: 2560,
                height: 1080
            }
        );
        assert_eq!(
            ResolutionMode::from_stored(false, 0, 1080),
            ResolutionMode::Height(1080),
            "a pin needs both dimensions; a zero falls back to the auto reading"
        );
    }

    #[test]
    fn match_display_records_the_panel_natively() {
        for panel in [
            ULTRAWIDE,
            SUPER_ULTRAWIDE,
            SIXTEEN_TEN,
            FOUR_THREE,
            UHD,
            RETINA_3_2,
        ] {
            assert_eq!(
                ResolutionMode::MatchDisplay.resolve(Some(panel)),
                panel,
                "native capture of {panel:?}"
            );
        }
    }

    #[test]
    fn match_display_budgets_by_area_not_by_height() {
        // The cap is a pixel count, so a panel taller than 4K but smaller than it stays native
        // rather than losing 3 % of its height for nothing.
        let (w, h) = RETINA_3_2;
        assert!(h > 2160, "the fixture must actually be taller than 4K");
        assert!(u64::from(w) * u64::from(h) < MAX_AUTO_PIXELS);
        assert_eq!(
            ResolutionMode::MatchDisplay.resolve(Some(RETINA_3_2)),
            RETINA_3_2
        );
    }

    #[test]
    fn match_display_caps_at_4k() {
        assert_eq!(
            ResolutionMode::MatchDisplay.resolve(Some(EIGHT_K)),
            (3840, 2160),
            "an 8K panel scales down to 4K, keeping 16:9"
        );
        // Whatever the shape, the result must fit the budget and keep the source's aspect.
        for panel in [EIGHT_K, (10_000, 4000), (4000, 10_000)] {
            let (w, h) = ResolutionMode::MatchDisplay.resolve(Some(panel));
            assert!(
                u64::from(w) * u64::from(h) <= MAX_AUTO_PIXELS,
                "{panel:?} resolved to {w}x{h}, over the budget"
            );
            let (sw, sh) = (f64::from(panel.0), f64::from(panel.1));
            let ratio = (f64::from(w) / f64::from(h)) / (sw / sh);
            assert!(
                (ratio - 1.0).abs() < 0.01,
                "{panel:?} → {w}x{h} changed shape"
            );
        }
    }

    #[test]
    fn a_quality_height_takes_its_width_from_the_display() {
        let cases = [
            (ULTRAWIDE, 1080, (2580, 1080)),
            (SUPER_ULTRAWIDE, 1080, (3840, 1080)),
            (SIXTEEN_TEN, 1080, (1728, 1080)),
            (FOUR_THREE, 1080, (1440, 1080)),
            (UHD, 1440, (2560, 1440)),
            (UHD, 720, (1280, 720)),
        ];
        for (panel, height, expected) in cases {
            assert_eq!(
                ResolutionMode::Height(height).resolve(Some(panel)),
                expected,
                "{height}p on {panel:?}"
            );
        }
    }

    #[test]
    fn a_quality_height_never_upscales() {
        assert_eq!(
            ResolutionMode::Height(1440).resolve(Some((1920, 1080))),
            (1920, 1080),
            "1440p on a 1080p panel stays at 1080 lines"
        );
    }

    #[test]
    fn a_pinned_size_is_honoured_verbatim() {
        assert_eq!(
            ResolutionMode::Fixed {
                width: 1920,
                height: 1080
            }
            .resolve(Some(ULTRAWIDE)),
            (1920, 1080),
            "the record path letterboxes the mismatch rather than changing the size"
        );
    }

    #[test]
    fn a_pinned_size_is_still_clamped_to_encodable_values() {
        assert_eq!(
            ResolutionMode::Fixed {
                width: 1921,
                height: 4
            }
            .resolve(None),
            (1920, 16),
            "odd width rounds down to even, a tiny height clamps up"
        );
        assert_eq!(
            ResolutionMode::Fixed {
                width: 99_999,
                height: 99_999
            }
            .resolve(None),
            (MAX_DIM, MAX_DIM)
        );
    }

    #[test]
    fn an_undetected_display_falls_back_to_the_reference_target() {
        assert_eq!(ResolutionMode::MatchDisplay.resolve(None), FALLBACK_DIMS);
        assert_eq!(ResolutionMode::Height(1080).resolve(None), (1920, 1080));
        assert_eq!(ResolutionMode::Height(720).resolve(None), (1280, 720));
        assert_eq!(ResolutionMode::Height(1440).resolve(None), (1920, 1080));
    }

    #[test]
    fn odd_source_dimensions_resolve_to_even_ones() {
        assert_eq!(
            ResolutionMode::MatchDisplay.resolve(Some((1367, 769))),
            (1366, 768)
        );
    }

    #[test]
    fn a_portrait_display_keeps_its_aspect() {
        assert_eq!(
            ResolutionMode::Height(1920).resolve(Some((1080, 1920))),
            (1080, 1920)
        );
    }

    #[test]
    fn fit_width_handles_a_degenerate_source() {
        assert_eq!(fit_width(0, 0, 1080), 1920, "no source → 16:9");
        assert_eq!(fit_width(3440, 0, 1080), 1920);
    }

    #[test]
    fn mode_height_reports_the_target_lines() {
        assert_eq!(ResolutionMode::MatchDisplay.height(), None);
        assert_eq!(ResolutionMode::Height(1440).height(), Some(1440));
        assert_eq!(
            ResolutionMode::Fixed {
                width: 2560,
                height: 1080
            }
            .height(),
            Some(1080)
        );
    }
}
