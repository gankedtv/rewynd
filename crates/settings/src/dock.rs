//! The macOS Dock icon.
//!
//! Unbundled, the Dock shows the launching host's icon (a terminal) because there is no
//! `Info.plist` to read a `CFBundleIconFile` from. `NSApplication`'s icon can be set at
//! runtime instead, which also wins over the bundle icon once one exists — so this stays
//! correct when packaging lands. iced's `window::Settings::icon` is the *window* icon,
//! which macOS does not show at all.

use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::NSData;

/// The Dock renders at 128 logical points, so a 512 px master stays crisp on Retina —
/// bigger than the tray/notification ladder in `rewynd-config` carries.
const DOCK_ICON: &[u8] = include_bytes!("../../../packaging/logo-512.png");

/// Point the Dock at the brand mark. Best-effort: a failure just leaves the generic icon.
pub fn set_icon() {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::debug!("not on the main thread; leaving the Dock icon alone");
        return;
    };
    let data = NSData::with_bytes(DOCK_ICON);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        tracing::warn!("could not decode the Dock icon");
        return;
    };
    // SAFETY: main-thread-only AppKit call, guaranteed by the MainThreadMarker above.
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}
