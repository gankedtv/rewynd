//! Focused-window watcher: is a fullscreen game frontmost right now, and which
//! one? The macOS counterpart of [`crate::linux`]'s `FocusWatcher`, with the same
//! public shape and fail-closed behavior.
//!
//! A background thread polls `NSWorkspace` for the frontmost application and the
//! CG window list for its windows: the app counts as "the game" when it is
//! frontmost, owns a normal-layer (0) on-screen window whose bounds equal one
//! active display's bounds, and its bundle id isn't a desktop-shell id — the
//! same conservative heuristic as the other platforms (capture too little rather
//! than too much). Transitions fire the caller's callback; when the thread exits
//! for any reason it publishes `None` first (fail closed: recording pauses
//! rather than silently capturing the desktop).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cidre::{arc, cf, cg, ns, objc, sys};
use thiserror::Error;

use super::lock_unpoisoned;
use crate::game::{GameInfo, is_shell_app_id};

/// How often the watcher samples the frontmost app + window list. Cheap queries;
/// sub-second pickup, and the poll doubles as the stop-flag check on drop.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How far window bounds may differ from a display's bounds (in points) and
/// still count as covering it — absorbs fractional-scaling rounding.
const BOUNDS_TOLERANCE: f64 = 1.0;

/// Runs on the watcher's own thread whenever the focused fullscreen game changes
/// (`Some` on focus/switch, `None` on unfocus or watcher death). It may block
/// briefly (name lookups, ring maintenance).
pub type FocusCallback = Box<dyn Fn(Option<&GameInfo>) + Send + Sync>;

/// Why the watcher could not start.
#[derive(Debug, Error)]
pub enum FocusError {
    #[error("could not spawn the focus watcher thread: {0}")]
    Spawn(std::io::Error),
}

/// cidre 0.16.1 binds `NSWorkspace` but not `frontmostApplication`; declared
/// here through the same selector machinery the rest of cidre uses.
trait WorkspaceFrontmost: objc::Obj {
    #[objc::msg_send(frontmostApplication)]
    fn frontmost_app(&self) -> Option<arc::R<ns::RunningApp>>;
}

impl WorkspaceFrontmost for ns::Workspace {}

/// State shared between the watcher thread and its readers. All transitions go
/// through [`publish`](Self::publish).
struct Shared {
    /// The focused fullscreen game right now, if any.
    current: Mutex<Option<GameInfo>>,
    /// The most recent game that was current — what the (paused) buffer still
    /// holds after a cmd-tab or a game exit.
    last: Mutex<Option<GameInfo>>,
    on_change: Option<FocusCallback>,
}

impl Shared {
    fn new(on_change: Option<FocusCallback>) -> Self {
        Self {
            current: Mutex::new(None),
            last: Mutex::new(None),
            on_change,
        }
    }

    /// Record a new focused-game observation; on change, update `last` and run
    /// the caller's callback. The callback runs outside the state locks so
    /// readers never wait on it.
    fn publish(&self, game: Option<GameInfo>) {
        {
            let mut current = lock_unpoisoned(&self.current);
            if *current == game {
                return;
            }
            if let Some(game) = &game {
                // Never log the title (documents/URLs); the bundle id is generic.
                tracing::info!(app_id = %game.app_id, "fullscreen game focused; recording");
                *lock_unpoisoned(&self.last) = Some(game.clone());
            } else {
                tracing::info!("no fullscreen game focused; replay buffer paused");
            }
            *current = game.clone();
        }
        if let Some(on_change) = &self.on_change {
            // A callback panic must not kill the watcher.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                on_change(game.as_ref());
            }));
            if outcome.is_err() {
                tracing::error!("focus change callback panicked; continuing to watch");
            }
        }
    }
}

/// Watches for the focused fullscreen game on a background thread. Dropping it
/// stops the thread and joins it.
pub struct FocusWatcher {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FocusWatcher {
    /// Start watching. `on_change` fires on the watcher's own thread for every
    /// focused-game transition.
    pub fn spawn(on_change: Option<FocusCallback>) -> Result<Self, FocusError> {
        let shared = Arc::new(Shared::new(on_change));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_shared = shared.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("rewynd-focus".to_owned())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while !thread_stop.load(Ordering::Relaxed) {
                        // Per-iteration pool: this plain thread has none, and the
                        // ObjC calls hand back autoreleased objects.
                        let game = objc::ar_pool(sample_game);
                        thread_shared.publish(game);
                        std::thread::sleep(POLL_INTERVAL);
                    }
                }));
                if outcome.is_err() {
                    tracing::error!("focus watcher thread panicked");
                }
                // Fail closed on any exit: without a live watcher, "a game is
                // focused" can't be trusted.
                thread_shared.publish(None);
            })
            .map_err(FocusError::Spawn)?;

        tracing::info!(
            backend = "nsworkspace",
            "focus watcher running (game detection)"
        );
        Ok(Self {
            shared,
            stop,
            thread: Some(thread),
        })
    }

    /// The focused fullscreen game right now, if any.
    #[must_use]
    pub fn current_game(&self) -> Option<GameInfo> {
        lock_unpoisoned(&self.shared.current).clone()
    }

    /// The most recent game that was focused fullscreen — the one the paused
    /// buffer still holds right after a cmd-tab or a game exit.
    #[must_use]
    pub fn last_game(&self) -> Option<GameInfo> {
        lock_unpoisoned(&self.shared.last).clone()
    }

    /// Which mechanism backs this watcher (for logs/diagnostics).
    #[must_use]
    pub fn backend(&self) -> &'static str {
        "nsworkspace"
    }
}

impl Drop for FocusWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One live sample: the frontmost app when it currently looks like a fullscreen
/// game.
fn sample_game() -> Option<GameInfo> {
    let front = ns::Workspace::shared().frontmost_app()?;
    let pid = front.pid();
    if pid <= 0 {
        return None;
    }
    // Unbundled processes have no bundle id; an empty app id still counts (the
    // title then names the clip folder), mirroring the Windows anti-cheat case.
    let app_id = front
        .bundle_id()
        .map(|id| id.to_string())
        .unwrap_or_default();
    if is_shell_app_id(&app_id) {
        return None;
    }
    if !has_fullscreen_window(pid) {
        return None;
    }
    let title = front
        .localized_name()
        .map(|name| name.to_string())
        .unwrap_or_default();
    Some(GameInfo {
        app_id,
        title,
        pid: u32::try_from(pid).ok(),
    })
}

/// Whether `pid` owns an on-screen, normal-layer window whose bounds equal one
/// active display's bounds (both exclusive fullscreen and borderless-fullscreen
/// land here; docked/tiled windows never cover a whole display).
fn has_fullscreen_window(pid: sys::Pid) -> bool {
    let Some(infos) = cg::WindowList::info(
        cg::WindowListOpt::ON_SCREEN_ONLY | cg::WindowListOpt::EXCLUDE_DESKTOP_ELEMENTS,
        cg::WINDOW_ID_NULL,
    ) else {
        return false;
    };
    let displays = active_display_bounds();
    infos
        .iter()
        .any(|info| window_is_fullscreen(info, pid, &displays))
}

fn window_is_fullscreen(
    info: &cf::DictionaryOf<cf::String, cf::Type>,
    pid: sys::Pid,
    displays: &[cg::Rect],
) -> bool {
    let owner = info
        .get(cg::window_keys::owner_pid())
        .and_then(cf::Type::try_as_number)
        .and_then(cf::Number::to_i64);
    if owner != Some(i64::from(pid)) {
        return false;
    }
    let layer = info
        .get(cg::window_keys::layer())
        .and_then(cf::Type::try_as_number)
        .and_then(cf::Number::to_i64);
    if layer != Some(0) {
        return false;
    }
    let Some(bounds) = info
        .get(cg::window_keys::bounds())
        .and_then(as_cf_dictionary)
        .and_then(cg::Rect::from_dictionary_representation)
    else {
        return false;
    };
    displays.iter().any(|display| rect_eq(*display, bounds))
}

/// Downcast a CF value to a dictionary (cidre exposes `try_as_number`/`_string`
/// but no dictionary variant).
fn as_cf_dictionary(value: &cf::Type) -> Option<&cf::Dictionary> {
    if value.get_type_id() == cf::Dictionary::type_id() {
        // SAFETY: the type id matches; CF references are layout-compatible.
        Some(unsafe { &*std::ptr::from_ref(value).cast::<cf::Dictionary>() })
    } else {
        None
    }
}

fn rect_eq(a: cg::Rect, b: cg::Rect) -> bool {
    (a.origin.x - b.origin.x).abs() < BOUNDS_TOLERANCE
        && (a.origin.y - b.origin.y).abs() < BOUNDS_TOLERANCE
        && (a.size.width - b.size.width).abs() < BOUNDS_TOLERANCE
        && (a.size.height - b.size.height).abs() < BOUNDS_TOLERANCE
}

/// The bounds of every active display, in the global (points) coordinate space
/// the window list reports in. cidre binds `CGDisplayBounds` but not the active
/// display list, so that one call is declared here.
fn active_display_bounds() -> Vec<cg::Rect> {
    const MAX_DISPLAYS: u32 = 16;
    let mut ids = [cg::DirectDisplayId::NULL; MAX_DISPLAYS as usize];
    let mut count: u32 = 0;
    // SAFETY: `ids` holds MAX_DISPLAYS entries; CoreGraphics writes at most that
    // many and stores the actual count in `count`.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &raw mut count) };
    if err != 0 {
        return vec![cg::DirectDisplayId::main().bounds()];
    }
    ids[..(count.min(MAX_DISPLAYS) as usize)]
        .iter()
        .map(|id| id.bounds())
        .collect()
}

// CoreGraphics is already linked through cidre's `cg` module.
unsafe extern "C-unwind" {
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut cg::DirectDisplayId,
        display_count: *mut u32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs the live window server (ignored like the GPU tests; run with
    /// `-- --ignored` on a macOS box): exercises the hand-declared
    /// `frontmostApplication` selector, the CG window list, and the active
    /// display enumeration.
    #[test]
    #[ignore]
    fn live_frontmost_and_displays() {
        let front = ns::Workspace::shared()
            .frontmost_app()
            .expect("some app is frontmost");
        assert!(front.pid() > 0);

        let displays = active_display_bounds();
        assert!(!displays.is_empty());
        assert!(displays.iter().all(|d| d.size.width > 0.0));

        // Whatever is frontmost, the sample must not panic and any hit must
        // carry the frontmost pid.
        let game = objc::ar_pool(sample_game);
        if let Some(game) = &game {
            assert_eq!(game.pid, u32::try_from(front.pid()).ok());
        }
    }
}
