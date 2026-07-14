//! In-game save feedback for macOS: the counterpart of the Linux layer-shell badge and
//! the Windows GDI overlay. A borderless, click-through, non-activating panel at the
//! screen-saver window level whose collection behavior joins every Space — including
//! fullscreen games — so the save confirmation lands over the game the way it does on
//! the other platforms. The main thread's AppKit pump owns its lifetime: [`show`]
//! builds and orders the window in, the pump closes it once [`ActiveBadge::expired`]
//! (a newer save's badge simply replaces an older one).
//!
//! The chime stays in the shared `crate::chime` module.

use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBox, NSBoxType, NSColor, NSFont, NSFontWeightSemibold, NSImage,
    NSImageView, NSScreen, NSScreenSaverWindowLevel, NSTextField, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize, NSString};

pub use crate::chime::Accent;

/// How long the badge stays on screen; matches the Linux/Windows badges.
const BADGE_MS: Duration = Duration::from_millis(4000);
/// Gap from the top-right corner of the screen (points).
const MARGIN: f64 = 28.0;
/// Badge box metrics (points), mirroring the Linux badge's layout.
const HEIGHT: f64 = 68.0;
const PAD: f64 = 18.0;
const LOGO: f64 = 32.0;
const GAP: f64 = 14.0;
const RADIUS: f64 = 10.0;
const TEXT_PX: f64 = 17.0;
const BAR: f64 = 5.0;
/// Whole-badge opacity, matching the Linux badge's 242/255.
const BADGE_ALPHA: f64 = 242.0 / 255.0;

impl Accent {
    /// The accent bar colour: mint on success, red on failure (the Linux badge's values).
    fn rgb(self) -> (f64, f64, f64) {
        match self {
            Accent::Success => (0x2E as f64, 0xCC as f64, 0x71 as f64),
            Accent::Failure => (0xE7 as f64, 0x4C as f64, 0x3C as f64),
        }
    }
}

/// A badge currently on screen. The pump polls [`expired`](Self::expired) and calls
/// [`close`](Self::close); dropping without closing just leaves the window until exit.
pub struct ActiveBadge {
    window: Retained<NSWindow>,
    until: Instant,
}

impl ActiveBadge {
    pub fn expired(&self) -> bool {
        Instant::now() >= self.until
    }

    pub fn close(self) {
        self.window.orderOut(None);
    }
}

/// Show the save badge on the primary screen (where the recorder captures). Returns
/// `None` — so the caller can fall back to a desktop notification — when no screen is
/// available.
pub fn show(mtm: MainThreadMarker, accent: Accent, text: &str) -> Option<ActiveBadge> {
    let screen_frame = NSScreen::screens(mtm).firstObject()?.frame();

    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    let font = unsafe { NSFont::systemFontOfSize_weight(TEXT_PX, NSFontWeightSemibold) };
    label.setFont(Some(&font));
    let text_color = NSColor::colorWithSRGBRed_green_blue_alpha(
        0xF2 as f64 / 255.0,
        0xF5 as f64 / 255.0,
        0xF7 as f64 / 255.0,
        1.0,
    );
    label.setTextColor(Some(&text_color));
    label.sizeToFit();
    let text_size = label.frame().size;

    let text_left = PAD + LOGO + GAP;
    let width = (text_left + text_size.width + PAD).ceil();

    // Top-right corner of the screen; AppKit's origin is bottom-left.
    let frame = NSRect::new(
        NSPoint::new(
            screen_frame.origin.x + screen_frame.size.width - width - MARGIN,
            screen_frame.origin.y + screen_frame.size.height - HEIGHT - MARGIN,
        ),
        NSSize::new(width, HEIGHT),
    );

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), frame.size),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // The Retained<NSWindow> is the one owner; AppKit must not also release on close.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setLevel(NSScreenSaverWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    window.setIgnoresMouseEvents(true);
    window.setOpaque(false);
    let clear = NSColor::clearColor();
    window.setBackgroundColor(Some(&clear));
    window.setHasShadow(true);
    window.setFrame_display(frame, false);

    // Panel: a custom NSBox gives the rounded, translucent dark card without any
    // custom drawing.
    let panel = NSBox::new(mtm);
    panel.setBoxType(NSBoxType::Custom);
    panel.setTitlePosition(objc2_app_kit::NSTitlePosition::NoTitle);
    panel.setBorderWidth(0.0);
    panel.setCornerRadius(RADIUS);
    let panel_color = NSColor::colorWithSRGBRed_green_blue_alpha(
        0x18 as f64 / 255.0,
        0x1C as f64 / 255.0,
        0x22 as f64 / 255.0,
        BADGE_ALPHA,
    );
    panel.setFillColor(&panel_color);
    panel.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
    window.contentView()?.addSubview(&panel);

    // Accent bar down the left edge.
    let (r, g, b) = accent.rgb();
    let bar = NSBox::new(mtm);
    bar.setBoxType(NSBoxType::Custom);
    bar.setTitlePosition(objc2_app_kit::NSTitlePosition::NoTitle);
    bar.setBorderWidth(0.0);
    bar.setCornerRadius(BAR / 2.0);
    let bar_color =
        NSColor::colorWithSRGBRed_green_blue_alpha(r / 255.0, g / 255.0, b / 255.0, 1.0);
    bar.setFillColor(&bar_color);
    bar.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(BAR, HEIGHT),
    ));
    panel.addSubview(&bar);

    // Brand mark, vertically centred (best-effort — skipped if it can't be decoded).
    if let Some(logo) = brand_image() {
        let view = NSImageView::imageViewWithImage(&logo, mtm);
        view.setFrame(NSRect::new(
            NSPoint::new(PAD + 2.0, (HEIGHT - LOGO) / 2.0),
            NSSize::new(LOGO, LOGO),
        ));
        panel.addSubview(&view);
    }

    label.setFrame(NSRect::new(
        NSPoint::new(text_left, (HEIGHT - text_size.height) / 2.0),
        text_size,
    ));
    panel.addSubview(&label);

    // Regardless: the recorder is an Accessory app with no key window to defer to.
    window.orderFrontRegardless();

    Some(ActiveBadge {
        window,
        until: Instant::now() + BADGE_MS,
    })
}

/// The brand mark nearest the badge's logo size, decoded by AppKit from the embedded PNG.
fn brand_image() -> Option<Retained<NSImage>> {
    let bytes = rewynd_config::brand_png(LOGO as u32);
    let data = NSData::with_bytes(bytes);
    NSImage::initWithData(NSImage::alloc(), &data)
}
