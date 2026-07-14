//! The macOS Dock icon.
//!
//! Unbundled, the Dock shows the launching host's icon (a terminal) because there is no
//! `Info.plist` to read a `CFBundleIconFile` from. `NSApplication`'s icon can be set at
//! runtime instead, which also wins over the bundle icon once one exists — so this stays
//! correct when packaging lands. iced's `window::Settings::icon` is the *window* icon,
//! which macOS does not show at all.

use std::io::Cursor;
use std::sync::Once;

use image::ImageFormat;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::NSData;

/// The Dock renders at 128 logical points, so a 512 px master stays crisp on Retina —
/// bigger than the tray/notification ladder in `rewynd-config` carries.
const DOCK_ICON: &[u8] = include_bytes!("../../../packaging/logo-512.png");

/// The share of the icon canvas macOS' icon grid gives the artwork (the system's own
/// squircle is 824 px on a 1024 px canvas). The brand mark is full-bleed, so without this
/// margin it renders visibly larger than every neighbour in the Dock.
const ARTWORK_SCALE: f32 = 0.80;

/// Point the Dock at the brand mark, once. Called from the render path rather than
/// before the event loop: AppKit resets the application icon while finishing launching,
/// so an icon set before that is thrown away. Best-effort — a failure leaves the generic
/// icon.
pub fn set_icon() {
    static ONCE: Once = Once::new();
    if ONCE.is_completed() {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::debug!("not on the main thread; leaving the Dock icon alone");
        return;
    };
    ONCE.call_once(|| set_icon_now(mtm));
}

fn set_icon_now(mtm: MainThreadMarker) {
    let Some(png) = grid_sized_icon() else {
        tracing::warn!("could not build the Dock icon");
        return;
    };
    let data = NSData::with_bytes(&png);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        tracing::warn!("could not decode the Dock icon");
        return;
    };
    // SAFETY: main-thread-only AppKit call, guaranteed by the MainThreadMarker above.
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}

/// The brand mark centred on a transparent canvas at the icon grid's artwork scale, as PNG.
fn grid_sized_icon() -> Option<Vec<u8>> {
    let art = image::load_from_memory_with_format(DOCK_ICON, ImageFormat::Png)
        .ok()?
        .into_rgba8();
    let canvas = ((art.width() as f32 / ARTWORK_SCALE).round() as u32).max(art.width());
    let offset = (canvas - art.width()) / 2;
    let mut out = image::RgbaImage::new(canvas, canvas);
    image::imageops::replace(&mut out, &art, i64::from(offset), i64::from(offset));

    let mut png = Cursor::new(Vec::new());
    out.write_to(&mut png, ImageFormat::Png).ok()?;
    Some(png.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_canvas_leaves_the_grid_margin() {
        let png = grid_sized_icon().expect("builds");
        let icon = image::load_from_memory_with_format(&png, ImageFormat::Png).expect("decodes");
        // 512 px of artwork at 80% → a 640 px canvas, so the Dock renders the mark at the
        // same visual size as a system squircle.
        assert_eq!((icon.width(), icon.height()), (640, 640));
    }
}
